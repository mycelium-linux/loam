//! Bounded `wasi:webgpu` host for Soil.
//!
//! This is intentionally a separate crate from `soil-runtime`: the Canvas
//! runtime is a core-Wasm host, while WebGPU is a component-model capability.
//! A Spore that imports `wasi:webgpu` gets exactly this linker and the
//! capabilities a presenter grants it. Adapter/device creation, validation,
//! limits and queue submission stay in the host through `wgpu-core`; no native
//! handle crosses the boundary.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ::soil::http_host::{
    self, CredentialProvider, DirectTransport, HttpPolicy, HttpTransport, NoCredentials,
    Request as HttpRequest,
};
use ::soil::desktops::DesktopsHost;
use ::soil::git_host::GitHost;
use ::soil::principal_host::PrincipalHost;
use ::soil::records_host::{self, Namespace, NamespacedRecordsHost};
use ::soil::unibus::{Bus, MemoryBus};
use ::soil::vfs_host::VfsHost;
use anyhow::{Context, Result};
use wasi_webgpu_wasmtime::reexports::{wgpu_core, wgpu_types};
use wasi_webgpu_wasmtime::{WasiWebGpuCtx, WasiWebGpuCtxView, WasiWebGpuOptions};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::soil::present::input;
use soil_agent_host::{AgentBackend, Answer, Binding, Page};
use soil_appdata_host::{AppDataHost, AppDataStore, appdata as appdata_host};
use ::soil::dispatch::{Dispatch, DispatchRole};

pub mod fb;

/// The private app store one Spore run may use.
///
/// The root is the host's; the app id is the one its manifest claims. Neither
/// is reachable from the guest, which only ever names `local` or `synced`.
#[derive(Clone, Debug)]
pub struct AppDataGrant {
    pub root: PathBuf,
    pub app_id: String,
}

/// The denying agent backend, for presenters that own no agent session.
pub use soil_agent_host::NoAgents;

wasmtime::component::bindgen!({
    path: "wit",
    world: "runner",
    exports: {
        "run": async,
    },
    with: {
        "wasi:webgpu/webgpu@0.3.0-rc.2.gpu-texture": wasi_webgpu_wasmtime::Texture,
        // Same WIT package, one binding: the runner answers with the host
        // crate's own `soil:vfs` and `soil:principal` types rather than a
        // second generated copy that would need converting field by field.
        "soil:vfs/vfs@1.0.0": ::soil::vfs_host::vfs,
        // `vfs-cid` answers "where do these bytes live" from the same index `vfs`
        // lists, so it is the same host object, not a second one that could
        // disagree with it.
        "soil:vfs-cid/cid@0.1.0": ::soil::vfs_host::cid,
        "soil:principal/principal@0.1.0": ::soil::principal_host::api,
        "soil:git/git@0.1.0": ::soil::git_host::git,
        "soil:vfs-mut/vfs-mut@0.1.0": ::soil::vfs_host::vfs_mut,
        "soil:desktops/desktops@0.1.0": ::soil::desktops::desktops,
        // A subscription is host state, not a generated shadow type: the
        // backend keeps the position, so two subscriptions cannot disagree.
        "soil:agents/observe.subscription": AgentSubscription,
    },
    require_store_data_send: true,
});

/// One composed Canvas frame, tightly packed top-to-bottom RGBA8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Host-side destination for a guest `soil:present` frame.
///
/// Canvas owns the actual window and final composition; this trait only lets
/// the WebGPU host hand over a completed frame. It is never called from inside
/// a guest callback.
pub trait CanvasGpuPresenter: Send {
    fn size(&self) -> (u32, u32);

    /// Device pixels per layout pixel. A capture host renders at 1:1.
    fn scale(&self) -> f64 {
        1.0
    }

    fn present(&mut self, frame: CanvasFrame) -> Result<()>;
}

struct DiscardPresenter {
    size: (u32, u32),
}

impl CanvasGpuPresenter for DiscardPresenter {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn present(&mut self, _frame: CanvasFrame) -> Result<()> {
        Ok(())
    }
}

/// A presenter that retains exactly one frame for tests and CLI output.
#[derive(Clone, Default)]
pub struct FrameCapture {
    frame: Arc<Mutex<Option<CanvasFrame>>>,
    size: (u32, u32),
    /// How many frames this run may present. A still capture takes one and
    /// fails if the guest offers more, which is how a guest that ignores the
    /// one-frame contract gets caught. A capture driven by keys wants the frame
    /// the run *ends* on, so it keeps the last one it is given.
    budget: usize,
}

impl FrameCapture {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            frame: Arc::new(Mutex::new(None)),
            size: (width, height),
            budget: 1,
        }
    }

    /// A capture that keeps the last frame instead of the only frame.
    pub fn streaming(width: u32, height: u32) -> Self {
        Self {
            frame: Arc::new(Mutex::new(None)),
            size: (width, height),
            budget: usize::MAX,
        }
    }

    pub fn take(&self) -> Option<CanvasFrame> {
        self.frame.lock().expect("frame capture poisoned").take()
    }
}

impl CanvasGpuPresenter for FrameCapture {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn present(&mut self, frame: CanvasFrame) -> Result<()> {
        anyhow::ensure!(self.budget > 0, "present budget");
        self.budget = self.budget.saturating_sub(1);
        let mut slot = self.frame.lock().expect("frame capture poisoned");
        *slot = Some(frame);
        Ok(())
    }
}

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

/// A catalogue call that did not happen, in the two pieces a window shows: what
/// kind of failure it was, and the host's sentence about it.
///
/// The enum is what a program branches on; the sentence is what a person reads.
/// A host is the only party that knows which import it does not link or which
/// file a package claims but does not carry, and none of that survives an enum,
/// so every launch and stop carries both.
#[derive(Clone, Debug)]
pub struct CatalogFailure {
    pub code: CatalogError,
    pub detail: String,
}

