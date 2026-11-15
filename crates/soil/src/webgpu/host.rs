//! The host state one Spore run is given, and the WIT traits that read it.
//!
//! [`HostSetup`] is the grant list. Everything is deny-by-default: a presenter
//! that does not fill in `vfs` gets `denied`, not an empty workspace, and a
//! host that forgot an agent adapter draws nothing and says so. The plain
//! records spore hosts speak (`CatalogHost`, `TerminalHost`, `AgentBackend`)
//! are converted at this boundary, so an adapter never depends on the ABI.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wasi_webgpu_wasmtime::reexports::{wgpu_core, wgpu_types};
use wasi_webgpu_wasmtime::{WasiWebGpuCtx, WasiWebGpuCtxView, WasiWebGpuOptions};
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use soil_agent_host::{AgentBackend, Answer, Binding, NoSessions, Page};

use super::AgentSubscription;
use super::present::{ClipboardHost, CursorHost, CursorStyle, InputEvent, PointerButton};
use super::soil as wit;
use crate::dispatch::{Dispatch, DispatchRole};
use crate::http_host::api as http_api;
use crate::http_host::{
    self, CredentialProvider, DirectTransport, HttpPolicy, HttpTransport, NoCredentials,
};
use crate::principal_host::PrincipalHost;
use crate::records_host::{Namespace, NamespacedRecordsHost, records as records_api};
use crate::search_host::{SearchService, api as search_api, mention_api};
use crate::unibus::{Bus, MemoryBus, bus as bus_api};
use crate::vfs_host::{VfsHost, vfs as vfs_api};

/// One installed Spore, in the plain shape a presenter supplies. The WIT
/// records are built at the boundary; hosts do not depend on generated types.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    /// Display label; the host resolves the manifest's `title` or title-cases
    /// `name`, so every front door agrees on what an app is called.
    pub title: String,
    pub version: String,
    pub summary: String,
    pub workspace: Option<String>,
    pub default_surface: String,
    pub surfaces: Vec<CatalogSurface>,
    /// The package's `[presentation] mark`, as SVG source. A launcher draws it;
    /// a host with no renderer ignores it.
    pub mark: Option<String>,
}

/// How large a mark may be. A tile is 104 px; anything that needs more than this
/// is not an icon, and shipping it through the catalogue would be a bug.
pub const MARK_LIMIT: usize = 64 * 1024;

/// Read a package's mark, if it declares one and ships it.
pub fn read_mark(root: &std::path::Path, manifest: &soil_spore::Manifest) -> Option<String> {
    let path = manifest.presentation.as_ref()?.mark.as_deref()?;
    let svg = std::fs::read_to_string(root.join(path)).ok()?;
    (svg.len() <= MARK_LIMIT && svg.contains("<svg")).then_some(svg)
}

#[derive(Clone, Debug)]
pub struct CatalogSurface {
    pub name: String,
    pub kind: CatalogSurfaceKind,
    pub entry: String,
    pub requires: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogSurfaceKind {
    Canvas,
    Web,
}

#[derive(Clone, Debug)]
pub struct CatalogLaunched {
    pub id: String,
    pub surface: String,
    pub target: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogError {
    Denied,
    NotFound,
    Invalid,
    Unsupported,
    Io,
}

/// The host's installed-Spore catalogue. `launch` is host-owned: the guest
/// asks, the host starts the process or opens the window.
pub trait CatalogHost: Send {
    fn entries(&mut self) -> Result<Vec<CatalogEntry>, CatalogError>;
    fn launch(&mut self, id: &str, surface: &str) -> Result<CatalogLaunched, CatalogError>;
}

struct NoCatalog;

impl CatalogHost for NoCatalog {
    fn entries(&mut self) -> Result<Vec<CatalogEntry>, CatalogError> {
        Ok(Vec::new())
    }

    fn launch(&mut self, _id: &str, _surface: &str) -> Result<CatalogLaunched, CatalogError> {
        Err(CatalogError::NotFound)
    }
}

/// The host's installed-Spore catalogue, read from a directory tree.
///
/// The host decides where Spores live and this answers with what is there. A
/// tree whose manifest does not parse is skipped rather than fatal: one broken
/// Spore must not empty a launcher or a storefront. `launch` is deliberately
/// unfinished — starting a window is the host's business, and a capture host
/// has nowhere to put one.
pub struct InstallCatalog {
    /// Every directory a Spore can live in, most specific first. A host joins
    /// them rather than choosing one, because a launcher that shows only what
    /// shipped or only what was installed is a launcher that lies about the
    /// machine: the same host may have a bundled set, store installs, and a
    /// project tree all at once.
    roots: Vec<std::path::PathBuf>,
}

impl InstallCatalog {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            roots: vec![root.into()],
        }
    }

    /// Join several directories, most specific first: an id found in an earlier
    /// root wins, so an install shadows a bundled copy of the same Spore.
    pub fn with_roots(roots: Vec<std::path::PathBuf>) -> Self {
        Self { roots }
    }
}

