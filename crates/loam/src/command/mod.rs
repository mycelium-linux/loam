//! Signed-command registry and wasm CLI runner.
//!
//! The registry is a virtual PATH: names resolve to installed command
//! directories, never to host binaries. The runner uses WASI stdio and links
//! only the host capabilities configured for that command.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wasmtime::component::Resource;
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{
    WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
    p2::{
        bindings::sync::Command,
        pipe::{MemoryInputPipe, MemoryOutputPipe},
    },
};

use crate::records_host::RecordsBookHost;
use crate::vfs_host::{VfsHost, VfsWriter, cid, index, vfs, vfs_mut};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    command: CommandMeta,
    #[serde(default)]
    limits: Limits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandMeta {
    name: String,
    version: String,
    summary: String,
    module: String,
    /// Capabilities the command imports. A standalone package has no
    /// `spore.toml` grants section, so it declares them here.
    #[serde(default)]
    requires: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default = "default_stdout")]
    max_stdout_bytes: usize,
    #[serde(default = "default_stderr")]
    max_stderr_bytes: usize,
    #[serde(default = "default_memory")]
    max_memory_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout(),
            max_stdout_bytes: default_stdout(),
            max_stderr_bytes: default_stderr(),
            max_memory_bytes: default_memory(),
        }
    }
}

fn default_timeout() -> u64 {
    30_000
}
fn default_stdout() -> usize {
    1024 * 1024
}
fn default_stderr() -> usize {
    1024 * 1024
}
fn default_memory() -> usize {
    256 * 1024 * 1024
}

/// Per-run limits, normally taken from the package's command entry.
///
/// A supervisor may override these for one instance without rewriting the
/// package manifest. The runner treats them as hard host limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    pub timeout_ms: u64,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_memory_bytes: usize,
}

impl RunLimits {
    pub fn from_entry(entry: &CommandEntry) -> Self {
        Self {
            timeout_ms: entry.timeout_ms,
            max_stdout_bytes: entry.max_stdout_bytes,
            max_stderr_bytes: entry.max_stderr_bytes,
            max_memory_bytes: entry.max_memory_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub name: String,
    pub version: String,
    pub summary: String,
    /// The package directory that declares it.
    pub package: PathBuf,
    /// Capabilities the package declares for this command. A command is run by a
    /// host that links some of them and not others, so the registry checks this
    /// before it promises a program.
    pub requires: Vec<String>,
    pub module: PathBuf,
    /// Composed runtime module, when resource providers were linked at install.
    pub runtime_module: Option<PathBuf>,
    /// Composition cache identity for `runtime_module`.
    pub composition_key: Option<String>,
    pub timeout_ms: u64,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_memory_bytes: usize,
}

/// A candidate that did not become a command, and why.
///
/// The registry keeps answering with everything else. One package that names a
/// module it does not carry, or one name claimed twice, must not empty a shell's
/// PATH; the refusal is what keeps the loss visible instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub name: String,
    pub reason: String,
}

pub mod cli;
pub mod plugin;
pub mod plugin_ipc;

/// What the runner can link for a command. A package that requires anything
/// else is refused by the registry instead of failing at instantiation.
pub const RUNNER_CAPABILITIES: &[&str] = &[
    "soil:records",
    "soil:time",
    "soil:unibus",
    "soil:mcp",
    "soil:http",
    "soil:vfs",
    "soil:vfs-mut",
    "soil:index",
];

#[derive(Debug, Clone)]
pub struct CommandRegistry {
    roots: Vec<PathBuf>,
    commands: BTreeMap<String, CommandEntry>,
    refused: Vec<Refusal>,
}

impl CommandRegistry {
    /// An empty registry for hosts that start before any command is installed.
    pub fn empty(root: impl AsRef<Path>) -> Self {
        Self::over(vec![root.as_ref().to_path_buf()])
    }

    /// Scan one root.
    pub fn scan(root: impl AsRef<Path>) -> Result<Self> {
        Self::scan_roots(&[root.as_ref().to_path_buf()])
    }

    /// Scan roots in order, most specific first.
    ///
    /// The first package to claim a name wins and every later claim is refused,
    /// so a standalone command shadows a Spore's command of the same name
    /// rather than colliding with it. A root that does not exist has no
    /// commands; a root that exists and cannot be read is an error, because that
    /// is a broken machine rather than a missing package.
    ///
    /// A host reads both roots: what an operator installed by hand, and what a
    /// store installed. Nothing has to be copied between them - the install tree
    /// is the package, and its manifest already says which commands it carries.
    pub fn scan_roots(roots: &[PathBuf]) -> Result<Self> {
        let mut registry = Self::over(roots.to_vec());
        for root in roots {
            let directory = match std::fs::read_dir(root) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(anyhow::Error::new(error)
                        .context(format!("read command root {}", root.display())));
                }
            };
            for entry in directory {
                let entry =
                    entry.with_context(|| format!("read command root {}", root.display()))?;
                // `.install`, `.staging` and friends are a store's bookkeeping,
                // not packages.
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let package = entry.path();
                // A package may be reached through a symlink; `file_type` would
                // report the link itself and skip it.
                if !package.is_dir() {
                    continue;
                }
                registry.collect(&package);
            }
        }
        Ok(registry)
    }

    fn over(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            commands: BTreeMap::new(),
            refused: Vec::new(),
        }
    }

    /// Add every command one package directory declares, or record why it
    /// cannot. A package is a unit: one broken command refuses all of them.
    fn collect(&mut self, package: &Path) {
        let entries = if package.join("command.toml").is_file() {
            standalone_command(package).map(|entry| vec![entry])
        } else if package.join("spore.toml").is_file() {
            // A Spore installs commands too. The package owns both faces - a
            // canvas and a terminal program - and the shell should not care
            // which kind of manifest declared them.
            spore_commands(package)
        } else {
            return;
        };
        match entries {
            Ok(entries) => {
                for entry in entries {
                    self.offer(entry);
                }
            }
            Err(error) => self.refused.push(Refusal {
                name: package_name(package),
                reason: format!("{error:#}"),
            }),
        }
    }

    fn offer(&mut self, entry: CommandEntry) {
        if let Some(missing) = unsupported(&entry.requires) {
            self.refused.push(Refusal {
                name: entry.name.clone(),
                reason: format!(
                    "needs {}, which this runner does not link (it links {})",
                    missing.join(", "),
                    RUNNER_CAPABILITIES.join(", ")
                ),
            });
            return;
        }
        if let Some(existing) = self.commands.get(&entry.name) {
            self.refused.push(Refusal {
                name: entry.name.clone(),
                reason: format!(
                    "{} is already claimed by {}",
                    entry.package.display(),
                    existing.package.display()
                ),
            });
            return;
        }
        self.commands.insert(entry.name.clone(), entry);
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn entries(&self) -> impl Iterator<Item = &CommandEntry> {
        self.commands.values()
    }

    pub fn path(&self) -> Vec<&str> {
        self.commands.keys().map(String::as_str).collect()
    }

    pub fn get(&self, name: &str) -> Option<&CommandEntry> {
        self.commands.get(name)
    }

    /// Names that lost a collision, and packages that could not be read.
    pub fn refused(&self) -> &[Refusal] {
        &self.refused
    }
}