impl CatalogFailure {
    pub fn new(code: CatalogError, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl From<CatalogError> for CatalogFailure {
    fn from(code: CatalogError) -> Self {
        Self {
            code,
            detail: String::new(),
        }
    }
}

/// What installing a listing produced.
#[derive(Clone, Debug)]
pub struct CatalogInstalled {
    pub id: String,
    pub version: String,
    pub digest: String,
}

/// The host's installed-Spore catalogue. `launch` is host-owned: the guest
/// asks, the host starts the process or opens the window. `install` is the same
/// division of labour: the window names a listing, the host fetches and writes.
pub trait CatalogHost: Send {
    fn entries(&mut self) -> Result<Vec<CatalogEntry>, CatalogError>;
    fn launch(&mut self, id: &str, surface: &str) -> Result<CatalogLaunched, CatalogFailure>;
    /// Install a listing this host can reach. A host with no store says so.
    fn install(&mut self, id: &str) -> Result<CatalogInstalled, CatalogFailure> {
        Err(CatalogFailure::new(
            CatalogError::Denied,
            format!("{id} cannot be installed here: this host has no store"),
        ))
    }
    /// The ids this host started and has not seen exit.
    ///
    /// The host is what starts and reaps a Spore, so it is the only thing that
    /// knows this. A default keeps a catalogue that launches nothing honest
    /// instead of pretending every tile is running.
    fn running(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// End a Spore this host started. `not-found` when it is not running one.
    fn stop(&mut self, id: &str) -> Result<(), CatalogFailure> {
        Err(CatalogFailure::new(
            CatalogError::NotFound,
            format!("{id} is not running"),
        ))
    }
}

struct NoCatalog;

impl CatalogHost for NoCatalog {
    fn entries(&mut self) -> Result<Vec<CatalogEntry>, CatalogError> {
        Ok(Vec::new())
    }

    fn launch(&mut self, id: &str, _surface: &str) -> Result<CatalogLaunched, CatalogFailure> {
        Err(CatalogFailure::new(
            CatalogError::NotFound,
            format!("{id} is not installed"),
        ))
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

    fn launch(&mut self, id: &str, _surface: &str) -> Result<CatalogLaunched, CatalogFailure> {
        Err(CatalogFailure::new(
            CatalogError::Unsupported,
            format!("this host presents frames, so it cannot open {id} in a window"),
        ))
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

struct NoTerminal;

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

/// The pointer shape a Spore asks for. A presenter maps it to a native cursor;
/// a headless host drops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStyle {
    Default,
    Text,
    Pointer,
    Crosshair,
    Move,
    Grab,
    Grabbing,
    NoDrop,
    Wait,
    Help,
}

/// The system clipboard, when the presenter has one. A host without a pasteboard
/// leaves it unset and `soil:present/clipboard` refuses loudly.
pub trait ClipboardHost: Send {
    fn set_text(&mut self, text: String) -> Result<(), String>;
}

/// The window's pointer shape. Presenters that own a window implement it;
/// a capture or headless host leaves it unset and `set` succeeds doing nothing.
pub trait CursorHost: Send {
    fn set_cursor(&mut self, style: CursorStyle);
}

/// One normalized key waiting for the guest. The WIT type is built at the
/// boundary; presenters only need this plain shape.
#[derive(Clone, Debug)]
pub struct InputKey {
    pub key: String,
    pub text: Option<String>,
    pub pressed: bool,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Presenter-independent pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

/// One event waiting for the guest, in the order the presenter produced it.
/// Keys and pointer actions share a queue, so a click cannot overtake a
/// keystroke that arrived before it.
#[derive(Clone, Debug)]
pub enum InputEvent {
    Key(InputKey),
    /// Surface-space position: the units `soil:present/surface.size` reports.
    Moved {
        x: f32,
        y: f32,
    },
    Button {
        x: f32,
        y: f32,
        button: PointerButton,
        pressed: bool,
    },
    /// Positive `lines` scrolls towards earlier content.
    Scrolled {
        x: f32,
        y: f32,
        lines: f32,
    },
}

/// Host state for one component instance.
/// The fastest the host takes frames: the panel's own 60 Hz. A guest cannot
/// talk its way past it, because the wait happens on the host's side of the
/// call.
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// The queue a presenter fills and the host drains: `soil:present/input`.
///
/// One queue per host, shared with whatever produces events — a window, an
/// evdev reader, or a script's `--keys`.
pub type InputQueue = Arc<Mutex<VecDeque<InputEvent>>>;

/// The host's UTC offset right now, in seconds east of UTC.
///
/// Read from libc rather than from a zoneinfo parser: the host already has a
/// correct local time, and the guest only needs the number.
fn local_utc_offset_secs() -> i64 {
    #[cfg(unix)]
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff
    }
    #[cfg(not(unix))]
    {
        0
    }
}

pub struct WebGpuHost {
    instance: Arc<wgpu_core::global::Global>,
    table: ResourceTable,
    options: WasiWebGpuOptions,
    wasi: WasiCtx,
    presenter: Box<dyn CanvasGpuPresenter>,
    input: Arc<Mutex<VecDeque<InputEvent>>>,
    interactive: bool,
    catalog: Box<dyn CatalogHost>,
    /// Host-owned package installation. `DenyInstall` refuses every request until
    /// the presenter supplies an install root, index and signing policy.
    install: Box<dyn soil_install::InstallHost>,
    /// The HTTP policy, credentials and transport a guest may use. `None`
    /// refuses every request instead of quietly reaching the network.
    http: Option<HttpAccess>,
    /// A namespaced records book the guest may read or write, when granted.
    records: Option<NamespacedRecordsHost>,
    /// One workspace the guest may list and read. `None` denies `soil:vfs`.
    vfs: Option<VfsHost>,
    /// Repository status for that same workspace. `None` denies `soil:git`.
    ///
    /// Deliberately not a second root: a host that may read a workspace may ask
    /// Git about the workspace it is reading, and one that may not cannot. Two
    /// roots would be two answers to "which tree is this".
    git: Option<GitHost>,
    /// Which Desktops this window may switch between. `None` denies
    /// `soil:desktops`: a window whose workspace is not a registered Desktop
    /// has no picker to offer.
    desktops: Option<DesktopsHost>,
    /// Who the guest is acting as. `None` denies `soil:principal`.
    principal: Option<PrincipalHost>,
    /// The cast of agents this run can see. `NoAgents` refuses every read, so a
    /// host that forgot to attach an adapter fails loudly instead of drawing an
    /// empty wall.
    agents: Box<dyn AgentBackend>,
    terminal: Box<dyn TerminalHost>,
    cursor: Option<Box<dyn CursorHost>>,
    /// Where this host is in the notice stream, and what it does with each one.
    dispatch: Dispatch,
    clipboard: Option<Box<dyn ClipboardHost>>,
    /// The Unibus this Spore publishes to and polls. In-process: the notices a
    /// surface emits are for surfaces on this machine, and a popup is one.
    bus: Arc<MemoryBus>,
    /// The private key/value book behind `soil:appdata`, opened for one app.
    appdata: Option<AppDataHost>,
    /// Frame accounting for `SOIL_WEBGPU_STATS=1`.
    stats: FrameStats,
    /// When the last frame was presented, so the host can hold the next one to
    /// `FRAME_INTERVAL` rather than taking whatever rate the guest offers.
    last_frame: Option<Instant>,
}

/// Frame accounting for `SOIL_WEBGPU_STATS=1`: how often the Spore presents, and
/// how long the host spends lifting each frame off the GPU.
///
/// That number is what makes the rest of the pipeline legible: at 1000x700 the
/// host readback is 5-12 ms and the blit 16-20 ms of a ~128 ms frame, so ~100 ms
/// is the Spore's own round trip (screenshot to CPU, upload back to a texture),
/// not the window.
struct FrameStats {
    frames: u64,
    readback_ms: f64,
    since: Instant,
}

impl FrameStats {
    fn new() -> Self {
        Self {
            frames: 0,
            readback_ms: 0.0,
            since: Instant::now(),
        }
    }

    /// Records one presented frame, and returns the line to print once the window
    /// is at least a second old — starting a new window. `None` before that, so a
    /// caller prints at most once a second no matter how fast frames arrive.
    fn record(&mut self, readback: Duration, now: Instant) -> Option<String> {
        self.frames += 1;
        self.readback_ms += readback.as_secs_f64() * 1000.0;
        let window = now.saturating_duration_since(self.since);
        if window < Duration::from_secs(1) {
            return None;
        }
        let line = format!(
            "webgpu: {:.1} fps presented, {:.1} ms host readback",
            self.frames as f64 / window.as_secs_f64(),
            self.readback_ms / self.frames as f64,
        );
        self.frames = 0;
        self.readback_ms = 0.0;
        self.since = now;
        Some(line)
    }
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

    /// The collections a package declares in its manifest, read-only.
    ///
    /// `book` names one book for all of them; absent, each collection is its own
    /// book, which is what a fresh host has before anybody decides where it
    /// lives. Declared collections are reads: a Spore that writes names its
    /// collection on the command line, so the write is an act somebody chose.
    pub fn declared(
        root: impl Into<PathBuf>,
        collections: Vec<String>,
        book: Option<String>,
    ) -> Self {
        let root = root.into();
        Self {
            root,
            namespaces: collections
                .into_iter()
                .map(|collection| {
                    let book = book.clone().unwrap_or_else(|| collection.clone());
                    // `write` for now: a read-only namespace does not reach `scan`
                    // in the records host today, and a declared collection that a
                    // Spore cannot read is not a grant. Tightening this is a host
                    // fix, not a manifest one.
                    Namespace::write(collection.clone(), book, collection)
                })
                .collect(),
        }
    }

    /// The read-only namespace the vitals renderer uses for the default
    /// wearer. A renderer that shows more than one person asks for
    /// [`Self::vitals_for`] instead.
    pub fn vitals(root: impl Into<PathBuf>) -> Self {
        Self::vitals_for(root, "owner")
    }

    /// The read-only namespace for one wearer's view. Records live one book per
    /// wearer, so the wearer is part of both the namespace and the book.
    pub fn vitals_for(root: impl Into<PathBuf>, wearer: &str) -> Self {
        Self {
            root: root.into(),
            namespaces: vec![Namespace::read(
                format!("vitals.view:{wearer}"),
                format!("personal/{wearer}/telemetry"),
                "vitals.view",
            )],
        }
    }
}

/// Everything one Spore run is allowed to talk to. A presenter fills in the
/// parts it owns; anything left as the denying default refuses the guest loudly
/// instead of quietly doing nothing.
pub struct HostSetup {
    pub presenter: Box<dyn CanvasGpuPresenter>,
    pub input: Arc<Mutex<VecDeque<InputEvent>>>,
    /// Tells the guest to keep polling after the first frame.
    pub interactive: bool,
    pub catalog: Box<dyn CatalogHost>,
    /// The install grant. `DenyInstall` is the denying default.
    pub install: Box<dyn soil_install::InstallHost>,
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
    /// What this host does with the bus. One surface in a process owns dispatch;
    /// the others watch, so a task read twice does not become two sessions. The
    /// default owns dispatch with no creator, so a host that cannot start
    /// sessions says so on the bus instead of dropping the task.
    pub dispatch: DispatchRole,
    /// The Unibus this Spore publishes to and polls. In-process by default: the
    /// notices a wall emits are for surfaces on this machine, and a popup is one
    /// of them.
    pub bus: Arc<dyn Bus>,
    /// Shorthand for a read-only `soil:http` grant for one origin, folded into
    /// `http` when that is unset. The storefront browser names its own origin
    /// this way.
    pub store_origin: Option<String>,
    pub terminal: Box<dyn TerminalHost>,
    /// `None` on hosts without a window; `soil:present/cursor` still succeeds.
    pub cursor: Option<Box<dyn CursorHost>>,
    /// `None` on hosts without a pasteboard.
    pub clipboard: Option<Box<dyn ClipboardHost>>,
    /// The private key/value book behind `soil:appdata`. `None` links the
    /// interface and denies every call, so a Spore that imports it still
    /// instantiates and says why it has nowhere to write.
    pub appdata: Option<AppDataGrant>,
}

/// The bus a surface publishes its notices to.
///
/// One per host process, shared by every surface in it. A caller that needs to
/// inspect or seed the log keeps the concrete [`MemoryBus`]; the host only needs
/// the trait.
pub fn in_process_bus() -> Arc<MemoryBus> {
    Arc::new(MemoryBus::default())
}

impl HostSetup {
    /// A one-shot capture host: one frame, no input, no catalog, no terminal.
    pub fn capture(presenter: Box<dyn CanvasGpuPresenter>) -> Self {
        Self {
            presenter,
            input: Arc::new(Mutex::new(VecDeque::new())),
            interactive: false,
            catalog: Box::new(NoCatalog),
            install: Box::new(soil_install::DenyInstall),
            http: None,
            records: None,
            vfs: None,
            principal: None,
            agents: Box::new(NoAgents),
            dispatch: DispatchRole::default(),
            bus: in_process_bus(),
            store_origin: None,
            terminal: Box::new(NoTerminal),
            cursor: None,
            clipboard: None,
            appdata: None,
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
        // Read before the workspace is moved into its host: one workspace, two
        // questions (what is in it, which repository is it in).
        let git = setup.vfs.as_ref().map(GitHost::open);
        let desktops = setup.vfs.as_deref().and_then(DesktopsHost::for_workspace);
        Ok(Self {
            instance: Arc::new(instance),
            table: ResourceTable::new(),
            options: WasiWebGpuOptions::default(),
            // A Spore's stderr is the only place its own error text can go: a
            // trap carries a backtrace, not the message. Inherit it so a failing
            // guest says why.
            wasi: WasiCtxBuilder::new()
                .inherit_stderr()
                // A guest has no zoneinfo, so the host says what local means.
                // `wasi:clocks` is UTC by definition; the offset is the only
                // part of "the time" a wasm Spore cannot work out for itself.
                .env("SOIL_UTC_OFFSET", local_utc_offset_secs().to_string())
                .build(),
            presenter: setup.presenter,
            input: setup.input,
            interactive: setup.interactive,
            catalog: setup.catalog,
            install: setup.install,
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
            git,
            desktops,
            principal: setup.principal,
            agents: setup.agents,
            dispatch: match setup.dispatch {
                DispatchRole::Own(sessions) => Dispatch::new(setup.bus.clone(), sessions),
                DispatchRole::Watch => Dispatch::watcher(setup.bus.clone()),
            },
            terminal: setup.terminal,
            cursor: setup.cursor,
            clipboard: setup.clipboard,
            appdata: setup
                .appdata
                .map(|grant| {
                    AppDataHost::new(&grant.root, &grant.app_id, None)
                        .context("open appdata for the WebGPU guest")
                })
                .transpose()?,
            bus: Arc::new(MemoryBus::default()),
            stats: FrameStats::new(),
            last_frame: None,
        })
    }

    /// Hold the next frame to [`FRAME_INTERVAL`].
    ///
    /// A framebuffer guest has no clock it can trust and no vsync to wait on,
    /// so a guest that renders in a loop spins every core it can reach: ohmOS
    /// on the phone sat at ~250% of one core drawing the same still home
    /// screen. The cap is the host's promise, not the guest's discipline.
    fn pace(&mut self) {
        if let Some(last) = self.last_frame {
            let elapsed = last.elapsed();
            if elapsed < FRAME_INTERVAL {
                std::thread::sleep(FRAME_INTERVAL - elapsed);
            }
        }
        self.last_frame = Some(Instant::now());
    }
}

impl soil::present::input::Host for WebGpuHost {
    fn interactive(&mut self) -> bool {
        self.interactive
    }

    fn poll(&mut self, timeout_ms: u32) -> Option<input::Event> {
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

impl soil::present::surface::Host for WebGpuHost {
    fn size(&mut self) -> (u32, u32) {
        self.presenter.size()
    }

    fn scale(&mut self) -> f64 {
        self.presenter.scale()
    }

    /// This host links `wasi:webgpu`, so it takes either delivery. A device
    /// host with no adapter would answer `[rgba]` and a GPU-only Spore would
    /// learn that from the one call, rather than by failing to create a device.
    fn delivery(&mut self) -> Vec<soil::present::types::DeliveryKind> {
        vec![
            soil::present::types::DeliveryKind::Texture,
            soil::present::types::DeliveryKind::Rgba,
        ]
    }

    /// The no-GPU arm: the Spore rasterized its own frame, so the host only
    /// validates it and hands it to the presenter. No device, no texture, no
    /// readback.
    fn present_rgba(
        &mut self,
        frame: soil::present::types::RgbaFrame,
    ) -> std::result::Result<(), String> {
        self.pace();
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
        if frame.pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0xFF) {
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
        self.pace();
        let started = Instant::now();
        let image = wasi_webgpu_wasmtime::read_texture_rgba(&self.instance, &self.table, texture)
            .map_err(|e| e.to_string())?;
        if std::env::var_os("SOIL_WEBGPU_STATS").is_some()
            && let Some(line) = self.stats.record(started.elapsed(), Instant::now())
        {
            eprintln!("{line}");
        }
        let frame = CanvasFrame {
            width: image.width,
            height: image.height,
            rgba: image.rgba,
        };
        self.presenter.present(frame).map_err(|e| e.to_string())
    }
}

impl ::soil::unibus::bus::Host for WebGpuHost {
    fn poll(
        &mut self,
        kinds: Vec<String>,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<::soil::unibus::bus::Page, ::soil::unibus::bus::Error> {
        let page = self
            .bus
            .poll(&kinds, cursor.as_deref(), limit)
            .map_err(bus_error)?;
        Ok(::soil::unibus::bus::Page {
            frames: page
                .frames
                .into_iter()
                .map(|frame| ::soil::unibus::bus::Frame {
                    kind: frame.kind,
                    producer: frame.producer,
                    issued_at_ms: frame.issued_at_ms,
                    body: frame.body,
                })
                .collect(),
            next: page.next,
        })
    }

    fn publish(
        &mut self,
        kind: String,
        body: Vec<u8>,
        destination: Option<String>,
        ttl_ms: u64,
    ) -> std::result::Result<::soil::unibus::bus::Receipt, ::soil::unibus::bus::Error> {
        self.bus
            .publish(&kind, &body, destination.as_deref(), ttl_ms)
            .map(|receipt| ::soil::unibus::bus::Receipt {
                request_id: receipt.request_id,
            })
            .map_err(bus_error)
    }
}

/// The bus reports failure in its own vocabulary; the guest sees the WIT one.
fn bus_error(error: ::soil::unibus::BusError) -> ::soil::unibus::bus::Error {
    use ::soil::unibus::BusError as Bus;
    use ::soil::unibus::bus::Error as Wire;
    match error {
        Bus::Denied => Wire::Denied,
        Bus::Invalid => Wire::Invalid,
        Bus::OverBudget => Wire::OverBudget,
        Bus::Stale => Wire::Stale,
        Bus::Unavailable => Wire::Unavailable,
    }
}

impl ::soil::vfs_host::vfs::Host for WebGpuHost {
    fn ls(
        &mut self,
        parent: String,
        offset: u64,
        limit: u32,
    ) -> std::result::Result<::soil::vfs_host::vfs::Page, ::soil::vfs_host::vfs::Error> {
        self.vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs::Error::Denied)?
            .ls(parent, offset, limit)
    }

    fn stat(
        &mut self,
        path: String,
    ) -> std::result::Result<::soil::vfs_host::vfs::Entry, ::soil::vfs_host::vfs::Error> {
        self.vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs::Error::Denied)?
            .stat(path)
    }

    fn read(
        &mut self,
        path: String,
        offset: u64,
        length: u32,
    ) -> std::result::Result<::soil::vfs_host::vfs::Chunk, ::soil::vfs_host::vfs::Error> {
        self.vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs::Error::Denied)?
            .read(path, offset, length)
    }
}

impl ::soil::vfs_host::vfs_mut::HostWriter for WebGpuHost {
    fn write(
        &mut self,
        _writer: wasmtime::component::Resource<::soil::vfs_host::VfsWriter>,
        _data: Vec<u8>,
    ) -> std::result::Result<u64, ::soil::vfs_host::vfs_mut::Error> {
        Err(::soil::vfs_host::vfs_mut::Error::Denied)
    }

    fn finish(
        &mut self,
        _writer: wasmtime::component::Resource<::soil::vfs_host::VfsWriter>,
    ) -> std::result::Result<
        ::soil::vfs_host::vfs_mut::WriteResult,
        ::soil::vfs_host::vfs_mut::Error,
    > {
        Err(::soil::vfs_host::vfs_mut::Error::Denied)
    }

    fn abort(
        &mut self,
        _writer: wasmtime::component::Resource<::soil::vfs_host::VfsWriter>,
    ) -> std::result::Result<(), ::soil::vfs_host::vfs_mut::Error> {
        Err(::soil::vfs_host::vfs_mut::Error::Denied)
    }

    fn drop(
        &mut self,
        _writer: wasmtime::component::Resource<::soil::vfs_host::VfsWriter>,
    ) -> std::result::Result<(), wasmtime::Error> {
        Ok(())
    }
}

impl ::soil::vfs_host::vfs_mut::Host for WebGpuHost {
    fn begin_write(
        &mut self,
        _path: String,
        _media_type: String,
        _expected_size: Option<u64>,
        _expected_version: Option<u64>,
        _mode: Option<u32>,
    ) -> std::result::Result<
        wasmtime::component::Resource<::soil::vfs_host::VfsWriter>,
        ::soil::vfs_host::vfs_mut::Error,
    > {
        Err(::soil::vfs_host::vfs_mut::Error::Denied)
    }

    fn write(
        &mut self,
        path: String,
        data: Vec<u8>,
        media_type: String,
        expected_version: Option<u64>,
        mode: Option<u32>,
    ) -> std::result::Result<
        ::soil::vfs_host::vfs_mut::WriteResult,
        ::soil::vfs_host::vfs_mut::Error,
    > {
        let vfs = self
            .vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs_mut::Error::Denied)?;
        ::soil::vfs_host::vfs_mut::Host::write(
            vfs,
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
    ) -> std::result::Result<
        ::soil::vfs_host::vfs_mut::WriteResult,
        ::soil::vfs_host::vfs_mut::Error,
    > {
        let vfs = self
            .vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs_mut::Error::Denied)?;
        ::soil::vfs_host::vfs_mut::Host::chmod(vfs, path, mode)
    }

    fn remove(
        &mut self,
        path: String,
    ) -> std::result::Result<(), ::soil::vfs_host::vfs_mut::Error> {
        let vfs = self
            .vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs_mut::Error::Denied)?;
        ::soil::vfs_host::vfs_mut::Host::remove(vfs, path)
    }

    fn mkdir(
        &mut self,
        path: String,
    ) -> std::result::Result<(), ::soil::vfs_host::vfs_mut::Error> {
        let vfs = self
            .vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs_mut::Error::Denied)?;
        ::soil::vfs_host::vfs_mut::Host::mkdir(vfs, path)
    }

    fn rmdir(
        &mut self,
        path: String,
    ) -> std::result::Result<(), ::soil::vfs_host::vfs_mut::Error> {
        let vfs = self
            .vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs_mut::Error::Denied)?;
        ::soil::vfs_host::vfs_mut::Host::rmdir(vfs, path)
    }

    fn rename(
        &mut self,
        source: String,
        target: String,
    ) -> std::result::Result<
        ::soil::vfs_host::vfs_mut::WriteResult,
        ::soil::vfs_host::vfs_mut::Error,
    > {
        let vfs = self
            .vfs
            .as_mut()
            .ok_or(::soil::vfs_host::vfs_mut::Error::Denied)?;
        ::soil::vfs_host::vfs_mut::Host::rename(vfs, source, target)
    }
}

impl ::soil::git_host::git::Host for WebGpuHost {
    fn status(
        &mut self,
        path: String,
    ) -> std::result::Result<::soil::git_host::git::Repository, ::soil::git_host::git::Error>
    {
        let git = self
            .git
            .as_mut()
            .ok_or(::soil::git_host::git::Error::Denied)?;
        ::soil::git_host::git::Host::status(git, path)
    }
}

impl ::soil::vfs_host::cid::Host for WebGpuHost {
    fn find(
        &mut self,
        content_id: String,
        offset: u64,
        limit: u32,
    ) -> std::result::Result<::soil::vfs_host::cid::Page, ::soil::vfs_host::cid::Error>
    {
        <VfsHost as ::soil::vfs_host::cid::Host>::find(
            self.vfs
                .as_mut()
                .ok_or(::soil::vfs_host::cid::Error::Denied)?,
            content_id,
            offset,
            limit,
        )
    }
}

impl ::soil::desktops::desktops::Host for WebGpuHost {
    fn available(
        &mut self,
    ) -> std::result::Result<
        Vec<::soil::desktops::desktops::Desktop>,
        ::soil::desktops::desktops::Error,
    > {
        let desktops = self
            .desktops
            .as_mut()
            .ok_or(::soil::desktops::desktops::Error::Denied)?;
        ::soil::desktops::desktops::Host::available(desktops)
    }

    fn open(
        &mut self,
        id: String,
    ) -> std::result::Result<
        ::soil::desktops::desktops::Desktop,
        ::soil::desktops::desktops::Error,
    > {
        let desktops = self
            .desktops
            .as_mut()
            .ok_or(::soil::desktops::desktops::Error::Denied)?;
        let row = ::soil::desktops::desktops::Host::open(desktops, id)?;
        // The barrier: rebind the workspace every path-shaped capability reads,
        // after `open` has already refused anything it could not have.
        let root = desktops.bound_root();
        self.vfs = Some(
            VfsHost::open(&root)
                .map_err(|_| ::soil::desktops::desktops::Error::Io)?,
        );
        self.git = Some(GitHost::open(&root));
        Ok(row)
    }
}

impl ::soil::principal_host::api::Host for WebGpuHost {
    fn current(
        &mut self,
    ) -> std::result::Result<
        ::soil::principal_host::api::Identity,
        ::soil::principal_host::api::Error,
    > {
        self.principal
            .as_mut()
            .ok_or(::soil::principal_host::api::Error::Denied)?
            .current()
    }

    fn signing_key(
        &mut self,
    ) -> std::result::Result<
        Option<::soil::principal_host::api::SigningKeyInfo>,
        ::soil::principal_host::api::Error,
    > {
        self.principal
            .as_mut()
            .ok_or(::soil::principal_host::api::Error::Denied)?
            .signing_key()
    }
}

impl appdata_host::Host for WebGpuHost {
    fn open(
        &mut self,
        scope: appdata_host::Scope,
    ) -> std::result::Result<Resource<AppDataStore>, appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::Host::open(host, scope),
            None => Err(appdata_host::Error::Denied),
        }
    }
}

