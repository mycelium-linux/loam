//! Tiny local web surface for manifest-declared static Spores.
//!
//! This is deliberately not the HTTP component gateway. It is the static
//! `[[surfaces]] kind = "web"` path: a package ships `web/index.html` and its
//! assets, the host serves them on a loopback listener, and `loamd` publishes
//! the matching Mycelium service lease.
//!
//! A package may also declare `[web] vfs_root = "notes"`. That gives its static
//! page a small JSON API over the host workspace, bounded to that prefix. The
//! page still has no ambient filesystem: it can only name paths the host
//! resolves under the declared root.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::spore::{Anycast, Surface, SurfaceKind, Ui, Web};

#[cfg(feature = "records")]
use crate::records_host::{RecordsBookHost, records};
#[cfg(feature = "records")]
use base64::{Engine as _, engine::general_purpose::STANDARD};
#[cfg(feature = "vfs")]
use fungos_vfs::{Expect, NodeRole, Vfs, normalize_path};

#[derive(Debug, Clone)]
pub struct StaticSurface {
    pub package: PathBuf,
    pub spore_id: String,
    pub version: String,
    pub anycast: Anycast,
    pub entry: PathBuf,
    pub name: String,
    pub kind: String,
    pub ttl_secs: u64,
    pub web: Option<Web>,
    pub writable: bool,
    pub records: Option<crate::spore::Records>,
    pub records_writable: bool,
    /// This surface may read the installed-Spore catalog and launch surfaces.
    pub spore_catalog: bool,
}

/// Find static web surfaces in a set of package roots.
///
/// `spore.soil` wins when a package carries both forms: it is the source
/// manifest the author edits, and a lease should follow the file being edited.
/// `spore.toml` remains the installed-package form.
pub fn discover(roots: &[PathBuf]) -> Result<Vec<StaticSurface>> {
    let mut found = Vec::new();
    for root in roots {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let package = entry.path();
            if !package.is_dir() {
                continue;
            }
            // One loader for source and installed packages. A surface is a
            // routing fact; it does not need a second manifest dialect.
            let Ok(manifest) = crate::spore::load_source(&package) else {
                continue;
            };
            let Some(anycast) = manifest.anycast.clone() else {
                continue;
            };
            let writable = manifest.grants.contains_key("soil:vfs-mut");
            let records = manifest.records.clone();
            let records_writable = manifest.grants.contains_key("soil:records")
                || manifest.grants.contains_key("soil:records.write");
            let spore_catalog = has_spore_catalog(&manifest.grants);
            if let Some(surface) = static_surface(
                &package,
                manifest.spore.id,
                manifest.spore.version,
                anycast,
                &manifest.surfaces,
                manifest.ui.as_ref(),
                manifest.web,
                writable,
                records,
                records_writable,
                spore_catalog,
            ) {
                found.push(surface);
            }
        }
    }
    found.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(found)
}

#[allow(clippy::too_many_arguments)]
fn static_surface(
    package: &Path,
    spore_id: String,
    version: String,
    anycast: Anycast,
    surfaces: &[Surface],
    ui: Option<&Ui>,
    web: Option<Web>,
    writable: bool,
    records: Option<crate::spore::Records>,
    records_writable: bool,
    spore_catalog: bool,
) -> Option<StaticSurface> {
    let surface = surfaces
        .iter()
        .find(|surface| surface.kind == SurfaceKind::Web)?;
    let entry = package.join(surface.entry(ui)?);
    if !entry.is_file() {
        return None;
    }
    Some(StaticSurface {
        package: package.to_path_buf(),
        spore_id,
        version,
        name: anycast.name.clone(),
        kind: anycast.kind.clone(),
        ttl_secs: anycast.ttl_secs,
        anycast,
        entry,
        web: web.filter(|web| !web.vfs_root.contains("..")),
        writable,
        records,
        records_writable,
        spore_catalog,
    })
}

/// Host operations a browser surface may call without importing WIT.
///
/// The launcher is HTML/CSS/JS, so it cannot call `soil:spore/catalog` itself.
/// This is the same catalog and launch policy over JSON: one host
/// implementation, two transports.
pub trait SporeApi: Send + Sync {
    fn entries(&self) -> std::result::Result<Value, String>;
    fn launch(&self, id: &str, surface: &str, native: bool) -> std::result::Result<Value, String>;
    fn running(&self) -> std::result::Result<Value, String>;
    fn stop(&self, id: &str) -> std::result::Result<Value, String>;
}

/// Serve one or more static surfaces. The first surface is the fallback when
/// the request Host does not name a package.
///
/// The surface list is shared rather than captured so an install can add a
/// Spore without restarting the listener.
pub fn serve(
    surfaces: Arc<RwLock<Vec<StaticSurface>>>,
    api: Arc<dyn SporeApi>,
    listener: TcpListener,
) -> Result<()> {
    loop {
        let (mut stream, _) = listener.accept()?;
        let sites = sites(&surfaces.read().expect("web surface lock poisoned"));
        let api = Arc::clone(&api);
        std::thread::spawn(move || {
            if sites.is_empty() {
                let _ = respond(
                    &mut stream,
                    503,
                    "text/plain",
                    b"no web surfaces are installed\n",
                    None,
                );
                return;
            }
            let _ = handle(stream, &sites, &api);
        });
    }
}

fn has_spore_catalog(grants: &std::collections::BTreeMap<String, String>) -> bool {
    grants.contains_key("soil:spore") || grants.contains_key("soil:spore.catalog")
}

fn sites(surfaces: &[StaticSurface]) -> Vec<(StaticSurface, PathBuf)> {
    surfaces
        .iter()
        .map(|surface| {
            let root = surface
                .entry
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| surface.package.clone());
            (surface.clone(), root)
        })
        .collect()
}