impl CatalogHost for InstallCatalog {
    fn entries(&mut self) -> Result<Vec<CatalogEntry>, CatalogError> {
        let mut found = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for root in &self.roots {
            let Ok(read) = std::fs::read_dir(root) else {
                continue;
            };
            for entry in read.flatten() {
                let dir = entry.path();
                if !dir.join("spore.toml").is_file() {
                    continue;
                }
                let Ok(manifest) = soil_spore::Manifest::load(&dir) else {
                    continue;
                };
                if !seen.insert(manifest.spore.id.clone()) {
                    continue;
                }
                let surfaces = if manifest.surfaces.is_empty() {
                    manifest
                        .launch
                        .surfaces
                        .iter()
                        .filter_map(|kind| surface_kind(kind))
                        .map(|kind| CatalogSurface {
                            name: kind_name(kind).to_owned(),
                            kind,
                            entry: String::new(),
                            requires: None,
                        })
                        .collect()
                } else {
                    manifest
                        .surfaces
                        .iter()
                        .filter_map(|surface| {
                            Some(CatalogSurface {
                                name: surface.name.clone(),
                                kind: surface_kind(surface.kind.as_str())?,
                                entry: surface
                                    .entry(manifest.ui.as_ref())
                                    .unwrap_or_default()
                                    .to_owned(),
                                requires: surface.requires.clone(),
                            })
                        })
                        .collect()
                };
                found.push(CatalogEntry {
                    id: manifest.spore.id.clone(),
                    title: soil_spore::display_title(&manifest.spore.name),
                    name: manifest.spore.name.clone(),
                    version: manifest.spore.version.clone(),
                    summary: manifest.spore.summary.clone(),
                    workspace: manifest.launch.canvas_module.clone(),
                    default_surface: manifest.launch.default.clone(),
                    surfaces,
                    mark: read_mark(&dir, &manifest),
                });
            }
        }
        found.sort_by_key(|entry| entry.title.to_lowercase());
        Ok(found)
    }

    fn launch(&mut self, _id: &str, _surface: &str) -> Result<CatalogLaunched, CatalogError> {
        Err(CatalogError::Unsupported)
    }
}

fn surface_kind(value: &str) -> Option<CatalogSurfaceKind> {
    match value {
        "canvas" => Some(CatalogSurfaceKind::Canvas),
        "web" => Some(CatalogSurfaceKind::Web),
        _ => None,
    }
}

fn kind_name(kind: CatalogSurfaceKind) -> &'static str {
    match kind {
        CatalogSurfaceKind::Canvas => "canvas",
        CatalogSurfaceKind::Web => "web",
    }
}

/// One terminal cell, in the plain shape the host's emulator publishes.
#[derive(Clone, Debug, Default)]
pub struct TerminalCell {
    pub text: String,
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub faint: bool,
    pub inverse: bool,
    pub wide_tail: bool,
}

/// One shell the host is running. The Spore draws these as tabs.
#[derive(Clone, Debug)]
pub struct TerminalSession {
    pub id: u32,
    pub title: String,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct TerminalSnapshot {
    pub cols: u32,
    pub rows: u32,
    pub cells: Vec<TerminalCell>,
    pub cursor: Option<(u32, u32)>,
    pub title: String,
    pub generation: u64,
}

/// The host's live terminal. The Spore asks for a snapshot and sends keys;
/// it never owns a PTY, a shell, or an emulator.
pub trait TerminalHost: Send {
    /// One shell the host is running, as a tab strip needs it.
    fn sessions(&mut self) -> Result<Vec<TerminalSession>, String> {
        Ok(Vec::new())
    }

    /// Start a shell and make it the active one.
    fn open_session(&mut self, _command: Option<String>) -> Result<u32, String> {
        Err("this host cannot open sessions".into())
    }

    fn select_session(&mut self, _id: u32) -> Result<(), String> {
        Err("this host cannot select sessions".into())
    }

    fn close_session(&mut self, _id: u32) -> Result<(), String> {
        Err("this host cannot close sessions".into())
    }

    fn snapshot(&mut self) -> Result<TerminalSnapshot, String>;
    fn write(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&mut self, cols: u32, rows: u32) -> Result<(), String>;
    /// Move through scrollback. Negative is towards history.
    fn scroll(&mut self, delta: i32) -> Result<(), String>;

    /// Wait until the grid moves past `after` or `timeout_ms` passes, and answer
    /// the generation that is current then.
    fn wait(&mut self, after: u64, timeout_ms: u32) -> Result<u64, String>;
}

/// The denying terminal: a host with no PTY says so on the first call.
pub struct NoTerminal;

impl TerminalHost for NoTerminal {
    fn snapshot(&mut self) -> Result<TerminalSnapshot, String> {
        Err("no terminal session attached".into())
    }

    fn write(&mut self, _bytes: &[u8]) -> Result<(), String> {
        Err("no terminal session attached".into())
    }

    fn resize(&mut self, _cols: u32, _rows: u32) -> Result<(), String> {
        Err("no terminal session attached".into())
    }

    fn scroll(&mut self, _delta: i32) -> Result<(), String> {
        Err("no terminal session attached".into())
    }

    fn wait(&mut self, _after: u64, _timeout_ms: u32) -> Result<u64, String> {
        Err("no terminal session attached".into())
    }
}

/// Host state for one component instance.
pub struct WebGpuHost {
    instance: Arc<wgpu_core::global::Global>,
    table: ResourceTable,
    options: WasiWebGpuOptions,
    wasi: WasiCtx,
    presenter: Box<dyn super::present::CanvasGpuPresenter>,
    input: Arc<Mutex<VecDeque<InputEvent>>>,
    interactive: bool,
    catalog: Box<dyn CatalogHost>,
    /// The HTTP policy, credentials and transport a guest may use. `None`
    /// refuses every request instead of quietly reaching the network.
    http: Option<HttpAccess>,
    /// A namespaced records book the guest may read or write, when granted.
    records: Option<NamespacedRecordsHost>,
    /// One workspace the guest may list and read. `None` denies `soil:vfs`.
    vfs: Option<VfsHost>,
    /// Who the guest is acting as. `None` denies `soil:principal`.
    principal: Option<PrincipalHost>,
    /// The cast of agents this run can see. `NoAgents` refuses every read, so a
    /// host that forgot to attach an adapter fails loudly instead of drawing an
    /// empty wall.
    agents: Box<dyn AgentBackend>,
    terminal: Box<dyn TerminalHost>,
    cursor: Option<Box<dyn CursorHost>>,
    clipboard: Option<Box<dyn ClipboardHost>>,
    /// The Unibus this Spore publishes to and polls. In-process by default:
    /// the notice frames a wall emits are for surfaces on this machine, and a
    /// popup is one of them.
    bus: Arc<dyn Bus>,
    /// Host-composed search over the granted sources. `None` denies
    /// `soil:search` and `soil:search/mentions` alike.
    search: Option<SearchService>,
    /// What this surface was opened for, when the presenter named it. The
    /// roster's own binding is the fallback.
    binding: Option<Binding>,
    /// Where this host is in the session-notice stream, and what it does when
    /// one arrives. A notice makes an adapter re-list once instead of polling
    /// the harness on every tick; a task notice becomes a session.
    dispatch: Dispatch,
}

/// Where a local `soil-store serve` listens. A desktop shell that runs the
/// storefront has to allow this origin; a capture host that needs no network
/// leaves it out.
pub const LOCAL_STORE_ORIGIN: &str = "http://127.0.0.1:8800";

/// The HTTP capability one Spore run may use.
///
/// The policy, credentials and transport are host objects; the guest only
/// names a URL and an opaque credential handle. `soil-host` owns the single
/// implementation of what that request is allowed to do.
#[derive(Clone)]
pub struct HttpAccess {
    policy: HttpPolicy,
    credentials: Arc<dyn CredentialProvider>,
    transport: Arc<dyn HttpTransport>,
}

impl HttpAccess {
    pub fn new(policy: HttpPolicy) -> Self {
        Self {
            policy,
            credentials: Arc::new(NoCredentials),
            transport: Arc::new(DirectTransport),
        }
    }

