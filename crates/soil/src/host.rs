//! Reusable `wasi:cli` + `wasi:http` store state.

use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::http_host::HttpTransport;
use crate::records_host::{RecordsBookHost, records};
use crate::stream::{StreamHub, StreamInfo, StreamSession, duplex as stream_api};
use crate::vfs_host::{VfsHost, VfsWriter, cid, index, vfs, vfs_mut};
use anyhow::Result;
use http_body_util::BodyExt;
use soil_inference_host::api as inference_api;
use wasmtime::component::{Resource, ResourceTable};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    DEFAULT_FORBIDDEN_HEADERS, Error, ForbiddenHeaderFilter, RequestOptions, WasiBody, WasiHttpCtx,
    WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
};

use crate::{
    context::HttpContext,
    creds::CredentialProvider,
    metrics::Metrics,
    policy::{HttpPolicy, WasiHostPolicy},
    unibus::Bus,
};

/// A `soil:http` installation for a Spore that serves `wasi:http`.
///
/// The policy is the same one the `runner`-world host uses — the allowlist, the
/// header rules and the budgets live in one place — and only the transport
/// differs. That is what lets a Spore reach the network through a browser
/// instead of this process's socket.
pub struct HttpCapability {
    pub(crate) policy: crate::http_host::HttpPolicy,
    pub(crate) transport: Arc<dyn HttpTransport>,
}

impl HttpCapability {
    pub fn new(policy: crate::http_host::HttpPolicy, transport: Arc<dyn HttpTransport>) -> Self {
        Self { policy, transport }
    }
}

/// One preopened host directory.
#[derive(Clone, Debug)]
pub struct Preopen {
    host: PathBuf,
    guest: String,
    perms: FsPerms,
}

/// Builder for a per-request [`WasiHost`].
#[derive(Clone, Default)]
pub struct WasiHostBuilder {
    policy: WasiHostPolicy,
    args: Vec<String>,
    env: Vec<(String, String)>,
    inherit_env: bool,
    inherit_stdio: bool,
    preopens: Vec<Preopen>,
    credentials: Option<Arc<dyn CredentialProvider>>,
    bus: Option<Arc<dyn Bus>>,
    /// Shared across requests on purpose: one SQLite store per process, not one
    /// per invocation.
    records: Option<Arc<Mutex<RecordsBookHost>>>,
    /// `soil:http` policy and route. Absent means the interface is not linked.
    http: Option<Arc<HttpCapability>>,
    /// One registry per hosted Spore: the guest writes it and the service
    /// renders it at `/metrics`. Held by the builder so every request store and
    /// the renderer are looking at the same series.
    metrics: Arc<Metrics>,
    /// Whether the manifest granted `soil:metrics`. The import is linked either
    /// way; the grant decides whether `emit` answers or refuses.
    metrics_granted: bool,
    /// Optional `soil:inference` provider. Absent answers `unsupported`.
    inference: Option<Arc<Mutex<soil_inference_host::InferenceHost>>>,
    /// Optional native codec sessions.
    pub(crate) codec: Option<Arc<Mutex<crate::codec_host::CodecHost>>>,
    /// Optional VFS. The same host serves `soil:vfs`, `soil:vfs-mut`,
    /// `soil:vfs-cid` and `soil:index`.
    vfs: Option<VfsHost>,
    /// Optional host-accepted byte streams.
    streams: Option<Arc<StreamHub>>,
}

/// What a Spore's granted dependencies look like from the host, for `/readyz`.
///
/// The gateway answers this, not the Spore: a Spore that cannot start is
/// precisely the one that cannot answer whether it is ready. A dependency the
/// host did not grant is omitted rather than reported as failing — a Spore with
/// no bus is not "unready", it simply has no bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    pub checks: Vec<(&'static str, &'static str)>,
}

