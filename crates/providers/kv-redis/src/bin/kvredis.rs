//! Redis implementation for wasmcloud:keyvalue.
//!
//! This implementation is multi-threaded and operations between different actors
//! use different connections and can run in parallel.
//! A single connection is shared by all instances of the same actor id (public key),
//! so there may be some brief lock contention if several instances of the same actor
//! are simultaneously attempting to communicate with redis. See documentation
//! on the [exec](#exec) function for more information.
//!
//! Note that this provider uses many *re-exported* dependencies of `wasmcloud_provider_wit_bindgen`
//! in order to reduce required dependencies on this binary itself. Using `serde` as a re-exported dependency
//! requires changing the crate location of `serde` with the `#[serde(crate = "...")]` annotation.
//!
//!
use core::future::Future;
use core::ops::{Deref as _, DerefMut as _};
use core::pin::pin;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use async_nats::HeaderMap;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt, TryStreamExt};
use once_cell::sync::Lazy;
use redis::aio::ConnectionManager;
use redis::{Cmd, FromRedisValue, Script, ScriptInvocation};
use tokio::spawn;
use tokio::{select, sync::RwLock};
use tracing::{debug, error, info, instrument, warn};

use wasmcloud_provider_sdk::core::HostData;
use wasmcloud_provider_sdk::provider::invocation_context;
use wasmcloud_provider_sdk::{
    get_connection, load_host_data, run_provider_handler, Context, LinkConfig, ProviderHandler,
    ProviderOperationResult,
};
use wrpc_transport::{AcceptedInvocation, Transmitter};

/// Default URL to use to connect to Redis
const DEFAULT_CONNECT_URL: &str = "redis://127.0.0.1:6379/";

/// Configuration key that will be used to search for Redis config
const CONFIG_REDIS_URL_KEY: &str = "URL";

static CAS: Lazy<Script> = Lazy::new(|| {
    Script::new(
        r#"if redis.call("GET", KEYS[1]) == ARGV[1] then
            return redis.call("SET", KEYS[1], ARGV[2])
        else
            return 0
        end"#,
    )
});

#[derive(Clone)]
enum DefaultConnection {
    Client(redis::Client),
    Conn(ConnectionManager),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let HostData { config, .. } = load_host_data()?;
    let client = redis::Client::open(retrieve_default_url(config))
        .context("failed to construct default Redis client")?;
    let default_connection = if let Ok(conn) = client.get_tokio_connection_manager().await {
        DefaultConnection::Conn(conn)
    } else {
        DefaultConnection::Client(client)
    };
    let provider = KvRedisProvider::new(default_connection);
    let fut = run_provider_handler(provider.clone(), "kv-redis-provider")
        .await
        .context("failed to run provider")?;
    provider.serve(fut).await?;
    eprintln!("KVRedis provider exiting");
    Ok(())
}

/// Redis keyValue provider implementation.
#[derive(Clone)]
struct KvRedisProvider {
    // store redis connections per source ID
    sources: Arc<RwLock<HashMap<String, ConnectionManager>>>,
    // default connection, which may be uninitialized
    default_connection: Arc<RwLock<DefaultConnection>>,
}

impl KvRedisProvider {
    fn new(default_connection: DefaultConnection) -> Self {
        KvRedisProvider {
            sources: Arc::default(),
            default_connection: Arc::new(RwLock::new(default_connection)),
        }
    }

