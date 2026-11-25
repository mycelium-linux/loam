//! Call a provider by interface and function name.
//!
//! This is the half of Soil↔WIT interoperability that has nothing to do with
//! Soil: given a registry, an interface name and a function name, instantiate
//! the component that exports it and call it. No provider is named here, so a
//! new provider needs no change in this file.
//!
//! The values are `wasmtime::component::Val`, not Soil's `Value`: the mapping
//! between the two belongs to whoever owns the interpreter, and keeping it out
//! of here is what lets the same invoker serve a different host later.

use std::path::Path;

use anyhow::{Context, Result};
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Instance, Linker, ResourceTable, Type, Val};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::plugin::PluginRegistry;
use super::plugin_ipc::{self, ipc};

/// Why a call failed, kept distinct on purpose: a missing capability, a
/// provider that will not start and a provider that answered badly are
/// different facts and a caller must be able to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// Nothing in the registry exports that interface. Not a grant failure.
    MissingCapability,
    /// The interface exists but the import list does not authorize it.
    Denied { interface: String },
    /// The provider could not be started or called.
    Infrastructure(String),
    /// The provider does not export that function, or exports another shape.
    BadRequest(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCapability => write!(f, "no provider exports this interface"),
            Self::Denied { interface } => write!(f, "not granted: {interface}"),
            Self::Infrastructure(reason) => write!(f, "provider failed: {reason}"),
            Self::BadRequest(reason) => write!(f, "bad request: {reason}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Host state a provider needs: WASI, and the IPC backend its sidecar speaks.
struct ProviderState {
    wasi: WasiCtx,
    table: ResourceTable,
    ipc: Option<Box<dyn plugin_ipc::Backend>>,
}

impl WasiView for ProviderState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl ipc::Host for ProviderState {
    fn request(&mut self, method: String, payload: Vec<u8>) -> Result<Vec<u8>, ipc::Error> {
        match self.ipc.as_mut() {
            Some(backend) => backend.request(&method, &payload),
            // Honest refusal: a provider whose sidecar is not wired must not be
            // reported as having answered.
            None => Err(ipc::Error::Unavailable),
        }
    }
}

/// A registry, plus the authority to use it.
pub struct ProviderHost {
    engine: Engine,
    registry: PluginRegistry,
    grants: std::collections::BTreeSet<String>,
}

impl ProviderHost {
    pub fn new(engine: Engine, registry: PluginRegistry) -> Self {
        Self { engine, registry, grants: std::collections::BTreeSet::new() }
    }

    /// Declare what a caller may reach. Mirrors the manifest's `[grants]`.
    pub fn grant(&mut self, interface: &str) {
        self.grants.insert(interface.to_owned());
    }

    pub fn grant_all(&mut self) {
        for (interface, _) in self.registry.interfaces() {
            self.grants.insert(interface.to_owned());
        }
    }

    pub fn accepts(&self, interface: &str, _function: &str) -> bool {
        self.registry.get(interface).is_some() && self.grants.contains(interface)
    }

    /// Instantiate the provider for `interface` and call `function`.
    ///
    /// `params` are already typed: converting a language's values into these is
    /// the interpreter's job, and doing it there keeps this generic.
    pub fn call(
        &self,
        interface: &str,
        function: &str,
        params: &[Val],
    ) -> Result<Val, ProviderError> {
        if self.registry.get(interface).is_none() {
            return Err(ProviderError::MissingCapability);
        }
        if !self.grants.contains(interface) {
            return Err(ProviderError::Denied { interface: interface.to_owned() });
        }
        let entry = self
            .registry
            .get(interface)
            .ok_or(ProviderError::MissingCapability)?;

        let component = Component::from_file(&self.engine, &entry.module)
            .map_err(|error| ProviderError::Infrastructure(format!("load provider: {error}")))?;
        let ipc = match (&entry.sidecar_path, &entry.native) {
            (Some(path), _) => Some(Box::new(
                plugin_ipc::Sidecar::spawn(&path.display().to_string(), &[], Some(&entry.package))
                    .map_err(|error| ProviderError::Infrastructure(format!("start sidecar: {error:#}")))?,
            ) as Box<dyn plugin_ipc::Backend>),
            (None, Some(program)) => Some(Box::new(
                plugin_ipc::Sidecar::spawn(program, &[], Some(&entry.package))
                    .map_err(|error| ProviderError::Infrastructure(format!("start sidecar: {error:#}")))?,
            ) as Box<dyn plugin_ipc::Backend>),
            (None, None) => None,
        };

        let mut linker = Linker::<ProviderState>::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| ProviderError::Infrastructure(format!("wasi linker: {error}")))?;
        plugin_ipc::add_to_linker::<ProviderState>(&mut linker)
            .map_err(|error| ProviderError::Infrastructure(format!("ipc linker: {error}")))?;

        let mut store = Store::new(
            &self.engine,
            ProviderState {
                wasi: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
                ipc,
            },
        );
        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(|error| ProviderError::Infrastructure(format!("instantiate: {error:#}")))?;
        let func = lookup(&mut store, &instance, interface, function)?;

        let signature = func.ty(&store);
        let wanted = signature.params().len();
        if wanted != params.len() {
            return Err(ProviderError::BadRequest(format!(
                "{interface}/{function} takes {wanted} argument(s), got {}",
                params.len()
            )));
        }
        let mut results = signature
            .results()
            .map(|ty| default_val(&ty))
            .collect::<Vec<_>>();
        func.call(&mut store, params, &mut results)
            .map_err(|error| ProviderError::Infrastructure(format!("call: {error:#}")))?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::BadRequest(format!("{interface}/{function} returned nothing")))
    }
}

