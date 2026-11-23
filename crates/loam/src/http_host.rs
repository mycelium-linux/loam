//! Bounded host implementation of `soil:http`.
//!
//! The guest names an absolute URL and a host-resolved credential handle. The
//! host owns the origin allowlist, request headers, TLS, redirects, body cap,
//! deadline and credential material. No socket, DNS API, proxy setting or
//! secret crosses the component boundary.

use std::{collections::BTreeSet, io::Read, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use url::Url;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit/http",
    world: "runner",
    with: {
        "soil:http/http.response-stream": ResponseStream,
    },
});

use self::soil::http::http;

// The generated `soil:metrics` interface, shared by both hosts that link it:
// the `runner` world's [`HttpHost`] and the `wasi:http` surface's [`WasiHost`].
pub(crate) use self::soil::metrics::metrics as metrics_api;

// The generated interface, re-exported so a host that links `soil:http` from
// its own store (rather than running a `runner`-world component) implements the
// same trait this crate does instead of a second copy of it.
pub use self::soil::http::http as api;

const ALLOWED_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "content-type",
    "user-agent",
    "if-none-match",
    "if-modified-since",
    "range",
];

/// A guest puts this literal in a URL path when the host should resolve a
/// credential into that segment instead of injecting it as a header.
pub const CREDENTIAL_MARKER: &str = "__CREDENTIAL__";

/// Percent-encode one path segment while preserving the RFC 3986 unreserved
/// characters (`-`, `.`, `_`, `~`).
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// One request in host-internal form.
///
/// The WIT types are generated per consumer, so two crates that import
/// `soil:http` cannot pass them to each other. Everything between the guest
/// boundary and the transport speaks these plain types instead, which is what
/// lets another host (mush) reuse this policy rather than keep a second copy.
#[derive(Clone, Debug)]
pub struct Request {
    pub url: String,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub credential: Option<String>,
    pub max_bytes: u32,
    pub timeout_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Origin, method, header, or credential was not granted.
    Denied,
    /// URL, header, credential handle, or budget is malformed.
    Invalid,
    /// The caller exceeded the host response/time budget.
    OverBudget,
    /// The request exceeded its deadline.
    Timeout,
    /// DNS, connect, or proxy failure.
    Unavailable,
    /// TLS verification or handshake failure.
    Tls,
    /// Upstream returned an error that is not a normal HTTP response.
    Http,
    /// The host transport failed.
    Io,
    /// This host has no HTTP backend.
    Unsupported,
}

/// Host-owned policy for one `soil:http` installation.
#[derive(Clone, Debug)]
pub struct HttpPolicy {
    origins: BTreeSet<String>,
    max_bytes: usize,
    timeout: Duration,
}

impl HttpPolicy {
    pub fn new(
        origins: impl IntoIterator<Item = String>,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<Self> {
        let mut allowed = BTreeSet::new();
        for origin in origins {
            allowed.insert(normalize_origin(&origin)?);
        }
        if max_bytes == 0 {
            anyhow::bail!("max_bytes must be positive");
        }
        Ok(Self {
            origins: allowed,
            max_bytes,
            timeout,
        })
    }

    pub fn allow(&mut self, origin: &str) -> Result<()> {
        self.origins.insert(normalize_origin(origin)?);
        Ok(())
    }

    pub fn origins(&self) -> impl Iterator<Item = &str> {
        self.origins.iter().map(String::as_str)
    }
}

/// Host credential resolver. A Geode adapter implements this trait; the
/// Spore only ever supplies the opaque handle.
pub trait CredentialProvider: Send + Sync {
    /// Return the header name/value to inject for `handle`.
    fn resolve(&self, handle: &str) -> Result<Option<(String, String)>>;

    /// Return a path segment to substitute for `credential`, when the host is
    /// configured to resolve path credentials.
    fn resolve_path(&self, _handle: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

#[derive(Default)]
pub struct NoCredentials;

impl CredentialProvider for NoCredentials {
    fn resolve(&self, _handle: &str) -> Result<Option<(String, String)>> {
        Ok(None)
    }
}

/// One bearer token, handed to a guest that asks for the `default` handle.
///
/// The token stays in the host process: a CLI can read it from the environment
/// so it never appears in a Spore's arguments, its output, or `ps`.
pub struct BearerCredential {
    value: String,
}

impl BearerCredential {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            value: format!("Bearer {}", token.into()),
        }
    }
}

impl CredentialProvider for BearerCredential {
    fn resolve(&self, handle: &str) -> Result<Option<(String, String)>> {
        Ok((handle == "default").then(|| ("authorization".to_string(), self.value.clone())))
    }
}

/// Geode as the HTTP host's credential resolver: the guest names a handle, the
/// host owns the reference, the purpose and whether the bytes become a header or
/// a path segment. One lease client serves both directions — `creds::GeodeCredentials`
/// is the implementation, and this impl is the view the HTTP host sees. A guest
/// can no longer spell a `geode://` reference of its own.
#[cfg(all(unix, feature = "creds", feature = "wasi"))]
impl CredentialProvider for crate::creds::GeodeCredentials {
    fn resolve(&self, handle: &str) -> Result<Option<(String, String)>> {
        let Some((spec, bytes)) = self
            .host_lease(handle)
            .map_err(|error| anyhow::anyhow!("geode lease for {handle}: {error:?}"))?
        else {
            return Ok(None);
        };
        let Some(header) = spec.header else {
            // A path credential presented as a header credential: refuse rather
            // than put a path secret in a header.
            return Ok(None);
        };
        let value = String::from_utf8(bytes).context("geode secret is not UTF-8")?;
        Ok(Some((header, value)))
    }