fn handle(
    mut stream: TcpStream,
    sites: &[(StaticSurface, PathBuf)],
    api: &Arc<dyn SporeApi>,
) -> Result<()> {
    let Some(request) = read_request(&mut stream)? else {
        return Ok(());
    };
    let origin = request.headers.get("origin").map(String::as_str);
    // The local listener sees the frontdoor's upstream Host, not the browser's.
    // A gateway forwards the original name in `x-forwarded-host`; prefer it so
    // several web Spores can share one listener.
    let host = request
        .headers
        .get("x-forwarded-host")
        .or_else(|| request.headers.get("host"))
        .map(|host| host.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let (site, root) = sites
        .iter()
        .find(|(site, _)| {
            host == format!("{}.myc", site.name) || host.starts_with(&format!("{}.", site.name))
        })
        .or_else(|| sites.first())
        .expect("sites is non-empty");

    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    if path == "/api/agent/settings" {
        return agent_settings_api(&request, origin, &mut stream);
    }
    if path == "/api/agent" {
        return agent_api(&request, origin, &mut stream);
    }
    if path == "/api/spore" || path.starts_with("/api/spore/") {
        if !site.spore_catalog {
            return respond(
                &mut stream,
                403,
                "application/json",
                br#"{"error":"this surface has no soil:spore.catalog grant"}"#,
                origin,
            );
        }
        return spore_api(&request, path, api, origin, &mut stream);
    }
    if path == "/api/vfs" || path.starts_with("/api/vfs/") {
        return vfs_api(
            site,
            &request.method,
            path,
            query,
            &request,
            origin,
            &mut stream,
        );
    }
    if path == "/v1/call" {
        if request.method == "OPTIONS" {
            return respond(&mut stream, 204, "text/plain", b"", origin);
        }
        if request.method != "POST" {
            return respond(
                &mut stream,
                405,
                "text/plain",
                b"method not allowed",
                origin,
            );
        }
        return records_api(site, &request.body, origin, &mut stream);
    }
    if request.method == "OPTIONS" {
        return respond(&mut stream, 204, "text/plain", b"", origin);
    }
    if !matches!(request.method.as_str(), "GET" | "HEAD") {
        return respond(
            &mut stream,
            405,
            "text/plain",
            b"method not allowed",
            origin,
        );
    }

    let mut relative = path.trim_start_matches('/');
    if relative.is_empty() {
        relative = "index.html";
    }
    if relative.split('/').any(|part| part == ".." || part == ".") {
        return respond(&mut stream, 400, "text/plain", b"invalid path", origin);
    }
    let file = root.join(relative);
    match fs::read(&file) {
        Ok(bytes) => {
            let mime = media_type(&file);
            respond(
                &mut stream,
                200,
                mime,
                if request.method == "HEAD" {
                    b""
                } else {
                    &bytes
                },
                origin,
            )
        }
        Err(_) => respond(&mut stream, 404, "text/plain", b"not found", origin),
    }
}

#[cfg(feature = "records")]
#[derive(serde::Deserialize)]
struct WireCallRequest {
    v: u32,
    call: WireCall,
}

#[cfg(feature = "records")]
#[derive(serde::Deserialize)]
struct WireCall {
    id: String,
    interface: String,
    function: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[cfg(feature = "records")]
#[derive(serde::Deserialize, serde::Serialize)]
struct WireKey {
    collection: String,
    id: String,
}

#[cfg(feature = "records")]
#[derive(serde::Deserialize, serde::Serialize)]
struct WireVersion {
    revision: u64,
    origin: String,
}

#[cfg(feature = "records")]
#[derive(serde::Deserialize)]
struct WireFilter {
    field: String,
    op: String,
    #[serde(default)]
    value: Option<String>,
}

#[cfg(feature = "records")]
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WireEntry {
    key: WireKey,
    #[serde(default)]
    schema: String,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    updated_at_ms: u64,
    #[serde(default)]
    origin: String,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    body: String,
}

#[cfg(feature = "records")]
impl WireEntry {
    fn from_entry(entry: records::Entry) -> Self {
        Self {
            key: WireKey {
                collection: entry.key.collection,
                id: entry.key.id,
            },
            schema: entry.schema,
            revision: entry.revision,
            updated_at_ms: entry.updated_at_ms,
            origin: entry.origin,
            deleted: entry.deleted,
            body: STANDARD.encode(entry.body),
        }
    }

    fn into_entry(self) -> std::result::Result<records::Entry, serde_json::Value> {
        let body = STANDARD
            .decode(self.body.as_bytes())
            .map_err(|_| call_error("invalid", "body is not base64"))?;
        Ok(records::Entry {
            key: records::Key {
                collection: self.key.collection,
                id: self.key.id,
            },
            schema: self.schema,
            revision: self.revision,
            updated_at_ms: self.updated_at_ms,
            origin: self.origin,
            deleted: self.deleted,
            body,
        })
    }
}

#[cfg(feature = "records")]
fn call_error(code: &str, message: &str) -> serde_json::Value {
    json!({"err": {"code": code, "message": message}})
}

#[cfg(feature = "records")]
fn records_error(error: records::Error) -> serde_json::Value {
    let code = match error {
        records::Error::Denied => "denied",
        records::Error::NotFound => "not-found",
        records::Error::OverBudget => "over-budget",
        records::Error::Invalid => "invalid",
        records::Error::Stale => "stale",
        records::Error::Io => "io",
        records::Error::Unsupported => "unsupported",
    };
    call_error(code, "records call failed")
}

#[cfg(feature = "records")]
fn records_root() -> PathBuf {
    if let Some(path) = std::env::var_os("MYCELIUM_RECORDS_ROOT") {
        return PathBuf::from(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local/share/fungos/records")
}

#[cfg(feature = "records")]
fn records_api(
    site: &StaticSurface,
    body: &[u8],
    origin: Option<&str>,
    stream: &mut TcpStream,
) -> Result<()> {
    let Ok(request) = serde_json::from_slice::<WireCallRequest>(body) else {
        return respond(
            stream,
            400,
            "application/json",
            br#"{"error":"malformed call"}"#,
            origin,
        );
    };
    if request.v != 1 {
        return json_response(
            stream,
            200,
            &json!({"v": 1, "id": request.call.id, "result": call_error("invalid", "unsupported protocol version")}),
            origin,
        );
    }
    if request.call.interface != "soil:records" {
        return json_response(
            stream,
            200,
            &json!({"v": 1, "id": request.call.id, "result": call_error("unsupported", "this surface serves soil:records only")}),
            origin,
        );
    }
    let Some(records_decl) = site.records.as_ref() else {
        return json_response(
            stream,
            200,
            &json!({"v": 1, "id": request.call.id, "result": call_error("unsupported", "no records declaration")}),
            origin,
        );
    };
    let Some(book) = records_decl.book.clone() else {
        return json_response(
            stream,
            200,
            &json!({"v": 1, "id": request.call.id, "result": call_error("unsupported", "records.book is not declared")}),
            origin,
        );
    };
    let declared = records_decl.names();

    // The collection is part of every call's key/params; open the host once the
    // collection is known so a records Spore can serve more than one schema.
    let collection_hint = request
        .call
        .params
        .get("collection")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            request
                .call
                .params
                .get("key")
                .and_then(|key| key.get("collection"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            request
                .call
                .params
                .get("entry")
                .and_then(|entry| entry.get("key"))
                .and_then(|key| key.get("collection"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("");
    if !declared.iter().any(|name| *name == collection_hint) {
        return json_response(
            stream,
            200,
            &json!({"v": 1, "id": request.call.id, "result": call_error("invalid", "collection is not declared by this Spore")}),
            origin,
        );
    }
    let mut host = match RecordsBookHost::open(records_root(), book, collection_hint) {
        Ok(host) => host,
        Err(error) => {
            return json_response(
                stream,
                200,
                &json!({"v": 1, "id": request.call.id, "result": call_error("io", &error.to_string())}),
                origin,
            );
        }
    };

    let result = (|| -> serde_json::Value {
        match request.call.function.as_str() {
            "scan" => {
                #[derive(serde::Deserialize)]
                struct Params {
                    #[serde(default)]
                    cursor: Option<String>,
                    #[serde(default)]
                    limit: Option<u32>,
                }
                let Ok(params) = serde_json::from_value::<Params>(request.call.params.clone())
                else {
                    return call_error("invalid", "scan params");
                };
                let limit = params.limit.unwrap_or(50).clamp(1, 200);
                match records::Host::scan(
                    &mut host,
                    collection_hint.to_owned(),
                    params.cursor,
                    limit,
                ) {
                    Ok(page) => {
                        json!({"ok": {"entries": page.entries.into_iter().map(WireEntry::from_entry).collect::<Vec<_>>(), "next": page.next}})
                    }
                    Err(error) => records_error(error),
                }
            }
            "query" => {
                #[derive(serde::Deserialize)]
                struct Params {
                    #[serde(default)]
                    prefix: Option<String>,
                    #[serde(default)]
                    filters: Vec<WireFilter>,
                    #[serde(default)]
                    fields: Vec<String>,
                    #[serde(default)]
                    cursor: Option<String>,
                    #[serde(default)]
                    limit: Option<u32>,
                }
                let Ok(params) = serde_json::from_value::<Params>(request.call.params.clone())
                else {
                    return call_error("invalid", "query params");
                };
                let mut filters = Vec::with_capacity(params.filters.len());
                for filter in params.filters {
                    let op = match filter.op.as_str() {
                        "eq" => records::FilterOp::Eq,
                        "prefix" => records::FilterOp::Prefix,
                        "contains" => records::FilterOp::Contains,
                        "exists" => records::FilterOp::Exists,
                        "lt" => records::FilterOp::Lt,
                        "lte" => records::FilterOp::Lte,
                        "gt" => records::FilterOp::Gt,
                        "gte" => records::FilterOp::Gte,
                        _ => return call_error("invalid", "unknown query filter"),
                    };
                    filters.push(records::Filter {
                        field: filter.field,
                        op,
                        value: filter.value,
                    });
                }
                let spec = records::QuerySpec {
                    collection: collection_hint.to_owned(),
                    prefix: params.prefix,
                    filters,
                    fields: params.fields,
                    cursor: params.cursor,
                    limit: params.limit.unwrap_or(50).clamp(1, 200),
                };
                match records::Host::query(&mut host, spec) {
                    Ok(page) => {
                        json!({"ok": {"entries": page.entries.into_iter().map(WireEntry::from_entry).collect::<Vec<_>>(), "next": page.next}})
                    }
                    Err(error) => records_error(error),
                }
            }
            "get" => {
                #[derive(serde::Deserialize)]
                struct Params {
                    key: WireKey,
                }
                let Ok(params) = serde_json::from_value::<Params>(request.call.params.clone())
                else {
                    return call_error("invalid", "get params");
                };
                match records::Host::get(
                    &mut host,
                    records::Key {
                        collection: params.key.collection,
                        id: params.key.id,
                    },
                ) {
                    Ok(entry) => json!({"ok": WireEntry::from_entry(entry)}),
                    Err(error) => records_error(error),
                }
            }
            "upsert" => {
                if !site.records_writable {
                    return call_error("denied", "soil:records.write is not granted");
                }
                #[derive(serde::Deserialize)]
                struct Params {
                    entry: WireEntry,
                }
                let Ok(params) = serde_json::from_value::<Params>(request.call.params.clone())
                else {
                    return call_error("invalid", "upsert params");
                };
                let Ok(entry) = params.entry.into_entry() else {
                    return call_error("invalid", "entry body");
                };
                match records::Host::upsert(&mut host, entry) {
                    Ok(entry) => json!({"ok": WireEntry::from_entry(entry)}),
                    Err(error) => records_error(error),
                }
            }
            "compare-and-swap" => {
                if !site.records_writable {
                    return call_error("denied", "soil:records.write is not granted");
                }
                #[derive(serde::Deserialize)]
                struct Params {
                    key: WireKey,
                    #[serde(default)]
                    expected: Option<WireVersion>,
                    entry: WireEntry,
                }
                let Ok(params) = serde_json::from_value::<Params>(request.call.params.clone())
                else {
                    return call_error("invalid", "compare-and-swap params");
                };
                let Ok(entry) = params.entry.into_entry() else {
                    return call_error("invalid", "entry body");
                };
                let expected = params.expected.map(|version| records::Version {
                    revision: version.revision,
                    origin: version.origin,
                });
                let key = records::Key {
                    collection: params.key.collection,
                    id: params.key.id,
                };
                match records::Host::compare_and_swap(&mut host, key, expected, entry) {
                    Ok(result) => {
                        json!({"ok": {"applied": result.applied, "entry": WireEntry::from_entry(result.entry)}})
                    }
                    Err(error) => records_error(error),
                }
            }
            "delete" => {
                if !site.records_writable {
                    return call_error("denied", "soil:records.write is not granted");
                }
                #[derive(serde::Deserialize)]
                struct Params {
                    key: WireKey,
                }
                let Ok(params) = serde_json::from_value::<Params>(request.call.params.clone())
                else {
                    return call_error("invalid", "delete params");
                };
                match records::Host::delete(
                    &mut host,
                    records::Key {
                        collection: params.key.collection,
                        id: params.key.id,
                    },
                ) {
                    Ok(()) => json!({"ok": null}),
                    Err(error) => records_error(error),
                }
            }
            _ => call_error("unsupported", "unsupported soil:records function"),
        }
    })();
    json_response(
        stream,
        200,
        &json!({"v": 1, "id": request.call.id, "result": result}),
        origin,
    )
}

#[cfg(not(feature = "records"))]
fn records_api(
    _site: &StaticSurface,
    _body: &[u8],
    origin: Option<&str>,
    stream: &mut TcpStream,
) -> Result<()> {
    respond(
        stream,
        501,
        "application/json",
        br#"{"error":"records feature is not built"}"#,
        origin,
    )
}

#[derive(Deserialize)]
struct LaunchRequest {
    id: String,
    #[serde(default)]
    surface: String,
    #[serde(default)]
    native: bool,
}

#[derive(Deserialize)]
struct StopRequest {
    id: String,
}

fn spore_api(
    request: &Request,
    path: &str,
    api: &Arc<dyn SporeApi>,
    origin: Option<&str>,
    stream: &mut TcpStream,
) -> Result<()> {
    let result = match (request.method.as_str(), path) {
        ("GET", "/api/spore/entries") => api.entries(),
        ("GET", "/api/spore/running") => api.running(),
        ("POST", "/api/spore/launch") => serde_json::from_slice::<LaunchRequest>(&request.body)
            .map_err(|error| format!("invalid launch request: {error}"))
            .and_then(|launch| api.launch(&launch.id, &launch.surface, launch.native)),
        ("POST", "/api/spore/stop") => serde_json::from_slice::<StopRequest>(&request.body)
            .map_err(|error| format!("invalid stop request: {error}"))
            .and_then(|stop| api.stop(&stop.id)),
        _ => return respond(stream, 404, "text/plain", b"unknown spore route", origin),
    };
    let (status, value) = match result {
        Ok(value) => (200, value),
        Err(error) => (400, json!({ "error": error })),
    };
    let body = serde_json::to_vec(&value)?;
    respond(stream, status, "application/json", &body, origin)
}

#[allow(clippy::too_many_arguments)]
fn vfs_api(
    site: &StaticSurface,
    method: &str,
    path: &str,
    query: &str,
    request: &Request,
    origin: Option<&str>,
    stream: &mut TcpStream,
) -> Result<()> {
    let Some(web) = site.web.as_ref() else {
        return respond(
            stream,
            404,
            "application/json",
            b"{\"error\":\"no vfs web grant\"}",
            origin,
        );
    };
    #[cfg(not(feature = "vfs"))]
    {
        let _ = (method, path, query, request, web);
        return respond(
            stream,
            501,
            "application/json",
            b"{\"error\":\"vfs feature is not built\"}",
            origin,
        );
    }
    #[cfg(feature = "vfs")]
    {
        if method == "OPTIONS" {
            return respond(stream, 204, "text/plain", b"", origin);
        }
        if path == "/api/vfs/roots" {
            return json_response(
                stream,
                200,
                &json!({"roots": configured_roots(), "active": active_root_id(query)}),
                origin,
            );
        }
        if path == "/api/vfs/desktops" {
            return json_response(
                stream,
                200,
                &json!({"desktops": configured_desktops(), "active": desktop_id(query, web)}),
                origin,
            );
        }
        let selected = active_root_id(query);
        let workspace = match workspace_for_root(selected.as_deref()) {
            Some(path) => path,
            None => {
                return json_response(stream, 400, &json!({"error": "unknown vfs root"}), origin);
            }
        };
        let mut vfs = match Vfs::open_with_role(&workspace, NodeRole::Full) {
            Ok(vfs) => vfs,
            Err(error) => {
                return json_response(stream, 500, &json!({"error": error.to_string()}), origin);
            }
        };
        let requested = query_value(query, "path").unwrap_or_default();
        let desktop = desktop_id(query, web);
        let desktop_root = match (desktop.is_empty(), web.vfs_root.is_empty()) {
            (true, _) => web.vfs_root.clone(),
            (false, true) => desktop,
            (false, false) => format!("{desktop}/{}", web.vfs_root),
        };
        let Some(vfs_path) = scoped_path(&desktop_root, &requested) else {
            return json_response(
                stream,
                400,
                &json!({"error": "path escapes the web vfs root"}),
                origin,
            );
        };
        match (method, path) {
            ("GET", "/api/vfs/list") => {
                let entries = match vfs.list(&vfs_path) {
                    Ok(entries) => entries,
                    Err(error) => {
                        return json_response(
                            stream,
                            500,
                            &json!({"error": error.to_string()}),
                            origin,
                        );
                    }
                };
                let entries = entries
                    .into_iter()
                    .take(5000)
                    .map(|entry| {
                        let relative = entry
                            .path
                            .strip_prefix(&format!("{}/", desktop_root.trim_end_matches('/')))
                            .unwrap_or(&entry.path)
                            .trim_start_matches('/')
                            .to_owned();
                        json!({
                            "path": entry.path,
                            "relative": relative,
                            "name": entry.path.rsplit('/').next().unwrap_or(&entry.path),
                            "size": entry.size,
                            "media_type": entry.media_type,
                            "version": entry.version,
                            "updated_at_ms": entry.updated_at_ms,
                        })
                    })
                    .collect::<Vec<_>>();
                json_response(
                    stream,
                    200,
                    &json!({"root": web.vfs_root, "path": requested, "entries": entries}),
                    origin,
                )
            }
            ("GET", "/api/vfs/read") => match vfs.get(&vfs_path) {
                Ok(bytes) => match String::from_utf8(bytes.clone()) {
                    Ok(text) => {
                        let entry = vfs.file(&vfs_path).ok().flatten();
                        json_response(
                            stream,
                            200,
                            &json!({
                                "path": vfs_path,
                                "text": text,
                                "size": bytes.len(),
                                "version": entry.as_ref().map(|entry| entry.version),
                                "media_type": entry.as_ref().map(|entry| entry.media_type.clone()),
                            }),
                            origin,
                        )
                    }
                    Err(_) => {
                        json_response(stream, 415, &json!({"error": "not utf-8 text"}), origin)
                    }
                },
                Err(error) => {
                    json_response(stream, 404, &json!({"error": error.to_string()}), origin)
                }
            },
            ("GET", "/api/vfs/raw") => match vfs.get(&vfs_path) {
                Ok(bytes) => {
                    let mime = vfs
                        .file(&vfs_path)
                        .ok()
                        .flatten()
                        .map(|entry| entry.media_type)
                        .unwrap_or_else(|| "application/octet-stream".into());
                    respond(stream, 200, &mime, &bytes, origin)
                }
                Err(error) => {
                    json_response(stream, 404, &json!({"error": error.to_string()}), origin)
                }
            },
            ("PUT", "/api/vfs/write") if site.writable => {
                let expected = query_value(query, "expected_version")
                    .and_then(|value| value.parse().ok())
                    .map_or(Expect::Any, Expect::Version);
                let media_type = request
                    .headers
                    .get("content-type")
                    .map(String::as_str)
                    .unwrap_or("application/octet-stream");
                match vfs.put(&vfs_path, &request.body, media_type, expected) {
                    Ok(entry) => json_response(
                        stream,
                        200,
                        &json!({"path": entry.path, "version": entry.version, "size": entry.size, "media_type": entry.media_type}),
                        origin,
                    ),
                    Err(error) => {
                        json_response(stream, 409, &json!({"error": error.to_string()}), origin)
                    }
                }
            }
            ("DELETE", "/api/vfs/remove") if site.writable => match vfs.remove(&vfs_path) {
                Ok(()) => json_response(stream, 200, &json!({"removed": vfs_path}), origin),
                Err(error) => {
                    json_response(stream, 404, &json!({"error": error.to_string()}), origin)
                }
            },
            ("PUT" | "DELETE", _) => json_response(
                stream,
                403,
                &json!({"error": "web surface is read-only"}),
                origin,
            ),
            _ => json_response(stream, 404, &json!({"error": "unknown vfs api"}), origin),
        }
    }
}

#[cfg(feature = "vfs")]
fn workspace_root() -> PathBuf {
    if let Some(path) = std::env::var_os("MYCELIUM_VFS_ROOT") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/share/fungos/vfs")
}

#[cfg(feature = "vfs")]
fn scoped_path(scope: &str, requested: &str) -> Option<String> {
    let scope = scope.trim_matches('/');
    let requested = requested.trim_matches('/');
    let joined = match (scope.is_empty(), requested.is_empty()) {
        (true, _) => requested.to_owned(),
        (false, true) => scope.to_owned(),
        (false, false) => format!("{scope}/{requested}"),
    };
    let normalized = normalize_path(&joined).ok()?;
    if scope.is_empty() || normalized == scope || normalized.starts_with(&format!("{scope}/")) {
        Some(normalized)
    } else {
        None
    }
}

fn desktop_id(query: &str, web: &Web) -> String {
    let candidate = query_value(query, "desktop").unwrap_or_default();
    let candidate = candidate.trim_matches('/').to_owned();
    if !candidate.is_empty()
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return candidate;
    }
    if web.vfs_root.is_empty() {
        String::new()
    } else {
        "personal".to_owned()
    }
}

fn titlecase(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => value.to_owned(),
    }
}

#[cfg(feature = "vfs")]
fn configured_desktops() -> Vec<serde_json::Value> {
    let workspace = workspace_root();
    let Ok(mut vfs) = Vfs::open_with_role(&workspace, NodeRole::Full) else {
        return vec![];
    };
    let entries = vfs.list("").unwrap_or_default();
    let mut prefixes = std::collections::BTreeSet::new();
    for entry in entries {
        let Some((prefix, _)) = entry.path.split_once('/') else {
            continue;
        };
        if !prefix.is_empty() && !prefix.starts_with('.') {
            prefixes.insert(prefix.to_owned());
        }
    }
    let mut desktops = Vec::new();
    for prefix in prefixes {
        let metadata = vfs
            .get(&format!("{prefix}/desktop.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let name = metadata
            .as_ref()
            .and_then(|value| value["name"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| titlecase(&prefix));
        let kind = metadata
            .as_ref()
            .and_then(|value| value["kind"].as_str())
            .unwrap_or_else(|| {
                if prefix == "personal" {
                    "personal"
                } else {
                    "work"
                }
            });
        desktops.push(json!({
            "id": prefix,
            "name": name,
            "kind": kind,
            "path": format!("/{prefix}"),
        }));
    }
    desktops.sort_by(|left, right| {
        let left_id = left["id"].as_str().unwrap_or_default();
        let right_id = right["id"].as_str().unwrap_or_default();
        (left_id != "personal")
            .cmp(&(right_id != "personal"))
            .then_with(|| left_id.cmp(right_id))
    });
    desktops
}

fn active_root_id(query: &str) -> Option<String> {
    query_value(query, "root").filter(|root| !root.is_empty())
}

fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        return PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        return home.join(rest);
    }
    PathBuf::from(value)
}

fn configured_roots() -> Vec<serde_json::Value> {
    let default = workspace_root();
    let mut roots = vec![json!({
        "id": "vfs",
        "name": "VFS",
        "root": default,
        "mount": std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("fungOS"),
        "kind": "vfs",
    })];
    let directory = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/fungos/desktops");
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                continue;
            }
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            let mut id = None;
            let mut name = None;
            let mut root = None;
            let mut mount = None;
            for line in source.lines() {
                let line = line.trim();
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                let value = value.trim().trim_matches('"').to_owned();
                match key.trim() {
                    "id" => id = Some(value),
                    "name" => name = Some(value),
                    "root" => root = Some(value),
                    "mount" => mount = Some(value),
                    _ => {}
                }
            }
            let (Some(id), Some(root)) = (id, root) else {
                continue;
            };
            if id == "vfs" || roots.iter().any(|root| root["id"] == id) {
                continue;
            }
            roots.push(json!({
                "id": id,
                "name": name.unwrap_or_else(|| id.clone()),
                "root": expand_tilde(&root),
                "mount": mount.map(|mount| expand_tilde(&mount)),
                "kind": "desktop",
            }));
        }
    }
    roots.sort_by(|left, right| {
        let left = left["id"].as_str().unwrap_or_default();
        let right = right["id"].as_str().unwrap_or_default();
        (left != "vfs")
            .cmp(&(right != "vfs"))
            .then_with(|| left.cmp(right))
    });
    roots
}

fn workspace_for_root(id: Option<&str>) -> Option<PathBuf> {
    let id = id.unwrap_or("vfs");
    configured_roots()
        .into_iter()
        .find(|root| root["id"] == id)
        .and_then(|root| root["root"].as_str().map(PathBuf::from))
}

fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone)]
struct AgentConfig {
    endpoint: String,
    workspace: Option<String>,
    model: Option<String>,
}

fn agent_config() -> Option<AgentConfig> {
    let path = std::env::var_os("LOAM_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::config::Config::default_path().filter(|path| path.is_file()))?;
    let config = crate::config::Config::load(&path).ok()?;
    Some(AgentConfig {
        endpoint: config.agent_endpoint?,
        workspace: config.agent_workspace,
        model: config.agent_model,
    })
}

fn agent_settings_path() -> Option<PathBuf> {
    std::env::var_os("LOAM_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::config::Config::default_path())
}

fn operator_config_path() -> Option<PathBuf> {
    std::env::var_os("OPERATOR_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".operator/config.toml"))
        })
}

fn operator_bifrost() -> serde_json::Value {
    let Some(path) = operator_config_path() else {
        return json!({});
    };
    let Ok(source) = fs::read_to_string(path) else {
        return json!({});
    };
    let Ok(value) = toml::from_str::<toml::Value>(&source) else {
        return json!({});
    };
    let bifrost = value.get("bifrost");
    let text = |key: &str| {
        bifrost
            .and_then(|table| table.get(key))
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    json!({
        "baseUrl": text("base_url"),
        "model": text("model"),
        "apiKeyFile": text("api_key_file"),
    })
}

fn agent_models(endpoint: &str) -> Vec<String> {
    let mut models = Vec::new();
    if let Ok(result) = acp_rpc(endpoint, "session/list", json!({})) {
        if let Some(sessions) = result.get("sessions").and_then(|value| value.as_array()) {
            for session in sessions {
                if let Some(model) = session.get("model").and_then(|value| value.as_str()) {
                    if !models.iter().any(|existing| existing == model) {
                        models.push(model.to_string());
                    }
                }
            }
        }
    }
    models
}

fn agent_settings() -> serde_json::Value {
    let Some(path) = agent_settings_path() else {
        return json!({});
    };
    let config = crate::config::Config::load(&path).unwrap_or_default();
    let endpoint = config.agent_endpoint.unwrap_or_default();
    let bifrost = operator_bifrost();
    let mut models = agent_models(&endpoint);
    if let Some(model) = bifrost.get("model").and_then(|value| value.as_str()) {
        if !model.is_empty() && !models.iter().any(|known| known == model) {
            models.push(model.to_string());
        }
    }
    json!({
        "configPath": path.display().to_string(),
        "endpoint": endpoint,
        "workspace": config.agent_workspace.unwrap_or_default(),
        "model": config.agent_model.unwrap_or_default(),
        "models": models,
        "bifrost": bifrost,
        "operatorConfigPath": operator_config_path().map(|path| path.display().to_string()).unwrap_or_default(),
    })
}

fn agent_settings_api(
    request: &Request,
    origin: Option<&str>,
    stream: &mut TcpStream,
) -> Result<()> {
    if request.method == "OPTIONS" {
        return respond(stream, 204, "text/plain", b"", origin);
    }
    if request.method == "GET" {
        return json_response(stream, 200, &agent_settings(), origin);
    }
    if request.method != "POST" {
        return json_response(stream, 405, &json!({"error": "GET or POST"}), origin);
    }
    let body: serde_json::Value = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(error) => {
            return json_response(stream, 400, &json!({"error": error.to_string()}), origin);
        }
    };
    let text = |key: &str| body.get(key).and_then(|value| value.as_str());
    if let Some(endpoint) = text("endpoint") {
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return json_response(
                stream,
                400,
                &json!({"error": "endpoint must be an absolute http(s) URL"}),
                origin,
            );
        }
    }
    if let Some(path) = agent_settings_path() {
        let source = fs::read_to_string(&path).unwrap_or_else(|_| "(loam\n  (version 1))\n".into());
        let endpoint = text("endpoint");
        let workspace = text("workspace");
        let model = text("model");
        let updated = update_agent_form(
            &source,
            endpoint.unwrap_or_default(),
            workspace.unwrap_or_default(),
            model.unwrap_or_default(),
        );
        if let Err(error) = fs::write(&path, updated) {
            return json_response(stream, 500, &json!({"error": error.to_string()}), origin);
        }
    }
    if let Some(path) = operator_config_path() {
        if body.get("bifrost").is_some() {
            let current = fs::read_to_string(&path).unwrap_or_default();
            let bifrost = body.get("bifrost").cloned().unwrap_or_else(|| json!({}));
            let updated = update_bifrost(
                &current,
                bifrost.get("baseUrl").and_then(|value| value.as_str()),
                bifrost.get("model").and_then(|value| value.as_str()),
                bifrost.get("apiKeyFile").and_then(|value| value.as_str()),
            );
            if let Err(error) = fs::write(&path, updated) {
                return json_response(stream, 500, &json!({"error": error.to_string()}), origin);
            }
        }
    }
    json_response(stream, 200, &agent_settings(), origin)
}

fn update_agent_form(source: &str, endpoint: &str, workspace: &str, model: &str) -> String {
    let block = format!(
        "(agent\n    (endpoint \"{}\")\n    (workspace \"{}\")\n    (model \"{}\"))",
        escape_soil(endpoint),
        escape_soil(workspace),
        escape_soil(model)
    );
    if let Some(start) = source.find("(agent") {
        let bytes = source.as_bytes();
        let mut depth = 0i32;
        let mut end = None;
        for (index, byte) in bytes.iter().enumerate().skip(start) {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index + 1);
                        break;
                    }
                }
                b'"' => {}
                _ => {}
            }
        }
        if let Some(end) = end {
            let mut out = String::new();
            out.push_str(&source[..start]);
            out.push_str(&block);
            out.push_str(&source[end..]);
            return out;
        }
    }
    let Some(at) = source.rfind(')') else {
        return source.to_string();
    };
    let mut out = String::new();
    out.push_str(&source[..at]);
    out.push_str("  ");
    out.push_str(&block);
    out.push('\n');
    out.push_str(&source[at..]);
    out
}

