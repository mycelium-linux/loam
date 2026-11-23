//! Local, atomic Spore/command installation.
//!
//! An install is a verified package copied into a generation directory, with
//! its resource providers composed into `runtime.wasm` and its module warmed
//! before the active package pointer changes. The source tree is never written.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasmtime::{Config, Engine};

use crate::{
    command::{
        self, CommandEntry, RUNNER_CAPABILITIES,
        plugin::{self, PluginRegistry},
    },
    spore::{self, Manifest},
};

const LOAM_DIR: &str = ".loam";
const STATE_FILE: &str = "state.json";
const CACHE_DIR: &str = "compositions";

/// Options for one local install.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub path: PathBuf,
    pub root: PathBuf,
    pub allow_unsigned_dev: bool,
    pub provider_roots: Vec<PathBuf>,
    pub composition_cache: Option<PathBuf>,
}

/// What an install changed.
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub id: String,
    pub version: String,
    pub package: String,
    pub package_digest: String,
    pub generation: String,
    pub previous: Option<String>,
    pub composition_key: Option<String>,
    pub warm: bool,
    pub dev: bool,
}

/// One managed install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallRecord {
    pub id: String,
    pub version: String,
    pub package: String,
    pub package_digest: String,
    /// Content key for the active generation (package + composition).
    pub current: String,
    pub previous: Option<String>,
    pub composition_key: Option<String>,
    pub warm: bool,
    pub dev: bool,
    pub installed_at: u64,
}

/// `loam status` output.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub root: PathBuf,
    pub installs: Vec<InstallRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    installs: BTreeMap<String, InstallRecord>,
}

/// Install a package directory into `root`.
pub fn install(options: InstallOptions) -> Result<InstallReport> {
    if !options.path.is_dir() {
        bail!("{} is not a package directory", options.path.display());
    }
    let source = options
        .path
        .canonicalize()
        .with_context(|| format!("resolve {}", options.path.display()))?;
    fs::create_dir_all(&options.root)
        .with_context(|| format!("create install root {}", options.root.display()))?;

    let is_spore = source.join("spore.toml").is_file();
    let is_command = source.join("command.toml").is_file();
    if !is_spore && !is_command {
        bail!(
            "{} has neither spore.toml nor command.toml",
            source.display()
        );
    }
    if is_spore {
        // Never refresh the lock here: a stale lock is a refusal, not a rewrite.
        spore::check(&source)?;
    }
    let entries = command::package_commands(&source)?;
    let manifest = if is_spore {
        Some(Manifest::load(&source)?)
    } else {
        None
    };
    let (id, version, package_digest, package_dir) =
        identity(&source, manifest.as_ref(), &entries)?;
    let dev = verify_source(&source, is_spore, options.allow_unsigned_dev)?;

    let txn = fresh_txn(&options.root);
    let staging_root = options.root.join(LOAM_DIR).join("staging").join(&txn);
    let staging_pkg = staging_root.join("pkg");
    fs::create_dir_all(&staging_pkg)
        .with_context(|| format!("create {}", staging_pkg.display()))?;

    let result = (|| -> Result<(String, Option<String>, bool)> {
        copy_package(&source, &staging_pkg, is_spore)?;
        let staged_entries = command::package_commands(&staging_pkg)?;
        let providers = if options.provider_roots.is_empty() {
            PluginRegistry::scan_default()?
        } else {
            PluginRegistry::scan_roots(&options.provider_roots)?
        };
        let cache = options
            .composition_cache
            .clone()
            .unwrap_or_else(|| options.root.join(LOAM_DIR).join(CACHE_DIR));
        let composition_key = compose(&staging_pkg, &staged_entries, &providers, &cache)?;

        let final_package = options.root.join(&package_dir);
        for entry in &staged_entries {
            command::warm_entry(entry, &final_package)
                .with_context(|| format!("warm command {}", entry.name))?;
        }
        let generation = generation_key(&package_digest, composition_key.as_deref());
        let destination = options
            .root
            .join(LOAM_DIR)
            .join("generations")
            .join(&package_dir)
            .join(&generation);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        if destination.exists() {
            fs::remove_dir_all(&staging_pkg)?;
        } else {
            fs::rename(&staging_pkg, &destination)
                .with_context(|| format!("move staged package to {}", destination.display()))?;
        }
        Ok((generation, composition_key, !staged_entries.is_empty()))
    })();

    let (generation, composition_key, warm) = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_root);
            return Err(error);
        }
    };
    let _ = fs::remove_dir_all(&staging_root);

    let mut state = load_state(&options.root)?;
    let previous = state.installs.get(&id).map(|record| record.current.clone());
    let record = InstallRecord {
        id: id.clone(),
        version,
        package: package_dir.clone(),
        package_digest,
        current: generation.clone(),
        previous,
        composition_key: composition_key.clone(),
        warm,
        dev,
        installed_at: now(),
    };
    activate(&options.root, &package_dir, &generation)?;
    state.installs.insert(id.clone(), record.clone());
    if let Err(error) = save_state(&options.root, &state) {
        return Err(error).with_context(|| "state write failed after activating generation");
    }

    Ok(InstallReport {
        id,
        version: record.version,
        package: package_dir,
        package_digest: record.package_digest,
        generation,
        previous: record.previous,
        composition_key,
        warm,
        dev,
    })
}

