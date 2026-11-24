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
//!   ;; A command is an entrypoint too: the runner calls wasi:cli/run on it.
//!   (commands
//!     (command (name "say") (summary "print one line") (module "bin/say.wasm"))))
//! ```

use super::{
    Anycast, Error, MAX_LEASE_SECS, Result, SporeCommand, Surface, SurfaceKind, default_lease_secs,
};

/// The subset of a `spore.soil` manifest a daemon needs to route a surface.
#[derive(Debug, Clone)]
pub struct SoilManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Listing facts carried by the source manifest. Optional because the
    /// reader existed before these fields did; `Manifest::from_soil` fills
    /// honest defaults when they are absent.
    pub license: Option<String>,
    pub summary: Option<String>,
    pub homepage: Option<String>,
    pub description: Option<String>,
    pub module: Option<String>,
    pub anycast: Option<Anycast>,
    pub surfaces: Vec<Surface>,
    /// Declared grants: the request half of consent. The host decides.
    pub grants: std::collections::BTreeMap<String, String>,
    /// The bounded workspace prefix a web surface may address.
    pub web: Option<crate::spore::Web>,
    /// Commands this Spore installs into a shell's virtual PATH.
    pub commands: Vec<SporeCommand>,
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
        let mut license = None;
        let mut summary = None;
        let mut homepage = None;
        let mut description = None;
        let mut module = None;
        let mut anycast = None;
        let mut surfaces = Vec::new();
        let mut grants = std::collections::BTreeMap::new();
        let mut web = None;
        let mut commands = Vec::new();
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
                "license" => license = string(items.get(1)),
                "summary" => summary = string(items.get(1)),
                "homepage" => homepage = string(items.get(1)),
                "description" => description = string(items.get(1)),
                "module" => module = string(items.get(1)),
                "anycast" => anycast = parse_anycast(items)?,
                "surfaces" => surfaces = parse_surfaces(items)?,
                // (commands (command (name "say") (summary "…") (module "bin/say.wasm")) ...)
                "commands" => commands = parse_commands(items)?,
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
                // (web (vfs-root "studio"))
                "web" => {
                    for field in items.iter().skip(1) {
                        let Sexp::List(pair) = field else { continue };
                        if let (Some(Sexp::Atom(key)), Some(value)) =
                            (pair.first(), string(pair.get(1)))
                            && key == "vfs-root"
                        {
                            web = Some(crate::spore::Web { vfs_root: value });
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
            license,
            summary,
            homepage,
            description,
            module,
            anycast,
            surfaces,
            grants,
            web,
            commands,
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
        let Some(kind) = kind.or_else(|| SurfaceKind::parse(&name)) else {
            // This reader answers one question: what does a router serve? A
            // `cli` or build-only surface is real in the source manifest but
            // has no presentation route here, so skip it rather than dropping
            // the whole manifest (and the web surface beside it).
            continue;
        };
        surfaces.push(Surface {
            name,
            kind,
            entry,
            requires,
        });
    }
    Ok(surfaces)
}

/// One `(command …)` per command: the name a shell resolves, one line of help,
/// and the component the runner calls `wasi:cli/run` on.
///
/// There is deliberately no per-command grant list. Consent is the Spore's
/// `grants` section: a command is a face of the package, not a second policy.
fn parse_commands(items: &[Sexp]) -> Result<Vec<SporeCommand>> {
    let mut commands = Vec::new();
    for item in items.iter().skip(1) {
        let Sexp::List(command) = item else {
            continue;
        };
        let Some(Sexp::Atom(head)) = command.first() else {
            continue;
        };
        if head != "command" {
            continue;
        }
        let (mut name, mut summary, mut module) = (None, None, None);
        for field in command.iter().skip(1) {
            let Sexp::List(pair) = field else {
                continue;
            };
            let Some(Sexp::Atom(key)) = pair.first() else {
                continue;
            };
            match key.as_str() {
                "name" => name = string(pair.get(1)),
                "summary" => summary = string(pair.get(1)),
                "module" => module = string(pair.get(1)),
                _ => {}
            }
        }
        let name = name.ok_or_else(|| Error::Invalid("command needs a name".into()))?;
        let summary =
            summary.ok_or_else(|| Error::Invalid(format!("command {name:?} needs a summary")))?;
        let module =
            module.ok_or_else(|| Error::Invalid(format!("command {name:?} needs a module")))?;
        if !module.ends_with(".wasm") {
            return Err(Error::Invalid(format!(
                "command {name:?} module must end in .wasm"
            )));
        }
        commands.push(SporeCommand {
            name,
            summary,
            module,
        });
    }
    Ok(commands)
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
    fn reads_commands_from_the_soil_manifest() {
        let manifest = SoilManifest::parse(
            r#"
            (spore
              (id "org.mycelium.say")
              (version "0.1.0")
              (grants
                ("soil:time" "stamps its line"))
              (commands
                (command (name "say") (summary "print one line") (module "bin/say.wasm"))))
            "#,
        )
        .expect("a soil manifest");
        assert!(manifest.module.is_none(), "a command-only Spore has no module");
        assert_eq!(manifest.commands.len(), 1, "{:?}", manifest.commands);
        assert_eq!(manifest.commands[0].name, "say");
        assert_eq!(manifest.commands[0].summary, "print one line");
        assert_eq!(manifest.commands[0].module, "bin/say.wasm");
        assert_eq!(
            manifest.grants.get("soil:time").map(String::as_str),
            Some("stamps its line")
        );
    }

    #[test]
    fn a_command_module_must_be_a_wasm_component() {
        let error = SoilManifest::parse(
            r#"
            (spore
              (id "org.mycelium.say")
              (version "0.1.0")
              (commands (command (name "say") (summary "s") (module "say.lua"))))
            "#,
        )
        .expect_err("a .lua command is not a component");
        assert!(format!("{error}").contains(".wasm"), "{error}");
    }

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
    fn reads_the_web_root_and_grants_from_the_source_manifest() {
        let manifest = SoilManifest::parse(
            r#"(spore
                 (id "fpl.fabrication-studio")
                 (version "0.1.0")
                 (surfaces
                   (surface "web" (kind "web") (entry "web/index.html"))
                   (surface "cli" (kind "cli") (world "wasi:cli/command@0.2.0")))
                 (web (vfs-root "studio"))
                 (grants
                   ("soil:vfs" "read pipeline documents")
                   ("soil:vfs-mut" "save pipeline documents")))"#,
        )
        .expect("a soil manifest");
        assert_eq!(manifest.web.expect("web root").vfs_root, "studio");
        assert_eq!(manifest.surfaces.len(), 1);
        assert_eq!(manifest.surfaces[0].kind, SurfaceKind::Web);
        assert!(manifest.grants.contains_key("soil:vfs"));
        assert!(manifest.grants.contains_key("soil:vfs-mut"));
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
}
