//! Run a component that exports `run(args: list<string>)`.
//!
//! This is the other half of [`crate::HttpService`]: the same host, the same
//! capability set, and the same [`WasiHostBuilder`]. A Spore is a command or an
//! HTTP endpoint because of what it exports, not because a different executable
//! started it.

use anyhow::{Context, Result};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Func, HasSelf, Instance, Linker},
};

use crate::{
    creds,
    host::{WasiHost, WasiHostBuilder},
    unibus,
};

/// What a command returned.
pub struct CommandOutcome {
    /// The bytes from an optional `snapshot` export.
    pub snapshot: Option<Vec<u8>>,
}

/// A compiled command component plus the host policy used to run it.
pub struct CommandService {
    engine: Engine,
    component: Component,
    builder: WasiHostBuilder,
    spore: String,
}

impl CommandService {
    pub fn from_bytes(bytes: &[u8], builder: WasiHostBuilder) -> Result<Self> {
        Self::from_bytes_named("spore", bytes, builder)
    }

    pub fn from_bytes_named(
        spore: impl Into<String>,
        bytes: &[u8],
        builder: WasiHostBuilder,
    ) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let component = Component::new(&engine, bytes)
            .map_err(anyhow::Error::from)
            .context("compile command component")?;
        Self::from_component_named(spore, component, builder)
    }

    pub fn from_component_named(
        spore: impl Into<String>,
        component: Component,
        builder: WasiHostBuilder,
    ) -> Result<Self> {
        let engine = component.engine().clone();
        let spore = spore.into();
        if spore.trim().is_empty() {
            anyhow::bail!("spore name must not be empty");
        }
        Ok(Self {
            engine,
            component,
            builder,
            spore,
        })
    }

    pub fn spore(&self) -> &str {
        &self.spore
    }

    /// Link the host and call `run(args)` once.
    pub fn run(&self, args: Vec<String>) -> Result<CommandOutcome> {
        let mut linker = Linker::<WasiHost>::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(anyhow::Error::from)
            .context("link WASI p2")?;
        crate::context::add_to_linker(&mut linker)?;
        soil_inference_host::add_to_linker(&mut linker).context("link soil:inference")?;
        creds::add_to_linker(&mut linker)?;
        unibus::add_to_linker(&mut linker)?;
        crate::time::add_to_linker(&mut linker)?;
        crate::stream::add_to_linker(&mut linker)?;
        if self.builder.has_codec() {
            crate::codec_host::add_to_linker::<WasiHost>(&mut linker)
                .context("link soil:codec")?;
        }
        if self.builder.has_vfs() {
            crate::vfs_host::add_to_linker::<WasiHost>(&mut linker).context("link soil:vfs")?;
            crate::vfs_host::add_mutation_to_linker::<WasiHost>(&mut linker)
                .context("link soil:vfs-mut")?;
        }
        if self.builder.has_records() {
            crate::records_host::add_to_linker(&mut linker).context("link soil:records")?;
        }
        if self.builder.has_http() {
            crate::http_host::api::add_to_linker::<WasiHost, HasSelf<WasiHost>>(
                &mut linker,
                |state| state,
            )?;
        }

        let mut store = Store::new(&self.engine, self.builder.build()?);
        let instance = linker
            .instantiate(&mut store, &self.component)
            .map_err(anyhow::Error::from)
            .context("instantiate command component")?;
        let run = function(&mut store, &instance, "run")?
            .typed::<(Vec<String>,), (Result<(), String>,)>(&mut store)
            .map_err(anyhow::Error::from)
            .context("run(args: list<string>) -> result<_, string>")?;
        let (result,) = run.call(&mut store, (args,)).map_err(anyhow::Error::from)?;
        result.map_err(|message| anyhow::anyhow!("{message}"))?;

        let snapshot = optional_function(&mut store, &instance, "snapshot")
            .map(|function| {
                let snapshot = function
                    .typed::<(), (Result<Vec<u8>, String>,)>(&mut store)
                    .map_err(anyhow::Error::from)
                    .context("snapshot() -> result<list<u8>, string>")?
                    .call(&mut store, ())
                    .map_err(anyhow::Error::from)?;
                let (bytes,) = snapshot;
                bytes.map_err(|message| anyhow::anyhow!("snapshot: {message}"))
            })
            .transpose()?;
        Ok(CommandOutcome { snapshot })
    }
}

fn function<T>(store: &mut Store<T>, instance: &Instance, name: &str) -> Result<Func> {
    instance
        .get_func(store, name)
        .with_context(|| format!("component does not export {name}"))
}

fn optional_function<T>(store: &mut Store<T>, instance: &Instance, name: &str) -> Option<Func> {
    instance.get_func(store, name)
}