/// A shim is ours if it says so. A hand-written one carries policy this tool
/// has no business replacing.
pub const SHIM_MARKER: &str = "generated by soil-command";

/// What [`write_shims`] did, so a caller can say it out loud.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShimReport {
    pub written: Vec<PathBuf>,
    /// Names left alone, and why. A hand-written shim is not a mistake.
    pub kept: Vec<(String, String)>,
    pub pruned: Vec<PathBuf>,
}

/// Write one executable per command, so a shell that is not Mush can run it.
///
/// A shim is three lines and carries no policy: the runner and the roots are
/// baked in so it works from any directory, and argv, stdin and the exit code
/// are the caller's. Anything that does not carry [`SHIM_MARKER`] is left
/// untouched - `mb` is a hand-written shim that owns a records binding and an
/// actor, and replacing it would be a data-loss bug, not an update.
///
/// `prune` removes shims this tool wrote for commands that no longer exist. It
/// never removes a file it cannot claim.
pub fn write_shims(
    registry: &CommandRegistry,
    dir: &Path,
    runner: &Path,
    subcommand: &[&str],
    prune: bool,
) -> Result<ShimReport> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create shim directory {}", dir.display()))?;
    let mut report = ShimReport::default();
    for entry in registry.entries() {
        let target = dir.join(&entry.name);
        if target.exists() {
            match std::fs::read_to_string(&target) {
                Ok(existing) if existing.contains(SHIM_MARKER) => {}
                Ok(_) => {
                    report.kept.push((
                        entry.name.clone(),
                        "a hand-written shim is in the way".into(),
                    ));
                    continue;
                }
                Err(error) => {
                    report
                        .kept
                        .push((entry.name.clone(), format!("unreadable: {error}")));
                    continue;
                }
            }
        }
        std::fs::write(
            &target,
            shim_script(entry, registry.roots(), runner, subcommand),
        )
        .with_context(|| format!("write {}", target.display()))?;
        make_executable(&target)?;
        report.written.push(target);
    }
    if prune {
        let known: std::collections::BTreeSet<&str> = registry.path().into_iter().collect();
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("read shim directory {}", dir.display()))?
        {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if known.contains(name.as_str()) {
                continue;
            }
            let owned =
                std::fs::read_to_string(entry.path()).is_ok_and(|text| text.contains(SHIM_MARKER));
            if owned && std::fs::remove_file(entry.path()).is_ok() {
                report.pruned.push(entry.path());
            }
        }
    }
    Ok(report)
}

fn shim_script(
    entry: &CommandEntry,
    roots: &[PathBuf],
    runner: &Path,
    subcommand: &[&str],
) -> String {
    let mut script = format!(
        "#!/bin/sh\n\
         # {SHIM_MARKER} for the {} command. Do not edit; regenerate it.\n\
         # Source: {}\n\
         #\n\
         # A command gets no ambient policy. What it may reach is this file, which a\n\
         # person writes; without a records binding a command that imports\n\
         # soil:records fails to start, and says so.\n\
         policy=\"${{XDG_CONFIG_HOME:-$HOME/.config}}/fungos/commands/{}.env\"\n\
         if [ -r \"$policy\" ]; then . \"$policy\"; fi\n",
        entry.name,
        entry.package.display(),
        entry.name
    );
    // A command gets no ambient environment either: a policy file derives the
    // variables it may see and names them in FUNGOS_COMMAND_ENV. Names are
    // checked so the expansion below cannot smuggle shell syntax into the
    // runner's argv; values travel by name, so quoting inside them never
    // matters.
    script.push_str(
        "env_flags=\"\"\n\
         for name in ${FUNGOS_COMMAND_ENV:-}; do\n\
         \x20   case $name in ''|*[!A-Za-z0-9_]*) continue ;; esac\n\
         \x20   env_flags=\"$env_flags --env-from $name\"\n\
         done\n",
    );
    // The runner may be a subcommand of a larger binary (`soil command`), so
    // the shim names it explicitly instead of assuming the argv shape.
    let mut subcommand_flag = String::new();
    for arg in subcommand {
        subcommand_flag.push(' ');
        subcommand_flag.push_str(&shell_quote(arg));
    }
    let mut roots_flag = String::new();
    for root in roots {
        roots_flag.push_str(" --root ");
        roots_flag.push_str(&shell_quote(&root.to_string_lossy()));
    }
    let runner = shell_quote(&runner.to_string_lossy());
    let name = shell_quote(&entry.name);
    script.push_str(&format!(
        "book=\"${{FUNGOS_RECORDS_BOOK:-${{RECORDS_BOOK:-}}}}\"\n\
         if [ -n \"$book\" ]; then\n\
         \x20   exec {runner}{subcommand_flag}{roots_flag} \\\n\
         \x20       --records-root \"${{FUNGOS_RECORDS_ROOT:-${{RECORDS_ROOT:-$HOME/.local/share/fungos/records}}}}\" \\\n\
         \x20       --book \"$book\" \\\n\
         \x20       --collection \"${{FUNGOS_RECORDS_COLLECTION:-${{RECORDS_COLLECTION:-records}}}}\" \\\n\
         \x20       $env_flags run {name} \"$@\"\n\
         fi\n\
         exec {runner}{subcommand_flag}{roots_flag} $env_flags run {name} \"$@\"\n"
    ));
    script
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// A `soil:*` capability the runner cannot link, if the package declares one.
///
/// `wasi:*` is always satisfied: the runner is a WASI p2 host. A `soil:*` grant
/// is only satisfied if this runner links it, and the caller is told which one
/// rather than handed a linker error at the first run.
fn unsupported(requires: &[String]) -> Option<Vec<String>> {
    let missing: Vec<String> = requires
        .iter()
        .filter(|grant| {
            grant
                .strip_prefix("soil:")
                .is_some_and(|_| !RUNNER_CAPABILITIES.contains(&grant.as_str()))
        })
        .cloned()
        .collect();
    (!missing.is_empty()).then_some(missing)
}

/// Compile every registered command into the engine's on-disk cache.
///
/// A component is in wasmtime's cache only after it has been compiled, so the
/// honest way to warm one is to compile it. The marker beside the package says
/// which module was warmed and when, so a later reader can tell warm from
/// merely installed without compiling anything.
///
/// This warms the cache shared by `soil-command` and every host that builds its
/// engine the same way. A host that keeps its own cache - the launcher writes
/// `.cwasm` files of its own - warms what it will run, on its own schedule.
pub fn warm(roots: &[PathBuf]) -> Result<WarmReport> {
    let registry = CommandRegistry::scan_roots(roots)?;
    let engine = Engine::new(&caching_config())?;
    let mut report = WarmReport::default();
    for entry in registry.entries() {
        let module = entry.runtime_module.as_ref().unwrap_or(&entry.module);
        let digest = match std::fs::read(module) {
            Ok(bytes) => hex(&Sha256::digest(&bytes)),
            Err(error) => {
                report
                    .skipped
                    .push((entry.name.clone(), format!("read module: {error}")));
                continue;
            }
        };
        let marker = warm_marker(&entry.package, &entry.name);
        if marker_digest(&marker).as_deref() == Some(digest.as_str())
            && marker_key(&marker).as_deref() == entry.composition_key.as_deref()
        {
            report.already.push(entry.name.clone());
            continue;
        }
        if let Err(error) = wasmtime::component::Component::from_file(&engine, module) {
            report
                .skipped
                .push((entry.name.clone(), format!("compile: {error}")));
            continue;
        }
        let body = serde_json::json!({
            "name": entry.name,
            "module": module.display().to_string(),
            "sha256": digest,
            "composition_key": entry.composition_key,
            "warmed_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default(),
        });
        if let Some(dir) = marker.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create warm directory {}", dir.display()))?;
        }
        std::fs::write(&marker, serde_json::to_vec_pretty(&body)?)
            .with_context(|| format!("write {}", marker.display()))?;
        report.warmed.push(entry.name.clone());
    }
    Ok(report)
}