    pub fn with_credentials(mut self, credentials: Arc<dyn CredentialProvider>) -> Self {
        self.credentials = credentials;
        self
    }

    pub fn with_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// A GET-only grant for one origin: what the storefront browser needs.
    pub fn store(origin: impl Into<String>) -> Result<Self> {
        let policy = HttpPolicy::new(
            [origin.into()],
            16 * 1024 * 1024,
            std::time::Duration::from_secs(30),
        )?;
        Ok(Self::new(policy))
    }

    /// Answer one `soil:http/fetch`. The field conversion and error mapping
    /// live in `http_host`, so this host and the WASI profile cannot disagree
    /// about what a request may do.
    fn fetch(
        &self,
        request: http_api::Request,
    ) -> std::result::Result<http_api::Response, http_api::Error> {
        http_host::fetch(&self.policy, &*self.credentials, &*self.transport, request)
    }
}

/// A records root plus the named books a Spore may see inside it.
#[derive(Clone, Debug)]
pub struct RecordsAccess {
    pub root: PathBuf,
    pub namespaces: Vec<Namespace>,
}

impl RecordsAccess {
    /// One named book collection a Spore may read and write.
    ///
    /// This is what a document-shaped Spore asks for: the guest names the
    /// collection, and the grant decides the book it maps onto.
    pub fn book(
        root: impl Into<PathBuf>,
        name: impl Into<String>,
        book: impl Into<String>,
        collection: impl Into<String>,
    ) -> Self {
        Self {
            root: root.into(),
            namespaces: vec![Namespace::write(
                name.into(),
                book.into(),
                collection.into(),
            )],
        }
    }

    /// The read-only namespace the vitals renderer uses.
    pub fn vitals(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            namespaces: vec![Namespace::read(
                "vitals.view",
                "dev/personal/telemetry",
                "vitals.view",
            )],
        }
    }
}

/// A search grant: the VFS root and records root it may compose, the record
/// books whose entries and graph links are visible, and any local git roots.
///
/// The service is host-owned; the guest names a query, never a source.
#[derive(Clone, Debug)]
pub struct SearchAccess {
    pub vfs: PathBuf,
    pub records: PathBuf,
    pub books: Vec<String>,
    pub git_roots: Vec<PathBuf>,
}

impl SearchAccess {
    pub fn open(vfs: impl Into<PathBuf>, records: impl Into<PathBuf>, books: Vec<String>) -> Self {
        Self {
            vfs: vfs.into(),
            records: records.into(),
            books,
            git_roots: Vec::new(),
        }
    }

    /// Grant explicit local Git roots. A Spore cannot discover or add one.
    pub fn with_git_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.git_roots = roots;
        self
    }
}

/// Everything one Spore run is allowed to talk to. A presenter fills in the
/// parts it owns; anything left as the denying default refuses the guest loudly
/// instead of quietly doing nothing.
pub struct HostSetup {
    pub presenter: Box<dyn super::present::CanvasGpuPresenter>,
    pub input: Arc<Mutex<VecDeque<InputEvent>>>,
    /// Tells the guest to keep polling after the first frame.
    pub interactive: bool,
    pub catalog: Box<dyn CatalogHost>,
    /// The HTTP grant. `None` denies every request: a capture host that does
    /// not need the network has none.
    pub http: Option<HttpAccess>,
    /// The records grant. `None` refuses every records call.
    pub records: Option<RecordsAccess>,
    /// The VFS grant: one workspace the guest may list and read. `None` denies
    /// `soil:vfs`, which is the honest answer for a Spore that only draws.
    pub vfs: Option<PathBuf>,
    /// The principal grant. `None` denies `soil:principal`: a host that cannot
    /// attest who is acting must not let a guest guess.
    pub principal: Option<PrincipalHost>,
    pub agents: Box<dyn AgentBackend>,
    /// What this surface was opened for. The intent belongs to the window, not
    /// to the roster: two windows over one adapter open on different views, and
    /// a popup on one agent while the wall shows the set.
    pub binding: Option<Binding>,
    /// What this host does with the bus. One surface in a process owns
    /// dispatch; the others watch, so a task read twice does not become two
    /// sessions. The default owns dispatch with no creator, so a host that
    /// cannot start sessions says so on the bus instead of dropping the task.
    pub dispatch: DispatchRole,
    /// Shorthand for a read-only `soil:http` grant for one origin, folded into
    /// `http` when that is unset. The storefront browser names its own origin
    /// this way.
    pub store_origin: Option<String>,
    pub terminal: Box<dyn TerminalHost>,
    /// `None` on hosts without a window; `soil:present/cursor` still succeeds.
    pub cursor: Option<Box<dyn CursorHost>>,
    /// `None` on hosts without a pasteboard.
    pub clipboard: Option<Box<dyn ClipboardHost>>,
    /// The Unibus this Spore publishes to and polls. In-process by default:
    /// the notice frames a wall emits are for surfaces on this machine, and a
    /// popup is one of them. Cross-process needs a hub, which is what
    /// [`crate::unibus::UnibusBus`] is for.
    pub bus: Arc<dyn Bus>,
    /// The search grant. `None` denies `soil:search`, which is the honest
    /// answer for a surface that only draws.
    pub search: Option<SearchAccess>,
}