impl Readiness {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.checks.iter().all(|(_, state)| *state == "ok")
    }

    /// `bus=ok,creds=ok` — one line a probe can log without parsing JSON.
    #[must_use]
    pub fn render(&self) -> String {
        self.checks
            .iter()
            .map(|(name, state)| format!("{name}={state}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl WasiHostBuilder {

    /// Which granted dependencies the host wired. This is a *grant* check, not
    /// a liveness check: a bus that is granted but whose hub is down fails at
    /// its next publish, and a probe must not force a network round trip on
    /// every scrape.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        let mut checks = Vec::new();
        if let Some(bus) = &self.bus {
            checks.push(("bus", if bus.ready() { "ok" } else { "down" }));
        }
        if let Some(credentials) = &self.credentials {
            checks.push(("creds", if credentials.ready() { "ok" } else { "down" }));
        }
        if self.records.is_some() {
            checks.push(("records", "ok"));
        }
        if self.http.is_some() {
            checks.push(("http", "ok"));
        }
        Readiness { checks }
    }
    pub fn new(policy: WasiHostPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args.extend(args);
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn inherit_env(mut self, inherit: bool) -> Self {
        self.inherit_env = inherit;
        self
    }

    pub fn inherit_stdio(mut self, inherit: bool) -> Self {
        self.inherit_stdio = inherit;
        self
    }

    pub fn preopen_dir(
        mut self,
        host: impl Into<PathBuf>,
        guest: impl Into<String>,
        perms: FsPerms,
    ) -> Self {
        self.preopens.push(Preopen {
            host: host.into(),
            guest: guest.into(),
            perms,
        });
        self
    }

    pub fn policy(&self) -> &WasiHostPolicy {
        &self.policy
    }

    /// Attach the host-side credential backend exposed as `soil:creds`.
    ///
    /// Without a provider, every `resolve`, `sign`, and `verify` call fails
    /// with `unavailable`; credentials are denied rather than defaulted.
    pub fn credential_provider(mut self, provider: Arc<dyn CredentialProvider>) -> Self {
        self.credentials = Some(provider);
        self
    }

    /// Attach the host-side bus exposed as `soil:unibus`.
    ///
    /// Without a bus, `publish` and `poll` fail with `unavailable`; a Spore
    /// cannot fall back to opening its own connection.
    pub fn bus(mut self, bus: Arc<dyn Bus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Link `soil:records` for this host. A component that imports the
    /// interface without a book configured fails at instantiation, naming the
    /// import, rather than answering `unsupported` on the first call.
    pub fn records_book(mut self, book: RecordsBookHost) -> Self {
        self.records = Some(Arc::new(Mutex::new(book)));
        self
    }

    pub fn has_records(&self) -> bool {
        self.records.is_some()
    }

    /// Link `soil:http` for this host.
    ///
    /// The capability decides what may be reached and how; without it the
    /// interface is not linked at all, so a component that imports it fails at
    /// instantiation instead of answering `unsupported` on the first fetch.
    pub fn with_http(mut self, capability: HttpCapability) -> Self {
        self.http = Some(Arc::new(capability));
        self
    }

    pub fn has_http(&self) -> bool {
        self.http.is_some()
    }

    /// The registry `/metrics` renders. Empty until the Spore emits.
    #[must_use]
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    /// Grant `soil:metrics` to this Spore.
    ///
    /// The interface is linked whether or not the grant is given, because a
    /// component that imports it must still instantiate: the Spore that asked
    /// for a capability it did not declare hears `denied` on its first emit,
    /// which is a bug it can fix, where a failed instantiation names only the
    /// import.
    pub fn metrics_granted(mut self, granted: bool) -> Self {
        self.metrics_granted = granted;
        self
    }

    /// Create fresh WASI state for one component invocation.
    pub fn build(&self) -> Result<WasiHost> {
        let mut wasi = WasiCtxBuilder::new();
        if self.inherit_stdio {
            wasi.inherit_stdio();
        }
        wasi.args(&self.args);
        if self.inherit_env {
            wasi.inherit_env();
        }
        wasi.envs(&self.env);
        for preopen in &self.preopens {
            wasi.preopened_dir(&preopen.host, &preopen.guest, preopen.perms)?;
        }
        self.policy.network.apply(&mut wasi);
        let mut http = WasiHttpCtx::new();
        http.set_field_size_limit(self.policy.http.max_field_size());
        Ok(WasiHost {
            wasi: wasi.build(),
            http,
            table: ResourceTable::new(),
            hooks: HttpHooks {
                policy: self.policy.http.clone(),
            },
            context: None,
            credentials: self.credentials.clone(),
            bus: self.bus.clone(),
            records: self.records.clone(),
            soil_http: self.http.clone(),
            metrics: self.metrics_granted.then(|| Arc::clone(&self.metrics)),
            inference: self.inference.clone(),
            codec: self.codec.clone(),
            vfs: self.vfs.clone(),
            streams: self.streams.clone(),
        })
    }

    /// Link `soil:stream` to the host's accepted-session queue.
    pub fn stream_hub(mut self, hub: StreamHub) -> Self {
        self.streams = Some(Arc::new(hub));
        self
    }

    pub fn has_streams(&self) -> bool {
        self.streams.is_some()
    }

    /// Link the VFS and its derived index.
    pub fn vfs(mut self, vfs: VfsHost) -> Self {
        self.vfs = Some(vfs);
        self
    }

    pub fn has_vfs(&self) -> bool {
        self.vfs.is_some()
    }

    /// Link `soil:inference` and route it to this provider.
    pub fn inference(mut self, host: soil_inference_host::InferenceHost) -> Self {
        self.inference = Some(Arc::new(Mutex::new(host)));
        self
    }

    pub fn codec(mut self, host: crate::codec_host::CodecHost) -> Self {
        self.codec = Some(Arc::new(Mutex::new(host)));
        self
    }

    pub fn has_codec(&self) -> bool {
        self.codec.is_some()
    }

    /// Convenience for hosts that only need a single preopen.
    pub fn preopen_read_only(self, host: impl AsRef<Path>, guest: impl Into<String>) -> Self {
        self.preopen_dir(host.as_ref(), guest, FsPerms::ReadOnly)
    }
}

/// Store data for a component that imports WASI.
pub struct WasiHost {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    hooks: HttpHooks,
    context: Option<HttpContext>,
    credentials: Option<Arc<dyn CredentialProvider>>,
    bus: Option<Arc<dyn Bus>>,
    records: Option<Arc<Mutex<RecordsBookHost>>>,
    soil_http: Option<Arc<HttpCapability>>,
    /// `Some` only when the manifest granted `soil:metrics`.
    metrics: Option<Arc<Metrics>>,
    inference: Option<Arc<Mutex<soil_inference_host::InferenceHost>>>,
    pub(crate) codec: Option<Arc<Mutex<crate::codec_host::CodecHost>>>,     vfs: Option<VfsHost>,
    streams: Option<Arc<StreamHub>>,
}

impl vfs::Host for WasiHost {
    fn ls(
        &mut self,
        parent: String,
        offset: u64,
        limit: u32,
    ) -> std::result::Result<vfs::Page, vfs::Error> {
        self.vfs
            .as_mut()
            .ok_or(vfs::Error::Denied)?
            .ls(parent, offset, limit)
    }

    fn stat(&mut self, path: String) -> std::result::Result<vfs::Entry, vfs::Error> {
        self.vfs.as_mut().ok_or(vfs::Error::Denied)?.stat(path)
    }

    fn read(
        &mut self,
        path: String,
        offset: u64,
        length: u32,
    ) -> std::result::Result<vfs::Chunk, vfs::Error> {
        self.vfs
            .as_mut()
            .ok_or(vfs::Error::Denied)?
            .read(path, offset, length)
    }
}

impl cid::Host for WasiHost {
    fn find(
        &mut self,
        content_id: String,
        offset: u64,
        limit: u32,
    ) -> std::result::Result<cid::Page, cid::Error> {
        self.vfs
            .as_mut()
            .ok_or(cid::Error::Denied)?
            .find(content_id, offset, limit)
    }
}

impl index::Host for WasiHost {
    fn browse(
        &mut self,
        parent: String,
        order: index::Order,
        offset: u64,
        limit: u32,
    ) -> std::result::Result<index::Page, index::Error> {
        self.vfs
            .as_mut()
            .ok_or(index::Error::Denied)?
            .browse(parent, order, offset, limit)
    }

    fn search(&mut self, query: index::Query) -> std::result::Result<index::Page, index::Error> {
        self.vfs.as_mut().ok_or(index::Error::Denied)?.search(query)
    }

    fn knn(&mut self, query: index::KnnQuery) -> std::result::Result<index::Page, index::Error> {
        self.vfs.as_mut().ok_or(index::Error::Denied)?.knn(query)
    }

    fn state(&mut self) -> std::result::Result<index::IndexState, index::Error> {
        self.vfs.as_mut().ok_or(index::Error::Denied)?.state()
    }

    fn refresh(&mut self) -> std::result::Result<index::IndexState, index::Error> {
        self.vfs.as_mut().ok_or(index::Error::Denied)?.refresh()
    }
}

impl vfs_mut::Host for WasiHost {
    fn begin_write(
        &mut self,
        path: String,
        media_type: String,
        expected_size: Option<u64>,
        expected_version: Option<u64>,
        mode: Option<u32>,
    ) -> std::result::Result<Resource<VfsWriter>, vfs_mut::Error> {
        let writer = self
            .vfs
            .as_ref()
            .ok_or(vfs_mut::Error::Denied)?
            .begin_writer(path, media_type, expected_size, expected_version, mode)?;
        self.table
            .push(writer)
            .map_err(|_| vfs_mut::Error::OverBudget)
    }

    fn write(
        &mut self,
        path: String,
        data: Vec<u8>,
        media_type: String,
        expected_version: Option<u64>,
        mode: Option<u32>,
    ) -> std::result::Result<vfs_mut::WriteResult, vfs_mut::Error> {
        self.vfs.as_mut().ok_or(vfs_mut::Error::Denied)?.write(
            path,
            data,
            media_type,
            expected_version,
            mode,
        )
    }

    fn chmod(
        &mut self,
        path: String,
        mode: u32,
    ) -> std::result::Result<vfs_mut::WriteResult, vfs_mut::Error> {
        self.vfs
            .as_mut()
            .ok_or(vfs_mut::Error::Denied)?
            .chmod(path, mode)
    }

    fn remove(&mut self, path: String) -> std::result::Result<(), vfs_mut::Error> {
        self.vfs
            .as_mut()
            .ok_or(vfs_mut::Error::Denied)?
            .remove(path)
    }

    fn mkdir(&mut self, path: String) -> std::result::Result<(), vfs_mut::Error> {
        self.vfs.as_mut().ok_or(vfs_mut::Error::Denied)?.mkdir(path)
    }

    fn rmdir(&mut self, path: String) -> std::result::Result<(), vfs_mut::Error> {
        self.vfs.as_mut().ok_or(vfs_mut::Error::Denied)?.rmdir(path)
    }

    fn rename(
        &mut self,
        source: String,
        target: String,
    ) -> std::result::Result<vfs_mut::WriteResult, vfs_mut::Error> {
        self.vfs
            .as_mut()
            .ok_or(vfs_mut::Error::Denied)?
            .rename(source, target)
    }
}

impl vfs_mut::HostWriter for WasiHost {
    fn write(
        &mut self,
        writer: Resource<VfsWriter>,
        data: Vec<u8>,
    ) -> std::result::Result<u64, vfs_mut::Error> {
        self.table
            .get_mut(&writer)
            .map_err(|_| vfs_mut::Error::Invalid)?
            .write(&data)
    }

    fn finish(
        &mut self,
        writer: Resource<VfsWriter>,
    ) -> std::result::Result<vfs_mut::WriteResult, vfs_mut::Error> {
        self.table
            .get_mut(&writer)
            .map_err(|_| vfs_mut::Error::Invalid)?
            .finish()
    }

    fn abort(
        &mut self,
        writer: Resource<VfsWriter>,
    ) -> std::result::Result<(), vfs_mut::Error> {
        self.table
            .get_mut(&writer)
            .map_err(|_| vfs_mut::Error::Invalid)?
            .abort()
    }

    fn drop(
        &mut self,
        writer: Resource<VfsWriter>,
    ) -> std::result::Result<(), wasmtime::Error> {
        let _ = self.table.delete(writer);
        Ok(())
    }
}

impl stream_api::Host for WasiHost {
    fn accept(
        &mut self,
        protocol: String,
    ) -> std::result::Result<Option<Resource<StreamSession>>, stream_api::Error> {
        let Some(hub) = &self.streams else {
            return Err(stream_api::Error::Denied);
        };
        let Some(session) = hub.accept(&protocol) else {
            return Ok(None);
        };
        self.table
            .push(session)
            .map(Some)
            .map_err(|_| stream_api::Error::OverBudget)
    }
}

impl stream_api::HostSession for WasiHost {
    fn info(
        &mut self,
        session: Resource<StreamSession>,
    ) -> stream_api::Info {
        let info = self
            .table
            .get(&session)
            .map(|session| session.info.clone())
            .unwrap_or_else(|_| StreamInfo {
                protocol: String::new(),
                peer: None,
                principal: None,
                media_type: None,
            });
        stream_api::Info {
            protocol: info.protocol,
            peer: info.peer,
            principal: info.principal,
            media_type: info.media_type,
        }
    }

    fn read(
        &mut self,
        session: Resource<StreamSession>,
        max: u32,
    ) -> std::result::Result<Option<Vec<u8>>, stream_api::Error> {
        self.table
            .get_mut(&session)
            .map_err(|_| stream_api::Error::Invalid)?
            .read(max)
    }

    fn write(
        &mut self,
        session: Resource<StreamSession>,
        data: Vec<u8>,
    ) -> std::result::Result<u64, stream_api::Error> {
        self.table
            .get_mut(&session)
            .map_err(|_| stream_api::Error::Invalid)?
            .write(&data)
    }

    fn close(
        &mut self,
        session: Resource<StreamSession>,
    ) -> std::result::Result<(), stream_api::Error> {
        self.table
            .get_mut(&session)
            .map_err(|_| stream_api::Error::Invalid)?
            .close()
    }

    fn drop(&mut self, session: Resource<StreamSession>) -> std::result::Result<(), wasmtime::Error> {
        let _ = self.table.delete(session);
        Ok(())
    }
}

impl crate::http_host::api::Host for WasiHost {
    fn fetch(
        &mut self,
        request: crate::http_host::api::Request,
    ) -> Result<crate::http_host::api::Response, crate::http_host::api::Error> {
        let Some(capability) = self.soil_http.as_ref() else {
            return Err(crate::http_host::api::Error::Unsupported);
        };
        crate::http_host::fetch(
            &capability.policy,
            &crate::http_host::NoCredentials,
            capability.transport.as_ref(),
            request,
        )
    }
}

impl crate::http_host::metrics_api::Host for WasiHost {
    fn emit(
        &mut self,
        samples: Vec<crate::http_host::metrics_api::Sample>,
    ) -> std::result::Result<(), crate::http_host::metrics_api::Error> {
        let Some(registry) = self.metrics.as_ref() else {
            return Err(crate::http_host::metrics_api::Error::Denied);
        };
        crate::http_host::emit_metrics(registry, samples)
    }

    fn snapshot(&mut self) -> Vec<crate::http_host::metrics_api::Sample> {
        self.metrics.as_ref().map_or_else(Vec::new, |registry| {
            crate::http_host::snapshot_metrics(registry)
        })
    }
}

impl WasiHost {
    pub fn new(policy: WasiHostPolicy) -> Result<Self> {
        WasiHostBuilder::new(policy).build()
    }

    /// Set the context reported by `fungos:http-context/current`.
    pub fn set_http_context(&mut self, context: HttpContext) {
        self.context = Some(context);
    }

    /// The current request context, when this store belongs to an HTTP call.
    pub fn http_context(&self) -> Option<&HttpContext> {
        self.context.as_ref()
    }

    pub(crate) fn credential_provider(&self) -> Option<Arc<dyn CredentialProvider>> {
        self.credentials.clone()
    }

    pub(crate) fn bus(&self) -> Option<Arc<dyn Bus>> {
        self.bus.clone()
    }

    /// Run one `soil:records` call under the shared book lock.
    ///
    /// The guard never crosses an await: every trait method is synchronous, so a
    /// blocked writer delays the next request instead of deadlocking the runtime.
    fn with_records<T>(
        &mut self,
        call: impl FnOnce(&mut RecordsBookHost) -> std::result::Result<T, records::Error>,
    ) -> std::result::Result<T, records::Error> {
        let Some(records) = &self.records else {
            return Err(records::Error::Unsupported);
        };
        let mut book = records.lock().map_err(|_| records::Error::Io)?;
        call(&mut book)
    }
}

impl records::Host for WasiHost {
    fn get(&mut self, key: records::Key) -> std::result::Result<records::Entry, records::Error> {
        self.with_records(|book| records::Host::get(book, key))
    }

    fn scan(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<records::Page, records::Error> {
        self.with_records(|book| records::Host::scan(book, collection, cursor, limit))
    }

    fn scan_packed(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<records::PackedPage, records::Error> {
        self.with_records(|book| records::Host::scan_packed(book, collection, cursor, limit))
    }

    fn upsert(
        &mut self,
        entry: records::Entry,
    ) -> std::result::Result<records::Entry, records::Error> {
        self.with_records(|book| records::Host::upsert(book, entry))
    }

    fn compare_and_swap(
        &mut self,
        key: records::Key,
        expected: Option<records::Version>,
        entry: records::Entry,
    ) -> std::result::Result<records::CasResult, records::Error> {
        self.with_records(|book| records::Host::compare_and_swap(book, key, expected, entry))
    }

    fn delete(&mut self, key: records::Key) -> std::result::Result<(), records::Error> {
        self.with_records(|book| records::Host::delete(book, key))
    }

    fn changes(
        &mut self,
        since: Option<String>,
        limit: u32,
    ) -> std::result::Result<records::Page, records::Error> {
        self.with_records(|book| records::Host::changes(book, since, limit))
    }

    fn apply(
        &mut self,
        entries: Vec<records::Entry>,
    ) -> std::result::Result<records::ApplyResult, records::Error> {
        self.with_records(|book| records::Host::apply(book, entries))
    }
}

impl inference_api::Host for WasiHost {
    fn models(&mut self) -> std::result::Result<Vec<inference_api::Model>, inference_api::Error> {
        let Some(host) = &self.inference else {
            return Err(inference_api::Error::Unsupported);
        };
        let mut host = host.lock().map_err(|_| inference_api::Error::Io)?;
        inference_api::Host::models(&mut *host)
    }

    fn complete(
        &mut self,
        request: inference_api::Request,
    ) -> std::result::Result<inference_api::Response, inference_api::Error> {
        let Some(host) = &self.inference else {
            return Err(inference_api::Error::Unsupported);
        };
        let mut host = host.lock().map_err(|_| inference_api::Error::Io)?;
        inference_api::Host::complete(&mut *host, request)
    }

    fn embed(
        &mut self,
        request: inference_api::EmbedRequest,
    ) -> std::result::Result<inference_api::EmbedResponse, inference_api::Error> {
        let Some(host) = &self.inference else {
            return Err(inference_api::Error::Unsupported);
        };
        let mut host = host.lock().map_err(|_| inference_api::Error::Io)?;
        inference_api::Host::embed(&mut *host, request)
    }

    fn voice_embed(
        &mut self,
        request: inference_api::VoiceEmbeddingRequest,
    ) -> std::result::Result<inference_api::VoiceEmbeddingResponse, inference_api::Error> {
        let Some(host) = &self.inference else {
            return Err(inference_api::Error::Unsupported);
        };
        let mut host = host.lock().map_err(|_| inference_api::Error::Io)?;
        inference_api::Host::voice_embed(&mut *host, request)
    }

    fn transcribe(
        &mut self,
        request: inference_api::TranscribeRequest,
    ) -> std::result::Result<inference_api::TranscribeResponse, inference_api::Error> {
        let Some(host) = &self.inference else {
            return Err(inference_api::Error::Unsupported);
        };
        let mut host = host.lock().map_err(|_| inference_api::Error::Io)?;
        inference_api::Host::transcribe(&mut *host, request)
    }

    fn open_stream(
        &mut self,
        _request: inference_api::Request,
    ) -> std::result::Result<Resource<inference_api::CompletionStream>, inference_api::Error> {
        Err(inference_api::Error::Unsupported)
    }

    fn state(
        &mut self,
    ) -> std::result::Result<inference_api::InferenceState, inference_api::Error> {
        let Some(host) = &self.inference else {
            return Err(inference_api::Error::Unsupported);
        };
        let mut host = host.lock().map_err(|_| inference_api::Error::Io)?;
        inference_api::Host::state(&mut *host)
    }
}

impl inference_api::HostCompletionStream for WasiHost {
    fn next(
        &mut self,
        _stream: Resource<inference_api::CompletionStream>,
        _timeout_ms: u32,
    ) -> std::result::Result<Option<inference_api::Chunk>, inference_api::Error> {
        Err(inference_api::Error::Unsupported)
    }

    fn cancel(
        &mut self,
        _stream: Resource<inference_api::CompletionStream>,
    ) -> std::result::Result<(), inference_api::Error> {
        Err(inference_api::Error::Unsupported)
    }

    fn drop(
        &mut self,
        _stream: Resource<inference_api::CompletionStream>,
    ) -> std::result::Result<(), wasmtime::Error> {
        Ok(())
    }
}

impl WasiView for WasiHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for WasiHost {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

struct HttpHooks {
    policy: HttpPolicy,
}

impl WasiHttpHooks for HttpHooks {
    fn forbidden_header_filter(&mut self) -> ForbiddenHeaderFilter {
        std::sync::Arc::new(|name| DEFAULT_FORBIDDEN_HEADERS.contains(name))
    }

    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = std::result::Result<(), Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = std::result::Result<
                    (
                        http::Response<WasiBody>,
                        Box<dyn Future<Output = std::result::Result<(), Error>> + Send>,
                    ),
                    Error,
                >,
            > + Send,
    > {
        if !self.policy.allows_uri(request.uri()) {
            drop(fut);
            return Box::new(async { Err(Error::HttpRequestDenied) });
        }
        Box::new(async move {
            let (response, body) =
                wasmtime_wasi_http::default_send_request(request, options).await?;
            let body: Box<dyn Future<Output = std::result::Result<(), Error>> + Send> =
                Box::new(body);
            Ok((response.map(BodyExt::boxed_unsync), body))
        })
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;

    struct DownBus;

    impl crate::unibus::Bus for DownBus {
        fn publish(
            &self,
            _kind: &str,
            _body: &[u8],
            _destination: Option<&str>,
            _ttl_ms: u64,
        ) -> std::result::Result<crate::unibus::BusReceipt, crate::unibus::BusError> {
            Err(crate::unibus::BusError::Unavailable)
        }

        fn poll(
            &self,
            _kinds: &[String],
            _cursor: Option<&str>,
            _limit: u32,
        ) -> std::result::Result<crate::unibus::Page, crate::unibus::BusError> {
            Err(crate::unibus::BusError::Unavailable)
        }

        fn ready(&self) -> bool {
            false
        }
    }

    struct DownCredentials;

    impl crate::creds::CredentialProvider for DownCredentials {
        fn sign(
            &self,
            _handle: &str,
            _algorithm: crate::creds::Algorithm,
            _message: &[u8],
        ) -> std::result::Result<Vec<u8>, crate::creds::CredentialError> {
            Err(crate::creds::CredentialError::Unavailable)
        }

        fn verify(
            &self,
            _handle: &str,
            _algorithm: crate::creds::Algorithm,
            _message: &[u8],
            _signature: &[u8],
        ) -> std::result::Result<bool, crate::creds::CredentialError> {
            Err(crate::creds::CredentialError::Unavailable)
        }

        fn resolve(
            &self,
            _name: &str,
        ) -> std::result::Result<crate::creds::ResolvedHandle, crate::creds::CredentialError>
        {
            Err(crate::creds::CredentialError::Invalid)
        }

        fn ready(&self) -> bool {
            false
        }
    }

    #[test]
    fn a_granted_dependency_that_is_down_is_not_ready() {
        let builder = WasiHostBuilder::new(WasiHostPolicy::default())
            .bus(std::sync::Arc::new(DownBus))
            .credential_provider(std::sync::Arc::new(DownCredentials));
        let readiness = builder.readiness();
        assert!(!readiness.is_ready());
        assert_eq!(readiness.render(), "bus=down,creds=down");
    }

    #[test]
    fn an_empty_readiness_is_ready() {
        let readiness = Readiness { checks: Vec::new() };
        assert!(readiness.is_ready());
        assert_eq!(readiness.render(), "");
    }

    fn gauge(name: &str, value: f64) -> crate::http_host::metrics_api::Sample {
        crate::http_host::metrics_api::Sample {
            name: name.into(),
            kind: crate::http_host::metrics_api::Kind::Gauge,
            value,
            help: None,
            labels: Vec::new(),
        }
    }

    #[test]
    fn a_granted_spore_writes_the_registry_the_gateway_renders() {
        use crate::http_host::metrics_api::Host as _;

        let builder = WasiHostBuilder::new(WasiHostPolicy::default()).metrics_granted(true);
        let mut host = builder.build().expect("host");
        host.emit(vec![gauge("soil_probe", 1.0)]).expect("granted");

        // The same registry the builder holds, which is what `metrics_text`
        // renders: a scrape of `/metrics` sees what the guest wrote.
        let rendered = builder.metrics().render();
        assert!(rendered.contains("# TYPE soil_probe gauge"), "{rendered}");
        assert!(rendered.contains("soil_probe 1"), "{rendered}");
    }

    #[test]
    fn an_ungranted_spore_is_denied_and_leaves_the_registry_empty() {
        use crate::http_host::metrics_api::{Error, Host as _};

        let builder = WasiHostBuilder::new(WasiHostPolicy::default());
        let mut host = builder.build().expect("host");
        assert_eq!(
            host.emit(vec![gauge("soil_probe", 1.0)]).unwrap_err(),
            Error::Denied
        );
        assert!(builder.metrics().render().is_empty());
    }

    #[test]
    fn a_failing_check_is_not_ready_and_is_named() {
        let readiness = Readiness {
            checks: vec![("bus", "ok"), ("creds", "down")],
        };
        assert!(!readiness.is_ready());
        assert_eq!(readiness.render(), "bus=ok,creds=down");
    }
}