/// What a warm pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WarmReport {
    pub warmed: Vec<String>,
    pub already: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

/// Where a command's warm marker lives: beside the package, in the bookkeeping
/// directory the scans skip.
#[must_use]
pub fn warm_marker(package: &Path, name: &str) -> PathBuf {
    package
        .parent()
        .unwrap_or(package)
        .join(".warm")
        .join(format!("{name}.json"))
}

fn marker_digest(marker: &Path) -> Option<String> {
    let bytes = std::fs::read(marker).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("sha256")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn marker_key(marker: &Path) -> Option<String> {
    let bytes = std::fs::read(marker).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("composition_key")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn caching_config() -> Config {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_stack_size(16 * 1024 * 1024);
    config.max_wasm_stack(16 * 1024 * 1024);
    // Keep compiled components on disk. `mb` is invoked hundreds of times a day
    // for the same 600 KB module, and without this every one of those invocations
    // pays to compile it again. Failing to open the cache is not fatal: wasmtime
    // falls back to compiling, which is the status quo ante.
    match wasmtime::Cache::new(wasmtime::CacheConfig::default()) {
        Ok(cache) => {
            config.cache(Some(cache));
        }
        Err(error) => eprintln!("soil-command: component cache unavailable: {error}"),
    }
    config
}

fn package_name(package: &Path) -> String {
    package.file_name().map_or_else(
        || package.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn installed_runtime(package: &Path) -> (Option<PathBuf>, Option<String>) {
    let runtime = package.join("runtime.wasm");
    if !runtime.is_file() {
        return (None, None);
    }
    let key = std::fs::read_to_string(package.join("runtime.key"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    (Some(runtime), key)
}

/// Read every command declared by one package directory.
///
/// A standalone command package is one command. A Spore package is every
/// `[[commands]]` entry it declares. The caller supplies the package root; this
/// does not scan a parent directory.
pub fn package_commands(package: &Path) -> Result<Vec<CommandEntry>> {
    if package.join("command.toml").is_file() {
        standalone_command(package).map(|entry| vec![entry])
    } else if package.join("spore.toml").is_file() {
        spore_commands(package)
    } else {
        bail!(
            "{} is neither a command package nor a Spore package",
            package.display()
        )
    }
}

/// Compile one command module and write its warm marker for `package`.
///
/// The installer uses this before swapping a new generation into place, so a
/// compile failure cannot disturb the active install. The marker is written
/// beside the final package path, not the staging path.
pub fn warm_entry(entry: &CommandEntry, package: &Path) -> Result<PathBuf> {
    let module = entry.runtime_module.as_ref().unwrap_or(&entry.module);
    let bytes =
        std::fs::read(module).with_context(|| format!("read module {}", module.display()))?;
    let digest = hex(&Sha256::digest(&bytes));
    let marker = warm_marker(package, &entry.name);
    if marker_digest(&marker).as_deref() == Some(digest.as_str())
        && marker_key(&marker).as_deref() == entry.composition_key.as_deref()
    {
        return Ok(marker);
    }
    let engine = Engine::new(&caching_config())?;
    wasmtime::component::Component::from_file(&engine, module)
        .map_err(|error| anyhow::anyhow!("compile module {}: {error}", module.display()))?;
    let body = serde_json::json!({
        "name": entry.name,
        "module": module.display().to_string(),
        "sha256": digest,
        "composition_key": entry.composition_key,
        "warmed_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default(),
    });
    if let Some(dir) = marker.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create warm directory {}", dir.display()))?;
    }
    std::fs::write(&marker, serde_json::to_vec_pretty(&body)?)
        .with_context(|| format!("write {}", marker.display()))?;
    Ok(marker)
}

/// One package that stands alone: `command.toml` beside its module.
fn standalone_command(package: &Path) -> Result<CommandEntry> {
    let manifest_path = package.join("command.toml");
    let source = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: Manifest =
        toml::from_str(&source).with_context(|| format!("parse {}", manifest_path.display()))?;
    if manifest.schema != 1 {
        bail!("unsupported command manifest schema {}", manifest.schema);
    }
    if !slug(&manifest.command.name) {
        bail!("invalid command name {:?}", manifest.command.name);
    }
    if manifest.command.summary.trim().is_empty() {
        bail!("command {} has no summary", manifest.command.name);
    }
    let module = package.join(&manifest.command.module);
    if !module.is_file() {
        bail!("command {} module missing", manifest.command.name);
    }
    let (runtime_module, composition_key) = installed_runtime(package);
    Ok(CommandEntry {
        name: manifest.command.name,
        version: manifest.command.version,
        summary: manifest.command.summary,
        package: package.to_path_buf(),
        // A standalone `command.toml` is the package's whole contract; the
        // runner either satisfies `requires` or refuses the command.
        requires: manifest.command.requires,
        module,
        runtime_module,
        composition_key,
        timeout_ms: manifest.limits.timeout_ms,
        max_stdout_bytes: manifest.limits.max_stdout_bytes,
        max_stderr_bytes: manifest.limits.max_stderr_bytes,
        max_memory_bytes: manifest.limits.max_memory_bytes,
    })
}

/// The commands an installed Spore declares.
///
/// Limits are the command defaults: a Spore declares what it runs, not how much
/// memory the host may give it.
fn spore_commands(package: &Path) -> Result<Vec<CommandEntry>> {
    if !package.join("spore.toml").is_file() {
        return Ok(Vec::new());
    }
    let manifest = crate::spore::Manifest::load(package)
        .map_err(|error| anyhow::anyhow!("{}: {error}", package.display()))?;
    let limits = Limits::default();
    let mut entries = Vec::new();
    for command in &manifest.commands {
        let module = package.join(&command.module);
        if !module.is_file() {
            bail!("spore command {} module missing", command.name);
        }
        let (runtime_module, composition_key) = installed_runtime(package);
        entries.push(CommandEntry {
            name: command.name.clone(),
            version: manifest.spore.version.clone(),
            summary: command.summary.clone(),
            package: package.to_path_buf(),
            requires: manifest.grants.keys().cloned().collect(),
            module,
            runtime_module,
            composition_key,
            timeout_ms: limits.timeout_ms,
            max_stdout_bytes: limits.max_stdout_bytes,
            max_stderr_bytes: limits.max_stderr_bytes,
            max_memory_bytes: limits.max_memory_bytes,
        });
    }
    Ok(entries)
}

#[derive(Debug, Clone)]
pub struct RecordsBinding {
    pub root: PathBuf,
    pub book: String,
    pub collection: String,
}

#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct CommandRunner {
    engine: Engine,
    /// Installed Spore exports this runner can link into a command.
    plugins: Option<Arc<crate::command::plugin::PluginRegistry>>,
    records: Option<RecordsBinding>,
    /// Workspace root for `soil:vfs` / `soil:vfs-mut` / `soil:index`.
    vfs_root: Option<PathBuf>,
    /// Origins a command may reach through `soil:http`. Empty means none.
    http_origins: Vec<String>,
    /// Host-owned credentials a command may name through `soil:http`.
    http_credentials: Option<Arc<dyn crate::http_host::CredentialProvider>>,
    /// The bus a command may publish on. Linked (possibly as an unavailable
    /// bus) for a command that declares `soil:unibus`, so a missing hub costs
    /// a publish, not the command run.
    bus: Option<Arc<dyn crate::Bus>>,
    components: Mutex<BTreeMap<PathBuf, Arc<wasmtime::component::Component>>>,
}

impl CommandRunner {
    pub fn new(records: Option<RecordsBinding>, bus: Option<Arc<dyn crate::Bus>>) -> Result<Self> {
        let mut config = caching_config();
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        Ok(Self {
            engine,
            plugins: None,
            records,
            vfs_root: None,
            http_origins: Vec::new(),
            http_credentials: None,
            bus,
            components: Mutex::new(BTreeMap::new()),
        })
    }

    /// Use a scanned plugin registry when resolving a command's imports.
    pub fn with_plugins(mut self, plugins: crate::command::plugin::PluginRegistry) -> Self {
        self.plugins = Some(Arc::new(plugins));
        self
    }

    /// Link `soil:vfs`, `soil:vfs-mut` and `soil:index` from this workspace.
    pub fn with_vfs_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.vfs_root = Some(root.into());
        self
    }

    /// Link `soil:http` for commands that declare it.
    pub fn with_http_origins(mut self, origins: impl IntoIterator<Item = String>) -> Self {
        self.http_origins = origins.into_iter().collect();
        self
    }

    /// Resolve guest credential handles through host-owned Geode entries.
    pub fn with_http_credentials(
        mut self,
        credentials: Option<Arc<dyn crate::http_host::CredentialProvider>>,
    ) -> Self {
        self.http_credentials = credentials;
        self
    }

    /// The engine this runner uses. A supervisor that needs per-instance
    /// cancellation can keep a clone and call [`wasmtime::Engine::increment_epoch`].
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn run(&self, entry: &CommandEntry, request: &RunRequest) -> Result<RunOutput> {
        self.run_with_limits(entry, request, RunLimits::from_entry(entry))
    }

    /// Run one command with explicit host limits.
    pub fn run_with_limits(
        &self,
        entry: &CommandEntry,
        request: &RunRequest,
        limits: RunLimits,
    ) -> Result<RunOutput> {
        let module = entry.runtime_module.as_ref().unwrap_or(&entry.module);
        let component = {
            let mut cache = self
                .components
                .lock()
                .map_err(|_| anyhow::anyhow!("command component cache poisoned"))?;
            if let Some(component) = cache.get(module) {
                component.clone()
            } else {
                let component = Arc::new(
                    wasmtime::component::Component::from_file(&self.engine, module).map_err(
                        |error| anyhow::anyhow!("load command {}: {error}", module.display()),
                    )?,
                );
                cache.insert(module.clone(), component.clone());
                component
            }
        };
        let mut linker = wasmtime::component::Linker::<Host>::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        crate::command::plugin_ipc::add_to_linker::<Host>(&mut linker)?;
        let records = match &self.records {
            Some(binding) => Some(
                RecordsBookHost::open(
                    &binding.root,
                    binding.book.clone(),
                    binding.collection.clone(),
                )
                .context("open records book for command")?,
            ),
            None => None,
        };
        if records.is_some() {
            crate::records_host::add_to_linker::<Host>(&mut linker)?;
        }
        let vfs = match &self.vfs_root {
            Some(root) => Some(VfsHost::open(root).context("open VFS for command")?),
            None => None,
        };
        if vfs.is_some() {
            crate::vfs_host::add_to_linker::<Host>(&mut linker)?;
            crate::vfs_host::add_mutation_to_linker::<Host>(&mut linker)?;
        }
        if entry.requires.iter().any(|cap| cap == "soil:time") {
            crate::time::add_to_linker::<Host>(&mut linker)?;
        }
        // The grant is the command's own declaration, not the caller's wish: a
        // command that lists `soil:unibus` in `requires` gets the interface,
        // and one that does not cannot reach the bus even if the host has one.
        let bus = if entry.requires.iter().any(|cap| cap == "soil:unibus") {
            // A declared capability is always linked. Without a configured hub
            // the host answers `unavailable`; a command that publishes after
            // its write still succeeds and can retry later.
            Some(
                self.bus
                    .clone()
                    .map(|bus| crate::unibus::BusHost::new(Some(bus)))
                    .unwrap_or_else(|| crate::unibus::BusHost::new(None)),
            )
        } else {
            None
        };
        if bus.is_some() {
            crate::unibus::add_to_linker::<Host>(&mut linker)?;
        }
        let http =
            if entry.requires.iter().any(|cap| cap == "soil:http") || !self.http_origins.is_empty()
            {
                let policy = crate::http_host::HttpPolicy::new(
                    self.http_origins.iter().cloned(),
                    16 * 1024 * 1024,
                    Duration::from_secs(120),
                )?;
                crate::http_host::api::add_to_linker::<Host, wasmtime::component::HasSelf<Host>>(
                    &mut linker,
                    |host| host,
                )?;
                let host = match &self.http_credentials {
                    Some(credentials) => crate::http_host::HttpHost::new(policy)
                        .with_credentials(credentials.clone()),
                    None => crate::http_host::HttpHost::new(policy),
                };
                Some(host)
            } else {
                None
            };

        let stdout = MemoryOutputPipe::new(limits.max_stdout_bytes);
        let stderr = MemoryOutputPipe::new(limits.max_stderr_bytes);
        let mut wasi = WasiCtxBuilder::new();
        let mut argv = vec![entry.name.clone()];
        argv.extend(request.args.iter().cloned());
        wasi.args(&argv);
        wasi.envs(&request.env);
        if let Some(cwd) = &request.cwd {
            wasi.env("PWD", cwd);
        }
        wasi.stdin(MemoryInputPipe::new(request.stdin.clone()));
        wasi.stdout(stdout.clone());
        wasi.stderr(stderr.clone());

        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .build();
        let ipc = self
            .plugins
            .as_ref()
            .and_then(|plugins| plugins.native_for(&self.engine, &component))
            .map(|(program, cwd)| {
                crate::command::plugin_ipc::Sidecar::spawn(&program, &[], cwd.as_deref()).map(
                    |sidecar| Box::new(sidecar) as Box<dyn crate::command::plugin_ipc::Backend>,
                )
            })
            .transpose()?;
        let mut store = Store::new(
            &self.engine,
            Host {
                wasi: wasi.build(),
                table: wasmtime::component::ResourceTable::new(),
                limits: store_limits,
                records,
                vfs,
                bus,
                http,
                ipc,
            },
        );
        store.limiter(|host| &mut host.limits);
        store.set_epoch_deadline(1);
        let engine = self.engine.clone();
        let timeout = limits.timeout_ms;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(timeout));
            engine.increment_epoch();
        });

        if let Some(plugins) = &self.plugins {
            plugins
                .link_allowed(
                    &self.engine,
                    &mut linker,
                    &mut store,
                    &component,
                    &entry.requires,
                )
                .context("link command plugins")?;
        }
        let command = Command::instantiate(&mut store, &component, &linker)?;
        // A guest that traps has usually already said why on stderr — a panic
        // message lands there before the abort. Returning the trap alone throws
        // that away and leaves a wasm backtrace with no cause, so carry whatever
        // the command managed to write.
        let result = match command.wasi_cli_run().call_run(&mut store) {
            Ok(result) => result,
            Err(error) => {
                let said = String::from_utf8_lossy(&stderr.contents())
                    .trim()
                    .to_string();
                if said.is_empty() {
                    return Err(error.into());
                }
                return Err(anyhow::anyhow!("{error}\n{said}"));
            }
        };
        Ok(RunOutput {
            code: if result.is_ok() { 0 } else { 1 },
            stdout: stdout.contents().to_vec(),
            stderr: stderr.contents().to_vec(),
        })
    }
}

