//! The `spore.soil` manifest, read for the fields a host needs before install.
//!
//! `spore.toml` is the installed-package manifest. `spore.soil` is the source
//! manifest a Soil package carries while it is being built and edited: the same
//! identity, surface and anycast facts in the language the package is written
//! in. A host should not have to translate one into the other just to serve a
//! declared web surface or publish its lease.
//!
//! This reader is deliberately a reader, not an evaluator. It recognizes the
//! data forms below and ignores everything else, so a build directive cannot
//! turn manifest discovery into code execution.
//!
//! ```soil
//! (spore
//!   (id "org.mycelium.web")
//!   (version "0.1.0")
//!   (module "module.wasm")
//!   (anycast
//!     (name "web")
//!     (kind "web")
//!     (alpns ("soil/http/1"))
//!     (ttl_secs 240))
//!   (surfaces
//!     (surface "web" (kind "web") (entry "index.html")))
//!   (http-client
//!     (origin "http://127.0.0.1:8791")
//!     (credential "default")))
//! ```

use super::{Anycast, Error, MAX_LEASE_SECS, Result, Surface, SurfaceKind, default_lease_secs};

/// The subset of a `spore.soil` manifest a daemon needs to route a surface.
#[derive(Debug, Clone)]
pub struct SoilManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub module: Option<String>,
    pub anycast: Option<Anycast>,
    pub surfaces: Vec<Surface>,
    /// Declared grants: the request half of consent. The host decides.
    pub grants: std::collections::BTreeMap<String, String>,
    /// The bounded workspace prefix a web surface may address.
    pub web: Option<crate::spore::Web>,
    /// Outbound HTTP the Spore asks the host to permit.
    pub http_client: Option<HttpClient>,
    /// A native sidecar behind this package's wasm adapter.
    ///
    /// Absent from this dialect until now, which meant a package declared in
    /// Soil lost its sidecar entirely: the lock did not cover it, the provider
    /// registered without `native`, and every call answered `unavailable` with
    /// nothing anywhere saying why.
    pub plugin: Option<crate::spore::Plugin>,
}

/// The request half of `soil:http`: origins and opaque credential handles.
///
/// No reference, token or purpose lives here. The host resolves a handle from
/// its own config and decides whether the origin is reachable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpClient {
    pub origins: Vec<String>,
    pub credentials: Vec<String>,
}

