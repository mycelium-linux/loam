//! Installed Spore exports and the linker that satisfies guest imports.
//!
//! A plugin is not a second package format. It is a Spore that exports one or
//! more capability interfaces instead of only consuming them. The registry
//! indexes those exports; the linker instantiates the provider and satisfies
//! the consumer's import.
//!
//! Linking is not granting. A provider's exports say what it offers. The
//! consumer still needs the same interface in its own `requires`/`[grants]`,
//! and the provider's own imports are checked against its own manifest.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use wasm_compose::{
    composer::ComponentComposer,
    config::{Config, Dependency, Instantiation, InstantiationArg},
};
use wasmtime::{
    Engine, Store,
    component::{
        Component, Instance, Linker,
        types::{ComponentInstance, ComponentItem},
    },
};

/// Where an installed Spore's module lives and what it exports.
#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub id: String,
    pub version: String,
    pub package: PathBuf,
    pub module: PathBuf,
    pub provides: Vec<String>,
    pub requires: Vec<String>,
    pub native: Option<String>,
}

/// Exports found on one Spore that could not be indexed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRefusal {
    pub name: String,
    pub reason: String,
}

/// Installed providers, keyed by canonical WIT interface name.
#[derive(Debug, Clone)]
pub struct PluginRegistry {
    roots: Vec<PathBuf>,
    plugins: BTreeMap<String, PluginEntry>,
    refused: Vec<PluginRefusal>,
}

impl PluginRegistry {
    pub fn empty() -> Self {
        Self {
            roots: Vec::new(),
            plugins: BTreeMap::new(),
            refused: Vec::new(),
        }
    }