/// The bus a surface publishes its notices to.
///
/// One per host process, shared by every surface in it: a wall announces a
/// prompt and a popup on the same process reads it. A caller that needs to
/// inspect or seed the log keeps the concrete [`MemoryBus`]; the host only
/// needs the trait.
pub fn in_process_bus() -> Arc<MemoryBus> {
    Arc::new(MemoryBus::default())
}

impl HostSetup {
    /// A one-shot capture host: one frame, no input, no catalog, no terminal.
    pub fn capture(presenter: Box<dyn super::present::CanvasGpuPresenter>) -> Self {
        Self {
            presenter,
            input: Arc::new(Mutex::new(VecDeque::new())),
            interactive: false,
            catalog: Box::new(NoCatalog),
            http: None,
            records: None,
            vfs: None,
            principal: None,
            agents: Box::new(soil_agent_host::NoAgents),
            binding: None,
            dispatch: DispatchRole::Own(Box::new(NoSessions)),
            store_origin: None,
            terminal: Box::new(NoTerminal),
            cursor: None,
            clipboard: None,
            bus: in_process_bus(),
            search: None,
        }
    }
}

impl WebGpuHost {
    pub fn new(setup: HostSetup) -> Result<Self> {
        let descriptor = wgpu_types::InstanceDescriptor {
            backends: wgpu_types::Backends::all(),
            flags: wgpu_types::InstanceFlags::from_build_config(),
            backend_options: Default::default(),
            memory_budget_thresholds: Default::default(),
            display: None,
        };
        let instance = wgpu_core::global::Global::new("webgpu", descriptor, None);
        // One HTTP implementation, two ways in: a full policy from the host, or
        // the single-origin shorthand the storefront browser uses.
        let http = match (setup.http, setup.store_origin.as_deref()) {
            (Some(http), _) => Some(http),
            (None, Some(origin)) => Some(HttpAccess::store(origin)?),
            (None, None) => None,
        };
        Ok(Self {
            instance: Arc::new(instance),
            table: ResourceTable::new(),
            options: WasiWebGpuOptions::default(),
            wasi: WasiCtxBuilder::new().build(),
            presenter: setup.presenter,
            input: setup.input,
            interactive: setup.interactive,
            catalog: setup.catalog,
            http,
            records: setup
                .records
                .map(|records| {
                    NamespacedRecordsHost::open(&records.root, records.namespaces)
                        .context("open records for the WebGPU guest")
                })
                .transpose()?,
            vfs: setup
                .vfs
                .map(|root| VfsHost::open(&root).context("open the workspace for soil:vfs"))
                .transpose()?,
            principal: setup.principal,
            agents: setup.agents,
            binding: setup.binding,
            dispatch: match setup.dispatch {
                DispatchRole::Own(sessions) => Dispatch::new(setup.bus.clone(), sessions),
                DispatchRole::Watch => Dispatch::watcher(setup.bus.clone()),
            },
            terminal: setup.terminal,
            cursor: setup.cursor,
            clipboard: setup.clipboard,
            bus: setup.bus,
            search: setup
                .search
                .map(|access| {
                    Ok::<_, anyhow::Error>(
                        SearchService::open(&access.vfs, &access.records, access.books)?
                            .with_git_roots(access.git_roots),
                    )
                })
                .transpose()?,
        })
    }
}

impl wit::present::input::Host for WebGpuHost {
    fn interactive(&mut self) -> bool {
        self.interactive
    }

    fn poll(&mut self, timeout_ms: u32) -> Option<wit::present::input::Event> {
        use wit::present::input;
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            if let Some(event) = self.input.lock().expect("input queue poisoned").pop_front() {
                return Some(match event {
                    InputEvent::Key(key) => input::Event::KeyInput(input::Key {
                        key: key.key,
                        text: key.text,
                        pressed: key.pressed,
                        shift: key.shift,
                        control: key.control,
                        alt: key.alt,
                        meta: key.meta,
                    }),
                    InputEvent::Moved { x, y } => input::Event::Moved(input::PointerMove { x, y }),
                    InputEvent::Button {
                        x,
                        y,
                        button,
                        pressed,
                    } => input::Event::Button(input::PointerPress {
                        x,
                        y,
                        button: match button {
                            PointerButton::Left => input::PointerButton::Left,
                            PointerButton::Right => input::PointerButton::Right,
                            PointerButton::Middle => input::PointerButton::Middle,
                        },
                        pressed,
                    }),
                    InputEvent::Scrolled { x, y, lines } => {
                        input::Event::Scrolled(input::PointerScroll { x, y, lines })
                    }
                });
            }
            if !self.interactive || timeout_ms == 0 || Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl WasiView for WebGpuHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wit::present::surface::Host for WebGpuHost {
    fn size(&mut self) -> (u32, u32) {
        self.presenter.size()
    }

    fn scale(&mut self) -> f64 {
        self.presenter.scale()
    }

    /// This host links `wasi:webgpu`, so it takes either delivery. A device
    /// host with no adapter would answer `[rgba]` and a GPU-only Spore would
    /// learn that from the one call, rather than by failing to create a device.
    fn delivery(&mut self) -> Vec<wit::present::types::DeliveryKind> {
        vec![
            wit::present::types::DeliveryKind::Texture,
            wit::present::types::DeliveryKind::Rgba,
        ]
    }

    /// The no-GPU arm: the Spore rasterized its own frame, so the host only
    /// validates it and hands it to the presenter. No device, no texture, no
    /// readback.
    fn present_rgba(
        &mut self,
        frame: wit::present::types::RgbaFrame,
    ) -> std::result::Result<(), String> {
        use super::present::CanvasFrame;
        let (width, height) = self.presenter.size();
        if frame.width != width || frame.height != height {
            return Err(format!(
                "frame is {}x{}, the granted surface is {width}x{height}",
                frame.width, frame.height
            ));
        }
        let expected = width as usize * height as usize * 4;
        if frame.pixels.len() != expected {
            return Err(format!(
                "frame is {} bytes, expected {expected}",
                frame.pixels.len()
            ));
        }
        if frame
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0xFF)
        {
            return Err("every pixel must be opaque".into());
        }
        self.presenter
            .present(CanvasFrame {
                width,
                height,
                rgba: frame.pixels,
            })
            .map_err(|error| error.to_string())
    }

    fn present(
        &mut self,
        texture: wasmtime::component::Resource<wasi_webgpu_wasmtime::Texture>,
    ) -> std::result::Result<(), String> {
        use super::present::CanvasFrame;
        let image = wasi_webgpu_wasmtime::read_texture_rgba(&self.instance, &self.table, texture)
            .map_err(|e| e.to_string())?;
        let frame = CanvasFrame {
            width: image.width,
            height: image.height,
            rgba: image.rgba,
        };
        self.presenter.present(frame).map_err(|e| e.to_string())
    }
}

impl vfs_api::Host for WebGpuHost {
    fn ls(
        &mut self,
        parent: String,
        offset: u64,
        limit: u32,
    ) -> std::result::Result<vfs_api::Page, vfs_api::Error> {
        self.vfs
            .as_mut()
            .ok_or(vfs_api::Error::Denied)?
            .ls(parent, offset, limit)
    }