impl SoilManifest {
    /// Read one `(spore ...)` form.
    pub fn parse(source: &str) -> Result<Self> {
        let forms = parse_sexps(source).map_err(Error::Invalid)?;
        let spore = forms
            .iter()
            .find_map(|form| match form {
                Sexp::List(items)
                    if matches!(items.first(), Some(Sexp::Atom(name)) if name == "spore") =>
                {
                    Some(items)
                }
                _ => None,
            })
            .ok_or_else(|| Error::Invalid("spore.soil has no (spore ...) form".into()))?;

        let mut id = None;
        let mut name = None;
        let mut version = None;
        let mut module = None;
        let mut anycast = None;
        let mut surfaces = Vec::new();
        let mut grants = std::collections::BTreeMap::new();
        let mut web = None;
        let mut http_client = None;
        let mut plugin = None;
        for field in spore.iter().skip(1) {
            let Sexp::List(items) = field else {
                continue;
            };
            let Some(Sexp::Atom(key)) = items.first() else {
                continue;
            };
            match key.as_str() {
                "id" => id = string(items.get(1)),
                "name" => name = string(items.get(1)),
                "version" => version = string(items.get(1)),
                // (module "path") and (module (path "path")) both mean the
                // same thing; the nested form mirrors the toml's [module] path,
                // which is what packages converted from toml still write.
                "module" => {
                    module = match items.get(1) {
                        Some(Sexp::List(inner)) if string(inner.first()).as_deref() == Some("path") => {
                            string(inner.get(1))
                        }
                        other => string(other),
                    }
                }
                "anycast" => anycast = parse_anycast(items)?,
                "surfaces" => surfaces = parse_surfaces(items)?,
                // (plugin (sidecar "sidecar/name"))
                "plugin" => {
                    if let Some(Sexp::List(inner)) = items.get(1) {
                        if string(inner.first()).as_deref() == Some("sidecar") {
                            plugin = Some(crate::spore::Plugin {
                                sidecar: string(inner.get(1)),
                            });
                        }
                    }
                }
                // (grants ("soil:vfs" "why it is needed") ...)
                "grants" => {
                    for grant in items.iter().skip(1) {
                        let Sexp::List(pair) = grant else { continue };
                        if let Some(name) = string(pair.first()) {
                            let reason = string(pair.get(1)).unwrap_or_default();
                            grants.insert(name, reason);
                        }
                    }
                }
                "http-client" => http_client = Some(parse_http_client(items)?),
                // (web (vfs-root "studio"))
                "web" => {
                    for field in items.iter().skip(1) {
                        let Sexp::List(pair) = field else { continue };
                        if let (Some(Sexp::Atom(key)), Some(value)) =
                            (pair.first(), string(pair.get(1)))
                        {
                            if key == "vfs-root" {
                                web = Some(crate::spore::Web { vfs_root: value });
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let id = id.ok_or_else(|| Error::Invalid("spore.soil has no id".into()))?;
        let version = version.ok_or_else(|| Error::Invalid("spore.soil has no version".into()))?;
        let name = name.unwrap_or_else(|| default_name(&id));
        Ok(Self {
            id,
            name,
            version,
            module,
            anycast,
            surfaces,
            grants,
            web,
            http_client,
            plugin,
        })
    }
}

/// A package name derived from an id when the manifest does not spell one out.
fn default_name(id: &str) -> String {
    id.rsplit_once(':')
        .map(|(_, name)| name)
        .or_else(|| id.rsplit_once('/').map(|(_, name)| name))
        .or_else(|| id.rsplit_once('.').map(|(_, name)| name))
        .unwrap_or(id)
        .to_owned()
}

fn parse_http_client(items: &[Sexp]) -> Result<HttpClient> {
    let mut client = HttpClient::default();
    for field in items.iter().skip(1) {
        let Sexp::List(pair) = field else {
            continue;
        };
        let Some(Sexp::Atom(key)) = pair.first() else {
            continue;
        };
        match key.as_str() {
            "origin" => {
                let origin = string(pair.get(1))
                    .ok_or_else(|| Error::Invalid("http-client origin needs a value".into()))?;
                client.origins.push(origin);
            }
            "origins" => client.origins.extend(strings(pair.get(1))),
            "credential" => {
                let handle = string(pair.get(1)).ok_or_else(|| {
                    Error::Invalid("http-client credential needs a handle".into())
                })?;
                client.credentials.push(handle);
            }
            other => {
                return Err(Error::Invalid(format!(
                    "http-client has unknown option {other:?}"
                )));
            }
        }
    }
    if client.origins.is_empty() {
        return Err(Error::Invalid(
            "http-client needs at least one origin".into(),
        ));
    }
    for origin in &client.origins {
        if !(origin.starts_with("http://") || origin.starts_with("https://")) {
            return Err(Error::Invalid(format!(
                "http-client origin must be an absolute http(s) URL, got {origin:?}"
            )));
        }
    }
    Ok(client)
}

fn parse_anycast(items: &[Sexp]) -> Result<Option<Anycast>> {
    if items.len() == 1 {
        return Ok(None);
    }
    let mut name = None;
    let mut kind = None;
    let mut alpns = Vec::new();
    let mut ttl_secs = default_lease_secs();
    for item in items.iter().skip(1) {
        let Sexp::List(pair) = item else {
            continue;
        };
        let Some(Sexp::Atom(key)) = pair.first() else {
            continue;
        };
        match key.as_str() {
            "name" => name = string(pair.get(1)),
            "kind" => kind = string(pair.get(1)),
            "alpns" => alpns = strings(pair.get(1)),
            "ttl_secs" | "ttl-secs" => {
                if let Some(Sexp::Atom(value)) = pair.get(1) {
                    ttl_secs = value
                        .parse()
                        .map_err(|_| Error::Invalid("anycast ttl_secs must be a number".into()))?;
                }
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| Error::Invalid("anycast needs a name".into()))?;
    let kind = kind.ok_or_else(|| Error::Invalid("anycast needs a kind".into()))?;
    if ttl_secs == 0 || ttl_secs > MAX_LEASE_SECS {
        return Err(Error::Invalid(format!(
            "anycast ttl_secs {ttl_secs} must be 1..={MAX_LEASE_SECS}"
        )));
    }
    Ok(Some(Anycast {
        name,
        kind,
        alpns,
        ttl_secs,
    }))
}

fn parse_surfaces(items: &[Sexp]) -> Result<Vec<Surface>> {
    let mut surfaces = Vec::new();
    for item in items.iter().skip(1) {
        let Sexp::List(surface) = item else {
            continue;
        };
        let Some(Sexp::Atom(head)) = surface.first() else {
            continue;
        };
        if head != "surface" {
            continue;
        }
        let name =
            string(surface.get(1)).ok_or_else(|| Error::Invalid("surface needs a name".into()))?;
        let mut kind = None;
        let mut entry = None;
        let mut requires = None;
        for field in surface.iter().skip(2) {
            let Sexp::List(pair) = field else {
                continue;
            };
            let Some(Sexp::Atom(key)) = pair.first() else {
                continue;
            };
            match key.as_str() {
                "kind" => {
                    let value = string(pair.get(1))
                        .ok_or_else(|| Error::Invalid("surface kind needs a value".into()))?;
                    kind = SurfaceKind::parse(&value);
                    if kind.is_none() {
                        return Err(Error::Invalid(format!(
                            "surface {name:?} has unknown kind {value:?}"
                        )));
                    }
                }
                "entry" => entry = string(pair.get(1)),
                "assets" => {
                    if let Some(assets) = string(pair.get(1)) {
                        entry = Some(if assets.ends_with('/') {
                            format!("{assets}index.html")
                        } else {
                            assets
                        });
                    }
                }
                "requires" => requires = string(pair.get(1)),
                _ => {}
            }
        }
        let kind = kind.or_else(|| SurfaceKind::parse(&name)).ok_or_else(|| {
            Error::Invalid(format!(
                "surface {name:?} declares no kind and its name is not one"
            ))
        })?;
        surfaces.push(Surface {
            name,
            kind,
            entry,
            requires,
        });
    }
    Ok(surfaces)
}

pub(crate) fn string(value: Option<&Sexp>) -> Option<String> {
    match value {
        Some(Sexp::String(value)) | Some(Sexp::Atom(value)) => Some(value.clone()),
        _ => None,
    }
}

pub(crate) fn strings(value: Option<&Sexp>) -> Vec<String> {
    match value {
        Some(Sexp::List(values)) => values
            .iter()
            .filter_map(|value| string(Some(value)))
            .collect(),
        Some(value) => string(Some(value)).into_iter().collect(),
        None => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sexp {
    Atom(String),
    String(String),
    List(Vec<Sexp>),
}

pub(crate) fn parse_sexps(source: &str) -> std::result::Result<Vec<Sexp>, String> {
    let mut stack = vec![Vec::new()];
    let mut chars = source.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        match ch {
            ';' => {
                for (_, next) in chars.by_ref() {
                    if next == '\n' {
                        break;
                    }
                }
            }
            '(' => stack.push(Vec::new()),
            ')' => {
                let list = stack.pop().ok_or_else(|| "unbalanced )".to_owned())?;
                stack
                    .last_mut()
                    .ok_or_else(|| "unbalanced )".to_owned())?
                    .push(Sexp::List(list));
            }
            '"' => {
                let mut value = String::new();
                loop {
                    match chars.next().map(|(_, ch)| ch) {
                        Some('"') => break,
                        Some('\\') => match chars.next().map(|(_, ch)| ch) {
                            Some('n') => value.push('\n'),
                            Some('t') => value.push('\t'),
                            Some(ch) => value.push(ch),
                            None => return Err("unterminated string".to_owned()),
                        },
                        Some(ch) => value.push(ch),
                        None => return Err("unterminated string".to_owned()),
                    }
                }
                stack
                    .last_mut()
                    .ok_or_else(|| "string outside a form".to_owned())?
                    .push(Sexp::String(value));
            }
            ch if ch.is_whitespace() => {}
            ch => {
                let mut atom = String::from(ch);
                while let Some((_, next)) = chars.peek() {
                    if next.is_whitespace() || matches!(next, '(' | ')' | ';') {
                        break;
                    }
                    atom.push(*next);
                    chars.next();
                }
                stack
                    .last_mut()
                    .ok_or_else(|| "atom outside a form".to_owned())?
                    .push(Sexp::Atom(atom));
            }
        }
    }
    if stack.len() != 1 {
        return Err("unterminated form".to_owned());
    }
    Ok(stack.pop().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_identity_anycast_and_web_surface() {
        let manifest = SoilManifest::parse(
            r#"
            (spore
              (id "org.mycelium.web")
              (version "0.1.0")
              (module "module.wasm")
              (anycast
                (name "web")
                (kind "web")
                (alpns ("soil/http/1"))
                (ttl_secs 240))
              (surfaces
                (surface "web" (kind "web") (entry "index.html"))))
            "#,
        )
        .expect("a soil manifest");
        assert_eq!(manifest.id, "org.mycelium.web");
        assert_eq!(manifest.name, "web");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.module.as_deref(), Some("module.wasm"));
        let anycast = manifest.anycast.expect("anycast");
        assert_eq!(anycast.name, "web");
        assert_eq!(anycast.kind, "web");
        assert_eq!(anycast.alpns, vec!["soil/http/1"]);
        assert_eq!(anycast.ttl_secs, 240);
        assert_eq!(manifest.surfaces.len(), 1);
        assert_eq!(manifest.surfaces[0].kind, SurfaceKind::Web);
        assert_eq!(manifest.surfaces[0].entry.as_deref(), Some("index.html"));
    }

    #[test]
    fn a_dotted_id_keeps_its_last_component_as_the_name() {
        let manifest = SoilManifest::parse("(spore (id \"dev.local:probe\") (version \"0.1.0\"))")
            .expect("a soil manifest");
        assert_eq!(manifest.name, "probe");
        assert!(manifest.anycast.is_none());
    }

    #[test]
    fn a_surface_without_a_kind_uses_its_name() {
        let manifest = SoilManifest::parse(
            r#"(spore
                 (id "acme:site") (version "0.1.0")
                 (surfaces (surface "web" (assets "assets/web/"))))"#,
        )
        .expect("a soil manifest");
        assert_eq!(manifest.surfaces[0].kind, SurfaceKind::Web);
        assert_eq!(
            manifest.surfaces[0].entry.as_deref(),
            Some("assets/web/index.html")
        );
    }

    #[test]
    fn refuses_an_anycast_without_a_name_or_kind() {
        assert!(
            SoilManifest::parse(
                r#"(spore (id "acme:site") (version "0.1.0") (anycast (kind "web")))"#
            )
            .is_err()
        );
        assert!(
            SoilManifest::parse(
                r#"(spore (id "acme:site") (version "0.1.0") (anycast (name "site")))"#
            )
            .is_err()
        );
    }
    #[test]
    fn reads_http_client_origins_and_handles() {
        let manifest = SoilManifest::parse(
            r#"
            (spore
              (id "dev.fungos.acp-agents")
              (version "0.1.0")
              (http-client
                (origin "http://127.0.0.1:8791")
                (credential "default")))
            "#,
        )
        .expect("a soil manifest");
        let client = manifest.http_client.expect("http client");
        assert_eq!(client.origins, vec!["http://127.0.0.1:8791"]);
        assert_eq!(client.credentials, vec!["default"]);
    }

    #[test]
    fn refuses_a_relative_http_client_origin() {
        assert!(
            SoilManifest::parse(
                r#"(spore (id "acme:site") (version "0.1.0")
                     (http-client (origin "127.0.0.1:8791")))"#
            )
            .is_err()
        );
    }
}