/// Build the host-owned credential resolver described by `loam.conf.soil`.
///
/// The config names Geode references and purposes; the secrets stay in the
/// daemon. A build without the Geode feature has no resolver, which is the
/// honest answer rather than a silent bearer token.
pub fn http_credentials(
    credentials: &[crate::config::HttpCredential],
) -> Result<Option<Arc<dyn crate::http_host::CredentialProvider>>> {
    #[cfg(all(unix, feature = "creds", feature = "wasi"))]
    {
        if credentials.is_empty() {
            return Ok(None);
        }
        let socket = std::env::var_os("GEODE_SOCKET")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".geode/geode.sock"))
            })
            .unwrap_or_else(|| PathBuf::from("/run/geode/geode.sock"));
        let mut resolver = crate::creds::GeodeCredentials::new(
            socket,
            "",
            "soil:http",
            std::time::Duration::from_secs(60),
        );
        for credential in credentials {
            resolver = resolver.with_host_handle(
                credential.handle.clone(),
                crate::creds::HostHandle {
                    reference: credential.reference.clone(),
                    purpose: credential.purpose.clone(),
                    header: credential.header.clone(),
                },
            );
        }
        Ok(Some(Arc::new(resolver)))
    }
    #[cfg(not(all(unix, feature = "creds", feature = "wasi")))]
    {
        let _ = credentials;
        Ok(None)
    }
}