fn escape_soil(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn update_bifrost(
    source: &str,
    base_url: Option<&str>,
    model: Option<&str>,
    api_key_file: Option<&str>,
) -> String {
    let mut lines: Vec<String> = source.lines().map(str::to_string).collect();
    let mut section = None;
    for index in 0..lines.len() {
        let trimmed = lines[index].trim().to_string();
        if trimmed.starts_with('[') {
            section = if trimmed == "[bifrost]" {
                Some(index)
            } else {
                None
            };
        }
        if section.is_none() {
            continue;
        }
        for (key, value) in [
            ("base_url", base_url),
            ("model", model),
            ("api_key_file", api_key_file),
        ] {
            let Some(value) = value else { continue };
            if trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")) {
                lines[index] = format!("{key} = \"{}\"", escape_toml(value));
            }
        }
    }
    if !lines.iter().any(|line| line.trim() == "[bifrost]") {
        let mut prefix = vec!["[bifrost]".to_string()];
        if let Some(value) = base_url {
            prefix.push(format!("base_url = \"{}\"", escape_toml(value)));
        }
        if let Some(value) = model {
            prefix.push(format!("model = \"{}\"", escape_toml(value)));
        }
        if let Some(value) = api_key_file {
            prefix.push(format!("api_key_file = \"{}\"", escape_toml(value)));
        }
        prefix.push(String::new());
        prefix.append(&mut lines);
        lines = prefix;
    }
    let mut out = lines.join("\n");
    if source.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn agent_api(request: &Request, origin: Option<&str>, stream: &mut TcpStream) -> Result<()> {
    if request.method == "OPTIONS" {
        return respond(stream, 204, "text/plain", b"", origin);
    }
    if request.method != "POST" {
        return json_response(stream, 405, &json!({"error": "POST only"}), origin);
    }
    let Some(config) = agent_config() else {
        return json_response(
            stream,
            503,
            &json!({"error": "agent endpoint is not configured in loam.conf.soil"}),
            origin,
        );
    };
    let body: serde_json::Value = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(error) => {
            return json_response(stream, 400, &json!({"error": error.to_string()}), origin);
        }
    };
    let text = body
        .get("text")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return json_response(stream, 400, &json!({"error": "text is required"}), origin);
    }
    let session = body
        .get("sessionId")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    match acp_prompt(&config, &session, text) {
        Ok((session, reply)) => json_response(
            stream,
            200,
            &json!({"sessionId": session, "reply": reply}),
            origin,
        ),
        Err(error) => json_response(stream, 502, &json!({"error": error}), origin),
    }
}

