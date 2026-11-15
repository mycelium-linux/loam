//! The engine, the linker, and the capture entry points.
//!
//! Compiling a Spore takes tens of seconds, so a runtime is a reusable
//! engine/linker pair with an optional on-disk component cache; running one is
//! a fresh store and resource table, so two guests never share handles or
//! validation state.

use std::path::PathBuf;

use anyhow::{Context, Result};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};

use super::host::{HostSetup, HttpAccess, RecordsAccess, WebGpuHost};
use super::present::{CanvasFrame, CanvasGpuPresenter, DiscardPresenter, FrameCapture};
use super::soil as wit;
use super::{CatalogHost, Runner};
use crate::principal_host::PrincipalHost;
use soil_agent_host::AgentBackend;

/// A reusable engine/linker pair. Each invocation gets a fresh store and
/// resource table, so two guests never share handles or validation state.
pub struct WebGpuRuntime {
    engine: Engine,
    linker: Linker<WebGpuHost>,
    /// Where compiled components are cached. Compiling a Spore takes tens of
    /// seconds; deserializing one that has not changed takes milliseconds.
    component_cache: Option<std::path::PathBuf>,
    /// The default records grant, when the host was built around a records
    /// root. A caller can override it per run with `HostSetup::records`.
    records: Option<RecordsAccess>,
}

impl WebGpuRuntime {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        let engine = Engine::new(&config).map_err(anyhow::Error::from)?;
        let mut linker = Linker::new(&engine);
        // `wasi:io/poll` is the transport for component-model async, not an
        // authority grant. The context has no preopens, env, args, or network
        // policy; the guest only imports poll.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(anyhow::Error::from)
            .context("link wasi:io polling")?;
        wasi_webgpu_wasmtime::add_to_linker(&mut linker)
            .map_err(anyhow::Error::from)
            .context("link wasi:webgpu")?;
        wit::present::surface::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:present")?;
        wit::present::input::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:present input")?;
        wit::present::cursor::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:present cursor")?;
        wit::present::clipboard::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:present clipboard")?;
        // The capability interfaces are linked from the modules that own
        // their implementations, so the runner and the WASI profile cannot
        // drift into two policies for the same WIT.
        crate::http_host::api::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:http")?;
        wit::spore::catalog::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:spore catalog")?;
        wit::agents::observe::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:agent observe")?;
        wit::agents::control::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:agent control")?;
        wit::terminal_screen::screen::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:terminal-screen")?;
        crate::records_host::add_to_linker::<WebGpuHost>(&mut linker)
            .context("link soil:records")?;
        crate::vfs_host::vfs::add_to_linker::<WebGpuHost, HasSelf<WebGpuHost>>(
            &mut linker,
            |host| host,
        )
        .map_err(anyhow::Error::from)
        .context("link soil:vfs")?;
        crate::principal_host::add_to_linker::<WebGpuHost>(&mut linker)
            .context("link soil:principal")?;
        crate::search_host::add_to_linker::<WebGpuHost>(&mut linker).context("link soil:search")?;
        crate::unibus::add_to_linker::<WebGpuHost>(&mut linker).context("link soil:unibus")?;
        Ok(Self {
            engine,
            linker,
            component_cache: None,
            records: None,
        })
    }

    /// Build a runtime whose guests may read the vitals view from this root.
    pub fn with_records(mut self, root: impl Into<PathBuf>) -> Self {
        self.records = Some(RecordsAccess::vitals(root));
        self
    }

    /// Keep compiled components in `directory`, named by the module's digest.
    /// A Spore that has not changed starts from the cache instead of being
    /// compiled again.
    pub fn with_component_cache(mut self, directory: impl Into<std::path::PathBuf>) -> Self {
        self.component_cache = Some(directory.into());
        self
    }

    /// Compile a component into the cache without running it. A Spore someone
    /// will open later costs nothing to start if it was paid for earlier.
    pub fn prewarm(&self, bytes: &[u8]) -> Result<()> {
        self.component(bytes).map(|_| ())
    }

    /// Compile a component, or reuse the cached compilation of the same bytes.
    fn component(&self, bytes: &[u8]) -> Result<Component> {
        let cache_path = self.component_cache.as_ref().map(|directory| {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(bytes);
            let mut name = String::with_capacity(digest.len() * 2 + 8);
            for byte in digest {
                use std::fmt::Write as _;
                let _ = write!(name, "{byte:02x}");
            }
            directory.join(format!("{name}.cwasm"))
        });
        if let Some(path) = &cache_path
            && let Ok(serialized) = std::fs::read(path)
        {
            // The file is ours: it is named by the digest of the module it was
            // compiled from, so a mismatch means a stale cache, not a bad actor.
            match unsafe { Component::deserialize(&self.engine, &serialized) } {
                Ok(component) => return Ok(component),
                Err(error) => {
                    eprintln!("soil: cached component unusable ({error}); compiling");
                }
            }
        }
        let component = Component::new(&self.engine, bytes)
            .map_err(anyhow::Error::from)
            .context("compile WebGPU component")?;
        if let Some(path) = &cache_path {
            if let Some(directory) = path.parent() {
                let _ = std::fs::create_dir_all(directory);
            }
            match component.serialize() {
                Ok(serialized) => {
                    if let Err(error) = std::fs::write(path, serialized) {
                        eprintln!("soil: could not cache component: {error}");
                    }
                }
                Err(error) => {
                    eprintln!("soil: component cannot be serialized: {error}");
                }
            }
        }
        Ok(component)
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn linker(&self) -> &Linker<WebGpuHost> {
        &self.linker
    }

    /// Compile and run one guest component. The guest must export
    /// `run: async func() -> u32`; all other imports are denied by the linker.
    pub async fn run_component(&self, bytes: &[u8]) -> Result<u32> {
        self.run_component_with(
            bytes,
            HostSetup::capture(Box::new(DiscardPresenter::new(480, 640))),
        )
        .await
    }

    pub async fn run_component_with_presenter(
        &self,
        bytes: &[u8],
        presenter: Box<dyn CanvasGpuPresenter>,
    ) -> Result<u32> {
        self.run_component_with(bytes, HostSetup::capture(presenter))
            .await
    }

    /// Run one guest against a fully specified host.
    pub async fn run_component_with(&self, bytes: &[u8], mut setup: HostSetup) -> Result<u32> {
        if setup.records.is_none() {
            setup.records = self.records.clone();
        }
        let component = self.component(bytes)?;
        let mut store = Store::new(&self.engine, WebGpuHost::new(setup)?);
        let instance = Runner::instantiate_async(&mut store, &component, &self.linker)
            .await
            .map_err(anyhow::Error::from)
            .context("instantiate WebGPU component")?;
        instance
            .func_run()
            .call_async(&mut store, ())
            .await
            .map_err(anyhow::Error::from)
            .context("run WebGPU component")
            .map(|(status,)| status)
    }
}