struct Host {
    wasi: WasiCtx,
    table: wasmtime::component::ResourceTable,
    limits: StoreLimits,
    records: Option<RecordsBookHost>,
    /// `soil:vfs`, `soil:vfs-mut` and `soil:index`, when the host named a root.
    vfs: Option<VfsHost>,
    /// `soil:unibus`, when the command declared it and the caller supplied a
    /// bus. The command publishes through this; the records host never learns
    /// the bus exists, which is what keeps the two capabilities independent.
    bus: Option<crate::unibus::BusHost>,
    /// `soil:http`, when the command declared it. The policy is the grant.
    http: Option<crate::http_host::HttpHost>,
    /// `loam:plugin/ipc` backend, when a plugin provider named a sidecar.
    ipc: Option<Box<dyn crate::command::plugin_ipc::Backend>>,
}

impl crate::command::plugin_ipc::ipc::Host for Host {
    fn request(
        &mut self,
        method: String,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, crate::command::plugin_ipc::ipc::Error> {
        crate::command::plugin_ipc::handle(&mut self.ipc, &method, &payload)
    }
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl crate::http_host::api::Host for Host {
    fn fetch(
        &mut self,
        request: crate::http_host::api::Request,
    ) -> std::result::Result<crate::http_host::api::Response, crate::http_host::api::Error> {
        let http = self
            .http
            .as_mut()
            .ok_or(crate::http_host::api::Error::Unsupported)?;
        crate::http_host::api::Host::fetch(http, request)
    }

    fn open(
        &mut self,
        request: crate::http_host::api::Request,
    ) -> std::result::Result<
        wasmtime::component::Resource<crate::http_host::api::ResponseStream>,
        crate::http_host::api::Error,
    > {
        let http = self
            .http
            .as_mut()
            .ok_or(crate::http_host::api::Error::Unsupported)?;
        crate::http_host::api::Host::open(http, request)
    }
}

impl crate::http_host::api::HostResponseStream for Host {
    fn status(
        &mut self,
        stream: wasmtime::component::Resource<crate::http_host::api::ResponseStream>,
    ) -> u16 {
        self.http
            .as_mut()
            .map(|http| crate::http_host::api::HostResponseStream::status(http, stream))
            .unwrap_or_default()
    }

    fn final_url(
        &mut self,
        stream: wasmtime::component::Resource<crate::http_host::api::ResponseStream>,
    ) -> String {
        self.http
            .as_mut()
            .map(|http| crate::http_host::api::HostResponseStream::final_url(http, stream))
            .unwrap_or_default()
    }

    fn headers(
        &mut self,
        stream: wasmtime::component::Resource<crate::http_host::api::ResponseStream>,
    ) -> Vec<crate::http_host::api::Header> {
        self.http
            .as_mut()
            .map(|http| crate::http_host::api::HostResponseStream::headers(http, stream))
            .unwrap_or_default()
    }

    fn read(
        &mut self,
        stream: wasmtime::component::Resource<crate::http_host::api::ResponseStream>,
        max: u32,
    ) -> std::result::Result<Option<Vec<u8>>, crate::http_host::api::Error> {
        let http = self
            .http
            .as_mut()
            .ok_or(crate::http_host::api::Error::Unsupported)?;
        crate::http_host::api::HostResponseStream::read(http, stream, max)
    }

    fn close(
        &mut self,
        stream: wasmtime::component::Resource<crate::http_host::api::ResponseStream>,
    ) -> std::result::Result<(), crate::http_host::api::Error> {
        let http = self
            .http
            .as_mut()
            .ok_or(crate::http_host::api::Error::Unsupported)?;
        crate::http_host::api::HostResponseStream::close(http, stream)
    }

    fn drop(
        &mut self,
        stream: wasmtime::component::Resource<crate::http_host::api::ResponseStream>,
    ) -> std::result::Result<(), wasmtime::Error> {
        if let Some(http) = self.http.as_mut() {
            return crate::http_host::api::HostResponseStream::drop(http, stream);
        }
        Ok(())
    }
}

impl crate::unibus::bus::Host for Host {
    fn publish(
        &mut self,
        kind: String,
        body: Vec<u8>,
        destination: Option<String>,
        ttl_ms: u64,
    ) -> Result<crate::unibus::bus::Receipt, crate::unibus::bus::Error> {
        let bus = self
            .bus
            .as_mut()
            .ok_or(crate::unibus::bus::Error::Unavailable)?;
        bus.publish(kind, body, destination, ttl_ms)
    }

    fn poll(
        &mut self,
        kinds: Vec<String>,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<crate::unibus::bus::Page, crate::unibus::bus::Error> {
        let bus = self
            .bus
            .as_mut()
            .ok_or(crate::unibus::bus::Error::Unavailable)?;
        bus.poll(kinds, cursor, limit)
    }
}

impl vfs::Host for Host {
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

impl cid::Host for Host {
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

impl index::Host for Host {
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

impl vfs_mut::Host for Host {
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

impl vfs_mut::HostWriter for Host {
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

    fn abort(&mut self, writer: Resource<VfsWriter>) -> std::result::Result<(), vfs_mut::Error> {
        self.table
            .get_mut(&writer)
            .map_err(|_| vfs_mut::Error::Invalid)?
            .abort()
    }

    fn drop(&mut self, writer: Resource<VfsWriter>) -> std::result::Result<(), wasmtime::Error> {
        let _ = self.table.delete(writer);
        Ok(())
    }
}

impl crate::time::api::Host for Host {
    fn monotonic_ms(&mut self) -> Result<u64, crate::time::api::Error> {
        crate::time::SystemTimeHost.monotonic_ms()
    }

    fn wall_clock(&mut self) -> Result<crate::time::api::DateTime, crate::time::api::Error> {
        crate::time::SystemTimeHost.wall_clock()
    }

    fn tick(&mut self, timeout_ms: u32) -> Result<Option<u64>, crate::time::api::Error> {
        crate::time::SystemTimeHost.tick(timeout_ms)
    }
}

impl crate::records_host::records::Host for Host {
    fn get(
        &mut self,
        key: crate::records_host::records::Key,
    ) -> Result<crate::records_host::records::Entry, crate::records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .get(key)
    }

    fn scan(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<crate::records_host::records::Page, crate::records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .scan(collection, cursor, limit)
    }

    fn query(
        &mut self,
        spec: crate::records_host::records::QuerySpec,
    ) -> Result<crate::records_host::records::Page, crate::records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .query(spec)
    }

    fn aggregate(
        &mut self,
        collection: String,
        spec: crate::records_host::records::AggregateSpec,
    ) -> Result<
        Vec<crate::records_host::records::AggregateGroup>,
        crate::records_host::records::Error,
    > {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .aggregate(collection, spec)
    }

    fn scan_packed(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<crate::records_host::records::PackedPage, crate::records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .scan_packed(collection, cursor, limit)
    }

    fn upsert(
        &mut self,
        entry: crate::records_host::records::Entry,
    ) -> Result<crate::records_host::records::Entry, crate::records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .upsert(entry)
    }

    fn compare_and_swap(
        &mut self,
        key: crate::records_host::records::Key,
        expected: Option<crate::records_host::records::Version>,
        entry: crate::records_host::records::Entry,
    ) -> Result<crate::records_host::records::CasResult, crate::records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .compare_and_swap(key, expected, entry)
    }

    fn delete(
        &mut self,
        key: crate::records_host::records::Key,
    ) -> Result<(), crate::records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .delete(key)
    }

    fn changes(
        &mut self,
        since: Option<String>,
        limit: u32,
    ) -> Result<crate::records_host::records::Page, crate::records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .changes(since, limit)
    }

    fn apply(
        &mut self,
        entries: Vec<crate::records_host::records::Entry>,
    ) -> Result<crate::records_host::records::ApplyResult, crate::records_host::records::Error>
    {
        self.records
            .as_mut()
            .ok_or(crate::records_host::records::Error::Denied)?
            .apply(entries)
    }
}

/// The Spores root that sits beside a command root.
///
/// Commands and Spores are siblings under the fungOS data directory, so one
/// setting moves both. `MUSH_SPORE_ROOT` overrides the sibling when a checkout
/// keeps the two apart.
pub fn spore_root(command_root: impl AsRef<Path>) -> PathBuf {
    let command_root = command_root.as_ref();
    std::env::var_os("MUSH_SPORE_ROOT")
        .map(PathBuf::from)
        .or_else(|| command_root.parent().map(|parent| parent.join("spores")))
        .unwrap_or_else(|| PathBuf::from("spores"))
}

/// Every root a shell reads, most specific first: the command packages an
/// operator installed, then the Spores a store installed.
///
/// The order is the whole rule for a name claimed twice, and it is why a Spore
/// needs nothing copied at install time: the registry reads its tree where the
/// install left it.
pub fn roots(command_root: impl AsRef<Path>) -> Vec<PathBuf> {
    let command_root = command_root.as_ref().to_path_buf();
    let spores = spore_root(&command_root);
    vec![command_root, spores]
}

fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standalone(root: &Path, name: &str) {
        let package = root.join(name);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("module.wasm"), b"\0asm").unwrap();
        std::fs::write(
            package.join("command.toml"),
            format!(
                "schema = 1\n[command]\nname = \"{name}\"\nversion = \"0.1.0\"\nsummary = \"test\"\nmodule = \"module.wasm\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn scans_a_standalone_command_root() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            standalone(dir.path(), name);
        }
        let registry = CommandRegistry::scan(dir.path()).unwrap();
        assert_eq!(registry.path(), vec!["a", "b"]);
        assert!(registry.get("a").is_some());
        assert!(registry.refused().is_empty());
    }