fn acp_rpc(
    endpoint: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(180))
        .build()
        .post(endpoint)
        .set("content-type", "application/json")
        .send_string(&request.to_string())
        .map_err(|error| error.to_string())?;
    let text = response.into_string().map_err(|error| error.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|error| error.to_string())?;
    if let Some(error) = value.get("error") {
        return Err(error.to_string());
    }
    Ok(value.get("result").cloned().unwrap_or(value))
}

fn acp_prompt(config: &AgentConfig, session: &str, text: &str) -> Result<(String, String), String> {
    let session = if session.is_empty() {
        let created = acp_rpc(
            &config.endpoint,
            "session/new",
            json!({"cwd": config.workspace.clone().unwrap_or_else(|| "/".into()), "mcpServers": []}),
        )?;
        created
            .get("sessionId")
            .and_then(|value| value.as_str())
            .ok_or("session/new returned no id")?
            .to_string()
    } else {
        session.to_string()
    };
    if let Some(model) = &config.model {
        let _ = acp_rpc(
            &config.endpoint,
            "session/set_mode",
            json!({"sessionId": session, "modeId": model}),
        );
    }
    let sse = if let Some(prefix) = config.endpoint.strip_suffix("/acp") {
        format!("{prefix}/sse?sessionId={session}")
    } else {
        format!(
            "{}/sse?sessionId={session}",
            config.endpoint.trim_end_matches('/')
        )
    };
    let stream = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .timeout_read(Duration::from_secs(2))
        .build()
        .get(&sse)
        .set("accept", "text/event-stream")
        .call()
        .map_err(|error| error.to_string())?;
    let result = acp_rpc(
        &config.endpoint,
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
    )?;
    let mut reply = String::new();
    if let Some(content) = result.get("content").and_then(|value| value.as_array()) {
        for part in content {
            if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
                reply.push_str(text);
            }
        }
    }
    // ACP leaves the SSE stream open after `end_turn`. Read it on its own
    // thread and stop collecting shortly after the prompt response returns.
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stream.into_reader());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    let Ok(update) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
                        continue;
                    };
                    if let Some(text) = update
                        .get("content")
                        .and_then(|content| content.get("text"))
                        .and_then(|value| value.as_str())
                    {
                        if sender.send(text.to_string()).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(chunk) => reply.push_str(&chunk),
            Err(_) => break,
        }
    }
    Ok((session, reply))
}

struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    const MAX_HEAD: usize = 64 * 1024;
    const MAX_BODY: usize = 16 * 1024 * 1024;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        if buffer.len() > MAX_HEAD {
            anyhow::bail!("request headers exceed {} bytes", MAX_HEAD);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_owned();
    let target = first.next().unwrap_or("/").to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        anyhow::bail!("request body exceeds {} bytes", MAX_BODY);
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(Some(Request {
        method,
        target,
        headers,
        body,
    }))
}

fn json_response(
    stream: &mut TcpStream,
    status: u16,
    body: &serde_json::Value,
    origin: Option<&str>,
) -> Result<()> {
    let bytes = serde_json::to_vec(body)?;
    respond(
        stream,
        status,
        "application/json; charset=utf-8",
        &bytes,
        origin,
    )
}

fn cors(origin: Option<&str>) -> String {
    let Some(origin) = origin else {
        return String::new();
    };
    let allowed = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .is_some_and(|host| host == "myc" || host.ends_with(".myc"));
    if allowed {
        format!(
            "Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: GET, POST, PUT, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: content-type\r\nVary: Origin\r\n"
        )
    } else {
        String::new()
    }
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    body: &[u8],
    origin: Option<&str>,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        415 => "Unsupported Media Type",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-cache\r\n{}X-Content-Type-Options: nosniff\r\n\r\n",
        body.len(),
        cors(origin)
    );
    stream.write_all(head.as_bytes())?;
    if status != 204 && !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()?;
    Ok(())
}

