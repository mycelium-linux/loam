//! `loam.conf.soil`: host defaults, not package state.
//!
//! A Spore's `spore.soil` says what the package declares. This file says how
//! this host chooses to run it: where the daemon socket lives, which roots to
//! scan, where logs go, and whether manifest web surfaces publish leases. It is
//! parsed as data and never evaluated.
//!
//! ```soil
//! (loam
//!   (version 1)
//!   (daemon
//!     (socket "~/.local/share/loam/loamd.sock")
//!     (max-instances 8)
//!     (roots ("~/.local/share/fungos/commands"
//!             "~/.local/share/fungos/spores"))
//!     (log (dir "~/.local/state/loam/logs") (max-bytes 1048576) (keep 4))
//!     (web (listen "127.0.0.1:8799")))
//!   (mycelium
//!     (home "~/.mycelium")
//!     (publish-leases #t)))
//! ```
//!
//! Live leases, current log sizes and installed packages are state. They stay
//! out of this file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::spore::soil::{Sexp, parse_sexps, string, strings};

/// Values a config file may supply. Every field is optional so the daemon can
/// keep its built-in default when a line is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub socket: Option<PathBuf>,
    pub roots: Vec<PathBuf>,
    pub max_instances: Option<usize>,
    pub image_runtime: Option<PathBuf>,
    pub window_runtime: Option<PathBuf>,
    pub web_listen: Option<String>,
    pub log_root: Option<PathBuf>,
    pub log_max_bytes: Option<u64>,
    pub log_keep: Option<usize>,
    pub mycelium_home: Option<PathBuf>,
    pub publish_leases: Option<bool>,
}

impl Config {
    /// Read one `(loam ...)` form.
    pub fn parse(source: &str) -> Result<Self> {
        let forms = parse_sexps(source).map_err(anyhow::Error::msg)?;
        let loam = forms
            .iter()
            .find_map(|form| match form {
                Sexp::List(items)
                    if matches!(items.first(), Some(Sexp::Atom(name)) if name == "loam") =>
                {
                    Some(items)
                }
                _ => None,
            })
            .context("loam.conf.soil has no (loam ...) form")?;

        let mut config = Self::default();
        let mut version = None;
        for field in loam.iter().skip(1) {
            let Sexp::List(items) = field else {
                continue;
            };
            let Some(Sexp::Atom(key)) = items.first() else {
                continue;
            };
            match key.as_str() {
                "version" => {
                    version = Some(number(items.get(1), "version")?);
                }
                "daemon" => config.parse_daemon(items)?,
                "mycelium" => config.parse_mycelium(items)?,
                other => bail!("loam.conf.soil: unknown top-level option {other:?}"),
            }
        }
        match version {
            Some(1) => {}
            Some(version) => bail!("loam.conf.soil: unsupported version {version}"),
            None => bail!("loam.conf.soil: missing (version 1)"),
        }
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let source =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&source).with_context(|| path.display().to_string())
    }

    /// The per-user config, when one exists.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
        {
            return Some(PathBuf::from(config).join("loam/loam.conf.soil"));
        }
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("loam/loam.conf.soil")
        })
    }

    fn parse_daemon(&mut self, items: &[Sexp]) -> Result<()> {
        for field in items.iter().skip(1) {
            let Sexp::List(pair) = field else {
                continue;
            };
            let Some(Sexp::Atom(key)) = pair.first() else {
                continue;
            };
            match key.as_str() {
                "socket" => self.socket = Some(path(pair.get(1), "daemon socket")?),
                "max-instances" => {
                    self.max_instances = Some(usize_number(pair.get(1), "daemon max-instances")?)
                }
                "roots" => {
                    self.roots = strings(pair.get(1))
                        .into_iter()
                        .map(|path| expand_path(&path))
                        .collect()
                }
                "image-runtime" => {
                    self.image_runtime = Some(path(pair.get(1), "daemon image-runtime")?)
                }
                "window-runtime" => {
                    self.window_runtime = Some(path(pair.get(1), "daemon window-runtime")?)
                }
                "log" => self.parse_log(pair)?,
                "web" => self.parse_web(pair)?,
                other => bail!("loam.conf.soil: unknown daemon option {other:?}"),
            }
        }
        Ok(())
    }

    fn parse_log(&mut self, items: &[Sexp]) -> Result<()> {
        for field in items.iter().skip(1) {
            let Sexp::List(pair) = field else {
                continue;
            };
            let Some(Sexp::Atom(key)) = pair.first() else {
                continue;
            };
            match key.as_str() {
                "dir" => self.log_root = Some(path(pair.get(1), "log dir")?),
                "max-bytes" => self.log_max_bytes = Some(number(pair.get(1), "log max-bytes")?),
                "keep" => self.log_keep = Some(usize_number(pair.get(1), "log keep")?),
                other => bail!("loam.conf.soil: unknown log option {other:?}"),
            }
        }
        Ok(())
    }

    fn parse_web(&mut self, items: &[Sexp]) -> Result<()> {
        for field in items.iter().skip(1) {
            let Sexp::List(pair) = field else {
                continue;
            };
            let Some(Sexp::Atom(key)) = pair.first() else {
                continue;
            };
            match key.as_str() {
                "listen" => {
                    self.web_listen =
                        Some(string(pair.get(1)).context("web listen needs an address")?)
                }
                other => bail!("loam.conf.soil: unknown web option {other:?}"),
            }
        }
        Ok(())
    }

    fn parse_mycelium(&mut self, items: &[Sexp]) -> Result<()> {
        for field in items.iter().skip(1) {
            let Sexp::List(pair) = field else {
                continue;
            };
            let Some(Sexp::Atom(key)) = pair.first() else {
                continue;
            };
            match key.as_str() {
                "home" => self.mycelium_home = Some(path(pair.get(1), "mycelium home")?),
                "publish-leases" => {
                    self.publish_leases = Some(boolean(pair.get(1), "mycelium publish-leases")?)
                }
                other => bail!("loam.conf.soil: unknown mycelium option {other:?}"),
            }
        }
        Ok(())
    }
}