    #[test]
    fn a_composed_runtime_module_is_selected() {
        let dir = tempfile::tempdir().unwrap();
        standalone(dir.path(), "a");
        let package = dir.path().join("a");
        std::fs::write(package.join("runtime.wasm"), b"\0asm").unwrap();
        std::fs::write(package.join("runtime.key"), b"abc123\n").unwrap();
        let registry = CommandRegistry::scan(dir.path()).unwrap();
        let entry = registry.get("a").unwrap();
        assert_eq!(
            entry.runtime_module.as_deref(),
            Some(package.join("runtime.wasm").as_path())
        );
        assert_eq!(entry.composition_key.as_deref(), Some("abc123"));
    }

    #[test]
    fn a_standalone_command_declares_its_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("mb");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("module.wasm"), b"\0asm").unwrap();
        std::fs::write(
            package.join("command.toml"),
            "schema = 1\n[command]\nname = \"mb\"\nversion = \"0.1.0\"\nsummary = \"test\"\nmodule = \"module.wasm\"\nrequires = [\"soil:records\", \"soil:unibus\"]\n",
        )
        .unwrap();
        let registry = CommandRegistry::scan(dir.path()).unwrap();
        let entry = registry.get("mb").unwrap();
        assert_eq!(
            entry.requires,
            vec!["soil:records".to_string(), "soil:unibus".to_string()]
        );
    }

    #[test]
    fn a_name_claimed_twice_is_refused_not_fatal() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        standalone(first.path(), "mb");
        standalone(second.path(), "mb");
        standalone(second.path(), "other");

        let registry =
            CommandRegistry::scan_roots(&[first.path().to_path_buf(), second.path().to_path_buf()])
                .unwrap();

        // The first root wins, so a hand-placed command shadows an installed
        // Spore's command of the same name.
        assert_eq!(registry.path(), vec!["mb", "other"]);
        assert_eq!(registry.get("mb").unwrap().package, first.path().join("mb"));
        assert_eq!(registry.refused().len(), 1);
        assert_eq!(registry.refused()[0].name, "mb");
        assert!(
            registry.refused()[0].reason.contains("already claimed"),
            "{}",
            registry.refused()[0].reason
        );
    }

    #[test]
    fn a_root_that_does_not_exist_has_no_commands() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-installed-yet");
        let registry = CommandRegistry::scan_roots(&[missing]).unwrap();
        assert!(registry.path().is_empty());
        assert!(registry.refused().is_empty());
    }

    #[test]
    fn bookkeeping_directories_are_not_packages() {
        let dir = tempfile::tempdir().unwrap();
        standalone(dir.path(), "mb");
        let staging = dir.path().join(".staging/mb-1234");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("module.wasm"), b"\0asm").unwrap();
        std::fs::write(
            staging.join("command.toml"),
            "schema = 1\n[command]\nname = \"mb\"\nversion = \"9.9.9\"\nsummary = \"half-written\"\nmodule = \"module.wasm\"\n",
        )
        .unwrap();

        let registry = CommandRegistry::scan(dir.path()).unwrap();
        assert_eq!(registry.path(), vec!["mb"]);
        assert_eq!(registry.get("mb").unwrap().version, "0.1.0");
        assert!(registry.refused().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_package_reached_through_a_symlink_is_followed() {
        let store = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        standalone(store.path(), "mb");
        std::os::unix::fs::symlink(store.path().join("mb"), root.path().join("mb")).unwrap();

        let registry = CommandRegistry::scan(root.path()).unwrap();
        assert_eq!(registry.path(), vec!["mb"]);
    }

    #[test]
    fn a_shim_names_the_runner_the_roots_and_the_command() {
        let commands = tempfile::tempdir().unwrap();
        standalone(commands.path(), "sfx");
        let prefix = tempfile::tempdir().unwrap();
        let dir = prefix.path().join("bin");
        let roots = vec![commands.path().to_path_buf()];
        let registry = CommandRegistry::scan_roots(&roots).unwrap();

        let report = write_shims(
            &registry,
            &dir,
            Path::new("/opt/soil/soil-command"),
            &[],
            false,
        )
        .unwrap();

        assert_eq!(report.written, vec![dir.join("sfx")]);
        let shim = std::fs::read_to_string(dir.join("sfx")).unwrap();
        assert!(shim.starts_with("#!/bin/sh\n"), "{shim}");
        assert!(shim.contains(SHIM_MARKER));
        // The roots are baked in, so the shim does not depend on the caller's
        // environment having FUNGOS_COMMAND_ROOT set.
        assert!(
            shim.contains(&format!("--root '{}'", commands.path().display())),
            "{shim}"
        );
        assert!(shim.contains("exec '/opt/soil/soil-command' "), "{shim}");
        // Policy is sourced, never baked in: a records binding is the host's to
        // name, and the shim only knows where to look for it.
        assert!(shim.contains("fungos/commands/sfx.env"), "{shim}");
        // The policy file derives the variables a command may see; the shim
        // forwards exactly those, by name, and checks each name first.
        assert!(shim.contains("FUNGOS_COMMAND_ENV"), "{shim}");
        assert!(shim.contains("--env-from $name"), "{shim}");
        assert!(shim.contains("*[!A-Za-z0-9_]*"), "{shim}");
        assert!(shim.contains("--records-root"), "{shim}");
        assert!(shim.ends_with("run 'sfx' \"$@\"\n"), "{shim}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("sfx"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "a shim has to be executable");
        }
    }

    #[test]
    fn a_shim_names_the_subcommand_that_brought_it() {
        let commands = tempfile::tempdir().unwrap();
        standalone(commands.path(), "sfx");
        let prefix = tempfile::tempdir().unwrap();
        let dir = prefix.path().join("bin");
        let roots = vec![commands.path().to_path_buf()];
        let registry = CommandRegistry::scan_roots(&roots).unwrap();

        write_shims(
            &registry,
            &dir,
            Path::new("/opt/soil/soil"),
            &["command"],
            false,
        )
        .unwrap();

        let shim = std::fs::read_to_string(dir.join("sfx")).unwrap();
        // A shim written by `soil command shims` has to come back through
        // `soil command`, not through a bare `soil` whose next word is a flag.
        assert!(shim.contains("exec '/opt/soil/soil' 'command' "), "{shim}");
        assert!(shim.ends_with("run 'sfx' \"$@\"\n"), "{shim}");
    }

    #[test]
    fn shims_leave_a_hand_written_script_alone_and_prune_only_their_own() {
        let commands = tempfile::tempdir().unwrap();
        standalone(commands.path(), "sfx");
        let prefix = tempfile::tempdir().unwrap();
        let dir = prefix.path().join("bin");
        let runner = Path::new("/opt/soil/soil-command");

        let registry = CommandRegistry::scan(commands.path()).unwrap();
        write_shims(&registry, &dir, runner, &[], false).unwrap();

        // A person put their own `mb` there, with a records binding and an actor.
        let policy = "#!/bin/sh\necho mine\n";
        std::fs::write(dir.join("mb"), policy).unwrap();
        standalone(commands.path(), "mb");
        let registry = CommandRegistry::scan(commands.path()).unwrap();
        let report = write_shims(&registry, &dir, runner, &[], false).unwrap();
        assert_eq!(report.kept.len(), 1, "{report:?}");
        assert_eq!(report.kept[0].0, "mb");
        assert_eq!(std::fs::read_to_string(dir.join("mb")).unwrap(), policy);

        // `sfx` goes away; pruning takes its shim and leaves the hand-written one.
        std::fs::remove_dir_all(commands.path().join("sfx")).unwrap();
        let registry = CommandRegistry::scan(commands.path()).unwrap();
        let report = write_shims(&registry, &dir, runner, &[], true).unwrap();
        assert_eq!(report.pruned, vec![dir.join("sfx")]);
        assert!(!dir.join("sfx").exists());
        assert_eq!(std::fs::read_to_string(dir.join("mb")).unwrap(), policy);
    }

    #[test]
    fn a_path_with_a_quote_survives_the_shim() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}