fn lookup(
    store: &mut Store<ProviderState>,
    instance: &Instance,
    interface: &str,
    function: &str,
) -> Result<wasmtime::component::Func, ProviderError> {
    let Some(index) = instance.get_export_index(&mut *store, None, interface) else {
        return Err(ProviderError::BadRequest(format!(
            "provider does not export {interface}"
        )));
    };
    let Some(function_index) = instance.get_export_index(&mut *store, Some(&index), function) else {
        return Err(ProviderError::BadRequest(format!(
            "{interface} does not export {function}"
        )));
    };
    instance
        .get_func(&mut *store, function_index)
        .ok_or_else(|| ProviderError::BadRequest(format!("{interface}/{function} is not a function")))
}

/// A zero value of the right shape, because `Func::call` fills caller-provided
/// slots and refuses mismatched types.
fn default_val(ty: &Type) -> Val {
    match ty {
        Type::Bool => Val::Bool(false),
        Type::S8 => Val::S8(0),
        Type::U8 => Val::U8(0),
        Type::S16 => Val::S16(0),
        Type::U16 => Val::U16(0),
        Type::S32 => Val::S32(0),
        Type::U32 => Val::U32(0),
        Type::S64 => Val::S64(0),
        Type::U64 => Val::U64(0),
        Type::Float32 => Val::Float32(0.0),
        Type::Float64 => Val::Float64(0.0),
        Type::Char => Val::Char(' '),
        Type::String => Val::String(String::new()),
        Type::List(_) => Val::List(Vec::new()),
        Type::Record(record) => Val::Record(
            record
                .fields()
                .map(|field| (field.name.to_string(), default_val(&field.ty)))
                .collect(),
        ),
        Type::Tuple(tuple) => Val::Tuple(tuple.types().map(|ty| default_val(&ty)).collect()),
        Type::Option(_) => Val::Option(None),
        // A default result is the ok arm with a default payload: `Ok(None)`
        // means an empty result, which is wrong for a non-empty type but is a
        // value the provider will overwrite.
        Type::Result(_) => Val::Result(Err(None)),
        Type::Enum(labels) => Val::Enum(
            labels
                .names()
                .next()
                .map(str::to_owned)
                .unwrap_or_default(),
        ),
        Type::Variant(variant) => match variant.cases().next() {
            Some(case) => Val::Variant(
                case.name.to_string(),
                case.ty.as_ref().map(|ty| Box::new(default_val(ty))),
            ),
            None => Val::Bool(false),
        },
        Type::Flags(_) => Val::Flags(Vec::new()),
        Type::Map(_) => Val::Map(Vec::new()),
        // Resources and anything newer: no default exists, and pretending
        // otherwise would hand the caller a value the provider cannot accept.
        _ => Val::Bool(false),
    }
}

/// Paths are handed to `Component::from_file`, which wants an absolute file.
pub fn absolute(path: &Path) -> Result<std::path::PathBuf> {
    path.canonicalize().with_context(|| format!("resolve {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::Config;

    /// A real provider, called by interface and function name with no
    /// provider-specific code in this crate. Skipped when nothing is installed,
    /// because a green test that never ran a provider proves nothing.
    #[test]
    fn calls_an_installed_provider_by_name() {
        let Ok(registry) = PluginRegistry::scan_default_dev(true) else {
            return;
        };
        let interface = "fpl:transmog/engineering@0.1.0";
        if registry.get(interface).is_none() {
            eprintln!("skipping: {interface} is not installed");
            return;
        }
        // The sidecar inherits this process's environment.
        if std::env::var_os("TRANSMOG_BIN").is_none()
            && std::path::Path::new("/tmp/transmog").exists()
        {
            unsafe { std::env::set_var("TRANSMOG_BIN", "/tmp/transmog") };
        }
        let config = {
            let mut config = Config::new();
            config.wasm_component_model(true);
            config
        };
        let engine = Engine::new(&config).expect("engine");

        // Denied first: a capability that exists but is not granted must not run.
        let host = ProviderHost::new(engine.clone(), PluginRegistry::scan_default_dev(true).unwrap());
        assert_eq!(
            host.call(interface, "identity", &[]),
            Err(ProviderError::Denied { interface: interface.to_owned() })
        );

        let mut host = ProviderHost::new(engine, PluginRegistry::scan_default_dev(true).unwrap());
        host.grant(interface);
        match host.call(interface, "identity", &[]) {
            Ok(Val::String(identity)) => {
                assert!(identity.contains("transmog"), "unexpected identity {identity:?}");
            }
            other => panic!("identity call: {other:?}"),
        }
    }

    /// A missing capability is not a grant failure, and neither is an unknown
    /// function: three different facts, three different errors.
    #[test]
    fn distinguishes_missing_from_denied_from_bad_request() {
        let Ok(registry) = PluginRegistry::scan_default_dev(true) else { return };
        let engine = {
            let mut config = Config::new();
            config.wasm_component_model(true);
            Engine::new(&config).expect("engine")
        };
        let mut host = ProviderHost::new(engine, registry);
        assert_eq!(
            host.call("fpl:not-installed/x@0.1.0", "f", &[]),
            Err(ProviderError::MissingCapability)
        );
        let interface = "fpl:transmog/engineering@0.1.0";
        if host.registry.get(interface).is_none() {
            return;
        }
        host.grant(interface);
        assert!(matches!(
            host.call(interface, "no-such-function", &[]),
            Err(ProviderError::BadRequest(_))
        ));
    }
}
