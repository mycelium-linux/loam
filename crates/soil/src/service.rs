//! A minimal host for a component exporting `wasi:http/incoming-handler`.
//!
//! The host owns the listener, HTTP parsing, TLS boundary, timeouts and policy.
//! The Spore sees only `wasi:http` requests and responses.

use std::{convert::Infallible, sync::Arc};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body::Body;
use http_body_util::{BodyExt, Full};
use hyper::{server::conn::http1, service::service_fn};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, HasSelf, Linker},
};
use wasmtime_wasi_http::{
    WasiHttpView,
    io::TokioIo,
    p2::{
        bindings::{Proxy, ProxyPre, http::types::Scheme},
        body::HyperOutgoingBody,
    },
};

use crate::{
    context::HttpContext,
    creds,
    host::{WasiHost, WasiHostBuilder},
    unibus,
};

/// A compiled component plus the policy used to create per-request stores.
pub struct HttpService {
    pre: ProxyPre<WasiHost>,
    builder: WasiHostBuilder,
    spore: String,
    /// Opt-in: reuse instantiated stores so guest statics (and therefore any
    /// cache the Spore keeps) survive across requests. `None` keeps the
    /// isolation of one fresh instance per request.
    pool: Option<Arc<WarmPool>>,
}

/// A bounded set of already-instantiated Spore instances.
///
/// A `wasi:http` guest may be called more than once. Reusing an instance is
/// what lets a Spore cache derived state in its own linear memory; the cost is
/// that state must be reset by the guest between requests. The pool never
/// exceeds `max` live instances, and one instance is in flight for at most one
/// request at a time.
struct WarmPool {
    pre: ProxyPre<WasiHost>,
    builder: WasiHostBuilder,
    idle: AsyncMutex<Vec<WarmInstance>>,
    permits: Arc<Semaphore>,
    max: usize,
}

struct WarmInstance {
    store: Store<WasiHost>,
    proxy: Proxy,
}

impl WarmPool {
    fn new(pre: ProxyPre<WasiHost>, builder: WasiHostBuilder, max: usize) -> Self {
        Self {
            pre,
            builder,
            idle: AsyncMutex::new(Vec::with_capacity(max)),
            permits: Arc::new(Semaphore::new(max)),
            max,
        }
    }

    async fn instantiate(&self) -> Result<WarmInstance> {
        let mut store = Store::new(self.pre.engine(), self.builder.build()?);
        let proxy = self.pre.instantiate_async(&mut store).await?;
        Ok(WarmInstance { store, proxy })
    }

    /// Instantiate `max` instances before the first request, so the warm path
    /// does not pay component start-up.
    async fn warm(&self) -> Result<()> {
        let mut idle = self.idle.lock().await;
        while idle.len() < self.max {
            idle.push(self.instantiate().await?);
        }
        Ok(())
    }

    /// Take one instance, or create one when the pool is still filling.
    async fn checkout(&self) -> Result<(WarmInstance, OwnedSemaphorePermit)> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("warm instance pool is closed"))?;
        let warm = match self.idle.lock().await.pop() {
            Some(warm) => warm,
            None => self.instantiate().await?,
        };
        Ok((warm, permit))
    }

    async fn checkin(&self, warm: WarmInstance) {
        self.idle.lock().await.push(warm);
    }
}

impl HttpService {
    /// Prometheus text for this Spore's registry, served by the gateway at
    /// `/metrics`. Empty is a valid answer: a Spore that was not granted
    /// `soil:metrics` has nothing to say, and that is not an error.
    #[must_use]
    pub fn metrics_text(&self) -> String {
        self.builder.metrics().render()
    }

    /// The host's view of this Spore's granted dependencies, for the gateway's
    /// `/readyz`. Answered by the host, never by the guest.
    #[must_use]
    pub fn readiness(&self) -> crate::host::Readiness {
        self.builder.readiness()
    }

    /// Reuse up to `instances` warm Spore instances across requests.
    ///
    /// This is opt-in and only for guests that keep per-request state in the
    /// call, not in globals they forget to reset. It makes an in-guest cache
    /// possible; it does not make one safe by itself.
    #[must_use]
    pub fn with_pool(mut self, instances: usize) -> Self {
        if instances > 0 {
            self.pool = Some(Arc::new(WarmPool::new(
                self.pre.clone(),
                self.builder.clone(),
                instances,
            )));
        }
        self
    }