    fn stat(&mut self, path: String) -> std::result::Result<vfs_api::Entry, vfs_api::Error> {
        self.vfs.as_mut().ok_or(vfs_api::Error::Denied)?.stat(path)
    }

    fn read(
        &mut self,
        path: String,
        offset: u64,
        length: u32,
    ) -> std::result::Result<vfs_api::Chunk, vfs_api::Error> {
        self.vfs
            .as_mut()
            .ok_or(vfs_api::Error::Denied)?
            .read(path, offset, length)
    }
}

impl crate::principal_host::api::Host for WebGpuHost {
    fn current(
        &mut self,
    ) -> std::result::Result<crate::principal_host::api::Identity, crate::principal_host::api::Error>
    {
        self.principal
            .as_mut()
            .ok_or(crate::principal_host::api::Error::Denied)?
            .current()
    }

    fn signing_key(
        &mut self,
    ) -> std::result::Result<
        Option<crate::principal_host::api::SigningKeyInfo>,
        crate::principal_host::api::Error,
    > {
        self.principal
            .as_mut()
            .ok_or(crate::principal_host::api::Error::Denied)?
            .signing_key()
    }
}

impl records_api::Host for WebGpuHost {
    fn get(
        &mut self,
        key: records_api::Key,
    ) -> std::result::Result<records_api::Entry, records_api::Error> {
        self.records
            .as_mut()
            .ok_or(records_api::Error::Unsupported)?
            .get(key)
    }

    fn scan(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<records_api::Page, records_api::Error> {
        self.records
            .as_mut()
            .ok_or(records_api::Error::Unsupported)?
            .scan(collection, cursor, limit)
    }

    fn upsert(
        &mut self,
        entry: records_api::Entry,
    ) -> std::result::Result<records_api::Entry, records_api::Error> {
        self.records
            .as_mut()
            .ok_or(records_api::Error::Unsupported)?
            .upsert(entry)
    }

    fn compare_and_swap(
        &mut self,
        key: records_api::Key,
        expected: Option<records_api::Version>,
        entry: records_api::Entry,
    ) -> std::result::Result<records_api::CasResult, records_api::Error> {
        self.records
            .as_mut()
            .ok_or(records_api::Error::Unsupported)?
            .compare_and_swap(key, expected, entry)
    }

    fn delete(&mut self, key: records_api::Key) -> std::result::Result<(), records_api::Error> {
        self.records
            .as_mut()
            .ok_or(records_api::Error::Unsupported)?
            .delete(key)
    }

    fn changes(
        &mut self,
        since: Option<String>,
        limit: u32,
    ) -> std::result::Result<records_api::Page, records_api::Error> {
        self.records
            .as_mut()
            .ok_or(records_api::Error::Unsupported)?
            .changes(since, limit)
    }

    fn apply(
        &mut self,
        entries: Vec<records_api::Entry>,
    ) -> std::result::Result<records_api::ApplyResult, records_api::Error> {
        self.records
            .as_mut()
            .ok_or(records_api::Error::Unsupported)?
            .apply(entries)
    }
}

impl http_api::Host for WebGpuHost {
    /// The guest adapter over the one `soil:http` policy implementation.
    ///
    /// The guest names a URL and an opaque credential handle; the policy owns
    /// the origin allowlist, method/header/body checks and the transport. A
    /// host with no HTTP grant returns `unsupported`, not a silent empty
    /// response.
    fn fetch(
        &mut self,
        request: http_api::Request,
    ) -> std::result::Result<http_api::Response, http_api::Error> {
        self.http
            .as_ref()
            .ok_or(http_api::Error::Unsupported)?
            .fetch(request)
    }
}

impl bus_api::Host for WebGpuHost {
    fn poll(
        &mut self,
        kinds: Vec<String>,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<bus_api::Page, bus_api::Error> {
        let page = self
            .bus
            .poll(&kinds, cursor.as_deref(), limit)
            .map_err(bus_api::Error::from)?;
        Ok(page.into())
    }

    fn publish(
        &mut self,
        kind: String,
        body: Vec<u8>,
        destination: Option<String>,
        ttl_ms: u64,
    ) -> std::result::Result<bus_api::Receipt, bus_api::Error> {
        let receipt = self
            .bus
            .publish(&kind, &body, destination.as_deref(), ttl_ms)
            .map_err(bus_api::Error::from)?;
        // A publish is a fact about the surface, and the only way to see one
        // from a capture host is the log.
        eprintln!("unibus: published {kind} ({} bytes)", body.len());
        Ok(bus_api::Receipt {
            request_id: receipt.request_id,
        })
    }
}

impl search_api::Host for WebGpuHost {
    fn search(
        &mut self,
        query: search_api::Query,
    ) -> std::result::Result<search_api::Page, search_api::Error> {
        let service = self.search.as_mut().ok_or(search_api::Error::Denied)?;
        search_api::Host::search(service, query)
    }