impl appdata_host::HostStore for WebGpuHost {
    fn get(
        &mut self,
        handle: Resource<AppDataStore>,
        key: String,
    ) -> std::result::Result<Option<Vec<u8>>, appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::HostStore::get(host, handle, key),
            None => Err(appdata_host::Error::Denied),
        }
    }

    fn get_meta(
        &mut self,
        handle: Resource<AppDataStore>,
        key: String,
    ) -> std::result::Result<Option<appdata_host::Meta>, appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::HostStore::get_meta(host, handle, key),
            None => Err(appdata_host::Error::Denied),
        }
    }

    fn set(
        &mut self,
        handle: Resource<AppDataStore>,
        key: String,
        value: Vec<u8>,
    ) -> std::result::Result<u64, appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::HostStore::set(host, handle, key, value),
            None => Err(appdata_host::Error::Denied),
        }
    }

    fn set_if_generation(
        &mut self,
        handle: Resource<AppDataStore>,
        key: String,
        value: Vec<u8>,
        generation: u64,
    ) -> std::result::Result<u64, appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => {
                appdata_host::HostStore::set_if_generation(host, handle, key, value, generation)
            }
            None => Err(appdata_host::Error::Denied),
        }
    }

    fn delete(
        &mut self,
        handle: Resource<AppDataStore>,
        key: String,
    ) -> std::result::Result<(), appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::HostStore::delete(host, handle, key),
            None => Err(appdata_host::Error::Denied),
        }
    }

    fn list_keys(
        &mut self,
        handle: Resource<AppDataStore>,
        prefix: String,
        limit: u32,
    ) -> std::result::Result<Vec<appdata_host::Meta>, appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::HostStore::list_keys(host, handle, prefix, limit),
            None => Err(appdata_host::Error::Denied),
        }
    }

    fn generation(
        &mut self,
        handle: Resource<AppDataStore>,
    ) -> std::result::Result<u64, appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::HostStore::generation(host, handle),
            None => Err(appdata_host::Error::Denied),
        }
    }

    fn sync_state(
        &mut self,
        handle: Resource<AppDataStore>,
    ) -> std::result::Result<appdata_host::SyncState, appdata_host::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::HostStore::sync_state(host, handle),
            None => Err(appdata_host::Error::Denied),
        }
    }

    fn drop(
        &mut self,
        handle: Resource<AppDataStore>,
    ) -> std::result::Result<(), wasmtime::Error> {
        match self.appdata.as_mut() {
            Some(host) => appdata_host::HostStore::drop(host, handle),
            None => Ok(()),
        }
    }
}