    fn resolve_path(&self, handle: &str) -> Result<Option<String>> {
        let Some((spec, bytes)) = self
            .host_lease(handle)
            .map_err(|error| anyhow::anyhow!("geode lease for {handle}: {error:?}"))?
        else {
            return Ok(None);
        };
        if spec.header.is_some() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8(bytes).context("geode secret is not UTF-8")?,
        ))
    }
}

/// One host-internal transport request. The Spore never sees this shape.
#[derive(Clone, Debug)]
pub struct TransportRequest {
    pub url: Url,
    pub method: http::Method,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl TransportRequest {
    /// The `soil:http` enum value (`get`, `post`, …).
    ///
    /// Not the HTTP verb: `ureq` wants `GET`, the WIT wants `get`, and a
    /// transport that speaks the value rather than this crate's enum should not
    /// have to name the generated type to read it.
    pub fn method_name(&self) -> &'static str {
        match self.method {
            http::Method::Get => "get",
            http::Method::Head => "head",
            http::Method::Post => "post",
            http::Method::Put => "put",
            http::Method::Patch => "patch",
            http::Method::Delete => "delete",
        }
    }

    /// Build a request from the names a wire transport uses.
    ///
    /// The generated `Method` enum is not part of this crate's public surface,
    /// so an out-of-tree transport (and its tests) needs a way in that does not
    /// depend on it.
    pub fn from_parts(
        url: &str,
        method: &str,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Self> {
        let method = match method.to_ascii_lowercase().as_str() {
            "get" => http::Method::Get,
            "head" => http::Method::Head,
            "post" => http::Method::Post,
            "put" => http::Method::Put,
            "patch" => http::Method::Patch,
            "delete" => http::Method::Delete,
            other => anyhow::bail!("{other:?} is not a soil:http method"),
        };
        Ok(Self {
            url: Url::parse(url).with_context(|| format!("parse {url}"))?,
            method,
            headers,
            body,
            timeout,
            max_bytes,
        })
    }
}

#[derive(Clone, Debug)]
pub struct TransportResponse {
    pub status: u16,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A response whose body is pulled from the transport, not buffered here.
pub struct TransportStream {
    pub status: u16,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub reader: Box<dyn Read + Send + Sync>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The transport cannot serve this request at all, so a composed transport
    /// may try another one. This is the only variant that means "not me"; every
    /// other one is an answer.
    Unsupported,
    Denied,
    OverBudget,
    Timeout,
    Unavailable,
    Tls,
    Http,
    Io,
}

/// A host-owned HTTP transport. Direct DNS/TLS is the first implementation.
///
/// An Iroh/Mycelium transport implements the same trait: resolve a peer or
/// service to an Iroh endpoint, open a stream, and speak HTTP over it. A router
/// transport can choose direct vs Iroh from the Mycelium directory or observed
/// health. The Spore never selects the route.
pub trait HttpTransport: Send + Sync {
    fn fetch(&self, request: &TransportRequest) -> Result<TransportResponse, TransportError>;

    /// Open a response without buffering its body. A transport that cannot
    /// stream returns `Unsupported`; the default keeps existing transports
    /// source-compatible.
    fn open(&self, _request: &TransportRequest) -> Result<TransportStream, TransportError> {
        Err(TransportError::Unsupported)
    }
}

/// Prefer one transport, fall back to another when the first says it cannot
/// serve the request.
///
/// The distinction is deliberate: [`TransportError::Unsupported`] means "not
/// this transport", and everything else — including a timeout or a refusal — is
/// an answer that must reach the caller. A fallback on *failure* would silently
/// route around a provider that explicitly declined a URL.
pub struct PreferProvider {
    provider: Arc<dyn HttpTransport>,
    fallback: Arc<dyn HttpTransport>,
}

impl PreferProvider {
    pub fn new(provider: Arc<dyn HttpTransport>, fallback: Arc<dyn HttpTransport>) -> Self {
        Self { provider, fallback }
    }
}

impl HttpTransport for PreferProvider {
    fn fetch(&self, request: &TransportRequest) -> Result<TransportResponse, TransportError> {
        match self.provider.fetch(request) {
            Err(TransportError::Unsupported) => self.fallback.fetch(request),
            other => other,
        }
    }