/// Read the installed state for one root.
pub fn status(root: &Path) -> Result<StatusReport> {
    let state = load_state(root)?;
    let installs = state.installs.into_values().collect();
    Ok(StatusReport {
        root: root.to_path_buf(),
        installs,
    })
}

/// Switch an install back to its previous generation.
pub fn rollback(root: &Path, id: &str) -> Result<InstallReport> {
    let mut state = load_state(root)?;
    let record = state
        .installs
        .get(id)
        .cloned()
        .with_context(|| format!("unknown install {id:?}"))?;
    let previous = record
        .previous
        .clone()
        .with_context(|| format!("install {id:?} has no previous generation"))?;
    let previous_package = generation_dir(root, &record.package, &previous);
    if !previous_package.is_dir() {
        bail!(
            "previous generation is missing: {}",
            previous_package.display()
        );
    }
    let entries = command::package_commands(&previous_package)?;
    let final_package = root.join(&record.package);
    for entry in &entries {
        command::warm_entry(entry, &final_package)
            .with_context(|| format!("warm rollback command {}", entry.name))?;
    }
    activate(root, &record.package, &previous)?;

    let rolled_back = InstallRecord {
        current: previous.clone(),
        previous: Some(record.current.clone()),
        warm: !entries.is_empty(),
        installed_at: now(),
        ..record
    };
    state.installs.insert(id.to_string(), rolled_back.clone());
    save_state(root, &state)?;

    Ok(InstallReport {
        id: id.to_string(),
        version: rolled_back.version,
        package: rolled_back.package,
        package_digest: rolled_back.package_digest,
        generation: previous,
        previous: rolled_back.previous,
        composition_key: rolled_back.composition_key,
        warm: rolled_back.warm,
        dev: rolled_back.dev,
    })
}

fn identity(
    source: &Path,
    manifest: Option<&Manifest>,
    entries: &[CommandEntry],
) -> Result<(String, String, String, String)> {
    let (id, version, package_digest, name) = if let Some(manifest) = manifest {
        let lock = spore::check(source)?;
        (
            manifest.spore.id.clone(),
            manifest.spore.version.clone(),
            lock.digest,
            manifest.spore.name.clone(),
        )
    } else {
        let entry = entries
            .first()
            .with_context(|| format!("{} declares no commands", source.display()))?;
        let digest = package_digest(source)?;
        (
            entry.name.clone(),
            entry.version.clone(),
            digest,
            source
                .file_name()
                .and_then(|name| name.to_str())
                .context("package directory has no UTF-8 name")?
                .to_string(),
        )
    };
    Ok((
        id,
        version,
        package_digest,
        safe_component(&name).context("package name is not a safe directory name")?,
    ))
}