impl records_host::records::Host for WebGpuHost {
    fn get(
        &mut self,
        key: records_host::records::Key,
    ) -> std::result::Result<records_host::records::Entry, records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .get(key)
    }

    fn scan(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<records_host::records::Page, records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .scan(collection, cursor, limit)
    }

    fn query(
        &mut self,
        spec: records_host::records::QuerySpec,
    ) -> std::result::Result<records_host::records::Page, records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .query(spec)
    }

    fn aggregate(
        &mut self,
        collection: String,
        spec: records_host::records::AggregateSpec,
    ) -> std::result::Result<
        Vec<records_host::records::AggregateGroup>,
        records_host::records::Error,
    > {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .aggregate(collection, spec)
    }

    fn scan_packed(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<records_host::records::PackedPage, records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .scan_packed(collection, cursor, limit)
    }

    fn upsert(
        &mut self,
        entry: records_host::records::Entry,
    ) -> std::result::Result<records_host::records::Entry, records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .upsert(entry)
    }

    fn compare_and_swap(
        &mut self,
        key: records_host::records::Key,
        expected: Option<records_host::records::Version>,
        entry: records_host::records::Entry,
    ) -> std::result::Result<records_host::records::CasResult, records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .compare_and_swap(key, expected, entry)
    }

    fn delete(
        &mut self,
        key: records_host::records::Key,
    ) -> std::result::Result<(), records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .delete(key)
    }

    fn changes(
        &mut self,
        since: Option<String>,
        limit: u32,
    ) -> std::result::Result<records_host::records::Page, records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .changes(since, limit)
    }

    fn apply(
        &mut self,
        entries: Vec<records_host::records::Entry>,
    ) -> std::result::Result<records_host::records::ApplyResult, records_host::records::Error> {
        self.records
            .as_mut()
            .ok_or(records_host::records::Error::Unsupported)?
            .apply(entries)
    }
}