    #[instrument(level = "trace", skip_all)]
    async fn get_default_connection(&self) -> anyhow::Result<ConnectionManager> {
        if let DefaultConnection::Conn(conn) = self.default_connection.read().await.deref() {
            Ok(conn.clone())
        } else {
            let mut default_conn = self.default_connection.write().await;
            match default_conn.deref_mut() {
                DefaultConnection::Conn(conn) => Ok(conn.clone()),
                DefaultConnection::Client(client) => {
                    let conn = client
                        .get_tokio_connection_manager()
                        .await
                        .context("failed to construct Redis connection manager")?;
                    *default_conn = DefaultConnection::Conn(conn.clone());
                    Ok(conn)
                }
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    async fn invocation_conn(
        &self,
        headers: Option<&HeaderMap>,
    ) -> anyhow::Result<ConnectionManager> {
        if let Some(ref source_id) = headers
            .map(invocation_context)
            .and_then(|Context { actor, .. }| actor)
        {
            let sources = self.sources.read().await;
            let Some(conn) = sources.get(source_id) else {
                error!("No Redis connection found for actor [{source_id}]. Please ensure the URL supplied in the link definition is a valid Redis URL");
                bail!("No Redis connection found for actor [{source_id}]. Please ensure the URL supplied in the link definition is a valid Redis URL")
            };
            Ok(conn.clone())
        } else {
            self.get_default_connection().await.map_err(|err| {
                error!(?err, "failed to get default connection for invocation");
                err
            })
        }
    }

    /// Execute Redis async command
    async fn exec_cmd<T: FromRedisValue>(
        &self,
        headers: Option<&HeaderMap>,
        cmd: &mut Cmd,
    ) -> anyhow::Result<T> {
        let mut conn = self.invocation_conn(headers).await?;
        match cmd.query_async(&mut conn).await {
            Ok(v) => Ok(v),
            Err(e) => {
                error!("failed to perform redis command: {e}");
                bail!("failed to perform redis command: {e}")
            }
        }
    }

    /// Execute Redis async script
    async fn exec_script<T: FromRedisValue>(
        &self,
        headers: Option<&HeaderMap>,
        cmd: &mut ScriptInvocation<'_>,
    ) -> anyhow::Result<T> {
        let mut conn = self.invocation_conn(headers).await?;
        match cmd.invoke_async(&mut conn).await {
            Ok(v) => Ok(v),
            Err(e) => {
                error!("failed to perform redis command: {e}");
                bail!("failed to perform redis command: {e}")
            }
        }
    }

    #[instrument(level = "trace", skip_all)]
    async fn serve(&self, commands: impl Future<Output = ()>) -> anyhow::Result<()> {
        let connection = get_connection();
        let wrpc = connection.get_wrpc_client(connection.provider_key());
        let mut commands = pin!(commands);
        'outer: loop {
            use wrpc_interface_keyvalue::{Atomic as _, Eventual as _};
            let delete_invocations = wrpc
                .serve_delete()
                .await
                .context("failed to serve `wrpc:keyvalue/eventual.delete` invocations")?;
            let mut delete_invocations = pin!(delete_invocations);

            let exists_invocations = wrpc
                .serve_exists()
                .await
                .context("failed to serve `wrpc:keyvalue/eventual.exists` invocations")?;
            let mut exists_invocations = pin!(exists_invocations);

            let get_invocations = wrpc
                .serve_get()
                .await
                .context("failed to serve `wrpc:keyvalue/eventual.get` invocations")?;
            let mut get_invocations = pin!(get_invocations);

            let set_invocations = wrpc
                .serve_set()
                .await
                .context("failed to serve `wrpc:keyvalue/eventual.set` invocations")?;
            let mut set_invocations = pin!(set_invocations);

            let compare_and_swap_invocations = wrpc
                .serve_compare_and_swap()
                .await
                .context("failed to serve `wrpc:keyvalue/atomic.compare-and-swap` invocations")?;
            let mut compare_and_swap_invocations = pin!(compare_and_swap_invocations);

            let increment_invocations = wrpc
                .serve_increment()
                .await
                .context("failed to serve `wrpc:keyvalue/atomic.increment` invocations")?;
            let mut increment_invocations = pin!(increment_invocations);
            loop {
                select! {
                    invocation = delete_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_delete(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:keyvalue/eventual.delete` invocation")
                            },
                            None => {
                                warn!("`wrpc:keyvalue/eventual.delete` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            }
                        }
                    }
                    invocation = exists_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_exists(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:keyvalue/eventual.exists` invocation")
                            },
                            None => {
                                warn!("`wrpc:keyvalue/eventual.exists` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            }
                        }
                    }
                    invocation = get_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_get(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:keyvalue/eventual.get` invocation")
                            },
                            None => {
                                warn!("`wrpc:keyvalue/eventual.get` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            }
                        }
                    }
                    invocation = set_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_set(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:keyvalue/eventual.set` invocation")
                            },
                            None => {
                                warn!("`wrpc:keyvalue/eventual.set` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            }
                        }
                    }
                    invocation = compare_and_swap_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_compare_and_swap(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:keyvalue/atomic.compare-and-swamp` invocation")
                            },
                            None => {
                                warn!("`wrpc:keyvalue/atomic.compare-and-swamp` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            }
                        }
                    }
                    invocation = increment_invocations.next() => {
                        match invocation {
                            Some(Ok(invocation)) => {
                                let provider = self.clone();
                                spawn(async move { provider.serve_increment(invocation).await });
                            },
                            Some(Err(err)) => {
                                error!(?err, "failed to accept `wrpc:keyvalue/atomic.increment` invocation")
                            },
                            None => {
                                warn!("`wrpc:keyvalue/atomic.increment` stream unexpectedly finished, resubscribe");
                                continue 'outer
                            }
                        }
                    }
                    _ = &mut commands => {
                        debug!("shutdown command received");
                        return Ok(())
                    }
                }
            }
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_delete<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (bucket, key),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, (String, String), Tx>,
    ) {
        // TODO: Use bucket
        _ = bucket;
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                self.exec_cmd::<()>(context.as_ref(), &mut Cmd::del(key))
                    .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_exists<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (bucket, key),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, (String, String), Tx>,
    ) {
        // TODO: Use bucket
        _ = bucket;
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                self.exec_cmd::<bool>(context.as_ref(), &mut Cmd::exists(key))
                    .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_get<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (bucket, key),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, (String, String), Tx>,
    ) {
        // TODO: Use bucket
        _ = bucket;
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                self.exec_cmd::<Bytes>(context.as_ref(), &mut Cmd::get(key))
                    .await
                    .map(Some),
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(
        level = "debug",
        skip(self, result_subject, error_subject, value, transmitter)
    )]
    async fn serve_set<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (bucket, key, value),
            error_subject,
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<
            Option<HeaderMap>,
            (String, String, impl Stream<Item = anyhow::Result<Bytes>>),
            Tx,
        >,
    ) {
        // TODO: Use bucket
        _ = bucket;
        let value: BytesMut = match value.try_collect().await {
            Ok(value) => value,
            Err(err) => {
                error!(?err, "failed to receive value");
                if let Err(err) = transmitter
                    .transmit_static(error_subject, err.to_string())
                    .await
                {
                    error!(?err, "failed to transmit error")
                }
                return;
            }
        };
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                self.exec_cmd::<()>(context.as_ref(), &mut Cmd::set(key, value.deref()))
                    .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_compare_and_swap<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (bucket, key, old, new),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, (String, String, u64, u64), Tx>,
    ) {
        // TODO: Use bucket
        _ = bucket;
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                self.exec_script::<bool>(context.as_ref(), CAS.key(key).arg(old).arg(new))
                    .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }

    /// Increments a numeric value, returning the new value
    #[instrument(level = "debug", skip(self, result_subject, transmitter))]
    async fn serve_increment<Tx: Transmitter>(
        &self,
        AcceptedInvocation {
            context,
            params: (bucket, key, value),
            result_subject,
            transmitter,
            ..
        }: AcceptedInvocation<Option<HeaderMap>, (String, String, u64), Tx>,
    ) {
        // TODO: Use bucket
        _ = bucket;
        if let Err(err) = transmitter
            .transmit_static(
                result_subject,
                self.exec_cmd::<u64>(context.as_ref(), &mut Cmd::incr(key, value))
                    .await,
            )
            .await
        {
            error!(?err, "failed to transmit result")
        }
    }
}

/// Handle provider control commands
#[async_trait]
impl ProviderHandler for KvRedisProvider {
    /// Provider should perform any operations needed for a new link,
    /// including setting up per-actor resources, and checking authorization.
    /// If the link is allowed, return true, otherwise return false to deny the link.
    #[instrument(level = "debug", skip(self, link_config), fields(source_id = %link_config.get_source_id()))]
    async fn receive_link_config_as_target(
        &self,
        link_config: impl LinkConfig,
    ) -> ProviderOperationResult<()> {
        let source_id = link_config.get_source_id();
        let conn = if let Some(url) = link_config.get_config().get(CONFIG_REDIS_URL_KEY) {
            match redis::Client::open(url.to_string()) {
                Ok(client) => match client.get_tokio_connection_manager().await {
                    Ok(conn) => {
                        info!(url, "established link");
                        conn
                    }
                    Err(err) => {
                        warn!(
                            url,
                            ?err,
                        "Could not create Redis connection manager for source [{source_id}], keyvalue operations will fail",
                    );
                        return Err(anyhow!("failed to create redis connection manager").into());
                    }
                },
                Err(err) => {
                    warn!(
                        ?err,
                        "Could not create Redis client for source [{source_id}], keyvalue operations will fail",
                    );
                    return Err(anyhow!("failed to create redis client").into());
                }
            }
        } else {
            self.get_default_connection().await.map_err(|err| {
                error!(?err, "failed to get default connection for link");
                err
            })?
        };
        let mut sources = self.sources.write().await;
        sources.insert(source_id.to_string(), conn);

        Ok(())
    }

    /// Handle notification that a link is dropped - close the connection
    #[instrument(level = "info", skip(self))]
    async fn delete_link(&self, source_id: &str) -> ProviderOperationResult<()> {
        let mut aw = self.sources.write().await;
        if let Some(conn) = aw.remove(source_id) {
            info!("redis closing connection for actor {}", source_id);
            drop(conn)
        }
        Ok(())
    }

    /// Handle shutdown request by closing all connections
    async fn shutdown(&self) -> ProviderOperationResult<()> {
        let mut aw = self.sources.write().await;
        // empty the actor link data and stop all servers
        for (_, conn) in aw.drain() {
            drop(conn)
        }
        Ok(())
    }
}

/// Fetch the default URL to use for connecting to Redis from the configuration, defaulting
/// to `DEFAULT_CONNECT_URL` if no URL is found in the configuration.
fn retrieve_default_url(config: &HashMap<String, String>) -> String {
    // To aid in user experience, find the URL key in the config that matches "URL" in a case-insensitive manner
    let config_supplied_url = config
        .keys()
        .find(|k| k.eq_ignore_ascii_case(CONFIG_REDIS_URL_KEY))
        .and_then(|url_key| config.get(url_key));

    if let Some(url) = config_supplied_url {
        info!(url, "Using Redis URL from config");
        url.to_string()
    } else {
        info!(DEFAULT_CONNECT_URL, "Using default Redis URL");
        DEFAULT_CONNECT_URL.to_string()
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use crate::retrieve_default_url;

    const PROPER_URL: &str = "redis://127.0.0.1:6379";

    #[test]
    fn can_deserialize_config_case_insensitive() {
        let lowercase_config = HashMap::from_iter([("url".to_string(), PROPER_URL.to_string())]);
        let uppercase_config = HashMap::from_iter([("URL".to_string(), PROPER_URL.to_string())]);
        let initial_caps_config = HashMap::from_iter([("Url".to_string(), PROPER_URL.to_string())]);

        assert_eq!(PROPER_URL, retrieve_default_url(&lowercase_config));
        assert_eq!(PROPER_URL, retrieve_default_url(&uppercase_config));
        assert_eq!(PROPER_URL, retrieve_default_url(&initial_caps_config));
    }
}