    fn open(&self, request: &TransportRequest) -> Result<TransportStream, TransportError> {
        match self.provider.open(request) {
            Err(TransportError::Unsupported) => self.fallback.open(request),
            other => other,
        }
    }
}

#[derive(Default)]
pub struct DirectTransport;

impl DirectTransport {
    fn send(&self, request: &TransportRequest) -> Result<ureq::Response, TransportError> {
        let agent = ureq::AgentBuilder::new()
            .timeout(request.timeout)
            .redirects(0)
            .build();
        let mut call = agent.request(method_name(request.method), request.url.as_str());
        for (name, value) in &request.headers {
            call = call.set(name, value);
        }
        let sent = match &request.body {
            Some(body) => call.send_bytes(body),
            None => call.call(),
        };
        match sent {
            Ok(response) => Ok(response),
            Err(ureq::Error::Status(_, response)) => Ok(response),
            Err(ureq::Error::Transport(error)) => {
                let text = error.to_string().to_ascii_lowercase();
                Err(
                    if text.contains("tls")
                        || text.contains("certificate")
                        || text.contains("handshake")
                    {
                        TransportError::Tls
                    } else {
                        TransportError::Unavailable
                    },
                )
            }
        }
    }
}

impl HttpTransport for DirectTransport {
    fn fetch(&self, request: &TransportRequest) -> Result<TransportResponse, TransportError> {
        let response = self.send(request)?;
        let status = response.status();
        let final_url = request.url.to_string();
        let mut headers = Vec::new();
        for name in response.headers_names() {
            if let Some(value) = response.header(&name) {
                headers.push((name, value.to_string()));
            }
        }
        let mut body = Vec::new();
        if request.method != http::Method::Head {
            response
                .into_reader()
                .take(request.max_bytes as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|_| TransportError::Io)?;
            if body.len() > request.max_bytes {
                return Err(TransportError::OverBudget);
            }
        }
        Ok(TransportResponse {
            status,
            final_url,
            headers,
            body,
        })
    }

    fn open(&self, request: &TransportRequest) -> Result<TransportStream, TransportError> {
        let response = self.send(request)?;
        let status = response.status();
        let final_url = request.url.to_string();
        let mut headers = Vec::new();
        for name in response.headers_names() {
            if let Some(value) = response.header(&name) {
                headers.push((name, value.to_string()));
            }
        }
        Ok(TransportStream {
            status,
            final_url,
            headers,
            reader: response.into_reader(),
        })
    }
}

fn method_name(method: http::Method) -> &'static str {
    match method {
        http::Method::Get => "GET",
        http::Method::Head => "HEAD",
        http::Method::Post => "POST",
        http::Method::Put => "PUT",
        http::Method::Patch => "PATCH",
        http::Method::Delete => "DELETE",
    }
}

pub struct HttpHost {
    policy: HttpPolicy,
    metrics: Arc<crate::metrics::Metrics>,
    credentials: Arc<dyn CredentialProvider>,
    transport: Arc<dyn HttpTransport>,
    table: ResourceTable,
    wasi: WasiCtx,
}

impl HttpHost {
    pub fn new(policy: HttpPolicy) -> Self {
        Self {
            policy,
            metrics: Arc::new(crate::metrics::Metrics::new()),
            credentials: Arc::new(NoCredentials),
            transport: Arc::new(DirectTransport),
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new()
                .inherit_stdout()
                .inherit_stderr()
                .build(),
        }
    }

    pub fn with_credentials(mut self, credentials: Arc<dyn CredentialProvider>) -> Self {
        self.credentials = credentials;
        self
    }

    /// Replace the host transport. Used by a direct/Iroh router or a test
    /// transport; the guest cannot select it.
    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// The registry this host writes to; a caller renders it for `/metrics`.
    ///
    /// This host is built by the process that runs the component, so linking
    /// the import *is* the grant: there is no manifest here to ask.
    #[must_use]
    pub fn metrics(&self) -> &Arc<crate::metrics::Metrics> {
        &self.metrics
    }

    pub fn policy(&self) -> &HttpPolicy {
        &self.policy
    }
}

impl WasiView for HttpHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl HttpHost {
    /// Run one request through the policy and the transport.
    ///
    /// This is the whole `soil:http` contract in plain types; the guest adapter
    /// below only converts. `perform` is free-standing for the same reason: mush
    /// links its own `soil:http` instance and calls it rather than keeping a
    /// second answer to "is this request allowed".
    pub fn perform(&mut self, request: Request) -> Result<Response, Error> {
        perform(&self.policy, &*self.credentials, &*self.transport, request)
    }
}

impl http::HostResponseStream for HttpHost {
    fn status(&mut self, stream: wasmtime::component::Resource<ResponseStream>) -> u16 {
        self.table
            .get(&stream)
            .map(ResponseStream::status)
            .unwrap_or_default()
    }

    fn final_url(&mut self, stream: wasmtime::component::Resource<ResponseStream>) -> String {
        self.table
            .get(&stream)
            .map(|stream| stream.final_url().to_string())
            .unwrap_or_default()
    }