fn media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The lease endpoint's bind address is not a route; it is a local hint.
pub fn loopback_endpoint(listener: &TcpListener) -> Result<String> {
    let addr = listener.local_addr().context("web listener address")?;
    Ok(format!("http://{addr}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(root: &Path, manifest_name: &str, manifest: &str) {
        let package = root.join("site");
        fs::create_dir_all(&package).expect("package");
        fs::write(package.join(manifest_name), manifest).expect("manifest");
        fs::write(package.join("index.html"), "<h1>site</h1>").expect("page");
    }

    #[test]
    #[ignore]
    fn debug_phonebook_discovery() {
        let surfaces = discover(&[PathBuf::from("/Users/ajmwagar/.mycelium/spores")]).unwrap();
        for surface in &surfaces {
            eprintln!(
                "surface name={} id={} kind={} entry={}",
                surface.name,
                surface.spore_id,
                surface.kind,
                surface.entry.display()
            );
        }
        assert!(surfaces.iter().any(|surface| surface.name == "phonebook"));
    }

    #[test]
    fn discovers_a_web_surface_from_spore_soil() {
        let temp = tempfile::tempdir().expect("temp");
        package(
            temp.path(),
            "spore.soil",
            r#"(spore
                 (id "org.mycelium.web")
                 (version "0.1.0")
                 (anycast (name "web") (kind "web") (alpns ("soil/http/1")))
                 (surfaces (surface "web" (kind "web") (entry "index.html"))))"#,
        );
        let surfaces = discover(&[temp.path().to_path_buf()]).expect("discover");
        assert_eq!(surfaces.len(), 1);
        let surface = &surfaces[0];
        assert_eq!(surface.spore_id, "org.mycelium.web");
        assert_eq!(surface.version, "0.1.0");
        assert_eq!(surface.name, "web");
        assert_eq!(surface.kind, "web");
        assert_eq!(surface.anycast.alpns, vec!["soil/http/1"]);
        assert_eq!(surface.entry, temp.path().join("site/index.html"));
    }

    #[test]
    fn a_soil_manifest_keeps_its_web_root_and_write_grant() {
        let temp = tempfile::tempdir().expect("temp");
        package(
            temp.path(),
            "spore.soil",
            r#"(spore
                 (id "fpl.fabrication-studio")
                 (version "0.1.0")
                 (anycast (name "studio") (kind "web") (alpns ("soil/http/1")))
                 (surfaces (surface "web" (kind "web") (entry "index.html")))
                 (web (vfs-root "studio"))
                 (grants
                   ("soil:vfs" "read pipeline documents")
                   ("soil:vfs-mut" "save pipeline documents")))"#,
        );
        let surfaces = discover(&[temp.path().to_path_buf()]).expect("discover");
        let surface = &surfaces[0];
        assert_eq!(
            surface.web.as_ref().map(|web| web.vfs_root.as_str()),
            Some("studio")
        );
        assert!(
            surface.writable,
            "the soil:vfs-mut grant must survive discovery"
        );
    }

    #[test]
    fn a_soil_package_without_an_anycast_claim_is_not_leased() {
        let temp = tempfile::tempdir().expect("temp");
        package(
            temp.path(),
            "spore.soil",
            r#"(spore
                 (id "org.mycelium.web")
                 (version "0.1.0")
                 (surfaces (surface "web" (kind "web") (entry "index.html"))))"#,
        );
        assert!(
            discover(&[temp.path().to_path_buf()])
                .expect("discover")
                .is_empty()
        );
    }

    #[cfg(feature = "vfs")]
    #[test]
    fn a_web_surface_vfs_root_is_bounded() {
        assert_eq!(
            scoped_path("notes", "hello.md").as_deref(),
            Some("notes/hello.md")
        );
        assert_eq!(
            scoped_path("", "notes/hello.md").as_deref(),
            Some("notes/hello.md")
        );
        assert!(scoped_path("notes", "../secrets").is_none());
    }
}