impl soil::http::http::Host for WebGpuHost {
    /// The guest adapter over `soil-host`'s one policy implementation.
    ///
    /// The guest names a URL and an opaque credential handle; the policy owns
    /// the origin allowlist, method/header/body checks and the transport. A
    /// host with no HTTP grant returns `unsupported`, not a silent empty
    /// response.
    fn fetch(
        &mut self,
        request: soil::http::http::Request,
    ) -> std::result::Result<soil::http::http::Response, soil::http::http::Error> {
        let Some(access) = self.http.as_ref() else {
            return Err(soil::http::http::Error::Unsupported);
        };
        let request = HttpRequest {
            url: request.url,
            method: match request.method {
                soil::http::http::Method::Get => http_host::Method::Get,
                soil::http::http::Method::Head => http_host::Method::Head,
                soil::http::http::Method::Post => http_host::Method::Post,
                soil::http::http::Method::Put => http_host::Method::Put,
                soil::http::http::Method::Patch => http_host::Method::Patch,
                soil::http::http::Method::Delete => http_host::Method::Delete,
            },
            headers: request
                .headers
                .into_iter()
                .map(|header| (header.name, header.value))
                .collect(),
            body: request.body,
            credential: request.credential,
            max_bytes: request.max_bytes,
            timeout_ms: request.timeout_ms,
        };
        let response = http_host::perform(
            &access.policy,
            &*access.credentials,
            &*access.transport,
            request,
        )
        .map_err(http_error)?;
        Ok(soil::http::http::Response {
            status: response.status,
            final_url: response.final_url,
            headers: response
                .headers
                .into_iter()
                .map(|(name, value)| soil::http::http::Header { name, value })
                .collect(),
            body: response.body,
        })
    }
}