    /// Scan roots in order. The first package to claim an interface wins; a
    /// later claim is refused rather than silently shadowing it.
    pub fn scan_roots(roots: &[PathBuf]) -> Result<Self> {
        let mut registry = Self {
            roots: roots.to_vec(),
            plugins: BTreeMap::new(),
            refused: Vec::new(),
        };
        for root in roots {
            let directory = match std::fs::read_dir(root) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(anyhow::Error::new(error)
                        .context(format!("read plugin root {}", root.display())));
                }
            };
            for entry in directory {
                let entry =
                    entry.with_context(|| format!("read plugin root {}", root.display()))?;
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let package = entry.path();
                if package.is_dir() {
                    registry.collect(&package);
                }
            }
        }
        Ok(registry)
    }

    /// Scan the same roots a shell uses: hand-installed commands first, then
    /// Spores installed by a store.
    pub fn scan_default() -> Result<Self> {
        let command_root = std::env::var_os("FUNGOS_COMMAND_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".local/share/fungos/commands")
            });
        Self::scan_roots(&super::roots(command_root))
    }

    fn collect(&mut self, package: &Path) {
        let manifest_path = package.join("spore.toml");
        if !manifest_path.is_file() {
            return;
        }
        let manifest = match crate::spore::Manifest::load(package) {
            Ok(manifest) => manifest,
            Err(error) => {
                self.refuse(package, format!("{error}"));
                return;
            }
        };
        let Some(module) = &manifest.module else {
            return;
        };
        let module_path = package.join(&module.path);
        let bytes = match std::fs::read(&module_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.refuse(
                    package,
                    format!("read module {}: {error}", module_path.display()),
                );
                return;
            }
        };
        let exports = match crate::spore::exported_interfaces(&bytes) {
            Ok(exports) => exports,
            Err(error) => {
                self.refuse(package, format!("{error}"));
                return;
            }
        };
        // A command exports `wasi:cli/run`; an HTTP service exports
        // `wasi:http/incoming-handler`. Those are faces, not plugin
        // capabilities. A plugin is a Spore exporting something outside WASI.
        let provides: Vec<String> = exports
            .into_iter()
            .filter(|interface| !interface.starts_with("wasi:"))
            .collect();
        if provides.is_empty() {
            return;
        }
        if let Err(error) = crate::spore::check(package) {
            self.refuse(package, format!("package is not sealed: {error}"));
            return;
        }
        // Installed packages are trusted after install; the signature check is
        // at install time. This scan rejects an unsealed tree so a half-copied
        // package cannot become a provider.
        if !package.join("spore.lock").is_file() {
            self.refuse(package, "plugin Spore has no spore.lock".into());
            return;
        }
        if !has_signature(package) {
            self.refuse(package, "plugin Spore has no signatures/*.asc".into());
            return;
        }
        for interface in &provides {
            if let Some(existing) = self.plugins.get(interface) {
                self.refuse(
                    package,
                    format!(
                        "{interface} is already provided by {}",
                        existing.package.display()
                    ),
                );
                return;
            }
        }
        let entry = PluginEntry {
            id: manifest.spore.id.clone(),
            version: manifest.spore.version.clone(),
            package: package.to_path_buf(),
            module: module_path,
            provides,
            requires: manifest.grants.keys().cloned().collect(),
            native: manifest
                .plugin
                .as_ref()
                .and_then(|plugin| plugin.sidecar.clone()),
        };
        for interface in &entry.provides {
            self.plugins.insert(interface.clone(), entry.clone());
        }
    }

    fn refuse(&mut self, package: &Path, reason: String) {
        let name = package.file_name().map_or_else(
            || package.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.refused.push(PluginRefusal { name, reason });
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn entries(&self) -> impl Iterator<Item = &PluginEntry> {
        self.plugins.values()
    }

    pub fn get(&self, interface: &str) -> Option<&PluginEntry> {
        self.plugins.get(interface)
    }

    pub fn interfaces(&self) -> impl Iterator<Item = (&str, &PluginEntry)> {
        self.plugins
            .iter()
            .map(|(interface, entry)| (interface.as_str(), entry))
    }

    pub fn refused(&self) -> &[PluginRefusal] {
        &self.refused
    }

    /// The sidecar command for the first provider this guest imports, if any.
    ///
    /// A wasm-only plugin has no sidecar. A provider with `launch.native` gets
    /// one process per command run; a relative name is resolved inside the
    /// package first, then on PATH.
    pub fn native_for(
        &self,
        engine: &Engine,
        guest: &Component,
    ) -> Option<(String, Option<PathBuf>)> {
        let mut seen = std::collections::BTreeSet::new();
        for (import, _) in guest.component_type().imports(engine) {
            let Some(entry) = self.plugins.get(import) else {
                continue;
            };
            if !seen.insert(entry.module.clone()) {
                continue;
            }
            let Some(program) = entry.native.as_deref() else {
                continue;
            };
            let path = Path::new(program);
            let local = entry.package.join(path);
            if path.is_absolute() || local.is_file() {
                return Some((local.display().to_string(), Some(entry.package.clone())));
            }
            return Some((program.to_string(), Some(entry.package.clone())));
        }
        None
    }

    /// Instantiate every provider this guest imports and link its exported
    /// instance into the guest's linker.
    ///
    /// Providers are deduplicated by package, so a Spore exporting three
    /// interfaces is instantiated once. The plugin instance and the guest share
    /// the store; a plugin gets exactly the imports its own linker already
    /// offered.
    pub fn link<T>(
        &self,
        engine: &Engine,
        linker: &mut Linker<T>,
        store: &mut Store<T>,
        guest: &Component,
    ) -> Result<Vec<String>>
    where
        T: 'static,
    {
        let mut linked = Vec::new();
        let mut instances: BTreeMap<PathBuf, Instance> = BTreeMap::new();
        for (import, _) in guest.component_type().imports(engine) {
            let Some(entry) = self.plugins.get(import) else {
                continue;
            };
            let instance = match instances.get(&entry.module) {
                Some(instance) => *instance,
                None => {
                    let component =
                        Component::from_file(engine, &entry.module).map_err(|error| {
                            anyhow::anyhow!("compile plugin {}: {error}", entry.module.display())
                        })?;
                    let instance =
                        linker
                            .instantiate(&mut *store, &component)
                            .map_err(|error| {
                                anyhow::anyhow!(
                                    "instantiate plugin {}: {error}",
                                    entry.package.display()
                                )
                            })?;
                    instances.insert(entry.module.clone(), instance);
                    instance
                }
            };
            link_export_instance(engine, linker, store, instance, import)?;
            linked.push(import.to_string());
        }
        Ok(linked)
    }
}

/// Compose one provider component into a root component at install/build time.
///
/// `import` is the canonical WIT instance import the provider satisfies.
pub fn compose_provider(root: &Path, provider: &Path, import: &str, out: &Path) -> Result<()> {
    let dir = root.parent().unwrap_or_else(|| Path::new("."));
    let mut config = Config {
        dir: dir.to_path_buf(),
        ..Config::default()
    };
    let name = "logger";
    config.dependencies.insert(
        name.to_string(),
        Dependency {
            path: provider.to_path_buf(),
        },
    );
    let mut instantiation = Instantiation::default();
    instantiation.dependency = Some(name.to_string());
    instantiation.arguments.insert(
        import.to_string(),
        InstantiationArg {
            instance: name.to_string(),
            export: Some(import.to_string()),
        },
    );
    config.instantiations.insert("root".to_string(), instantiation);
    let bytes = ComponentComposer::new(root, &config).compose()?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, bytes)?;
    Ok(())
}