fn package_digest(root: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    for (path, bytes) in spore::collect(root)? {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_source(source: &Path, is_spore: bool, allow_unsigned_dev: bool) -> Result<bool> {
    if allow_unsigned_dev {
        return Ok(true);
    }
    let signatures = signatures(source)?;
    if signatures.is_empty() {
        bail!("package is unsigned; pass --allow-unsigned-dev for a local development install");
    }
    if !is_spore {
        bail!(
            "command packages cannot be signed yet; use --allow-unsigned-dev for this local install"
        );
    }
    let archive = spore::pack_locked(source)?;
    let temp = tempfile::NamedTempFile::new()?;
    fs::write(temp.path(), archive)?;
    for signature in signatures {
        spore::verify_signature(temp.path(), &signature, None, None)
            .with_context(|| format!("verify {}", signature.display()))?;
    }
    Ok(false)
}

fn signatures(source: &Path) -> Result<Vec<PathBuf>> {
    let dir = source.join(spore::SIGNATURE_DIR);
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "asc") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn copy_package(source: &Path, destination: &Path, is_spore: bool) -> Result<()> {
    for (path, bytes) in spore::collect(source)? {
        if path == "runtime.wasm" || path == "runtime.key" || path.starts_with(".warm/") {
            continue;
        }
        let target = destination.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, bytes).with_context(|| format!("write {}", target.display()))?;
    }
    if is_spore {
        let lock = source.join(spore::LOCK_NAME);
        fs::copy(&lock, destination.join(spore::LOCK_NAME))
            .with_context(|| format!("copy {}", lock.display()))?;
    }
    Ok(())
}

fn compose(
    package: &Path,
    entries: &[CommandEntry],
    providers: &PluginRegistry,
    cache: &Path,
) -> Result<Option<String>> {
    let mut needs_composition = false;
    for entry in entries {
        for import in imports(&entry.module)? {
            if providers.get(&import).is_some() {
                needs_composition = true;
            } else if !host_import(&import) {
                bail!("no provider for resource import {import}");
            }
        }
    }
    if !needs_composition {
        remove_runtime(package)?;
        return Ok(None);
    }
    if entries.len() != 1 {
        bail!("resource-importing Spores with more than one command are not supported yet");
    }
    let entry = &entries[0];
    let mut current = entry.module.clone();
    let mut keys = Vec::new();
    let mut step = 0;
    for import in imports(&current)? {
        if let Some(provider) = providers.get(&import) {
            if provider.native.is_some() {
                bail!(
                    "provider for {import} is a native sidecar and cannot be composed into runtime.wasm"
                );
            }
            let next = package.join(format!(".compose-{step}.wasm"));
            keys.push(plugin::composition_key(
                &current,
                &provider.module,
                &import,
            )?);
            plugin::compose_cached(&current, &provider.module, &import, cache, &next)?;
            current = next;
            step += 1;
        } else if !host_import(&import) {
            bail!("no provider for resource import {import}");
        }
    }
    let runtime = package.join("runtime.wasm");
    fs::copy(&current, &runtime).with_context(|| format!("write {}", runtime.display()))?;
    for index in 0..step {
        let _ = fs::remove_file(package.join(format!(".compose-{index}.wasm")));
    }
    let key = composite_key(&keys);
    fs::write(package.join("runtime.key"), format!("{key}\n"))?;
    Ok(Some(key))
}

fn imports(module: &Path) -> Result<Vec<String>> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = wasmtime::component::Component::from_file(&engine, module)
        .map_err(|error| anyhow::anyhow!("read component {}: {error}", module.display()))?;
    Ok(component
        .component_type()
        .imports(&engine)
        .map(|(name, _)| name.to_string())
        .collect())
}

fn host_import(import: &str) -> bool {
    import.starts_with("wasi:")
        || RUNNER_CAPABILITIES
            .iter()
            .any(|cap| import == *cap || import.starts_with(&format!("{cap}/")))
}