fn http_error(error: http_host::Error) -> soil::http::http::Error {
    match error {
        http_host::Error::Denied => soil::http::http::Error::Denied,
        http_host::Error::Invalid => soil::http::http::Error::Invalid,
        http_host::Error::OverBudget => soil::http::http::Error::OverBudget,
        http_host::Error::Timeout => soil::http::http::Error::Timeout,
        http_host::Error::Unavailable => soil::http::http::Error::Unavailable,
        http_host::Error::Tls => soil::http::http::Error::Tls,
        http_host::Error::Http => soil::http::http::Error::Http,
        http_host::Error::Io => soil::http::http::Error::Io,
        http_host::Error::Unsupported => soil::http::http::Error::Unsupported,
    }
}

impl soil::spore::catalog::Host for WebGpuHost {
    fn entries(
        &mut self,
    ) -> std::result::Result<Vec<soil::spore::catalog::Entry>, soil::spore::catalog::Error> {
        let entries = self.catalog.entries().map_err(catalog_error)?;
        Ok(entries
            .into_iter()
            .map(|entry| soil::spore::catalog::Entry {
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
                    .map(|surface| soil::spore::catalog::Surface {
                        name: surface.name,
                        kind: match surface.kind {
                            CatalogSurfaceKind::Canvas => soil::spore::catalog::SurfaceKind::Canvas,
                            CatalogSurfaceKind::Web => soil::spore::catalog::SurfaceKind::Web,
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
    ) -> std::result::Result<soil::spore::catalog::Launched, soil::spore::catalog::Failure> {
        let launched = self
            .catalog
            .launch(&id, &surface)
            .map_err(catalog_failure)?;
        Ok(soil::spore::catalog::Launched {
            id: launched.id,
            surface: launched.surface,
            target: launched.target,
        })
    }

    fn running(&mut self) -> Vec<String> {
        self.catalog.running()
    }

    fn stop(&mut self, id: String) -> std::result::Result<(), soil::spore::catalog::Failure> {
        self.catalog.stop(&id).map_err(catalog_failure)
    }

    fn install(
        &mut self,
        id: String,
    ) -> std::result::Result<soil::spore::catalog::Installed, soil::spore::catalog::Failure> {
        let installed = self.catalog.install(&id).map_err(catalog_failure)?;
        Ok(soil::spore::catalog::Installed {
            id: installed.id,
            version: installed.version,
            digest: installed.digest,
        })
    }
}

fn catalog_failure(failure: CatalogFailure) -> soil::spore::catalog::Failure {
    soil::spore::catalog::Failure {
        code: catalog_error(failure.code),
        detail: failure.detail,
    }
}

impl soil::spore::install::Host for WebGpuHost {
    fn install(
        &mut self,
        id: String,
        digest: String,
    ) -> std::result::Result<soil::spore::install::Installed, soil::spore::install::Error> {
        let state = self.install.install(&id, &digest).map_err(install_error)?;
        Ok(installed(state))
    }

    fn rollback(
        &mut self,
        id: String,
    ) -> std::result::Result<soil::spore::install::Installed, soil::spore::install::Error> {
        let state = self.install.rollback(&id).map_err(install_error)?;
        Ok(installed(state))
    }

    fn status(
        &mut self,
        id: String,
    ) -> std::result::Result<Option<soil::spore::install::Installed>, soil::spore::install::Error>
    {
        let state = self.install.status(&id).map_err(install_error)?;
        Ok(state.map(installed))
    }
}

fn installed(state: soil_install::InstalledState) -> soil::spore::install::Installed {
    soil::spore::install::Installed {
        id: state.id,
        name: state.name.unwrap_or_default(),
        version: state.version,
        digest: state.digest,
        previous_digest: state.previous_digest,
    }
}

fn install_error(error: soil_install::InstallError) -> soil::spore::install::Error {
    use crate::soil::spore::install::Error;
    match error {
        soil_install::InstallError::Denied => Error::Denied,
        soil_install::InstallError::NotFound => Error::NotFound,
        soil_install::InstallError::Invalid => Error::Invalid,
        soil_install::InstallError::Consent => Error::Consent,
        soil_install::InstallError::Io => Error::Io,
        soil_install::InstallError::Unsupported => Error::Unsupported,
    }
}

fn catalog_error(error: CatalogError) -> soil::spore::catalog::Error {
    match error {
        CatalogError::Denied => soil::spore::catalog::Error::Denied,
        CatalogError::NotFound => soil::spore::catalog::Error::NotFound,
        CatalogError::Invalid => soil::spore::catalog::Error::Invalid,
        CatalogError::Unsupported => soil::spore::catalog::Error::Unsupported,
        CatalogError::Io => soil::spore::catalog::Error::Io,
    }
}

/// One per `observe.subscribe`.
pub struct AgentSubscription;

fn agent_error(error: soil_agent_host::Error) -> soil::agents::types::Error {
    use crate::soil::agents::types::Error;
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

fn agent_phase(phase: soil_agent_host::Phase) -> soil::agents::types::Phase {
    use crate::soil::agents::types::Phase;
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

fn agent_activity_kind(kind: soil_agent_host::ActivityKind) -> soil::agents::types::ActivityKind {
    use crate::soil::agents::types::ActivityKind;
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

fn agent_status(status: soil_agent_host::Status) -> soil::agents::types::Status {
    use crate::soil::agents::types::Status;
    match status {
        soil_agent_host::Status::Running => Status::Running,
        soil_agent_host::Status::Ok => Status::Ok,
        soil_agent_host::Status::Failed => Status::Failed,
        soil_agent_host::Status::Blocked => Status::Blocked,
    }
}

fn agent_prompt_kind(kind: soil_agent_host::PromptKind) -> soil::agents::types::PromptKind {
    use crate::soil::agents::types::PromptKind;
    match kind {
        soil_agent_host::PromptKind::Question => PromptKind::Question,
        soil_agent_host::PromptKind::Approval => PromptKind::Approval,
        soil_agent_host::PromptKind::Choice => PromptKind::Choice,
    }
}

fn agent_role(role: soil_agent_host::Role) -> soil::agents::types::Role {
    use crate::soil::agents::types::Role;
    match role {
        soil_agent_host::Role::User => Role::User,
        soil_agent_host::Role::Assistant => Role::Assistant,
        soil_agent_host::Role::Tool => Role::Tool,
    }
}

fn agent_owner(owner: soil_agent_host::Owner) -> soil::agents::types::Owner {
    soil::agents::types::Owner {
        actor: owner.actor,
        display_name: owner.display_name,
    }
}

fn agent_device(device: soil_agent_host::Device) -> soil::agents::types::Device {
    soil::agents::types::Device {
        id: device.id,
        label: device.label,
        current: device.current,
        attached: device.attached,
        detail: device.detail,
    }
}

fn agent_scope(scope: soil_agent_host::Scope) -> soil::agents::types::Scope {
    use crate::soil::agents::types::ScopeKind;
    soil::agents::types::Scope {
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

fn agent_record(agent: soil_agent_host::Agent) -> soil::agents::types::Agent {
    soil::agents::types::Agent {
        id: agent.id,
        harness: agent.harness,
        title: agent.title,
        workspace: agent.workspace,
        model: agent.model,
        device: agent.device,
        phase: agent_phase(agent.phase),
        goal: agent.goal.map(|goal| soil::agents::types::Goal {
            text: goal.text,
            set_at_ms: goal.set_at_ms,
        }),
        owner: agent.owner.map(agent_owner),
        activity: agent
            .activity
            .into_iter()
            .map(|row| soil::agents::types::Activity {
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
            .map(|prompt| soil::agents::types::Prompt {
                id: prompt.id,
                kind: agent_prompt_kind(prompt.kind),
                title: prompt.title,
                body: prompt.body,
                details: prompt.details,
                origin: prompt.origin,
                options: prompt
                    .choices
                    .into_iter()
                    .map(|choice| soil::agents::types::Choice {
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
        usage: soil::agents::types::Usage {
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

fn agent_binding(binding: Binding) -> soil::agents::types::Binding {
    soil::agents::types::Binding {
        agent: binding.agent,
        view: binding.view,
    }
}

fn agent_page(page: Page) -> soil::agents::observe::Page {
    soil::agents::observe::Page {
        turns: page
            .turns
            .into_iter()
            .map(|turn| soil::agents::types::Turn {
                role: agent_role(turn.role),
                text: turn.text,
                at_ms: turn.at_ms,
            })
            .collect(),
        before: page.before,
    }
}

impl soil::agents::observe::Host for WebGpuHost {
    fn agents(
        &mut self,
    ) -> std::result::Result<Vec<soil::agents::types::Agent>, soil::agents::types::Error> {
        // Asking for the roster is the tick: one bounded bus read, then the
        // answer. Nothing polls a harness in the background.
        self.dispatch.pump(&mut *self.agents);
        Ok(self.agents.list().into_iter().map(agent_record).collect())
    }

    fn get(
        &mut self,
        id: String,
    ) -> std::result::Result<soil::agents::types::Agent, soil::agents::types::Error> {
        self.dispatch.pump(&mut *self.agents);
        self.agents
            .get(&id)
            .map(agent_record)
            .ok_or(soil::agents::types::Error::NotFound)
    }

    fn intent(&mut self) -> soil::agents::types::Binding {
        let binding = self.agents.binding();
        agent_binding(binding)
    }

    fn current_scope(&mut self) -> soil::agents::types::Scope {
        let scope = self.agents.scope();
        agent_scope(scope)
    }

    /// The devices the host was told about. Pumped first so a notice that
    /// arrived with the last tick cannot make the picker disagree with the
    /// roster it sits beside.
    fn devices(&mut self) -> Vec<soil::agents::types::Device> {
        // No pump: placement and a model catalogue come from the handshake, not
        // from the bus. Pumping here put a bus read (and, when a notice was
        // waiting, an adapter re-list over the network) inside a render.
        self.agents.devices().into_iter().map(agent_device).collect()
    }

    fn models(&mut self) -> Vec<String> {
        self.agents.models()
    }

    fn transcript(
        &mut self,
        id: String,
        before: Option<u64>,
        limit: u32,
    ) -> std::result::Result<soil::agents::observe::Page, soil::agents::types::Error> {
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
        soil::agents::types::Error,
    > {
        self.table
            .push(AgentSubscription)
            .map_err(|_| soil::agents::types::Error::Io)
    }
}

impl soil::agents::observe::HostSubscription for WebGpuHost {
    fn next(
        &mut self,
        subscription: wasmtime::component::Resource<AgentSubscription>,
        timeout_ms: u32,
    ) -> std::result::Result<Option<soil::agents::observe::Change>, soil::agents::types::Error>
    {
        self.dispatch.pump(&mut *self.agents);
        // A handle the guest already dropped is `stale`, not an empty poll.
        self.table
            .get(&subscription)
            .map_err(|_| soil::agents::types::Error::Stale)?;
        Ok(self.agents.next(timeout_ms).map(|change| match change {
            soil_agent_host::Change::Upserted(agent) => {
                soil::agents::observe::Change::Upserted(agent_record(*agent))
            }
            soil_agent_host::Change::Removed(id) => soil::agents::observe::Change::Removed(id),
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

impl soil::agents::control::Host for WebGpuHost {
    fn respond(
        &mut self,
        id: String,
        prompt_id: String,
        answer: soil::agents::types::Response,
    ) -> std::result::Result<(), soil::agents::types::Error> {
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
    ) -> std::result::Result<(), soil::agents::types::Error> {
        self.agents.send(&id, &text).map_err(agent_error)
    }

    fn interrupt(&mut self, id: String) -> std::result::Result<(), soil::agents::types::Error> {
        self.agents.interrupt(&id).map_err(agent_error)
    }
}

impl soil::terminal_screen::screen::Host for WebGpuHost {
    fn sessions(
        &mut self,
    ) -> std::result::Result<Vec<soil::terminal_screen::screen::Session>, String> {
        Ok(self
            .terminal
            .sessions()?
            .into_iter()
            .map(|session| soil::terminal_screen::screen::Session {
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

    fn snapshot(&mut self) -> std::result::Result<soil::terminal_screen::screen::Grid, String> {
        let snapshot = self.terminal.snapshot()?;
        Ok(soil::terminal_screen::screen::Grid {
            cols: snapshot.cols,
            rows: snapshot.rows,
            cells: snapshot
                .cells
                .into_iter()
                .map(|cell| soil::terminal_screen::screen::Cell {
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

impl soil::present::clipboard::Host for WebGpuHost {
    fn set_text(&mut self, text: String) -> std::result::Result<(), String> {
        match self.clipboard.as_mut() {
            Some(clipboard) => clipboard.set_text(text),
            None => Err("this host has no clipboard".to_owned()),
        }
    }
}

impl soil::present::cursor::Host for WebGpuHost {
    fn set(
        &mut self,
        style: soil::present::cursor::CursorStyle,
    ) -> std::result::Result<(), String> {
        let style = match style {
            soil::present::cursor::CursorStyle::Default => CursorStyle::Default,
            soil::present::cursor::CursorStyle::Text => CursorStyle::Text,
            soil::present::cursor::CursorStyle::Pointer => CursorStyle::Pointer,
            soil::present::cursor::CursorStyle::Crosshair => CursorStyle::Crosshair,
            soil::present::cursor::CursorStyle::Move => CursorStyle::Move,
            soil::present::cursor::CursorStyle::Grab => CursorStyle::Grab,
            soil::present::cursor::CursorStyle::Grabbing => CursorStyle::Grabbing,
            soil::present::cursor::CursorStyle::NoDrop => CursorStyle::NoDrop,
            soil::present::cursor::CursorStyle::Wait => CursorStyle::Wait,
            soil::present::cursor::CursorStyle::Help => CursorStyle::Help,
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

/// The file a compiled component is cached in: `<sha256 of the module>.cwasm`.
///
/// The digest is the whole contract between whoever writes this file and whoever
/// reads it — `soil spore warm` on one side, a window on the other — so it is
/// derived in one place rather than twice and kept in step by hand.
#[must_use]
pub fn component_cache_file(directory: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut name = String::with_capacity(digest.len() * 2 + 8);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    directory.join(format!("{name}.cwasm"))
}

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
        soil::present::surface::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:present")?;
        soil::present::input::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:present input")?;
        soil::present::cursor::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:present cursor")?;
        soil::present::clipboard::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:present clipboard")?;
        soil::http::http::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:http")?;
        soil::spore::catalog::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:spore catalog")?;
        soil::spore::install::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:spore install")?;
        soil::agents::observe::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:agent observe")?;
        soil::agents::control::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:agent control")?;
        soil::terminal_screen::screen::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:terminal-screen")?;
        records_host::add_to_linker::<WebGpuHost>(&mut linker).context("link soil:records")?;
        soil_appdata_host::add_to_linker::<WebGpuHost>(&mut linker)
            .context("link soil:appdata")?;
        ::soil::vfs_host::vfs::add_to_linker::<WebGpuHost, HasSelf<WebGpuHost>>(
            &mut linker,
            |host| host,
        )
        .map_err(anyhow::Error::from)
        .context("link soil:vfs")?;
        ::soil::vfs_host::vfs_mut::add_to_linker::<WebGpuHost, HasSelf<WebGpuHost>>(
            &mut linker,
            |host| host,
        )
        .map_err(anyhow::Error::from)
        .context("link soil:vfs-mut")?;
        ::soil::git_host::git::add_to_linker::<WebGpuHost, HasSelf<WebGpuHost>>(
            &mut linker,
            |host| host,
        )
        .map_err(anyhow::Error::from)
        .context("link soil:git")?;
        ::soil::vfs_host::cid::add_to_linker::<WebGpuHost, HasSelf<WebGpuHost>>(
            &mut linker,
            |host| host,
        )
        .map_err(anyhow::Error::from)
        .context("link soil:vfs-cid")?;
        ::soil::desktops::desktops::add_to_linker::<WebGpuHost, HasSelf<WebGpuHost>>(
            &mut linker,
            |host| host,
        )
        .map_err(anyhow::Error::from)
        .context("link soil:desktops")?;
        ::soil::principal_host::add_to_linker::<WebGpuHost>(&mut linker)
            .context("link soil:principal")?;
        ::soil::unibus::add_to_linker::<WebGpuHost>(&mut linker).context("link soil:unibus")?;
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
        let cache_path = self
            .component_cache
            .as_ref()
            .map(|directory| component_cache_file(directory, bytes));
        if let Some(path) = &cache_path
            && let Ok(serialized) = std::fs::read(path)
        {
            // The file is ours: it is named by the digest of the module it was
            // compiled from, so a mismatch means a stale cache, not a bad actor.
            match unsafe { Component::deserialize(&self.engine, &serialized) } {
                Ok(component) => return Ok(component),
                Err(error) => {
                    eprintln!("soil-webgpu: cached component unusable ({error}); compiling");
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
                        eprintln!("soil-webgpu: could not cache component: {error}");
                    }
                }
                Err(error) => {
                    eprintln!("soil-webgpu: component cannot be serialized: {error}");
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
            HostSetup::capture(Box::new(DiscardPresenter { size: (480, 640) })),
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
    /// The agents this surface may see. `None` denies `soil:agents`, which is
    /// the honest answer for a Spore that only draws.
    pub agents: Option<Box<dyn AgentBackend>>,
    /// The private app store this run may use. `None` denies `soil:appdata`.
    pub appdata: Option<AppDataGrant>,
}

impl Grants {
    /// Offer `soil:spore/catalog` rooted at `roots`.
    ///
    /// A launcher lists what is installed, and a capture of a launcher has to
    /// see the same machine a window does — so the roots are a grant, not a
    /// property of the window surface.
    pub fn with_catalog_roots(self, roots: Vec<std::path::PathBuf>) -> Self {
        Self {
            catalog: Some(Box::new(InstallCatalog::with_roots(roots))),
            ..self
        }
    }
}

impl Grants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attest `actor` as a human principal. Local development only: a host
    /// that verifies a real identity builds its own provider.
    pub fn actor(self, actor: impl Into<String>) -> Self {
        let principal = PrincipalHost::new(::soil::principal_host::StaticPrincipalProvider::new(
            ::soil::principal_host::Principal {
                actor: actor.into(),
                kind: ::soil::principal_host::PrincipalKind::Human,
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
    run_component_capture_with_input(bytes, width, height, grants, None)
}

/// A capture run that can be driven by keys.
///
/// The headless surface is the one a script can check, so it has to take keys
/// too: a run with no window is otherwise a single frame of whatever the guest
/// drew first, and a screen behind a menu is unreachable. `input` is the queue
/// the caller fills (from `--keys`, or an evdev reader); a non-empty queue also
/// makes the host interactive, which is what tells the guest to keep going.
pub fn run_component_capture_with_input(
    bytes: &[u8],
    width: u32,
    height: u32,
    grants: Grants,
    input: Option<InputQueue>,
) -> Result<(u32, Option<CanvasFrame>)> {
    let runtime = WebGpuRuntime::new()?;
    let capture = match &input {
        Some(_) => FrameCapture::streaming(width, height),
        None => FrameCapture::new(width, height),
    };
    let mut setup = HostSetup::capture(Box::new(capture.clone()));
    if let Some(input) = input {
        setup.input = input;
        setup.interactive = true;
    }
    setup.vfs = grants.vfs;
    setup.records = grants.records;
    setup.principal = grants.principal;
    setup.http = grants.http;
    setup.appdata = grants.appdata;
    setup.catalog = grants.catalog.unwrap_or(setup.catalog);
    // A recorded transcript read back as an agent: the guest holds the
    // contract and never learns which harness wrote the book.
    setup.agents = grants.agents.unwrap_or(setup.agents);
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

#[cfg(test)]
mod tests {
    const WEBGPU_WIT: &str = include_str!("../wit/deps/wasi-webgpu-0.3.0-rc.2/package.wit");
    const RUNNER_WIT: &str = include_str!("../wit/runner.wit");

    #[test]
    fn pins_the_upstream_wasi_webgpu_contract() {
        assert!(WEBGPU_WIT.starts_with("package wasi:webgpu@0.3.0-rc.2;"));
        assert!(RUNNER_WIT.contains("include wasi:webgpu/imports@0.3.0-rc.2;"));
        assert!(RUNNER_WIT.contains("export run: async func() -> u32;"));
    }

    fn stats_since(start: std::time::Instant) -> super::FrameStats {
        super::FrameStats {
            frames: 0,
            readback_ms: 0.0,
            since: start,
        }
    }

    #[test]
    fn the_stats_line_closes_a_window_and_starts_the_next() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut stats = stats_since(start);
        // Nine frames of 10 ms inside the first second: still nothing to print.
        for step in 1..=9 {
            let at = start + Duration::from_millis(100 * step);
            assert_eq!(stats.record(Duration::from_millis(10), at), None);
        }
        // The tenth crosses one second and reports the window just closed.
        let at = start + Duration::from_secs(1);
        assert_eq!(
            stats.record(Duration::from_millis(10), at),
            Some("webgpu: 10.0 fps presented, 10.0 ms host readback".to_string())
        );
        // And the next window counts only frames it saw.
        for step in 1..=4 {
            let at = start + Duration::from_secs(1) + Duration::from_millis(200 * step);
            assert_eq!(stats.record(Duration::from_millis(20), at), None);
        }
        let at = start + Duration::from_secs(2);
        assert_eq!(
            stats.record(Duration::from_millis(20), at),
            Some("webgpu: 5.0 fps presented, 20.0 ms host readback".to_string())
        );
    }

    #[test]
    fn a_slow_spore_reports_the_rate_it_actually_presented() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut stats = stats_since(start);
        // One frame in 1.2 s is 0.8 fps, not a rounded-up 1.0.
        let at = start + Duration::from_millis(1_200);
        assert_eq!(
            stats.record(Duration::from_millis(250), at),
            Some("webgpu: 0.8 fps presented, 250.0 ms host readback".to_string())
        );
    }

    #[test]
    fn the_runner_offers_agents_but_not_their_authority() {
        // The point of the surface is that it can watch and answer. Spawning a
        // harness, choosing a model, or reading a credential is not here, and
        // dropping this assertion is how one of them would quietly arrive.
        assert!(RUNNER_WIT.contains("import soil:agents/observe@0.1.0;"));
        assert!(RUNNER_WIT.contains("import soil:agents/control@0.1.0;"));
        for forbidden in ["spawn", "config", "credential", "secrets"] {
            assert!(
                !RUNNER_WIT.contains(forbidden),
                "the agent surface grew `{forbidden}` authority"
            );
        }
    }
}