    /// Instantiate the pool before serving. A no-op when pooling is off.
    pub async fn warm(&self) -> Result<()> {
        match &self.pool {
            Some(pool) => pool.warm().await,
            None => Ok(()),
        }
    }

    pub fn from_bytes(bytes: &[u8], builder: WasiHostBuilder) -> Result<Self> {
        Self::from_bytes_named("spore", bytes, builder)
    }

    /// Compile a component and give it the identity reported by
    /// `fungos:http-context`.
    pub fn from_bytes_named(
        spore: impl Into<String>,
        bytes: &[u8],
        builder: WasiHostBuilder,
    ) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let component = Component::new(&engine, bytes)
            .map_err(anyhow::Error::from)
            .context("compile wasi:http component")?;
        Self::from_component_named(spore, component, builder)
    }

    pub fn from_component(component: Component, builder: WasiHostBuilder) -> Result<Self> {
        Self::from_component_named("spore", component, builder)
    }

    pub fn from_component_named(
        spore: impl Into<String>,
        component: Component,
        builder: WasiHostBuilder,
    ) -> Result<Self> {
        let mut linker = Linker::new(component.engine());
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(anyhow::Error::from)
            .context("link WASI p2")?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)
            .map_err(anyhow::Error::from)
            .context("link wasi:http")?;
        crate::context::add_to_linker(&mut linker)?;
        soil_inference_host::add_to_linker(&mut linker).context("link soil:inference")?;
        creds::add_to_linker(&mut linker)?;
        unibus::add_to_linker(&mut linker)?;
        crate::time::add_to_linker(&mut linker)?;
        // Linked whether or not the Spore was granted `soil:metrics`: the grant
        // is what `emit` answers with, not whether the import resolves.
        crate::http_host::metrics_api::add_to_linker::<
            super::host::WasiHost,
            HasSelf<super::host::WasiHost>,
        >(&mut linker, |state| state)?;
        crate::stream::add_to_linker(&mut linker)?;
        if builder.has_codec() {
            crate::codec_host::add_to_linker::<WasiHost>(&mut linker)
                .context("link soil:codec")?;
        }
        if builder.has_vfs() {
            crate::vfs_host::add_to_linker::<WasiHost>(&mut linker).context("link soil:vfs")?;
            crate::vfs_host::add_mutation_to_linker::<WasiHost>(&mut linker)
                .context("link soil:vfs-mut")?;
        }
        if builder.has_records() {
            crate::records_host::add_to_linker(&mut linker).context("link soil:records")?;
        }
        if builder.has_http() {
            crate::http_host::api::add_to_linker::<
                super::host::WasiHost,
                HasSelf<super::host::WasiHost>,
            >(&mut linker, |state| state)?;
        }
        let pre = ProxyPre::new(linker.instantiate_pre(&component)?)
            .map_err(anyhow::Error::from)
            .context("component must export wasi:http/incoming-handler")?;
        let spore = spore.into();
        if spore.trim().is_empty() {
            bail!("spore name must not be empty");
        }
        Ok(Self {
            pre,
            builder,
            spore,
            pool: None,
        })
    }

    /// The host-granted Spore name reported in the request context.
    pub fn spore(&self) -> &str {
        &self.spore
    }

    /// Serve HTTP/1.1 until the listener fails or the task is cancelled.
    ///
    /// This single-component entrypoint has no vhost or route grant. A host
    /// that serves more than one Spore or enforces route policy uses
    /// [`crate::HttpGateway`] instead.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        loop {
            let (client, remote) = listener.accept().await?;
            let service = Arc::clone(&self);
            tokio::spawn(async move {
                let result =
                    http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(
                            TokioIo::new(client),
                            service_fn(move |request| {
                                let service = Arc::clone(&service);
                                let context = HttpContext::new(
                                    service.spore.clone(),
                                    "/",
                                    Some(remote.to_string()),
                                    None,
                                );
                                async move {
                                    Ok::<_, Infallible>(service.handle(request, context).await)
                                }
                            }),
                        )
                        .await;
                if let Err(error) = result {
                    eprintln!("soil-wasi-http: {remote}: {error}");
                }
            });
        }
    }

    /// Handle one request with an explicit host context.
    ///
    /// The response is always an HTTP response; invocation and guest failures
    /// become an explicit `500` with the failure reason in the body.
    pub async fn handle<B>(
        &self,
        request: hyper::Request<B>,
        context: HttpContext,
    ) -> hyper::Response<HyperOutgoingBody>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<wasmtime_wasi_http::Error>,
    {
        match self.try_handle(request, context).await {
            Ok(response) => response,
            Err(error) => error_response(format!("{error:#}")),
        }
    }

    async fn try_handle<B>(
        &self,
        request: hyper::Request<B>,
        context: HttpContext,
    ) -> Result<hyper::Response<HyperOutgoingBody>>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<wasmtime_wasi_http::Error>,
    {
        if let Some(pool) = &self.pool {
            return Self::try_handle_pooled(Arc::clone(pool), request, context).await;
        }
        let mut store = Store::new(self.pre.engine(), self.builder.build()?);
        store.data_mut().set_http_context(context);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let request = store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, request)?;
        let outparam = store.data_mut().http().new_response_outparam(sender)?;
        let pre = self.pre.clone();
        let task = TaskGuard::new(tokio::spawn(async move {
            let proxy = pre.instantiate_async(&mut store).await?;
            proxy
                .wasi_http_incoming_handler()
                .call_handle(store, request, outparam)
                .await?;
            Ok::<(), anyhow::Error>(())
        }));

        match receiver.await {
            Ok(Ok(response)) => {
                // The guest has handed over the response but may still be
                // writing its body. Let the task finish; dropping the guard
                // would abort a stream the caller is still reading.
                task.disarm();
                Ok(response)
            }
            Ok(Err(error)) => {
                if let Some(task) = task.disarm() {
                    let _ = task.await;
                }
                bail!("guest returned {error:?}");
            }
            Err(_) => match task.disarm() {
                Some(task) => match task.await {
                    Ok(Ok(())) => bail!("guest never set a response"),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.into()),
                },
                None => bail!("guest task vanished"),
            },
        }
    }

    /// One request through a reusable instance.
    ///
    /// The response head is returned as soon as the guest sets it; the instance
    /// goes back to the pool only after the guest call (including body writes)
    /// finishes. A guest that keeps writing after handing over the head still
    /// owns the instance, and the permit, until it is done.
    async fn try_handle_pooled<B>(
        pool: Arc<WarmPool>,
        request: hyper::Request<B>,
        context: HttpContext,
    ) -> Result<hyper::Response<HyperOutgoingBody>>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<wasmtime_wasi_http::Error>,
    {
        let (mut warm, permit) = pool.checkout().await?;
        warm.store.data_mut().set_http_context(context);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let request = warm
            .store
            .data_mut()
            .http()
            .new_incoming_request(Scheme::Http, request)?;
        let outparam = warm.store.data_mut().http().new_response_outparam(sender)?;
        let WarmInstance { mut store, proxy } = warm;
        let task = tokio::spawn(async move {
            let result = proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, request, outparam)
                .await;
            (store, proxy, result, permit)
        });

        match receiver.await {
            Ok(Ok(response)) => {
                // The head is ready. Return it now and let the guest finish the
                // body; the instance returns to the pool only after that.
                let pool = Arc::clone(&pool);
                tokio::spawn(async move {
                    let abort = task.abort_handle();
                    match tokio::time::timeout(std::time::Duration::from_secs(30), task).await {
                        Ok(Ok((store, proxy, Ok(()), permit))) => {
                            pool.checkin(WarmInstance { store, proxy }).await;
                            drop(permit);
                        }
                        Ok(_) => {}
                        Err(_) => abort.abort(),
                    }
                });
                Ok(response)
            }
            Ok(Err(error)) => {
                let _ = task.await;
                bail!("guest returned {error:?}");
            }
            Err(_) => {
                let _ = task.await;
                bail!("guest never set a response")
            }
        }
    }
}

/// Abort a guest task if the request is dropped before it produces a response.
struct TaskGuard(Option<tokio::task::JoinHandle<anyhow::Result<()>>>);

impl TaskGuard {
    fn new(task: tokio::task::JoinHandle<anyhow::Result<()>>) -> Self {
        Self(Some(task))
    }

    fn disarm(mut self) -> Option<tokio::task::JoinHandle<anyhow::Result<()>>> {
        self.0.take()
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

fn error_response(message: String) -> hyper::Response<HyperOutgoingBody> {
    let body = Full::new(Bytes::from(message))
        .map_err(|never| match never {})
        .boxed_unsync();
    hyper::Response::builder()
        .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(body)
        .expect("static response")
}