fn has_signature(package: &Path) -> bool {
    let Ok(directory) = std::fs::read_dir(package.join("signatures")) else {
        return false;
    };
    directory.flatten().any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "asc")
    })
}

fn link_export_instance<T>(
    engine: &Engine,
    linker: &mut Linker<T>,
    store: &mut Store<T>,
    instance: Instance,
    name: &str,
) -> Result<()>
where
    T: 'static,
{
    let component = instance
        .get_export(&mut *store, None, name)
        .ok_or_else(|| anyhow::anyhow!("plugin does not export {name}"))?;
    let ComponentItem::ComponentInstance(exports) = component.0 else {
        bail!("plugin export {name} is not an instance");
    };
    let instance_index = instance
        .get_export_index(&mut *store, None, name)
        .ok_or_else(|| anyhow::anyhow!("plugin export {name} has no runtime index"))?;
    let mut target = linker
        .instance(name)
        .map_err(|error| anyhow::anyhow!("link plugin instance {name}: {error}"))?;
    link_instance_exports_into(
        engine,
        &mut target,
        store,
        instance,
        &instance_index,
        &exports,
    )
}

fn link_instance_exports_into<T>(
    engine: &Engine,
    linker: &mut wasmtime::component::LinkerInstance<'_, T>,
    store: &mut Store<T>,
    instance: Instance,
    instance_index: &wasmtime::component::ComponentExportIndex,
    exports: &ComponentInstance,
) -> Result<()>
where
    T: 'static,
{
    for (name, export) in exports.exports(engine) {
        match export.ty {
            ComponentItem::ComponentFunc(function) => {
                if function.async_() {
                    bail!("plugin export {name} is async, which this linker cannot proxy yet");
                }
                let index = instance
                    .get_export_index(&mut *store, Some(instance_index), name)
                    .ok_or_else(|| anyhow::anyhow!("plugin export {name} has no runtime index"))?;
                let func = instance
                    .get_func(&mut *store, index)
                    .ok_or_else(|| anyhow::anyhow!("plugin export {name} is not a function"))?;
                linker.func_new(name, move |mut store, _ty, params, results| {
                    func.call(&mut store, params, results)?;
                    Ok(())
                })?;
            }
            ComponentItem::ComponentInstance(nested) => {
                let index = instance
                    .get_export_index(&mut *store, Some(instance_index), name)
                    .ok_or_else(|| anyhow::anyhow!("plugin export {name} has no runtime index"))?;
                let mut target = linker
                    .instance(name)
                    .map_err(|error| anyhow::anyhow!("link plugin instance {name}: {error}"))?;
                link_instance_exports_into(engine, &mut target, store, instance, &index, &nested)?;
            }
            // Interface types (records, enums, variants) are named exports,
            // not values the linker supplies. Wasmtime's typechecker ignores
            // them for the same reason.
            ComponentItem::Type(_) => {}
            _ => bail!("nested plugin export {name} is not a function or instance"),
        }
    }
    Ok(())
}