fn composite_key(keys: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"loam-runtime-key-v1\0");
    for key in keys {
        hasher.update(key.as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

fn remove_runtime(package: &Path) -> Result<()> {
    for name in ["runtime.wasm", "runtime.key"] {
        let path = package.join(name);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn generation_key(package_digest: &str, composition_key: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(package_digest.as_bytes());
    hasher.update(b"\0");
    hasher.update(composition_key.unwrap_or("none").as_bytes());
    format!("{:x}", hasher.finalize())
}

fn generation_dir(root: &Path, package: &str, generation: &str) -> PathBuf {
    root.join(LOAM_DIR)
        .join("generations")
        .join(package)
        .join(generation)
}

fn activate(root: &Path, package: &str, generation: &str) -> Result<()> {
    let link = root.join(package);
    if let Ok(metadata) = fs::symlink_metadata(&link)
        && metadata.is_dir()
        && !metadata.file_type().is_symlink()
    {
        bail!(
            "{} is a real directory; refusing to replace an unmanaged package",
            link.display()
        );
    }
    let target = Path::new(LOAM_DIR)
        .join("generations")
        .join(package)
        .join(generation);
    let temp = root.join(format!(".loam-swap-{}-{}", std::process::id(), now()));
    let _ = fs::remove_file(&temp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &temp)?;
    #[cfg(not(unix))]
    bail!("atomic local installs currently require Unix symlinks");
    fs::rename(&temp, &link).with_context(|| format!("activate {}", link.display()))?;
    Ok(())
}

fn state_path(root: &Path) -> PathBuf {
    root.join(LOAM_DIR).join(STATE_FILE)
}

fn load_state(root: &Path) -> Result<State> {
    let path = state_path(root);
    match fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn save_state(root: &Path, state: &State) -> Result<()> {
    let path = state_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = root
        .join(LOAM_DIR)
        .join(format!(".state-{}-{}.tmp", std::process::id(), now()));
    let bytes = serde_json::to_vec_pretty(state)?;
    {
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, &path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn fresh_txn(_root: &Path) -> String {
    format!("{}-{}", std::process::id(), now())
}

fn safe_component(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        bail!("empty package name");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("package name must use letters, digits, `.`, `_` or `-`");
    }
    Ok(name.to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn component_bytes() -> Vec<u8> {
        wat::parse_str("(component)").unwrap()
    }

    fn command_package(root: &Path, name: &str, module: &[u8]) -> PathBuf {
        let package = root.join(name);
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("module.wasm"), module).unwrap();
        fs::write(
            package.join("command.toml"),
            format!(
                "schema = 1\n[command]\nname = \"{name}\"\nversion = \"0.1.0\"\nsummary = \"test\"\nmodule = \"module.wasm\"\n"
            ),
        )
        .unwrap();
        package
    }

    fn provider_package(root: &Path, name: &str, module: &[u8]) -> PathBuf {
        let package = root.join(name);
        fs::create_dir_all(package.join("signatures")).unwrap();
        fs::write(package.join("module.wasm"), module).unwrap();
        fs::write(
            package.join("spore.toml"),
            "schema = 1\n[spore]\nid = \"test.logger\"\nname = \"logger\"\nversion = \"0.1.0\"\nlicense = \"MIT\"\nsummary = \"test provider\"\n[engine]\nsoil = \">=0.1\"\ntarget = \"wasm32-wasip2\"\n[module]\npath = \"module.wasm\"\n[launch]\ndefault = \"canvas\"\nsurfaces = [\"canvas\"]\n[grants]\n\"soil:logging\" = \"test provider resource\"\n",
        )
        .unwrap();
        let manifest = crate::spore::Manifest::load(&package).unwrap();
        let lock = crate::spore::compute_lock(&package, &manifest).unwrap();
        fs::write(package.join("spore.lock"), lock.render()).unwrap();
        fs::write(package.join("signatures/test.asc"), b"test signature").unwrap();
        package
    }

    fn options(path: &Path, root: &Path, dev: bool) -> InstallOptions {
        InstallOptions {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
            allow_unsigned_dev: dev,
            provider_roots: Vec::new(),
            composition_cache: None,
        }
    }

    #[test]
    fn installs_warms_status_and_rolls_back_a_function_command() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let package = command_package(source.path(), "hello", &component_bytes());
        let first = install(options(&package, root.path(), true)).unwrap();
        assert!(root.path().join("hello").is_symlink());
        assert!(root.path().join(".warm/hello.json").is_file());
        let status = status(root.path()).unwrap();
        assert_eq!(status.installs[0].current, first.generation);

        let second = install(options(&package, root.path(), true)).unwrap();
        assert_eq!(second.previous.as_deref(), Some(first.generation.as_str()));
        let rolled = rollback(root.path(), &first.id).unwrap();
        assert_eq!(rolled.generation, first.generation);
    }

    #[test]
    fn a_stale_spore_lock_is_refused() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let package = source.path().join("bad");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("module.wasm"), b"\0asm").unwrap();
        fs::write(package.join("spore.toml"), "schema = 1\n").unwrap();
        fs::write(package.join("spore.lock"), "bad\n").unwrap();
        assert!(install(options(&package, root.path(), true)).is_err());
    }

    #[test]
    fn an_unsigned_package_needs_dev_mode() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let package = command_package(source.path(), "hello", &component_bytes());
        assert!(install(options(&package, root.path(), false)).is_err());
    }

    fn resource_command(source: &Path, name: &str) -> PathBuf {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/composition/resource/root.wat");
        let source_text = fs::read_to_string(fixture).unwrap().replace(
            "example:service/logging@0.1.0",
            "soil:logging/logging@0.1.0",
        );
        command_package(source, name, &wat::parse_str(source_text).unwrap())
    }

    #[test]
    fn a_missing_provider_is_refused() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let package = resource_command(source.path(), "needs-provider");
        assert!(install(options(&package, root.path(), true)).is_err());
    }

    #[test]
    fn a_resource_import_is_composed_and_the_source_is_unchanged() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let providers = tempfile::tempdir().unwrap();
        let logger = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/composition/resource/logger.wat");
        provider_package(
            providers.path(),
            "logger",
            &wat::parse_str(fs::read_to_string(logger).unwrap().replace(
                "example:service/logging@0.1.0",
                "soil:logging/logging@0.1.0",
            ))
            .unwrap(),
        );
        let package = resource_command(source.path(), "needs-provider");
        let before = fs::read(package.join("command.toml")).unwrap();
        let mut opts = options(&package, root.path(), true);
        opts.provider_roots = vec![providers.path().to_path_buf()];
        let report = install(opts).unwrap();
        assert!(report.composition_key.is_some());
        let generation = root
            .path()
            .join(LOAM_DIR)
            .join("generations")
            .join("needs-provider")
            .join(&report.generation);
        assert!(generation.join("runtime.wasm").is_file());
        assert!(generation.join("runtime.key").is_file());
        assert!(root.path().join(".warm/needs-provider.json").is_file());
        assert_eq!(fs::read(package.join("command.toml")).unwrap(), before);
        assert!(!package.join("runtime.wasm").exists());
        let registry = command::CommandRegistry::scan(root.path()).unwrap();
        let entry = registry.get("needs-provider").unwrap();
        assert!(entry.runtime_module.is_some());
        assert!(entry.composition_key.is_some());
    }

    #[test]
    fn a_failed_warm_keeps_the_old_symlink() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let package = command_package(source.path(), "hello", &component_bytes());
        let first = install(options(&package, root.path(), true)).unwrap();
        let before = fs::read_link(root.path().join("hello")).unwrap();
        fs::write(package.join("module.wasm"), b"not wasm").unwrap();
        assert!(install(options(&package, root.path(), true)).is_err());
        assert_eq!(fs::read_link(root.path().join("hello")).unwrap(), before);
        assert_eq!(
            status(root.path()).unwrap().installs[0].current,
            first.generation
        );
    }

    #[test]
    fn install_copies_the_source_instead_of_writing_it() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let package = command_package(source.path(), "hello", &component_bytes());
        let before = fs::read(package.join("command.toml")).unwrap();
        install(options(&package, root.path(), true)).unwrap();
        assert_eq!(fs::read(package.join("command.toml")).unwrap(), before);
        assert!(!package.join("runtime.wasm").exists());
    }

    #[test]
    fn status_is_empty_for_a_new_root() {
        let root = tempfile::tempdir().unwrap();
        assert!(status(root.path()).unwrap().installs.is_empty());
    }

    #[test]
    fn package_digest_is_stable_and_changes_with_content() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        command_package(one.path(), "hello", &component_bytes());
        command_package(two.path(), "hello", &component_bytes());
        let a = package_digest(&one.path().join("hello")).unwrap();
        let b = package_digest(&two.path().join("hello")).unwrap();
        assert_eq!(a, b);
        let file = one.path().join("hello/module.wasm");
        let mut bytes = fs::read(&file).unwrap();
        bytes.extend_from_slice(b"x");
        fs::write(file, bytes).unwrap();
        assert_ne!(a, package_digest(&one.path().join("hello")).unwrap());
    }

    #[test]
    fn source_read_is_not_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let package = command_package(dir.path(), "hello", &component_bytes());
        let mut file = File::open(package.join("command.toml")).unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert!(text.contains("hello"));
    }
}