/// Synchronous convenience wrapper for CLI/integration tests. The actual host
/// calls remain async internally so `wasi:webgpu` request/map futures work.
pub fn run_component_blocking(bytes: &[u8]) -> Result<u32> {
    let runtime = WebGpuRuntime::new()?;
    pollster::block_on(runtime.run_component(bytes))
}

/// Run with a capture sink and return both guest status and the first frame.
pub fn run_component_capture(
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<(u32, Option<CanvasFrame>)> {
    run_component_capture_with(bytes, width, height, None, None, None)
}

/// Capture one frame with a host that can answer `soil:spore/catalog` and
/// `soil:http` and `soil:agent`. A capture host is interactive=false, so the
/// guest renders one frame and exits: this is how a Spore's first screen is
/// inspected in CI.
pub fn run_component_capture_with(
    bytes: &[u8],
    width: u32,
    height: u32,
    catalog: Option<Box<dyn CatalogHost>>,
    http: Option<HttpAccess>,
    agents: Option<Box<dyn AgentBackend>>,
) -> Result<(u32, Option<CanvasFrame>)> {
    let runtime = WebGpuRuntime::new()?;
    let capture = FrameCapture::new(width, height);
    let mut setup = HostSetup::capture(Box::new(capture.clone()));
    if let Some(catalog) = catalog {
        setup.catalog = catalog;
    }
    if let Some(agents) = agents {
        setup.agents = agents;
    }
    setup.http = http;
    let status = pollster::block_on(runtime.run_component_with(bytes, setup))?;
    Ok((status, capture.take()))
}

/// What a capture run is allowed to reach.
///
/// Everything a Spore can be granted is named here, once, so a caller that
/// forgets a grant gets the denying default rather than a host that quietly
/// answers as somebody else.
#[derive(Default)]
pub struct Grants {
    /// One workspace the Spore may list and read through `soil:vfs`.
    pub vfs: Option<PathBuf>,
    /// The books the Spore may read or write through `soil:records`.
    pub records: Option<RecordsAccess>,
    /// Who the Spore is acting as, through `soil:principal`.
    pub principal: Option<PrincipalHost>,
    pub http: Option<HttpAccess>,
    pub catalog: Option<Box<dyn CatalogHost>>,
}

impl Grants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attest `actor` as a human principal. Local development only: a host
    /// that verifies a real identity builds its own provider.
    pub fn actor(self, actor: impl Into<String>) -> Self {
        let principal = PrincipalHost::new(crate::principal_host::StaticPrincipalProvider::new(
            crate::principal_host::Principal {
                actor: actor.into(),
                kind: crate::principal_host::PrincipalKind::Human,
                display_name: None,
                device: Some(gethostname()),
                teams: Vec::new(),
                roles: Vec::new(),
                generation: 1,
            },
        ));
        Self {
            principal: Some(principal),
            ..self
        }
    }
}

fn gethostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "localhost".into())
}

/// Capture one frame with whatever [`Grants`] the caller assembled.
pub fn run_component_capture_with_grants(
    bytes: &[u8],
    width: u32,
    height: u32,
    grants: Grants,
) -> Result<(u32, Option<CanvasFrame>)> {
    let runtime = WebGpuRuntime::new()?;
    let capture = FrameCapture::new(width, height);
    let mut setup = HostSetup::capture(Box::new(capture.clone()));
    setup.vfs = grants.vfs;
    setup.records = grants.records;
    setup.principal = grants.principal;
    setup.http = grants.http;
    if let Some(catalog) = grants.catalog {
        setup.catalog = catalog;
    }
    let status = pollster::block_on(runtime.run_component_with(bytes, setup))?;
    Ok((status, capture.take()))
}

/// Capture one frame with a read-only `vitals.view` records grant.
pub fn run_component_capture_with_records(
    bytes: &[u8],
    width: u32,
    height: u32,
    records_root: impl Into<PathBuf>,
) -> Result<(u32, Option<CanvasFrame>)> {
    let runtime = WebGpuRuntime::new()?.with_records(records_root);
    let capture = FrameCapture::new(width, height);
    let setup = HostSetup::capture(Box::new(capture.clone()));
    let status = pollster::block_on(runtime.run_component_with(bytes, setup))?;
    Ok((status, capture.take()))
}