    fn state(&mut self) -> std::result::Result<search_api::SearchState, search_api::Error> {
        let service = self.search.as_mut().ok_or(search_api::Error::Denied)?;
        search_api::Host::state(service)
    }

    fn refresh(&mut self) -> std::result::Result<search_api::SearchState, search_api::Error> {
        let service = self.search.as_mut().ok_or(search_api::Error::Denied)?;
        search_api::Host::refresh(service)
    }
}

impl mention_api::Host for WebGpuHost {
    fn suggest(
        &mut self,
        text: String,
        kinds: Vec<String>,
        limit: u32,
    ) -> std::result::Result<Vec<mention_api::Candidate>, search_api::Error> {
        let service = self.search.as_mut().ok_or(search_api::Error::Denied)?;
        mention_api::Host::suggest(service, text, kinds, limit)
    }
}

impl wit::spore::catalog::Host for WebGpuHost {
    fn entries(
        &mut self,
    ) -> std::result::Result<Vec<wit::spore::catalog::Entry>, wit::spore::catalog::Error> {
        let entries = self.catalog.entries().map_err(catalog_error)?;
        Ok(entries
            .into_iter()
            .map(|entry| wit::spore::catalog::Entry {
                id: entry.id,
                name: entry.name,
                title: entry.title,
                version: entry.version,
                summary: entry.summary,
                workspace: entry.workspace,
                default_surface: entry.default_surface,
                mark: entry.mark,
                surfaces: entry
                    .surfaces
                    .into_iter()
                    .map(|surface| wit::spore::catalog::Surface {
                        name: surface.name,
                        kind: match surface.kind {
                            CatalogSurfaceKind::Canvas => wit::spore::catalog::SurfaceKind::Canvas,
                            CatalogSurfaceKind::Web => wit::spore::catalog::SurfaceKind::Web,
                        },
                        entry: surface.entry,
                        requires: surface.requires,
                    })
                    .collect(),
            })
            .collect())
    }