#[cfg(test)]
mod spore_command_tests {
    use super::*;

    fn scratch() -> PathBuf {
        // Parallel tests share a process, and a clock alone is not enough to keep
        // two scratch roots apart.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "soil-command-scan-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    const SPORE: &str = r#"schema = 1

[spore]
id = "dev.fungos.unibus"
name = "unibus"
version = "0.2.0"
license = "MIT"
summary = "inbox"

[engine]
soil = ">=0.1"
target = "wasm32-wasip2"

[ui]
entry = "ui/inbox.lua"

[launch]
default = "canvas"
surfaces = ["canvas"]

[[commands]]
name = "unibus"
summary = "read the inbox"
module = "bin/unibus.wasm"
"#;

    #[test]
    fn a_spore_installs_its_commands_into_the_registry() {
        let root = scratch();
        let spore = root.join("unibus");
        std::fs::create_dir_all(spore.join("bin")).expect("scratch");
        std::fs::write(spore.join("spore.toml"), SPORE).expect("manifest");
        std::fs::write(spore.join("bin/unibus.wasm"), b"\0asm").expect("module");

        let registry = CommandRegistry::scan(&root).expect("scan");
        let entry = registry.get("unibus").expect("the Spore's command");
        assert_eq!(
            entry.version, "0.2.0",
            "the package's version, not the command's"
        );
        assert_eq!(entry.summary, "read the inbox");
        assert!(entry.module.ends_with("bin/unibus.wasm"));
        assert_eq!(registry.path(), vec!["unibus"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_command_whose_module_is_missing_is_refused() {
        let root = scratch();
        let spore = root.join("unibus");
        std::fs::create_dir_all(&spore).expect("scratch");
        std::fs::write(spore.join("spore.toml"), SPORE).expect("manifest");
        // No bin/unibus.wasm: a registry that listed it would promise a program
        // that cannot run. It is refused by name, and the registry still answers.
        let registry = CommandRegistry::scan(&root).expect("a broken package is not fatal");
        assert!(registry.path().is_empty());
        assert_eq!(registry.refused().len(), 1);
        assert_eq!(registry.refused()[0].name, "unibus");
        assert!(
            registry.refused()[0].reason.contains("module missing"),
            "{}",
            registry.refused()[0].reason
        );
        let _ = std::fs::remove_dir_all(root);
    }

    const SPORE_WITH_AUDIO: &str = r#"schema = 1

[spore]
id = "dev.fungos.sfx"
name = "sfx"
version = "0.1.0"
license = "MIT"
summary = "soundboard"

[engine]
soil = ">=0.1"
target = "wasm32-wasip2"

[ui]
entry = "ui/board.lua"

[launch]
default = "canvas"
surfaces = ["canvas"]

[grants]
"wasi:cli" = "std diagnostics"
"soil:audio" = "write PCM to the room"

[[commands]]
name = "sfx"
summary = "play a pad"
module = "bin/sfx.wasm"
"#;

    #[test]
    fn a_command_that_needs_an_unlinked_capability_is_refused() {
        let root = scratch();
        let spore = root.join("sfx");
        std::fs::create_dir_all(spore.join("bin")).expect("scratch");
        std::fs::write(spore.join("spore.toml"), SPORE_WITH_AUDIO).expect("manifest");
        std::fs::write(spore.join("bin/sfx.wasm"), b"\0asm").expect("module");

        // `sfx` imports soil:audio, and this runner links only soil:records. A
        // registry that listed it would put a program on PATH that traps on
        // start, so it is refused here with the reason instead.
        let registry = CommandRegistry::scan(&root).expect("a refusal is not fatal");
        assert!(registry.path().is_empty());
        assert_eq!(registry.refused().len(), 1, "{:?}", registry.refused());
        assert_eq!(registry.refused()[0].name, "sfx");
        assert!(
            registry.refused()[0].reason.contains("soil:audio"),
            "{}",
            registry.refused()[0].reason
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_installed_spore_and_a_standalone_command_share_one_path() {
        let root = scratch();
        let commands = root.join("commands");
        let spores = root.join("spores");
        let spore = spores.join("unibus");
        std::fs::create_dir_all(spore.join("bin")).expect("scratch");
        std::fs::create_dir_all(commands.join("mb")).expect("scratch");
        std::fs::write(spore.join("spore.toml"), SPORE).expect("manifest");
        std::fs::write(spore.join("bin/unibus.wasm"), b"\0asm").expect("module");
        std::fs::write(commands.join("mb/module.wasm"), b"\0asm").expect("module");
        std::fs::write(
            commands.join("mb/command.toml"),
            "schema = 1\n[command]\nname = \"mb\"\nversion = \"0.1.0\"\nsummary = \"tasks\"\nmodule = \"module.wasm\"\n",
        )
        .expect("manifest");

        let registry = CommandRegistry::scan_roots(&[commands, spores]).expect("scan");
        assert_eq!(registry.path(), vec!["mb", "unibus"]);
        let unibus = registry.get("unibus").expect("the Spore's command");
        assert!(unibus.module.ends_with("spores/unibus/bin/unibus.wasm"));
        assert!(registry.refused().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