    fn headers(
        &mut self,
        stream: wasmtime::component::Resource<ResponseStream>,
    ) -> Vec<http::Header> {
        self.table
            .get(&stream)
            .map(|stream| {
                stream
                    .headers()
                    .iter()
                    .map(|(name, value)| http::Header {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn read(
        &mut self,
        stream: wasmtime::component::Resource<ResponseStream>,
        max: u32,
    ) -> Result<Option<Vec<u8>>, http::Error> {
        self.table
            .get_mut(&stream)
            .map_err(|_| http::Error::Invalid)?
            .read(max)
            .map_err(error_to_wit)
    }

    fn close(
        &mut self,
        stream: wasmtime::component::Resource<ResponseStream>,
    ) -> Result<(), http::Error> {
        self.table
            .get_mut(&stream)
            .map_err(|_| http::Error::Invalid)?
            .close()
            .map_err(error_to_wit)
    }

    fn drop(
        &mut self,
        stream: wasmtime::component::Resource<ResponseStream>,
    ) -> Result<(), wasmtime::Error> {
        let _ = self.table.delete(stream);
        Ok(())
    }
}

impl http::Host for HttpHost {
    fn fetch(&mut self, request: http::Request) -> Result<http::Response, http::Error> {
        fetch(&self.policy, &*self.credentials, &*self.transport, request)
    }

    fn open(
        &mut self,
        request: http::Request,
    ) -> Result<wasmtime::component::Resource<http::ResponseStream>, http::Error> {
        let stream = open_stream(
            &self.policy,
            &*self.credentials,
            &*self.transport,
            Request {
                url: request.url,
                method: method_from_wit(request.method),
                headers: request
                    .headers
                    .into_iter()
                    .map(|header| (header.name, header.value))
                    .collect(),
                body: request.body,
                credential: request.credential,
                max_bytes: request.max_bytes,
                timeout_ms: request.timeout_ms,
            },
        )
        .map_err(error_to_wit)?;
        self.table.push(stream).map_err(|_| http::Error::OverBudget)
    }
}

/// The one implementation of `soil:http` policy, in host-internal types.
///
/// Every host that links the interface calls this: the `runner`-world host
/// above, `soil` through `fetch` below, and mush directly. A second
/// copy would be a second answer to "is this request allowed".
pub fn perform(
    policy: &HttpPolicy,
    credentials: &dyn CredentialProvider,
    transport: &dyn HttpTransport,
    request: Request,
) -> Result<Response, Error> {
    let (transport_request, path_secret) = prepare(policy, credentials, request)?;
    let response = transport
        .fetch(&transport_request)
        .map_err(transport_error)?;
    let final_url = redact_final_url(response.final_url, path_secret.as_ref());
    Ok(Response {
        status: response.status,
        final_url,
        headers: response.headers,
        body: response.body,
    })
}

/// A response resource kept in the host's `ResourceTable`.
pub struct ResponseStream {
    status: u16,
    final_url: String,
    headers: Vec<(String, String)>,
    reader: Box<dyn Read + Send + Sync>,
    remaining: usize,
    done: bool,
}

impl ResponseStream {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn final_url(&self) -> &str {
        &self.final_url
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn read(&mut self, max: u32) -> Result<Option<Vec<u8>>, Error> {
        if max == 0 {
            return Err(Error::Invalid);
        }
        if self.done {
            return Ok(None);
        }
        let want = (max as usize).min(self.remaining).saturating_add(1);
        let mut buffer = vec![0; want];
        match self.reader.read(&mut buffer) {
            Ok(0) => {
                self.done = true;
                Ok(None)
            }
            Ok(read) => {
                if read > self.remaining {
                    self.done = true;
                    return Err(Error::OverBudget);
                }
                self.remaining -= read;
                buffer.truncate(read);
                Ok(Some(buffer))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => Ok(None),
            Err(_) => Err(Error::Io),
        }
    }

    pub fn close(&mut self) -> Result<(), Error> {
        self.done = true;
        Ok(())
    }
}

/// Open a streamed response through the same policy and credential path as
/// `fetch`. The caller owns the returned resource.
pub fn open_stream(
    policy: &HttpPolicy,
    credentials: &dyn CredentialProvider,
    transport: &dyn HttpTransport,
    request: Request,
) -> Result<ResponseStream, Error> {
    let (transport_request, path_secret) = prepare(policy, credentials, request)?;
    // The transport request already reflects the host cap; use its max_bytes as
    // the total body budget for this response.
    let max_bytes = transport_request.max_bytes;
    let response = transport
        .open(&transport_request)
        .map_err(transport_error)?;
    Ok(ResponseStream {
        status: response.status,
        final_url: redact_final_url(response.final_url, path_secret.as_ref()),
        headers: response.headers,
        reader: response.reader,
        remaining: max_bytes,
        done: false,
    })
}

fn prepare(
    policy: &HttpPolicy,
    credentials: &dyn CredentialProvider,
    request: Request,
) -> Result<(TransportRequest, Option<(String, String)>), Error> {
    let mut request_url = request.url.clone();
    let mut path_secret = None;
    if request_url.contains(CREDENTIAL_MARKER) {
        if request_url.matches(CREDENTIAL_MARKER).count() != 1 {
            return Err(Error::Invalid);
        }
        let handle = request.credential.as_deref().ok_or(Error::Denied)?;
        let secret = credentials
            .resolve_path(handle)
            .map_err(|_| Error::Denied)?
            .ok_or(Error::Denied)?;
        if secret.contains(['\r', '\n', '\0']) {
            return Err(Error::Denied);
        }
        let encoded = utf8_percent_encode(&secret, PATH_SEGMENT).to_string();
        request_url = request_url.replace(CREDENTIAL_MARKER, &encoded);
        path_secret = Some((secret, encoded));
    }
    let url = Url::parse(&request_url).map_err(|_| Error::Invalid)?;
    if url.path().contains(CREDENTIAL_MARKER) {
        return Err(Error::Denied);
    }
    let origin = origin_of(&url).ok_or(Error::Invalid)?;
    if !policy.origins.contains(&origin) {
        return Err(Error::Denied);
    }
    if request.max_bytes == 0
        || request.max_bytes as usize > policy.max_bytes
        || request.timeout_ms == 0
        || Duration::from_millis(request.timeout_ms as u64) > policy.timeout
    {
        return Err(Error::OverBudget);
    }
    for (name, value) in &request.headers {
        if !ALLOWED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(Error::Invalid);
        }
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(Error::Invalid);
        }
    }
    if matches!(request.method, Method::Get | Method::Head) && request.body.is_some() {
        return Err(Error::Invalid);
    }
    if request
        .body
        .as_ref()
        .is_some_and(|body| body.len() > policy.max_bytes)
    {
        return Err(Error::OverBudget);
    }
    let mut auth = None;
    if path_secret.is_none()
        && let Some(handle) = &request.credential
    {
        let header = credentials.resolve(handle).map_err(|_| Error::Denied)?;
        let header = header.ok_or(Error::Denied)?;
        if header.0.contains(['\r', '\n']) || header.1.contains(['\r', '\n']) {
            return Err(Error::Denied);
        }
        auth = Some(header);
    }

    let mut headers = request.headers.clone();
    if let Some(header) = auth {
        headers.push(header);
    }
    Ok((
        TransportRequest {
            url,
            method: method_to_wit(request.method),
            headers,
            body: request.body,
            timeout: Duration::from_millis(request.timeout_ms as u64),
            max_bytes: request.max_bytes as usize,
        },
        path_secret,
    ))
}

fn transport_error(error: TransportError) -> Error {
    match error {
        TransportError::Denied => Error::Denied,
        TransportError::OverBudget => Error::OverBudget,
        TransportError::Timeout => Error::Timeout,
        TransportError::Unavailable => Error::Unavailable,
        TransportError::Tls => Error::Tls,
        TransportError::Http => Error::Http,
        TransportError::Io => Error::Io,
        TransportError::Unsupported => Error::Unsupported,
    }
}

fn redact_final_url(final_url: String, path_secret: Option<&(String, String)>) -> String {
    if let Some((raw, encoded)) = path_secret {
        final_url
            .replace(encoded, CREDENTIAL_MARKER)
            .replace(raw, CREDENTIAL_MARKER)
    } else {
        final_url
    }
}

/// `soil:http` as a guest sees it: convert, run the policy, convert back.
pub fn fetch(
    policy: &HttpPolicy,
    credentials: &dyn CredentialProvider,
    transport: &dyn HttpTransport,
    request: http::Request,
) -> Result<http::Response, http::Error> {
    let response = perform(
        policy,
        credentials,
        transport,
        Request {
            url: request.url,
            method: method_from_wit(request.method),
            headers: request
                .headers
                .into_iter()
                .map(|header| (header.name, header.value))
                .collect(),
            body: request.body,
            credential: request.credential,
            max_bytes: request.max_bytes,
            timeout_ms: request.timeout_ms,
        },
    )
    .map_err(error_to_wit)?;
    Ok(http::Response {
        status: response.status,
        final_url: response.final_url,
        headers: response
            .headers
            .into_iter()
            .map(|(name, value)| http::Header { name, value })
            .collect(),
        body: response.body,
    })
}

fn method_from_wit(method: http::Method) -> Method {
    match method {
        http::Method::Get => Method::Get,
        http::Method::Head => Method::Head,
        http::Method::Post => Method::Post,
        http::Method::Put => Method::Put,
        http::Method::Patch => Method::Patch,
        http::Method::Delete => Method::Delete,
    }
}

fn method_to_wit(method: Method) -> http::Method {
    match method {
        Method::Get => http::Method::Get,
        Method::Head => http::Method::Head,
        Method::Post => http::Method::Post,
        Method::Put => http::Method::Put,
        Method::Patch => http::Method::Patch,
        Method::Delete => http::Method::Delete,
    }
}

fn error_to_wit(error: Error) -> http::Error {
    match error {
        Error::Denied => http::Error::Denied,
        Error::Invalid => http::Error::Invalid,
        Error::OverBudget => http::Error::OverBudget,
        Error::Timeout => http::Error::Timeout,
        Error::Unavailable => http::Error::Unavailable,
        Error::Tls => http::Error::Tls,
        Error::Http => http::Error::Http,
        Error::Io => http::Error::Io,
        Error::Unsupported => http::Error::Unsupported,
    }
}

fn normalize_origin(origin: &str) -> Result<String> {
    let url = Url::parse(origin).with_context(|| format!("parse origin {origin}"))?;
    origin_of(&url).ok_or_else(|| anyhow::anyhow!("origin {origin} must be http or https"))
}

fn origin_of(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    Some(format!("{}://{}:{port}", url.scheme(), host))
}

impl metrics_api::Host for HttpHost {
    fn emit(
        &mut self,
        samples: Vec<metrics_api::Sample>,
    ) -> std::result::Result<(), metrics_api::Error> {
        emit_metrics(&self.metrics, samples)
    }

    fn snapshot(&mut self) -> Vec<metrics_api::Sample> {
        snapshot_metrics(&self.metrics)
    }
}

/// Translate one `soil:metrics` sample into the registry's own shape.
fn registry_sample(sample: metrics_api::Sample) -> crate::metrics::Sample {
    crate::metrics::Sample {
        name: sample.name,
        kind: match sample.kind {
            metrics_api::Kind::Counter => crate::metrics::Kind::Counter,
            metrics_api::Kind::Gauge => crate::metrics::Kind::Gauge,
        },
        value: sample.value,
        help: sample.help,
        labels: sample.labels.into_iter().collect(),
    }
}

/// Translate one registry sample back onto the interface, for `snapshot`.
fn guest_sample(sample: crate::metrics::Sample) -> metrics_api::Sample {
    metrics_api::Sample {
        name: sample.name,
        kind: match sample.kind {
            crate::metrics::Kind::Counter => metrics_api::Kind::Counter,
            crate::metrics::Kind::Gauge => metrics_api::Kind::Gauge,
        },
        value: sample.value,
        help: sample.help,
        labels: sample.labels.into_iter().collect(),
    }
}

/// The one implementation of `emit`, used by both hosts so the budgets and the
/// error mapping cannot drift between the `runner` world and `wasi:http`.
pub(crate) fn emit_metrics(
    registry: &crate::metrics::Metrics,
    samples: Vec<metrics_api::Sample>,
) -> std::result::Result<(), metrics_api::Error> {
    let mapped: Vec<crate::metrics::Sample> = samples.into_iter().map(registry_sample).collect();
    registry.emit(&mapped).map_err(|error| match error {
        crate::metrics::MetricsError::Invalid => metrics_api::Error::Invalid,
        crate::metrics::MetricsError::OverBudget => metrics_api::Error::OverBudget,
    })
}

pub(crate) fn snapshot_metrics(registry: &crate::metrics::Metrics) -> Vec<metrics_api::Sample> {
    registry.snapshot().into_iter().map(guest_sample).collect()
}

/// Run a component that imports `soil:http` and exports `run(args)`.
pub struct HttpRuntime {
    engine: Engine,
    linker: Linker<HttpHost>,
}

impl HttpRuntime {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(anyhow::Error::from)
            .context("link wasi:io polling")?;
        http::add_to_linker::<HttpHost, wasmtime::component::HasSelf<HttpHost>>(
            &mut linker,
            |host| host,
        )?;
        metrics_api::add_to_linker::<HttpHost, wasmtime::component::HasSelf<HttpHost>>(
            &mut linker,
            |host| host,
        )?;
        Ok(Self { engine, linker })
    }

    pub fn run_component(&self, bytes: &[u8], host: HttpHost, args: &[String]) -> Result<()> {
        let component = Component::new(&self.engine, bytes)?;
        let mut store = Store::new(&self.engine, host);
        let bindings = Runner::instantiate(&mut store, &component, &self.linker)?;
        let output = bindings
            .call_run(&mut store, args)?
            .map_err(|error| anyhow::anyhow!("guest: {error}"))?;
        if !output.is_empty() {
            if output.ends_with('\n') {
                print!("{output}");
            } else {
                println!("{output}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_host::soil::http::http::{Host as _, HostResponseStream as _};
    use std::{io::Write as _, net::TcpListener, sync::mpsc, thread};

    struct StaticCredentials;
    impl CredentialProvider for StaticCredentials {
        fn resolve(&self, handle: &str) -> Result<Option<(String, String)>> {
            Ok((handle == "test").then(|| ("authorization".into(), "Bearer spike".into())))
        }

        fn resolve_path(&self, handle: &str) -> Result<Option<String>> {
            Ok((handle == "path").then(|| "path-secret".into()))
        }
    }

    fn serve_once(
        body: &'static str,
        expected_auth: Option<&'static str>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let text = read_request(&mut stream);
            tx.send(text.clone()).unwrap();
            if let Some(expected) = expected_auth {
                assert!(
                    text.to_ascii_lowercase()
                        .contains(&format!("authorization: {expected}").to_ascii_lowercase())
                );
            }
            stream
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}", body.len(), body).as_bytes())
                .unwrap();
        });
        (format!("http://{addr}"), rx, handle)
    }

    /// Read one request, headers and body. A write may arrive in more than one
    /// packet, so a single `read` is not enough to see its body.
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]);
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buf.len() >= end + 4 + length {
                break;
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    #[test]
    fn fetches_allowlisted_origin_and_bounds_body() {
        let (origin, _rx, handle) = serve_once("hello", None);
        let policy = HttpPolicy::new([origin.clone()], 1024, Duration::from_secs(5)).unwrap();
        let mut host = HttpHost::new(policy);
        let response = host
            .fetch(http::Request {
                url: format!("{origin}/feed.xml"),
                method: http::Method::Get,
                headers: vec![http::Header {
                    name: "accept".into(),
                    value: "text/xml".into(),
                }],
                body: None,
                credential: None,
                max_bytes: 128,
                timeout_ms: 2_000,
            })
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello");
        handle.join().unwrap();
    }

    #[test]
    fn opens_a_streaming_response_and_reads_it_incrementally() {
        let (origin, _rx, handle) = serve_once("hello", None);
        let policy = HttpPolicy::new([origin.clone()], 1024, Duration::from_secs(5)).unwrap();
        let mut host = HttpHost::new(policy);
        let stream = host
            .open(http::Request {
                url: format!("{origin}/events"),
                method: http::Method::Get,
                headers: vec![],
                body: None,
                credential: None,
                max_bytes: 128,
                timeout_ms: 2_000,
            })
            .unwrap();
        assert_eq!(
            host.status(wasmtime::component::Resource::new_borrow(stream.rep())),
            200
        );
        let mut body = Vec::new();
        while let Some(chunk) = host
            .read(wasmtime::component::Resource::new_borrow(stream.rep()), 2)
            .unwrap()
        {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, b"hello");
        host.close(wasmtime::component::Resource::new_borrow(stream.rep()))
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn denies_unlisted_origin_and_injects_credentials() {
        let (origin, rx, handle) = serve_once("ok", Some("Bearer spike"));
        let policy = HttpPolicy::new([origin.clone()], 1024, Duration::from_secs(5)).unwrap();
        let mut host = HttpHost::new(policy).with_credentials(Arc::new(StaticCredentials));
        let response = host
            .fetch(http::Request {
                url: format!("{origin}/private.xml"),
                method: http::Method::Get,
                headers: vec![],
                body: None,
                credential: Some("test".into()),
                max_bytes: 128,
                timeout_ms: 2_000,
            })
            .unwrap();
        assert_eq!(response.status, 200);
        assert!(
            rx.recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("authorization: bearer spike")
        );
        handle.join().unwrap();

        let mut denied = HttpHost::new(
            HttpPolicy::new(
                vec!["https://example.com".to_string()],
                1024,
                Duration::from_secs(5),
            )
            .unwrap(),
        );
        assert_eq!(
            denied
                .fetch(http::Request {
                    url: "https://not-allowed.invalid/".into(),
                    method: http::Method::Get,
                    headers: vec![],
                    body: None,
                    credential: None,
                    max_bytes: 128,
                    timeout_ms: 2_000,
                })
                .unwrap_err(),
            http::Error::Denied
        );
    }

    #[test]
    fn substitutes_a_path_credential_and_redacts_the_final_url() {
        let (origin, rx, handle) = serve_once("ok", None);
        let policy = HttpPolicy::new([origin.clone()], 1024, Duration::from_secs(5)).unwrap();
        let mut host = HttpHost::new(policy).with_credentials(Arc::new(StaticCredentials));
        let response = host
            .fetch(http::Request {
                url: format!("{origin}/api/{CREDENTIAL_MARKER}/groups/0"),
                method: http::Method::Get,
                headers: vec![],
                body: None,
                credential: Some("path".into()),
                max_bytes: 128,
                timeout_ms: 2_000,
            })
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            response.final_url,
            format!("{origin}/api/{CREDENTIAL_MARKER}/groups/0")
        );
        assert!(
            rx.recv()
                .unwrap()
                .starts_with("GET /api/path-secret/groups/0"),
        );
        handle.join().unwrap();
    }

    #[test]
    fn a_marker_without_a_path_credential_is_denied() {
        let mut host = HttpHost::new(
            HttpPolicy::new(
                vec!["http://127.0.0.1:9".to_string()],
                1024,
                Duration::from_secs(5),
            )
            .unwrap(),
        );
        assert_eq!(
            host.fetch(http::Request {
                url: format!("http://127.0.0.1:9/api/{CREDENTIAL_MARKER}/groups"),
                method: http::Method::Get,
                headers: vec![],
                body: None,
                credential: None,
                max_bytes: 128,
                timeout_ms: 2_000,
            })
            .unwrap_err(),
            http::Error::Denied
        );
    }

    #[cfg(unix)]
    #[test]
    fn geode_path_credentials_lease_over_the_socket() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("geode.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert!(request.contains("geode://home/hue/bridge"), "{request}");
            assert!(request.contains("hue.control"), "{request}");
            stream
                .write_all(b"{\"ok\":true,\"secret_b64\":\"c2VjcmV0\"}\n")
                .unwrap();
        });

        // One lease client, host-configured handle: the guest names "bridge",
        // and the reference plus purpose stay on the host side.
        let credentials = crate::creds::GeodeCredentials::new(
            &socket,
            "geode://",
            "default.purpose",
            Duration::from_secs(5),
        )
        .with_host_handle(
            "bridge",
            crate::creds::HostHandle {
                reference: "geode://home/hue/bridge".into(),
                purpose: "hue.control".into(),
                header: None,
            },
        );
        assert_eq!(
            credentials.resolve_path("bridge").unwrap().as_deref(),
            Some("secret")
        );
        // A handle the host does not grant resolves to nothing, rather than to a
        // reference the guest spelled.
        assert_eq!(
            credentials.resolve_path("geode://home/hue/bridge").unwrap(),
            None
        );
        server.join().unwrap();
    }

    #[test]
    fn put_carries_a_json_body_to_the_origin() {
        let (origin, rx, handle) = serve_once("{}", None);
        let policy = HttpPolicy::new([origin.clone()], 1024, Duration::from_secs(5)).unwrap();
        let mut host = HttpHost::new(policy);
        let response = host
            .fetch(http::Request {
                url: format!("{origin}/api/key/groups/0/action"),
                method: http::Method::Put,
                headers: vec![http::Header {
                    name: "content-type".into(),
                    value: "application/json".into(),
                }],
                body: Some(br#"{"on":true}"#.to_vec()),
                credential: None,
                max_bytes: 128,
                timeout_ms: 2_000,
            })
            .unwrap();
        assert_eq!(response.status, 200);
        let request = rx.recv().unwrap();
        assert!(
            request.starts_with("PUT /api/key/groups/0/action"),
            "{request}"
        );
        assert!(request.contains(r#"{"on":true}"#), "{request}");
        handle.join().unwrap();
    }

    #[test]
    fn the_provider_is_preferred_and_only_unsupported_falls_back() {
        struct Always(Result<TransportResponse, TransportError>);
        impl HttpTransport for Always {
            fn fetch(
                &self,
                _request: &TransportRequest,
            ) -> Result<TransportResponse, TransportError> {
                self.0.clone()
            }
        }

        let request = TransportRequest {
            url: Url::parse("https://example.com/").unwrap(),
            method: http::Method::Get,
            headers: vec![],
            body: None,
            timeout: Duration::from_secs(1),
            max_bytes: 64,
        };
        let answer = |from: &'static str| {
            Arc::new(Always(Ok(TransportResponse {
                status: 200,
                final_url: from.into(),
                headers: vec![],
                body: from.as_bytes().to_vec(),
            }))) as Arc<dyn HttpTransport>
        };

        let prefer = PreferProvider::new(answer("page"), answer("node"));
        assert_eq!(prefer.fetch(&request).unwrap().body, b"page");

        let absent = PreferProvider::new(
            Arc::new(Always(Err(TransportError::Unsupported))),
            answer("node"),
        );
        assert_eq!(absent.fetch(&request).unwrap().body, b"node");

        // A refusal is an answer, not an absence: it must reach the caller
        // rather than quietly routing around the provider.
        let denied = PreferProvider::new(
            Arc::new(Always(Err(TransportError::Denied))),
            answer("node"),
        );
        assert_eq!(denied.fetch(&request).unwrap_err(), TransportError::Denied);
    }

    #[test]
    fn a_body_on_a_read_is_rejected() {
        let mut host = HttpHost::new(
            HttpPolicy::new(
                vec!["https://example.com".to_string()],
                1024,
                Duration::from_secs(5),
            )
            .unwrap(),
        );
        assert_eq!(
            host.fetch(http::Request {
                url: "https://example.com/".into(),
                method: http::Method::Get,
                headers: vec![],
                body: Some(b"unexpected".to_vec()),
                credential: None,
                max_bytes: 128,
                timeout_ms: 2_000,
            })
            .unwrap_err(),
            http::Error::Invalid
        );
    }

    #[test]
    fn an_oversized_body_is_rejected_before_the_transport() {
        let mut host = HttpHost::new(
            HttpPolicy::new(
                vec!["https://example.com".to_string()],
                8,
                Duration::from_secs(5),
            )
            .unwrap(),
        );
        assert_eq!(
            host.fetch(http::Request {
                url: "https://example.com/".into(),
                method: http::Method::Post,
                headers: vec![],
                body: Some(b"more than eight bytes".to_vec()),
                credential: None,
                max_bytes: 8,
                timeout_ms: 2_000,
            })
            .unwrap_err(),
            http::Error::OverBudget
        );
    }
}