/// `loam plugin` command.
pub fn cli(args: &[String]) -> Result<()> {
    let mut roots = Vec::new();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                roots.push(PathBuf::from(args.get(i + 1).context("--root DIR")?));
                i += 2;
            }
            value if value.starts_with('-') => bail!("unknown plugin option {value}"),
            value => {
                positional.push(value.to_string());
                i += 1;
            }
        }
    }
    let command = positional.first().map(String::as_str).unwrap_or("list");
    if command == "compose" {
        let root = PathBuf::from(positional.get(1).context("plugin compose ROOT")?);
        let provider = PathBuf::from(positional.get(2).context("plugin compose ROOT PROVIDER")?);
        let import = positional.get(3).context("plugin compose ROOT PROVIDER IMPORT")?;
        let out = PathBuf::from(
            positional
                .get(4)
                .context("plugin compose ROOT PROVIDER IMPORT OUT")?,
        );
        compose_provider(&root, &provider, import, &out)?;
        println!("{}", out.display());
        return Ok(());
    }
    if command == "pack" {
        let package = PathBuf::from(positional.get(1).context("plugin pack DIR")?);
        let manifest = crate::spore::Manifest::load(&package)?;
        let lock = crate::spore::compute_lock(&package, &manifest)?;
        std::fs::write(package.join(crate::spore::LOCK_NAME), lock.render()).with_context(
            || format!("write {}", package.join(crate::spore::LOCK_NAME).display()),
        )?;
        println!("{}", lock.digest);
        return Ok(());
    }
    let registry = if roots.is_empty() {
        PluginRegistry::scan_default()?
    } else {
        PluginRegistry::scan_roots(&roots)?
    };
    match command {
        "list" => {
            for (interface, entry) in registry.interfaces() {
                println!(
                    "{interface}\t{}\t{}\t{}",
                    entry.id,
                    entry.version,
                    entry.package.display()
                );
            }
            for refusal in registry.refused() {
                eprintln!("plugin: {}: {}", refusal.name, refusal.reason);
            }
            Ok(())
        }
        "verify" => {
            for refusal in registry.refused() {
                eprintln!("plugin: {}: {}", refusal.name, refusal.reason);
            }
            if !registry.refused().is_empty() {
                bail!("{} plugin package(s) refused", registry.refused().len());
            }
            println!("plugins ok: {} interfaces", registry.interfaces().count());
            Ok(())
        }
        "roots" => {
            for root in registry.roots() {
                println!("{}", root.display());
            }
            Ok(())
        }
        other => bail!("unknown plugin command {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use wasmtime::{
        Config, Engine, Store,
        component::{Component, Linker},
    };

    const PROVIDER: &str = r#"
        (component
          (core module $m
            (func (export "answer") (result i32) i32.const 42)
          )
          (core instance $i (instantiate $m))
          (func $answer (result u32) (canon lift (core func $i "answer")))
          (instance $api (export "answer" (func $answer)))
          (export "test:echo/echo@0.1.0" (instance $api))
        )
    "#;

    const GUEST: &str = r#"
        (component
          (import "test:echo/echo@0.1.0"
            (instance $api (export "answer" (func (result u32)))))
          (core func $answer (canon lower (func $api "answer")))
          (core module $m
            (import "" "answer" (func $answer (result i32)))
            (func (export "run") (result i32) call $answer)
          )
          (core instance $i
            (instantiate $m (with "" (instance (export "answer" (func $answer))))))
          (func (export "run") (result u32) (canon lift (core func $i "run")))
        )
    "#;

    fn manifest() -> String {
        "schema = 1\n[spore]\nid = \"test.echo\"\nname = \"echo\"\nversion = \"0.1.0\"\nlicense = \"MIT\"\nsummary = \"test plugin\"\n[engine]\nsoil = \">=0.1\"\ntarget = \"wasm32-wasip2\"\n[module]\npath = \"module.wasm\"\n[launch]\ndefault = \"canvas\"\nsurfaces = [\"canvas\"]\n".into()
    }

    fn package(root: &Path, name: &str, signed: bool) -> PathBuf {
        let package = root.join(name);
        fs::create_dir_all(package.join("signatures")).unwrap();
        fs::write(
            package.join("module.wasm"),
            wat::parse_str(PROVIDER).unwrap(),
        )
        .unwrap();
        fs::write(package.join("spore.toml"), manifest()).unwrap();
        let manifest = crate::spore::Manifest::load(&package).unwrap();
        let lock = crate::spore::compute_lock(&package, &manifest).unwrap();
        fs::write(package.join("spore.lock"), lock.render()).unwrap();
        if signed {
            fs::write(package.join("signatures/test.asc"), b"test signature").unwrap();
        }
        package
    }

    fn engine() -> Engine {
        let mut config = Config::new();
        config.wasm_component_model(true);
        Engine::new(&config).unwrap()
    }

    #[test]
    fn scans_a_spore_that_exports_a_capability() {
        let dir = tempfile::tempdir().unwrap();
        package(dir.path(), "echo", true);
        let registry = PluginRegistry::scan_roots(&[dir.path().to_path_buf()]).unwrap();
        let entry = registry.get("test:echo/echo@0.1.0").expect("provider");
        assert_eq!(entry.id, "test.echo");
        assert_eq!(entry.provides, vec!["test:echo/echo@0.1.0"]);
    }

    #[test]
    fn an_unsigned_provider_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        package(dir.path(), "echo", false);
        let registry = PluginRegistry::scan_roots(&[dir.path().to_path_buf()]).unwrap();
        assert!(registry.get("test:echo/echo@0.1.0").is_none());
        assert!(
            registry.refused()[0].reason.contains("signatures"),
            "{}",
            registry.refused()[0].reason
        );
    }

    #[test]
    fn a_second_provider_of_one_interface_is_refused() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        package(first.path(), "echo", true);
        package(second.path(), "echo", true);
        let registry =
            PluginRegistry::scan_roots(&[first.path().to_path_buf(), second.path().to_path_buf()])
                .unwrap();
        assert!(registry.get("test:echo/echo@0.1.0").is_some());
        assert!(
            registry
                .refused()
                .iter()
                .any(|refusal| { refusal.reason.contains("already provided") })
        );
    }

    #[test]
    fn dynamic_host_func_in_named_instance_works() {
        let engine = engine();
        let guest = Component::new(&engine, wat::parse_str(GUEST).unwrap()).unwrap();
        let mut linker = Linker::<()>::new(&engine);
        {
            let mut api = linker.instance("test:echo/echo@0.1.0").unwrap();
            api.func_new("answer", |_store, _ty, _params, results| {
                results[0] = wasmtime::component::Val::U32(7);
                Ok(())
            })
            .unwrap();
        }
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate(&mut store, &guest).unwrap();
        let run = instance
            .get_typed_func::<(), (u32,)>(&mut store, "run")
            .unwrap();
        assert_eq!(run.call(&mut store, ()).unwrap(), (7,));
    }

    #[test]
    fn composes_a_resource_provider() {
        let engine = engine();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/composition/resource/root.wat");
        let provider = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/composition/resource/logger.wat");
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("composed.wasm");
        compose_provider(&root, &provider, "example:service/logging@0.1.0", &out)
            .expect("compose resource provider");
        Component::from_file(&engine, &out).expect("composed component");
    }

    #[test]
    fn links_a_provider_export_to_a_guest_import() {
        let dir = tempfile::tempdir().unwrap();
        package(dir.path(), "echo", true);
        let registry = PluginRegistry::scan_roots(&[dir.path().to_path_buf()]).unwrap();
        let engine = engine();
        let guest = Component::new(&engine, wat::parse_str(GUEST).unwrap()).unwrap();
        let mut linker = Linker::<()>::new(&engine);
        let mut store = Store::new(&engine, ());
        let linked = registry
            .link(&engine, &mut linker, &mut store, &guest)
            .unwrap();
        assert_eq!(linked, vec!["test:echo/echo@0.1.0"]);
        let instance = linker.instantiate(&mut store, &guest).unwrap();
        let run = instance
            .get_typed_func::<(), (u32,)>(&mut store, "run")
            .unwrap();
        assert_eq!(run.call(&mut store, ()).unwrap(), (42,));
    }
}