    fn launch(
        &mut self,
        id: String,
        surface: String,
    ) -> std::result::Result<wit::spore::catalog::Launched, wit::spore::catalog::Error> {
        let launched = self.catalog.launch(&id, &surface).map_err(catalog_error)?;
        Ok(wit::spore::catalog::Launched {
            id: launched.id,
            surface: launched.surface,
            target: launched.target,
        })
    }
}

fn catalog_error(error: CatalogError) -> wit::spore::catalog::Error {
    use wit::spore::catalog::Error;
    match error {
        CatalogError::Denied => Error::Denied,
        CatalogError::NotFound => Error::NotFound,
        CatalogError::Invalid => Error::Invalid,
        CatalogError::Unsupported => Error::Unsupported,
        CatalogError::Io => Error::Io,
    }
}

fn agent_error(error: soil_agent_host::Error) -> wit::agents::types::Error {
    use wit::agents::types::Error;
    match error {
        soil_agent_host::Error::Denied => Error::Denied,
        soil_agent_host::Error::NotFound => Error::NotFound,
        soil_agent_host::Error::NotReady => Error::NotReady,
        soil_agent_host::Error::Invalid => Error::Invalid,
        soil_agent_host::Error::OverBudget => Error::OverBudget,
        soil_agent_host::Error::Stale => Error::Stale,
        soil_agent_host::Error::Unsupported => Error::Unsupported,
        soil_agent_host::Error::Io => Error::Io,
    }
}

fn agent_phase(phase: soil_agent_host::Phase) -> wit::agents::types::Phase {
    use wit::agents::types::Phase;
    match phase {
        soil_agent_host::Phase::Connecting => Phase::Connecting,
        soil_agent_host::Phase::Idle => Phase::Idle,
        soil_agent_host::Phase::Thinking => Phase::Thinking,
        soil_agent_host::Phase::Working => Phase::Working,
        soil_agent_host::Phase::NeedsYou => Phase::NeedsYou,
        soil_agent_host::Phase::Finished => Phase::Finished,
        soil_agent_host::Phase::Failed => Phase::Failed,
        soil_agent_host::Phase::Paused => Phase::Paused,
    }
}

fn agent_activity_kind(kind: soil_agent_host::ActivityKind) -> wit::agents::types::ActivityKind {
    use wit::agents::types::ActivityKind;
    match kind {
        soil_agent_host::ActivityKind::You => ActivityKind::You,
        soil_agent_host::ActivityKind::Model => ActivityKind::Model,
        soil_agent_host::ActivityKind::Memory => ActivityKind::Memory,
        soil_agent_host::ActivityKind::Files => ActivityKind::Files,
        soil_agent_host::ActivityKind::Shell => ActivityKind::Shell,
        soil_agent_host::ActivityKind::Network => ActivityKind::Network,
        soil_agent_host::ActivityKind::Browser => ActivityKind::Browser,
        soil_agent_host::ActivityKind::Agent => ActivityKind::Agent,
        soil_agent_host::ActivityKind::Skill => ActivityKind::Skill,
        soil_agent_host::ActivityKind::Hook => ActivityKind::Hook,
        soil_agent_host::ActivityKind::Other => ActivityKind::Other,
    }
}

fn agent_status(status: soil_agent_host::Status) -> wit::agents::types::Status {
    use wit::agents::types::Status;
    match status {
        soil_agent_host::Status::Running => Status::Running,
        soil_agent_host::Status::Ok => Status::Ok,
        soil_agent_host::Status::Failed => Status::Failed,
        soil_agent_host::Status::Blocked => Status::Blocked,
    }
}

fn agent_prompt_kind(kind: soil_agent_host::PromptKind) -> wit::agents::types::PromptKind {
    use wit::agents::types::PromptKind;
    match kind {
        soil_agent_host::PromptKind::Question => PromptKind::Question,
        soil_agent_host::PromptKind::Approval => PromptKind::Approval,
        soil_agent_host::PromptKind::Choice => PromptKind::Choice,
    }
}

fn agent_role(role: soil_agent_host::Role) -> wit::agents::types::Role {
    use wit::agents::types::Role;
    match role {
        soil_agent_host::Role::User => Role::User,
        soil_agent_host::Role::Assistant => Role::Assistant,
        soil_agent_host::Role::Tool => Role::Tool,
    }
}

fn agent_owner(owner: soil_agent_host::Owner) -> wit::agents::types::Owner {
    wit::agents::types::Owner {
        actor: owner.actor,
        display_name: owner.display_name,
    }
}

fn agent_scope(scope: soil_agent_host::Scope) -> wit::agents::types::Scope {
    use wit::agents::types::ScopeKind;
    wit::agents::types::Scope {
        kind: match scope.kind {
            soil_agent_host::ScopeKind::Device => ScopeKind::Device,
            soil_agent_host::ScopeKind::Desktop => ScopeKind::Desktop,
            soil_agent_host::ScopeKind::Principal => ScopeKind::Principal,
        },
        id: scope.id,
        label: scope.label,
        owner: scope.owner.map(agent_owner),
    }
}

fn agent_record(agent: soil_agent_host::Agent) -> wit::agents::types::Agent {
    wit::agents::types::Agent {
        id: agent.id,
        harness: agent.harness,
        title: agent.title,
        workspace: agent.workspace,
        model: agent.model,
        phase: agent_phase(agent.phase),
        goal: agent.goal.map(|goal| wit::agents::types::Goal {
            text: goal.text,
            set_at_ms: goal.set_at_ms,
        }),
        owner: agent.owner.map(agent_owner),
        activity: agent
            .activity
            .into_iter()
            .map(|row| wit::agents::types::Activity {
                kind: agent_activity_kind(row.kind),
                label: row.label,
                detail: row.detail,
                status: agent_status(row.status),
                started_at_ms: row.started_at_ms,
                finished_at_ms: row.finished_at_ms,
            })
            .collect(),
        prompts: agent
            .prompts
            .into_iter()
            .map(|prompt| wit::agents::types::Prompt {
                id: prompt.id,
                kind: agent_prompt_kind(prompt.kind),
                title: prompt.title,
                body: prompt.body,
                details: prompt.details,
                origin: prompt.origin,
                options: prompt
                    .choices
                    .into_iter()
                    .map(|choice| wit::agents::types::Choice {
                        id: choice.id,
                        label: choice.label,
                        detail: choice.detail,
                        danger: choice.danger,
                    })
                    .collect(),
                allows_text: prompt.allows_text,
                asked_at_ms: prompt.asked_at_ms,
                deadline_at_ms: prompt.deadline_at_ms,
            })
            .collect(),
        usage: wit::agents::types::Usage {
            context_tokens: agent.usage.context_tokens,
            context_window: agent.usage.context_window,
            input_tokens: agent.usage.input_tokens,
            output_tokens: agent.usage.output_tokens,
            turns: agent.usage.turns,
        },
        revision: agent.revision,
        started_at_ms: agent.started_at_ms,
        updated_at_ms: agent.updated_at_ms,
    }
}

fn agent_binding(binding: Binding) -> wit::agents::types::Binding {
    wit::agents::types::Binding {
        agent: binding.agent,
        view: binding.view,
    }
}

fn agent_page(page: Page) -> wit::agents::observe::Page {
    wit::agents::observe::Page {
        turns: page
            .turns
            .into_iter()
            .map(|turn| wit::agents::types::Turn {
                role: agent_role(turn.role),
                text: turn.text,
                at_ms: turn.at_ms,
            })
            .collect(),
        before: page.before,
    }
}

impl wit::agents::observe::Host for WebGpuHost {
    fn agents(
        &mut self,
    ) -> std::result::Result<Vec<wit::agents::types::Agent>, wit::agents::types::Error> {
        self.dispatch.pump(&mut *self.agents);
        Ok(self.agents.list().into_iter().map(agent_record).collect())
    }

    fn get(
        &mut self,
        id: String,
    ) -> std::result::Result<wit::agents::types::Agent, wit::agents::types::Error> {
        self.dispatch.pump(&mut *self.agents);
        self.agents
            .get(&id)
            .map(agent_record)
            .ok_or(wit::agents::types::Error::NotFound)
    }

    fn intent(&mut self) -> wit::agents::types::Binding {
        // The surface's own intent wins: a shared roster cannot know which
        // window is asking, and a popup opened on one agent must say so.
        match &self.binding {
            Some(binding) => agent_binding(binding.clone()),
            None => agent_binding(self.agents.binding()),
        }
    }

    fn current_scope(&mut self) -> wit::agents::types::Scope {
        agent_scope(self.agents.scope())
    }

    fn transcript(
        &mut self,
        id: String,
        before: Option<u64>,
        limit: u32,
    ) -> std::result::Result<wit::agents::observe::Page, wit::agents::types::Error> {
        self.dispatch.pump(&mut *self.agents);
        self.agents
            .transcript(&id, before, limit)
            .map(agent_page)
            .map_err(agent_error)
    }