fn path(value: Option<&Sexp>, name: &str) -> Result<PathBuf> {
    string(value)
        .map(|value| expand_path(&value))
        .with_context(|| format!("{name} needs a path"))
}

/// Only a leading `~` is expanded. `$VARS` are deliberately not: a config file
/// should not become a shell.
fn expand_path(value: &str) -> PathBuf {
    if value == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

fn number(value: Option<&Sexp>, name: &str) -> Result<u64> {
    let value = string(value).with_context(|| format!("{name} needs a number"))?;
    value
        .parse()
        .with_context(|| format!("{name} must be a number, got {value:?}"))
}

fn usize_number(value: Option<&Sexp>, name: &str) -> Result<usize> {
    let value = number(value, name)?;
    usize::try_from(value).with_context(|| format!("{name} is too large: {value}"))
}

fn boolean(value: Option<&Sexp>, name: &str) -> Result<bool> {
    match string(value).as_deref() {
        Some("#t" | "true") => Ok(true),
        Some("#f" | "false") => Ok(false),
        Some(value) => bail!("{name} must be #t or #f, got {value:?}"),
        None => bail!("{name} needs a boolean"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_daemon_log_web_and_mycelium_defaults() {
        let config = Config::parse(
            r#"
            (loam
              (version 1)
              (daemon
                (socket "/tmp/loamd.sock")
                (max-instances 4)
                (roots ("/a" "/b"))
                (image-runtime "/bin/soil-image")
                (window-runtime "/bin/loam-window")
                (log (dir "/var/log/loam") (max-bytes 4096) (keep 2))
                (web (listen "127.0.0.1:9000")))
              (mycelium
                (home "/tmp/mycelium")
                (publish-leases #f)))
            "#,
        )
        .expect("config");
        assert_eq!(config.socket.as_deref(), Some(Path::new("/tmp/loamd.sock")));
        assert_eq!(config.max_instances, Some(4));
        assert_eq!(config.roots, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(
            config.image_runtime.as_deref(),
            Some(Path::new("/bin/soil-image"))
        );
        assert_eq!(
            config.window_runtime.as_deref(),
            Some(Path::new("/bin/loam-window"))
        );
        assert_eq!(config.log_root.as_deref(), Some(Path::new("/var/log/loam")));
        assert_eq!(config.log_max_bytes, Some(4096));
        assert_eq!(config.log_keep, Some(2));
        assert_eq!(config.web_listen.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(
            config.mycelium_home.as_deref(),
            Some(Path::new("/tmp/mycelium"))
        );
        assert_eq!(config.publish_leases, Some(false));
    }

    #[test]
    fn a_minimal_file_only_needs_a_version() {
        assert_eq!(
            Config::parse("(loam (version 1))").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn refuses_unknown_options_and_versions() {
        assert!(Config::parse("(loam (version 2))").is_err());
        assert!(Config::parse("(loam (version 1) (daemon (wibble #t)))").is_err());
        assert!(Config::parse("(loam (version 1) (mycelium (publish-leases maybe)))").is_err());
    }

    #[test]
    fn expands_only_a_leading_tilde() {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        assert_eq!(expand_path("~/x"), home.join("x"));
        assert_eq!(expand_path("$HOME/x"), PathBuf::from("$HOME/x"));
    }
}