    fn subscribe(
        &mut self,
    ) -> std::result::Result<
        wasmtime::component::Resource<AgentSubscription>,
        wit::agents::types::Error,
    > {
        self.table
            .push(AgentSubscription)
            .map_err(|_| wit::agents::types::Error::Io)
    }
}

impl wit::agents::observe::HostSubscription for WebGpuHost {
    fn next(
        &mut self,
        subscription: wasmtime::component::Resource<AgentSubscription>,
        timeout_ms: u32,
    ) -> std::result::Result<Option<wit::agents::observe::Change>, wit::agents::types::Error> {
        self.dispatch.pump(&mut *self.agents);
        // A handle the guest already dropped is `stale`, not an empty poll.
        self.table
            .get(&subscription)
            .map_err(|_| wit::agents::types::Error::Stale)?;
        Ok(self.agents.next(timeout_ms).map(|change| match change {
            soil_agent_host::Change::Upserted(agent) => {
                wit::agents::observe::Change::Upserted(agent_record(*agent))
            }
            soil_agent_host::Change::Removed(id) => wit::agents::observe::Change::Removed(id),
        }))
    }

    fn drop(
        &mut self,
        subscription: wasmtime::component::Resource<AgentSubscription>,
    ) -> std::result::Result<(), wasmtime::Error> {
        let _ = self.table.delete(subscription);
        Ok(())
    }
}

impl wit::agents::control::Host for WebGpuHost {
    fn respond(
        &mut self,
        id: String,
        prompt_id: String,
        answer: wit::agents::types::Response,
    ) -> std::result::Result<(), wit::agents::types::Error> {
        self.agents
            .answer(
                &id,
                &prompt_id,
                Answer {
                    choice: answer.choice,
                    text: answer.text,
                },
            )
            .map_err(agent_error)
    }

    fn send(
        &mut self,
        id: String,
        text: String,
    ) -> std::result::Result<(), wit::agents::types::Error> {
        self.agents.send(&id, &text).map_err(agent_error)
    }

    fn interrupt(&mut self, id: String) -> std::result::Result<(), wit::agents::types::Error> {
        self.agents.interrupt(&id).map_err(agent_error)
    }
}

impl wit::terminal_screen::screen::Host for WebGpuHost {
    fn sessions(
        &mut self,
    ) -> std::result::Result<Vec<wit::terminal_screen::screen::Session>, String> {
        Ok(self
            .terminal
            .sessions()?
            .into_iter()
            .map(|session| wit::terminal_screen::screen::Session {
                id: session.id,
                title: session.title,
                active: session.active,
            })
            .collect())
    }

    fn open_session(&mut self, command: Option<String>) -> std::result::Result<u32, String> {
        self.terminal.open_session(command)
    }

    fn select_session(&mut self, id: u32) -> std::result::Result<(), String> {
        self.terminal.select_session(id)
    }

    fn close_session(&mut self, id: u32) -> std::result::Result<(), String> {
        self.terminal.close_session(id)
    }

    fn snapshot(&mut self) -> std::result::Result<wit::terminal_screen::screen::Grid, String> {
        let snapshot = self.terminal.snapshot()?;
        Ok(wit::terminal_screen::screen::Grid {
            cols: snapshot.cols,
            rows: snapshot.rows,
            cells: snapshot
                .cells
                .into_iter()
                .map(|cell| wit::terminal_screen::screen::Cell {
                    text: cell.text,
                    fg: cell.fg,
                    bg: cell.bg,
                    bold: cell.bold,
                    italic: cell.italic,
                    underline: cell.underline,
                    faint: cell.faint,
                    inverse: cell.inverse,
                    wide_tail: cell.wide_tail,
                })
                .collect(),
            cursor_row: snapshot.cursor.map(|cursor| cursor.0),
            cursor_col: snapshot.cursor.map(|cursor| cursor.1),
            title: snapshot.title,
            generation: snapshot.generation,
        })
    }

    fn write(&mut self, bytes: Vec<u8>) -> std::result::Result<(), String> {
        self.terminal.write(&bytes)
    }

    fn scroll(&mut self, delta: i32) -> std::result::Result<(), String> {
        self.terminal.scroll(delta)
    }

    fn wait(&mut self, after: u64, timeout_ms: u32) -> std::result::Result<u64, String> {
        self.terminal.wait(after, timeout_ms)
    }

    fn resize(&mut self, cols: u32, rows: u32) -> std::result::Result<(), String> {
        self.terminal.resize(cols, rows)
    }
}

impl wit::present::clipboard::Host for WebGpuHost {
    fn set_text(&mut self, text: String) -> std::result::Result<(), String> {
        match self.clipboard.as_mut() {
            Some(clipboard) => clipboard.set_text(text),
            None => Err("this host has no clipboard".to_owned()),
        }
    }
}

impl wit::present::cursor::Host for WebGpuHost {
    fn set(&mut self, style: wit::present::cursor::CursorStyle) -> std::result::Result<(), String> {
        let style = match style {
            wit::present::cursor::CursorStyle::Default => CursorStyle::Default,
            wit::present::cursor::CursorStyle::Text => CursorStyle::Text,
            wit::present::cursor::CursorStyle::Pointer => CursorStyle::Pointer,
            wit::present::cursor::CursorStyle::Crosshair => CursorStyle::Crosshair,
            wit::present::cursor::CursorStyle::Move => CursorStyle::Move,
            wit::present::cursor::CursorStyle::Grab => CursorStyle::Grab,
            wit::present::cursor::CursorStyle::Grabbing => CursorStyle::Grabbing,
            wit::present::cursor::CursorStyle::NoDrop => CursorStyle::NoDrop,
            wit::present::cursor::CursorStyle::Wait => CursorStyle::Wait,
            wit::present::cursor::CursorStyle::Help => CursorStyle::Help,
        };
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.set_cursor(style);
        }
        Ok(())
    }
}

impl WasiWebGpuCtxView for WebGpuHost {
    fn webgpu_ctx(&mut self) -> WasiWebGpuCtx<'_> {
        WasiWebGpuCtx {
            instance: &self.instance,
            table: &mut self.table,
            options: &self.options,
        }
    }
}
