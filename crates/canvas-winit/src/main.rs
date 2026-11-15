//! A native window for a GPUI Spore.
//!
//! This is the smallest canvas-level wrapper: winit owns the window, the Spore
//! owns the pixels. The host runs the component in a wasmtime store, receives
//! each `soil:present` frame, and blits it to the window. No scene, no layout,
//! no hit-testing here — those belong to the Spore.

use std::{
    collections::{HashMap, VecDeque},
    num::NonZeroU32,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
};

use anyhow::{Context as _, Result};
use soil_spore::Manifest;
use url::Url;

mod terminal;

#[cfg(unix)]
use ::soil::http_host::GeodeCredentials;
use ::soil::http_host::{
    DirectTransport, HttpPolicy, HttpTransport, TransportError, TransportRequest, TransportResponse,
};
use soil::webgpu::{
    CanvasFrame, CanvasGpuPresenter, CatalogEntry, CatalogError, CatalogHost, CatalogLaunched,
    Binding, CatalogSurface, CatalogSurfaceKind, ClipboardHost, CursorHost, CursorStyle,
    DispatchRole, HostSetup, HttpAccess, InputEvent, InputKey, NoAgents, PointerButton,
    RecordsAccess, SharedAgents, WebGpuRuntime,
};
use soil::unibus::Bus;
use terminal::Terminals;
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{Key as WinitKey, ModifiersState, NamedKey},
    window::{CursorIcon, Window, WindowId},
};

type SoftContext = softbuffer::Context<Arc<Window>>;
type SoftSurface = softbuffer::Surface<Arc<Window>, Arc<Window>>;

const LOGICAL_SIZE: (f64, f64) = (1000.0, 700.0);

/// Host grants that follow a Spore when the launcher opens it.
///
/// The launcher is the thing the user configured, so passing its grants to a
/// child is one decision rather than a second configuration channel. A child
/// that does not import the capability still cannot use it.
#[derive(Clone, Debug, Default)]
struct LaunchConfig {
    origins: Vec<String>,
    geode_socket: Option<PathBuf>,
    geode_purpose: Option<String>,
    hue_bridge: Option<Url>,
    records_root: Option<PathBuf>,
    /// One workspace the Spore may list and read through `soil:vfs`.
    vfs_root: Option<PathBuf>,
    /// A book collection the Spore may read and write, when it was granted one.
    records_book: Option<BookGrant>,
    /// Who the Spore acts as, when this host was told. A window that was not
    /// told denies `soil:principal` rather than inventing a name.
    actor: Option<String>,
}

/// One book collection a Spore may read and write.
#[derive(Clone, Debug)]
struct BookGrant {
    collection: String,
    book: String,
}

impl LaunchConfig {
    fn append_args(&self, command: &mut std::process::Command) {
        for origin in &self.origins {
            command.arg("--allow-origin").arg(origin);
        }
        if let (Some(socket), Some(purpose)) = (&self.geode_socket, &self.geode_purpose) {
            command
                .arg("--geode-socket")
                .arg(socket)
                .arg("--geode-purpose")
                .arg(purpose);
        }
        if let Some(bridge) = &self.hue_bridge {
            command.arg("--hue-bridge").arg(bridge.as_str());
        }
        if let Some(root) = &self.records_root {
            command.arg("--records-root").arg(root);
        }
        if let Some(root) = &self.vfs_root {
            command.arg("--vfs-root").arg(root);
        }
        if let Some(grant) = &self.records_book {
            command.arg("--records-book").arg(&grant.book);
            command
                .arg("--records-collection")
                .arg(&grant.collection);
        }
        if let Some(actor) = &self.actor {
            command.arg("--actor").arg(actor);
        }
    }
}

/// Bind `hue.local` to the bridge the user configured.
///
/// The guest cannot resolve a bridge IP or hold its key. The host owns both:
/// the guest names a capability origin, this transport maps it to the real
/// bridge, and `perform` injects the leased Geode path segment before the
/// request leaves the process.
struct HueTransport {
    bridge: Option<Url>,
    inner: DirectTransport,
}

impl HttpTransport for HueTransport {
    fn fetch(
        &self,
        request: &TransportRequest,
    ) -> std::result::Result<TransportResponse, TransportError> {
        if request.url.host_str() != Some("hue.local") {
            return self.inner.fetch(request);
        }
        let Some(bridge) = &self.bridge else {
            return Err(TransportError::Unavailable);
        };
        let mut rewritten = request.clone();
        rewritten
            .url
            .set_scheme(bridge.scheme())
            .map_err(|_| TransportError::Io)?;
        rewritten
            .url
            .set_host(bridge.host_str())
            .map_err(|_| TransportError::Io)?;
        rewritten
            .url
            .set_port(bridge.port())
            .map_err(|_| TransportError::Io)?;
        self.inner.fetch(&rewritten)
    }
}

struct Installed {
    root: PathBuf,
    manifest: Manifest,
}

struct SporeCatalog {
    installed: Vec<Installed>,
    launch: LaunchConfig,
}

impl SporeCatalog {
    /// Join every directory a Spore can live in, most specific first. The
    /// shell's own bundle set is a fallback, not the truth: what a store
    /// installed, and what a project tree holds, belong in the same list.
    fn new(roots: &[PathBuf], launch: LaunchConfig) -> Result<Self, CatalogError> {
        let mut installed = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for root in roots {
            let Ok(directory) = std::fs::read_dir(root) else {
                continue;
            };
            for entry in directory.flatten() {
                if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    continue;
                }
                let package = entry.path();
                if !package.join("spore.toml").is_file() {
                    continue;
                }
                let manifest = match Manifest::load(&package) {
                    Ok(manifest) => manifest,
                    Err(error) => {
                        // One half-written package must not hide every other Spore.
                        eprintln!("canvas-winit: skip {}: {error}", package.display());
                        continue;
                    }
                };
                // The first root to offer an id wins, so a bundle cannot shadow
                // what a store installed.
                if !seen.insert(manifest.spore.id.clone()) {
                    continue;
                }
                installed.push(Installed {
                    root: package,
                    manifest,
                });
            }
        }
        installed.sort_by(|left, right| left.manifest.spore.name.cmp(&right.manifest.spore.name));
        Ok(Self { installed, launch })
    }
}

impl CatalogHost for SporeCatalog {
    fn entries(&mut self) -> Result<Vec<CatalogEntry>, CatalogError> {
        Ok(self
            .installed
            .iter()
            .map(|installed| {
                let manifest = &installed.manifest;
                CatalogEntry {
                    id: manifest.spore.id.clone(),
                    name: manifest.spore.name.clone(),
                    title: manifest
                        .spore
                        .title
                        .clone()
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or_else(|| soil_spore::display_title(&manifest.spore.name)),
                    version: manifest.spore.version.clone(),
                    summary: manifest.spore.summary.clone(),
                    workspace: None,
                    default_surface: manifest.launch.default.clone(),
                    surfaces: if manifest.surfaces.is_empty() {
                        // Legacy manifests state their surfaces as kinds. The
                        // catalogue enum has no native case yet, so name the
                        // native surface and let launch() route it to its host;
                        // the kind is only a hint to the picker.
                        manifest
                            .launch
                            .surfaces
                            .iter()
                            .filter_map(|name| {
                                let kind = match name.as_str() {
                                    "canvas" | "native" => CatalogSurfaceKind::Canvas,
                                    "web" => CatalogSurfaceKind::Web,
                                    _ => return None,
                                };
                                Some(CatalogSurface {
                                    name: name.clone(),
                                    kind,
                                    entry: String::new(),
                                    requires: None,
                                })
                            })
                            .collect()
                    } else {
                        manifest
                            .surfaces
                            .iter()
                            .map(|surface| CatalogSurface {
                                name: surface.name.clone(),
                                kind: match surface.kind {
                                    soil_spore::SurfaceKind::Canvas => CatalogSurfaceKind::Canvas,
                                    soil_spore::SurfaceKind::Web => CatalogSurfaceKind::Web,
                                },
                                entry: surface
                                    .entry(manifest.ui.as_ref())
                                    .unwrap_or_default()
                                    .to_owned(),
                                requires: surface.requires.clone(),
                            })
                            .collect()
                    },
                    mark: soil::webgpu::read_mark(&installed.root, manifest),
                }
            })
            .collect())
    }

    fn launch(&mut self, id: &str, surface: &str) -> Result<CatalogLaunched, CatalogError> {
        let installed = self
            .installed
            .iter()
            .find(|installed| installed.manifest.spore.id == id)
            .ok_or(CatalogError::NotFound)?;
        let module = installed
            .manifest
            .module
            .as_ref()
            .ok_or(CatalogError::Unsupported)?;
        let path = installed.root.join(&module.path);
        // Spawning a path that is not there would "succeed" and put the failure
        // in a child's stderr, where nobody is looking. Refuse it here so the
        // launcher can say which app did not open.
        if !path.is_file() {
            eprintln!(
                "canvas-winit: {} declares {} but the package does not carry it",
                installed.manifest.spore.id,
                path.display()
            );
            return Err(CatalogError::NotFound);
        }
        let executable = std::env::current_exe().map_err(|_| CatalogError::Io)?;
        // A package that ships a native host for this surface is launched in
        // that host, not in the window presenter. The presenter owns pixels
        // and input; the host owns the capabilities the Spore asked for.
        let native = (surface == "native")
            .then(|| installed.manifest.launch.native.as_deref())
            .flatten();
        let mut command = match native {
            Some(host) => native_command(host, &installed.root, &path, id, surface)?,
            None => launch_command(&executable, &path, &self.launch),
        };
        let mut child = command.spawn().map_err(|_| CatalogError::Io)?;
        // Reap the child without blocking the launcher's event loop. Launch
        // Services hands the window to another process, so there is often
        // nothing to wait for; `open` exits as soon as the app is running.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(CatalogLaunched {
            id: id.to_owned(),
            surface: surface.to_owned(),
            target: match native {
                Some(host) => format!("native {host} {}", path.display()),
                None => format!("fungOS {}", path.display()),
            },
        })
    }
}

/// How one Spore opens another.
///
/// A bundled app is started through Launch Services: it gets its own activation
/// and its own session, so its window becomes key and its clicks and keys reach
/// it. A binary run from a shell has no bundle to launch, so it re-executes
/// itself — with a null stdin and stdout, because a Spore that inherits its
/// parent's terminal inherits the wrong tty along with it.
fn launch_command(
    executable: &std::path::Path,
    module: &std::path::Path,
    launch: &LaunchConfig,
) -> std::process::Command {
    if let Some(bundle) = bundle_root(executable) {
        // Every Spore opens as its own app: its own bundle identity, its own
        // icon in the Dock, and therefore its own tile to come back to. Sharing
        // the launcher's identity would give every window one Dock entry.
        let mut command = std::process::Command::new("/usr/bin/open");
        match spore_bundle(bundle, module) {
            Some(target) => {
                command.arg("-n").arg(target).arg("--args");
            }
            None => {
                command.arg("-n").arg(bundle).arg("--args").arg(module);
            }
        }
        launch.append_args(&mut command);
        return command;
    }
    let mut command = std::process::Command::new(executable);
    command
        .arg(module)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    launch.append_args(&mut command);
    command
}

/// Start the package's native host for a surface this window cannot present.
///
/// The host contract is two environment variables: the module to run and the
/// package root it belongs to. A host that also knows a private variable (the
/// terminal's window switch, for example) can read it; the presenter stays
/// unaware of that host's internals.
fn native_command(
    host: &str,
    package: &std::path::Path,
    module: &std::path::Path,
    id: &str,
    surface: &str,
) -> Result<std::process::Command, CatalogError> {
    let program = resolve_native(host, package).ok_or(CatalogError::Io)?;
    let mut command = std::process::Command::new(&program);
    command
        .env("SOIL_SPORE_ID", id)
        .env("SOIL_SPORE_SURFACE", surface)
        .env("SOIL_SPORE_PACKAGE", package)
        .env("SOIL_SPORE_MODULE", module)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The terminal host predates the generic environment and still reads its
    // own names. Keep both working until it moves to the contract.
    if program
        .file_name()
        .is_some_and(|name| name == "terminal-host")
    {
        command
            .env("TERMINAL_SPORE", module)
            .env("TERMINAL_WINDOW", "1");
    }
    Ok(command)
}

/// Resolve a native host the way a shell would: an absolute or package-relative
/// path first, then the two runtime bin directories, then `PATH`.
fn resolve_native(host: &str, package: &std::path::Path) -> Option<std::path::PathBuf> {
    let expanded = if let Some(rest) = host.strip_prefix("~/") {
        std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(rest))?
    } else {
        std::path::PathBuf::from(host)
    };
    if expanded.is_absolute() {
        return expanded.is_file().then_some(expanded);
    }
    if host.contains('/') {
        let beside = package.join(&expanded);
        return beside.is_file().then_some(beside);
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if let Some(home) = home {
        for directory in [
            home.join(".local/share/fungos/bin"),
            home.join(".local/bin"),
        ] {
            let candidate = directory.join(&expanded);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(&expanded))
        .find(|candidate| candidate.is_file())
}

/// Build (or refresh) a `.app` that stands for one Spore, and answer its path.
///
/// The bundle carries its own copy of the binary and the module. A symlink would
/// be cheaper, but macOS names a Dock tile by the executable a process actually
/// runs: through a symlink that resolves back to the launcher, so every Spore
/// showed up as "fungOS". Copies are refreshed when the source changes.
fn spore_bundle(app: &std::path::Path, module: &std::path::Path) -> Option<std::path::PathBuf> {
    let slug = module
        .parent()
        .and_then(|package| package.file_name())
        .and_then(|name| name.to_str())?
        .to_owned();
    let title = manifest_title(module).unwrap_or_else(|| soil_spore::display_title(&slug));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let bundle = home
        .join("Library/Application Support/fungOS/apps")
        .join(format!("{slug}.app"));
    let contents = bundle.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&macos).ok()?;
    std::fs::create_dir_all(&resources).ok()?;

    let executable = std::env::current_exe().ok()?;
    refresh_copy(&executable, &macos.join("canvas-winit"))?;
    refresh_copy(module, &resources.join("spore.wasm"))?;
    // The icon comes from the launcher bundle so every Spore is a fungOS app.
    if let Some(parent) = app.parent() {
        let icon = parent.join("Resources/fungos.icns");
        if icon.is_file() {
            let _ = std::fs::copy(&icon, resources.join("fungos.icns"));
        }
    }
    // The bundle says what it is called; the module beside it is just
    // `spore.wasm`, so the title travels with the bundle.
    let _ = std::fs::write(resources.join("title.txt"), &title);
    let identifier = format!("dev.fungos.spore.{slug}");
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\n\
  <key>CFBundleExecutable</key><string>canvas-winit</string>\n\
  <key>CFBundleIdentifier</key><string>{identifier}</string>\n\
  <key>CFBundleName</key><string>{title}</string>\n\
  <key>CFBundleDisplayName</key><string>{title}</string>\n\
  <key>CFBundleIconFile</key><string>fungos</string>\n\
  <key>CFBundlePackageType</key><string>APPL</string>\n\
  <key>CFBundleShortVersionString</key><string>0.1.0</string>\n\
  <key>CFBundleVersion</key><string>0.1.0</string>\n\
  <key>LSMinimumSystemVersion</key><string>13.0</string>\n\
  <key>NSHighResolutionCapable</key><true/>\n\
</dict></plist>\n"
    );
    std::fs::write(contents.join("Info.plist"), plist).ok()?;
    Some(bundle)
}

/// Copy `source` over `destination` when the destination is missing, older, or a
/// different size. A stale copy would run yesterday's binary.
fn refresh_copy(source: &std::path::Path, destination: &std::path::Path) -> Option<()> {
    let fresh = match (
        std::fs::metadata(source).ok(),
        std::fs::metadata(destination).ok(),
    ) {
        (Some(source), Some(destination)) => {
            destination.len() == source.len()
                && destination.modified().ok() >= source.modified().ok()
        }
        _ => false,
    };
    if fresh {
        return Some(());
    }
    std::fs::copy(source, destination).ok().map(|_| ())
}

/// The display name a module's package gives itself, if it has a manifest.
fn manifest_title(module: &std::path::Path) -> Option<String> {
    let package = module.parent()?;
    let manifest = Manifest::load(package).ok()?;
    manifest
        .spore
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| Some(soil_spore::display_title(&manifest.spore.name)))
}

/// The `.app` an executable lives in, if it lives in one.
fn bundle_root(executable: &std::path::Path) -> Option<&std::path::Path> {
    executable.ancestors().find(|ancestor| {
        ancestor
            .extension()
            .is_some_and(|extension| extension == "app")
    })
}

struct FramePresenter {
    /// The surface extent in the Spore's layout space, kept current by the
    /// window: a Spore asks every frame, so a drag-resize reaches the layout
    /// without a second event path.
    size: Arc<Mutex<(u32, u32)>>,
    tx: mpsc::Sender<CanvasFrame>,
    window: Arc<Window>,
}

impl CanvasGpuPresenter for FramePresenter {
    fn size(&self) -> (u32, u32) {
        *self.size.lock().expect("surface size poisoned")
    }

    fn scale(&self) -> f64 {
        // The window is the only thing that knows which display it is on.
        self.window.scale_factor()
    }

    fn present(&mut self, frame: CanvasFrame) -> Result<()> {
        self.tx
            .send(frame)
            .map_err(|_| anyhow::anyhow!("the window closed before the frame arrived"))?;
        // The channel does not wake winit's loop; a redraw request does.
        self.window.request_redraw();
        Ok(())
    }
}

/// What every surface in one process shares.
///
/// A window is a view of a machine, not a machine: the module, the roster, the
/// bus and the grants belong to the process. Only the window, the software
/// surface it blits through, and the clocks that decide when it repaints belong
/// to one surface.
#[derive(Clone)]
struct Shared {
    module: PathBuf,
    /// The edgerunner service a task notice dispatches to.
    edgerunner: Option<String>,
    /// What runs on the terminal's pty: a login shell when absent.
    command: Option<String>,
    spores: Vec<PathBuf>,
    /// The HTTP grant passed to the Spore. `None` means no network at all.
    http: Option<HttpAccess>,
    /// The records grant passed to the Spore, when it was given a root.
    records: Option<RecordsAccess>,
    /// The workspace grant passed to the Spore, when it was given a root.
    vfs_root: Option<PathBuf>,
    /// The principal the host attests for this Spore, when it was told.
    actor: Option<String>,
    /// Grants a child Spore inherits when the launcher opens it.
    launch: LaunchConfig,
    /// One bus per process, shared by every window in it: the wall announces a
    /// prompt and the popup it opens reads the same frame.
    bus: Arc<dyn Bus>,
    /// One roster per process, behind a lock: two windows must not each spawn
    /// an adapter process for the same conversation.
    roster: SharedAgents,
}

/// One window and the Spore instance that draws into it.
struct Surface {
    /// The size and view this window was asked for. Kept for messages: a
    /// process with two windows has to say which one failed.
    spec: SurfaceSpec,
    window: Arc<Window>,
    /// Kept alive for the window's life: a software surface borrows the display
    /// the context opened.
    _context: SoftContext,
    surface: SoftSurface,
    /// Surface extent shared with the presenter; the window owns it.
    extent: Arc<Mutex<(u32, u32)>>,
    tx: mpsc::Sender<CanvasFrame>,
    rx: mpsc::Receiver<CanvasFrame>,
    frame: Option<CanvasFrame>,
    input: Arc<Mutex<VecDeque<InputEvent>>>,
    /// Which splash to draw.
    splash: Splash,
    /// When the window opened, so the splash animation has a clock.
    opened: std::time::Instant,
    /// When the splash last painted, so it runs at a steady rate instead of
    /// spinning the event loop for the whole start-up.
    splashed: std::time::Instant,
    /// The size the last frame was painted at, so the window narrates a
    /// resolution change once instead of every frame.
    painted_size: Option<(u32, u32)>,
    /// Device pixels per surface pixel. The window is sized in points; the Spore
    /// lays out in points too, so pointer events are converted before they reach
    /// it and a click lands where the user aimed.
    scale: f32,
    /// Last surface-space pointer position, so a click can name its location.
    pointer: (f32, f32),
    /// The size the window last reported, and when. A resize is applied once
    /// the drag goes quiet: reflowing the emulator and re-rendering the grid on
    /// every mouse-move is what makes a drag feel broken.
    pending_resize: Option<(std::time::Instant, (u32, u32))>,
    /// Where captured frames are written, when this surface was asked for any.
    capture: Option<PathBuf>,
    /// How many frames the capture wants before the app may exit.
    frames: usize,
    frames_seen: usize,
    started: bool,
    /// Print a frame-rate line once a second: what the guest presents, and what
    /// the window spends drawing it.
    stats: bool,
    painted: u64,
    painted_ms: f64,
    stats_at: std::time::Instant,
}

impl Surface {
    /// Bring up one window and the software surface it presents through.
    fn open(
        event_loop: &ActiveEventLoop,
        spec: &SurfaceSpec,
        title: String,
        splash: Splash,
        input: Arc<Mutex<VecDeque<InputEvent>>>,
        capture: Option<PathBuf>,
        frames: usize,
        stats: bool,
    ) -> Result<Self> {
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(spec.size.0 as f64, spec.size.1 as f64))
            .with_resizable(true);
        let window = Arc::new(event_loop.create_window(attributes).context("window")?);
        let context = SoftContext::new(window.clone())
            .map_err(|error| anyhow::anyhow!("softbuffer context: {error:?}"))?;
        let surface = SoftSurface::new(&context, window.clone())
            .map_err(|error| anyhow::anyhow!("softbuffer surface: {error:?}"))?;

        // `surface.size()` is the coordinate space the Spore lays out in. It is
        // the logical size of the window on every display, so a layout does not
        // change shape when the window moves to a Retina screen; the presenter
        // blits whatever resolution the Spore renders at.
        let scale = window.scale_factor() as f32;
        let inner = window.inner_size();
        let extent = Arc::new(Mutex::new((
            (inner.width as f32 / scale).round() as u32,
            (inner.height as f32 / scale).round() as u32,
        )));
        let (tx, rx) = mpsc::channel();
        let mut open = Self {
            spec: spec.clone(),
            window,
            _context: context,
            surface,
            extent,
            tx,
            rx,
            frame: None,
            input,
            splash,
            opened: std::time::Instant::now(),
            splashed: std::time::Instant::now(),
            painted_size: None,
            scale,
            pointer: (0.0, 0.0),
            pending_resize: None,
            capture,
            frames,
            frames_seen: 0,
            started: false,
            stats,
            painted: 0,
            painted_ms: 0.0,
            stats_at: std::time::Instant::now(),
        };
        let physical = LogicalSize::new(spec.size.0 as f64, spec.size.1 as f64)
            .to_physical::<u32>(open.window.scale_factor());
        open.paint_splash((physical.width, physical.height), 0.0);
        Ok(open)
    }

    /// The view this window was asked to open on, for messages.
    fn label(&self) -> &str {
        self.spec.label()
    }

    fn push(&self, event: InputEvent) {
        let mut queue = self.input.lock().expect("input queue poisoned");
        // Pointer motion is the one event that arrives faster than the guest can
        // draw. Queuing every position makes a hover trail behind the pointer;
        // the guest only ever needs the newest one.
        if matches!(event, InputEvent::Moved { .. }) {
            queue.retain(|queued| !matches!(queued, InputEvent::Moved { .. }));
        }
        queue.push_back(event);
        drop(queue);
        // No redraw request here. The guest wakes within a tick, draws, and its
        // presenter asks for the redraw; asking on every pointer move repainted
        // the last frame at mouse rate for nothing.
    }

    /// Open this surface's Spore. One thread and one store per window: two
    /// windows are two instances of the component, which is what lets a popup
    /// and a wall hold different views of the same roster.
    fn start(&mut self, shared: &Shared, dispatch: DispatchRole) {
        if self.started {
            return;
        }
        self.started = true;
        let extent = self.extent.clone();
        let (width, height) = *extent.lock().expect("surface extent poisoned");
        eprintln!(
            "canvas-winit: {}: starting component at {width}x{height}",
            self.label()
        );
        let binding = Binding {
            agent: self.spec.agent.clone(),
            view: self.spec.view.clone(),
        };
        let label = self.label().to_string();
        let shared = shared.clone();
        let window = self.window.clone();
        let tx = self.tx.clone();
        let input = self.input.clone();
        thread::spawn(move || {
            let result = (|| -> Result<()> {
                let bytes = std::fs::read(&shared.module)
                    .with_context(|| format!("read {}", shared.module.display()))?;
                // A Spore that has not changed should not be compiled again:
                // the first start is seconds, every one after it is a read.
                let runtime =
                    WebGpuRuntime::new()?.with_component_cache(component_cache(&shared.module));
                let catalog = SporeCatalog::new(&shared.spores, shared.launch.clone())
                    .map_err(|error| anyhow::anyhow!("catalog: {error:?}"))?;
                let status = pollster::block_on(runtime.run_component_with(
                    &bytes,
                    HostSetup {
                        presenter: Box::new(FramePresenter {
                            size: extent,
                            tx,
                            window: window.clone(),
                        }),
                        input,
                        interactive: true,
                        catalog: Box::new(catalog),
                        http: shared.http.clone(),
                        records: shared.records.clone(),
                        vfs: shared.vfs_root.clone(),
                        principal: shared
                            .actor
                            .clone()
                            .map(|actor| ::soil::webgpu::Grants::new().actor(actor).principal)
                            .flatten(),
                        // One roster per process: a second window is another
                        // view of the same agents, not another adapter.
                        agents: Box::new(shared.roster.clone()),
                        // The window named its view, so the intent is the
                        // window's even though the roster is shared.
                        binding: Some(binding),
                        // The first window owns dispatch; a popup watches the
                        // announcements the wall publishes.
                        dispatch,
                        // The storefront origin is already in the HTTP policy
                        // `build_http` constructs; this presenter has no agent
                        // cast of its own and needs no single-origin shorthand.
                        store_origin: None,
                        terminal: Box::new(Terminals::new(80, 24, shared.command.clone())),
                        cursor: Some(Box::new(WindowCursor(window))),
                        clipboard: Some(Box::new(SystemClipboard)),
                        bus: shared.bus.clone(),
                        // Search is granted by the desktop root once the thread
                        // surface lands; the window does not guess at one.
                        search: None,
                    },
                ))?;
                eprintln!("canvas-winit: {label}: guest returned {status}");
                anyhow::ensure!(status == 0, "guest returned status {status}");
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("canvas-winit: {error:#}");
            }
        });
    }

    /// Note the window's new size. The extent itself is committed once the drag
    /// has been still for a moment.
    fn resized(&mut self) {
        let size = self.window.inner_size();
        let width = (size.width as f32 / self.scale).round() as u32;
        let height = (size.height as f32 / self.scale).round() as u32;
        if width == 0 || height == 0 {
            return;
        }
        self.pending_resize = Some((std::time::Instant::now(), (width, height)));
    }

    /// Apply a pending resize when the window has stopped moving.
    fn settle_resize(&mut self) {
        let Some((at, size)) = self.pending_resize else {
            return;
        };
        let momentarily = std::time::Duration::from_millis(120);
        if at.elapsed() < momentarily {
            return;
        }
        self.pending_resize = None;
        if let Ok(mut extent) = self.extent.lock() {
            *extent = size;
        }
        self.window.request_redraw();
    }

    /// How soon this window needs the loop back, if at all.
    fn next_wake(&self) -> Option<std::time::Duration> {
        if self.frame.is_none() {
            Some(std::time::Duration::from_millis(16))
        } else if self.pending_resize.is_some() {
            Some(std::time::Duration::from_millis(50))
        } else {
            None
        }
    }

    /// Take every frame the Spore has published. Returns true when this
    /// surface's capture is complete, which ends the run.
    fn take_frames(&mut self) -> bool {
        while let Ok(frame) = self.rx.try_recv() {
            if self.capture.is_some() {
                eprintln!("canvas-winit: frame {}x{}", frame.width, frame.height);
            }
            if let Some(path) = &self.capture {
                // One frame is a still; several are a sequence, and they cannot
                // share a path or every write lands on the last one. A sequence
                // writes `x.rgba` as `x.0000.rgba`, `x.0001.rgba`, so an
                // animation can be assembled from what was actually drawn.
                let path = if self.frames > 1 {
                    sequence_path(path, self.frames_seen)
                } else {
                    path.clone()
                };
                if let Err(error) = std::fs::write(&path, &frame.rgba) {
                    eprintln!("canvas-winit: write {}: {error}", path.display());
                }
            }
            self.frames_seen += 1;
            self.frame = Some(frame);
            if self.frames_seen >= self.frames {
                return true;
            }
        }
        false
    }

    /// What the window shows until the Spore's first frame arrives.
    ///
    /// Starting a Spore is not instant: a component compiles, a wgpu device
    /// comes up, and text shaping loads fonts. The mark breathes through that
    /// wait with a lissajous pair the way the fungus draws itself, so the pause
    /// reads as starting rather than as hung.
    fn paint_splash(&mut self, (width, height): (u32, u32), phase: f32) {
        let (Some(width), Some(height)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        if self.surface.resize(width, height).is_err() {
            return;
        }
        let Ok(mut buffer) = self.surface.buffer_mut() else {
            return;
        };
        let (width_px, height_px) = (width.get(), height.get());
        let opened = self.opened;
        buffer.fill(BACKGROUND);
        match self.splash {
            // A small, slow figure, drawn thick enough to read as a mark rather
            // than a wireframe: no logo, nothing to wait for but motion.
            Splash::Flow => {
                // One figure that morphs: each Lissajous ratio and phase is a
                // symbol, and the mark eases from one to the next instead of
                // cutting between them.
                let symbols = [
                    (3.0f32, 2.0f32, 0.0f32),
                    (5.0, 4.0, std::f32::consts::FRAC_PI_4),
                    (2.0, 3.0, std::f32::consts::FRAC_PI_2),
                    (5.0, 2.0, std::f32::consts::FRAC_PI_3),
                    (4.0, 3.0, std::f32::consts::FRAC_PI_6),
                ];
                let hold = 1.6f32;
                let travel = opened.elapsed().as_secs_f32() / hold;
                let index = travel.floor() as usize;
                let t = travel.fract();
                let ease = t * t * (3.0 - 2.0 * t);
                let (a0, b0, d0) = symbols[index % symbols.len()];
                let (a1, b1, d1) = symbols[(index + 1) % symbols.len()];
                let lerp = |from: f32, to: f32| from + (to - from) * ease;
                draw_lissajous(
                    &mut buffer,
                    width_px,
                    height_px,
                    0.0,
                    SplashCurve {
                        a: lerp(a0, a1),
                        b: lerp(b0, b1),
                        delta: lerp(d0, d1),
                        radius: 0.24,
                        color: ACCENT_CURVE,
                        weight: 0.85,
                        spin: 0.0,
                    },
                );
            }
            Splash::Logo => {
                draw_lissajous(
                    &mut buffer,
                    width_px,
                    height_px,
                    phase,
                    SplashCurve {
                        a: 3.0,
                        b: 2.0,
                        delta: std::f32::consts::FRAC_PI_2,
                        radius: 0.42,
                        color: ACCENT_CURVE,
                        weight: 1.0,
                        spin: 0.0,
                    },
                );
                draw_lissajous(
                    &mut buffer,
                    width_px,
                    height_px,
                    phase,
                    SplashCurve {
                        a: 5.0,
                        b: 4.0,
                        delta: 0.0,
                        radius: 0.30,
                        color: ACCENT_GLOW,
                        weight: 1.0,
                        spin: 0.0,
                    },
                );
            }
        }
        if self.splash == Splash::Logo
            && let Some(logo) = logo_mark()
        {
            let side = 220.min(width_px).min(height_px);
            let scaled =
                image::imageops::resize(logo, side, side, image::imageops::FilterType::Lanczos3);
            let left = (width_px - side) / 2;
            let top = (height_px - side) / 2;
            for (x, y, pixel) in scaled.enumerate_pixels() {
                // The mark is an app, not a picture: mask it to the same
                // rounded square the Dock icon uses, so the window and the Dock
                // show one shape at every size.
                let coverage = squircle_coverage(x, y, side);
                if coverage <= 0.0 {
                    continue;
                }
                let Some(destination) = buffer.get_mut(((top + y) * width_px + left + x) as usize)
                else {
                    continue;
                };
                let alpha = u32::from(pixel[3]) as f32 * coverage;
                if alpha <= 0.0 {
                    continue;
                }
                let alpha = alpha.round() as u32;
                let blend = |foreground: u8, background: u8| {
                    ((u32::from(foreground) * alpha + u32::from(background) * (255 - alpha)) / 255)
                        as u8
                };
                let background = *destination;
                let r = blend(pixel[0], (background >> 16) as u8);
                let g = blend(pixel[1], (background >> 8) as u8);
                let b = blend(pixel[2], background as u8);
                *destination = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
            }
        }
        if let Err(error) = buffer.present() {
            eprintln!("canvas-winit: splash: {error}");
        }
    }

    fn paint(&mut self) {
        let started = std::time::Instant::now();
        self.paint_inner();
        self.painted += 1;
        self.painted_ms += started.elapsed().as_secs_f64() * 1000.0;
        if self.stats && self.stats_at.elapsed() >= std::time::Duration::from_secs(1) {
            let seconds = self.stats_at.elapsed().as_secs_f64();
            eprintln!(
                "canvas-winit: {}: {:.1} fps presented, {:.1} ms paint",
                self.label(),
                self.painted as f64 / seconds,
                self.painted_ms / self.painted.max(1) as f64
            );
            self.painted = 0;
            self.painted_ms = 0.0;
            self.stats_at = std::time::Instant::now();
        }
    }

    fn paint_inner(&mut self) {
        let Some(frame) = self.frame.as_ref() else {
            return;
        };
        // The window decides the resolution; the Spore decides the layout. A
        // frame rendered at a different pixel count than the window is scaled
        // here rather than cropped, so a Spore is never clipped by a display it
        // did not know about.
        let size = self.window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        let scaled;
        let rgba: &[u8] = if (frame.width, frame.height) == (size.width, size.height) {
            frame.rgba.as_slice()
        } else {
            let Some(source) =
                image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
            else {
                return;
            };
            scaled = image::imageops::resize(
                &source,
                size.width,
                size.height,
                image::imageops::FilterType::Triangle,
            );
            scaled.as_raw().as_slice()
        };
        if let Err(error) = self.surface.resize(width, height) {
            eprintln!("canvas-winit: resize: {error}");
            return;
        }
        let mut buffer = match self.surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(error) => {
                eprintln!("canvas-winit: buffer: {error}");
                return;
            }
        };
        for (pixel, rgba) in buffer.iter_mut().zip(rgba.chunks_exact(4)) {
            *pixel = (u32::from(rgba[0]) << 16) | (u32::from(rgba[1]) << 8) | u32::from(rgba[2]);
        }
        if let Err(error) = buffer.present() {
            eprintln!("canvas-winit: present: {error}");
        } else if self.painted_size != Some((size.width, size.height)) {
            // Once per size, not once per frame: a live window paints at 60 Hz
            // and the terminal that started it is not a frame log.
            self.painted_size = Some((size.width, size.height));
            eprintln!(
                "canvas-winit: {}: painted {}x{} into {}x{}",
                self.label(),
                frame.width,
                frame.height,
                size.width,
                size.height
            );
        }
    }
}

/// Bring up another launcher. It is a separate app instance, so it gets its own
/// window without disturbing the surfaces in this one.
fn open_launcher() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let Some(bundle) = bundle_root(&executable) else {
        // A bare binary has no launcher bundle to open; the shortcut is only
        // meaningful for the app.
        return;
    };
    // The launcher is the app bundle itself: no module argument means "show me
    // the Spores".
    let mut command = std::process::Command::new("/usr/bin/open");
    command.arg("-n").arg(bundle);
    if let Ok(mut child) = command.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

struct App {
    shared: Shared,
    /// Every surface this process was asked to open, in order. The first owns
    /// dispatch; the rest watch the announcements it publishes.
    specs: Vec<SurfaceSpec>,
    surfaces: HashMap<WindowId, Surface>,
    splash: Splash,
    modifiers: ModifiersState,
    /// Input scripted on the command line, delivered to the first surface.
    seed: Option<Arc<Mutex<VecDeque<InputEvent>>>>,
    stats: bool,
    /// Where the first surface writes frames, and how many it writes before
    /// the run ends. One capture belongs to one window; two would race for the
    /// same path.
    capture: Option<PathBuf>,
    frames: usize,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.surfaces.is_empty() {
            return;
        }
        for (index, spec) in self.specs.clone().into_iter().enumerate() {
            let input = match index {
                0 => self
                    .seed
                    .take()
                    .unwrap_or_else(|| Arc::new(Mutex::new(VecDeque::new()))),
                _ => Arc::new(Mutex::new(VecDeque::new())),
            };
            // A capture is one file for one window; asking two surfaces to
            // write the same frames would only race them.
            let capture = (index == 0).then(|| self.capture.clone()).flatten();
            // The frame budget belongs to the capture, not to every window: a
            // popup that paints faster must not end a wall's capture early.
            let frames = if index == 0 { self.frames } else { usize::MAX };
            let title = match &spec.view {
                Some(view) => format!("{} · {view}", window_title(&self.shared.module)),
                None => window_title(&self.shared.module),
            };
            let mut surface = match Surface::open(
                event_loop,
                &spec,
                title,
                self.splash,
                input,
                capture,
                frames,
                self.stats,
            ) {
                Ok(surface) => surface,
                Err(error) => {
                    eprintln!("canvas-winit: {}: {error:#}", spec.label());
                    event_loop.exit();
                    return;
                }
            };
            let dispatch = if index == 0 {
                soil::webgpu::session_role(self.shared.edgerunner.as_deref())
            } else {
                soil::webgpu::watch_only()
            };
            surface.start(&self.shared, dispatch);
            let id = surface.window.id();
            self.surfaces.insert(id, surface);
        }
        warm_installed(&self.shared.spores, &self.shared.module);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if matches!(event, WindowEvent::CloseRequested) {
            // One window closing is not the app closing while another is open.
            self.surfaces.remove(&window_id);
            if self.surfaces.is_empty() {
                event_loop.exit();
            }
            return;
        }
        if let WindowEvent::ModifiersChanged(modifiers) = event {
            self.modifiers = modifiers.state();
            return;
        }
        let Some(surface) = self.surfaces.get_mut(&window_id) else {
            return;
        };
        match event {
            WindowEvent::RedrawRequested => {
                surface.settle_resize();
                if surface.take_frames() {
                    event_loop.exit();
                    return;
                }
                if surface.frame.is_some() {
                    surface.paint();
                } else {
                    // Sixty frames a second: the sweep is the only thing
                    // moving, and a stepping animation reads as a stutter.
                    if surface.splashed.elapsed() >= std::time::Duration::from_millis(16) {
                        surface.splashed = std::time::Instant::now();
                        let size = surface.window.inner_size();
                        let phase = surface.opened.elapsed().as_secs_f32();
                        surface.paint_splash((size.width, size.height), phase);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed
                    && (self.modifiers.super_key() || self.modifiers.control_key())
                    && matches!(
                        event.logical_key,
                        WinitKey::Character(ref text) if matches!(text.as_str(), "o" | "l")
                    )
                {
                    open_launcher();
                }
                if event.state == ElementState::Pressed {
                    let key = key_name(&event.logical_key);
                    let text = event.text.as_ref().map(|text| text.to_string());
                    surface.push(InputEvent::Key(InputKey {
                        key,
                        text,
                        pressed: true,
                        shift: self.modifiers.shift_key(),
                        control: self.modifiers.control_key(),
                        alt: self.modifiers.alt_key(),
                        meta: self.modifiers.super_key(),
                    }));
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                surface.scale = scale_factor as f32;
                surface.resized();
            }
            WindowEvent::Resized(_) => surface.resized(),
            WindowEvent::CursorMoved { position, .. } => {
                surface.pointer = (
                    (position.x as f32) / surface.scale,
                    (position.y as f32) / surface.scale,
                );
                surface.push(InputEvent::Moved {
                    x: surface.pointer.0,
                    y: surface.pointer.1,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed {
                    // A click on a terminal means "type here". The window has to
                    // be key before any of that reaches the Spore.
                    surface.window.focus_window();
                }
                let Some(button) = pointer_button(button) else {
                    return;
                };
                surface.push(InputEvent::Button {
                    x: surface.pointer.0,
                    y: surface.pointer.1,
                    button,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // One notch is a terminal row or a list row; a pixel-only
                // device is converted so the guest never sees raw pixels.
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, vertical) => vertical,
                    MouseScrollDelta::PixelDelta(position) => (position.y as f32) / 20.0,
                };
                surface.push(InputEvent::Scrolled {
                    x: surface.pointer.0,
                    y: surface.pointer.1,
                    lines,
                });
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let mut wake: Option<std::time::Duration> = None;
        for surface in self.surfaces.values_mut() {
            surface.settle_resize();
            // Come back on a clock while there is something to animate on any
            // window: the splash before its first frame, and a resize settling.
            // With a frame on every window and nothing moving, wait for an
            // event instead of spinning.
            if let Some(next) = surface.next_wake() {
                wake = Some(wake.map_or(next, |current| current.min(next)));
            }
            let before = surface.frames_seen;
            if surface.take_frames() {
                event_loop.exit();
                return;
            }
            if surface.frames_seen > before || surface.frame.is_none() {
                // Before the first frame the splash is the only thing on screen,
                // so it keeps its own clock; afterwards a redraw means a frame.
                surface.window.request_redraw();
            }
        }
        match wake {
            Some(after) => event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                std::time::Instant::now() + after,
            )),
            None => event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait),
        }
    }
}

/// Compile the Spores this machine has, in the background, so opening one from
/// the launcher does not pay for it. The module already running is skipped; a
/// Spore whose cache is warm costs a file read and a deserialize to check.
fn warm_installed(roots: &[std::path::PathBuf], running: &std::path::Path) {
    let mut packages = Vec::new();
    for root in roots {
        let Ok(directory) = std::fs::read_dir(root) else {
            continue;
        };
        packages.extend(directory.flatten().map(|entry| entry.path()));
    }
    let modules: Vec<std::path::PathBuf> = packages
        .into_iter()
        .filter(|package| package.join("spore.toml").is_file())
        .filter_map(|package| {
            let manifest = Manifest::load(&package).ok()?;
            let module = manifest.module?;
            Some(package.join(module.path))
        })
        .filter(|module| module != running)
        .collect();
    if modules.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for module in modules {
            let Ok(bytes) = std::fs::read(&module) else {
                continue;
            };
            let Ok(runtime) = WebGpuRuntime::new() else {
                continue;
            };
            let runtime = runtime.with_component_cache(component_cache(&module));
            if let Err(error) = runtime.prewarm(&bytes) {
                eprintln!("canvas-winit: warm {}: {error:#}", module.display());
            }
        }
    });
}

/// Where compiled Spores are kept. Outside the bundle, because a cache is not
/// part of what was installed, and keyed by module digest so a new build is a
/// new file rather than a stale hit.
fn component_cache(module: &std::path::Path) -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let base = match home {
        Some(home) => home.join("Library/Caches/dev.fungos.launcher"),
        None => module
            .parent()
            .map(|parent| parent.join(".components"))
            .unwrap_or_else(|| std::path::PathBuf::from(".components")),
    };
    base
}

/// What the window calls itself.
///
/// An installed Spore names itself in its manifest, which is the same source the
/// launcher lists it from; a bare module falls back to its file name. Either way
/// the user sees "Launcher" or "Term", never "canvas-winit — launcher.wasm".
fn window_title(module: &std::path::Path) -> String {
    // A Spore bundle names itself beside its module.
    if let Some(title) = module
        .parent()
        .map(|resources| resources.join("title.txt"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|title| title.trim().to_owned())
        .filter(|title| !title.is_empty())
    {
        return title;
    }
    let manifest = module
        .ancestors()
        .skip(1)
        .find(|directory| directory.join("spore.toml").is_file())
        .and_then(|directory| Manifest::load(directory).ok());
    if let Some(manifest) = manifest {
        if let Some(title) = manifest
            .spore
            .title
            .clone()
            .filter(|title| !title.trim().is_empty())
        {
            return title;
        }
        return soil_spore::display_title(&manifest.spore.name);
    }
    let stem = module
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("spore");
    soil_spore::display_title(stem)
}

fn point(value: String) -> Result<(f32, f32)> {
    let (x, y) = value
        .split_once(',')
        .with_context(|| format!("{value} is not X,Y"))?;
    Ok((
        x.trim().parse().context("X is not a number")?,
        y.trim().parse().context("Y is not a number")?,
    ))
}

/// The colour a window sits on before its Spore draws anything.
const BACKGROUND: u32 = 0x0c10_15;

/// What the window shows while a Spore starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Splash {
    /// The mark, masked like the Dock icon, over two thin curves.
    Logo,
    /// No mark: a small, thick lissajous figure that moves.
    Flow,
}
/// The two hues the mark draws itself with: the fungal line and its glow.
const ACCENT_CURVE: (f32, f32, f32) = (0.60, 0.80, 0.52);
const ACCENT_GLOW: (f32, f32, f32) = (0.36, 0.55, 0.46);

/// One parametric curve: x = sin(a·t + δ), y = sin(b·t).
struct SplashCurve {
    a: f32,
    b: f32,
    delta: f32,
    /// Fraction of the short side the figure spans.
    radius: f32,
    color: (f32, f32, f32),
    /// Line weight: 1 draws a hairline, 3 draws a stroke.
    weight: f32,
    /// Rotation of the whole figure, in radians. Changing the phase morphs a
    /// Lissajous; rotating it is what reads as spinning.
    spin: f32,
}

fn draw_lissajous(buffer: &mut [u32], width: u32, height: u32, phase: f32, curve: SplashCurve) {
    let (half_w, half_h) = (width as f32 / 2.0, height as f32 / 2.0);
    let radius = curve.radius * width.min(height) as f32;
    // A stroke, not a hairline: the figure is the only thing on screen.
    let stroke = 1.2 + curve.weight.max(0.2) * 4.0;
    let (sin, cos) = (curve.spin.sin(), curve.spin.cos());
    let point = |t: f32| {
        let x = radius * (curve.a * t + curve.delta).sin();
        let y = radius * (curve.b * t).sin();
        (half_w + x * cos - y * sin, half_h + x * sin + y * cos)
    };
    // Dense samples, each drawn as an antialiased capsule: a splat of pixels
    // would show stair-steps at this angle, and the figure is the only thing on
    // screen while a Spore starts. Enough samples for a smooth curve, few
    // enough to hold 60 fps on one core.
    let steps = 1200;
    let mut previous = point(phase);
    for step in 1..=steps {
        let t = step as f32 / steps as f32 * std::f32::consts::TAU + phase;
        let current = point(t);
        draw_capsule(
            buffer,
            width,
            height,
            previous,
            current,
            stroke,
            curve.color,
        );
        previous = current;
    }
}

/// One antialiased segment: every pixel near the line gets coverage from its
/// distance to it, and the colour is added so crossings glow.
fn draw_capsule(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    from: (f32, f32),
    to: (f32, f32),
    half_width: f32,
    color: (f32, f32, f32),
) {
    let reach = half_width + 1.0;
    let min_x = (from.0.min(to.0) - reach).floor().max(0.0) as i32;
    let max_x = (from.0.max(to.0) + reach).ceil().min(width as f32 - 1.0) as i32;
    let min_y = (from.1.min(to.1) - reach).floor().max(0.0) as i32;
    let max_y = (from.1.max(to.1) + reach).ceil().min(height as f32 - 1.0) as i32;
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length_squared = (dx * dx + dy * dy).max(f32::EPSILON);
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance from the pixel centre to the segment.
            let t = (((px - from.0) * dx + (py - from.1) * dy) / length_squared).clamp(0.0, 1.0);
            let (cx, cy) = (from.0 + t * dx, from.1 + t * dy);
            let distance = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let coverage = (half_width + 0.5 - distance).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let Some(pixel) = buffer.get_mut((y as u32 * width + x as u32) as usize) else {
                continue;
            };
            let add = |channel: u32, value: f32| {
                let current = channel as f32 / 255.0;
                ((current + value * coverage * 0.16).min(1.0) * 255.0).round() as u32
            };
            let r = add((*pixel >> 16) & 0xff, color.0);
            let g = add((*pixel >> 8) & 0xff, color.1);
            let b = add(*pixel & 0xff, color.2);
            *pixel = (r << 16) | (g << 8) | b;
        }
    }
}

/// The mark, decoded once: the splash redraws many times while a Spore starts.
fn logo_mark() -> Option<&'static image::RgbaImage> {
    static MARK: std::sync::OnceLock<Option<image::RgbaImage>> = std::sync::OnceLock::new();
    MARK.get_or_init(|| {
        image::load_from_memory(include_bytes!("../assets/fungos.png"))
            .ok()
            .map(|image| image.to_rgba8())
    })
    .as_ref()
}

/// Coverage of the rounded square an app icon is cut to, 0 outside and 1 well
/// inside. A superellipse would be more faithful to macOS; a rounded rect is
/// what every icon looks like at the sizes this draws.
fn squircle_coverage(x: u32, y: u32, side: u32) -> f32 {
    let side = side as f32;
    let radius = side * 0.2237;
    let half = side / 2.0;
    let px = x as f32 + 0.5 - half;
    let py = y as f32 + 0.5 - half;
    let qx = (px.abs() - (half - radius)).max(0.0);
    let qy = (py.abs() - (half - radius)).max(0.0);
    let distance = (qx * qx + qy * qy).sqrt() - radius;
    (0.5 - distance).clamp(0.0, 1.0)
}

fn drag_point(value: String) -> Result<((f32, f32), (f32, f32))> {
    let mut parts = value.split(',');
    let mut next = || -> Result<f32> {
        parts
            .next()
            .context("expected four numbers")?
            .trim()
            .parse()
            .context("not a number")
    };
    Ok(((next()?, next()?), (next()?, next()?)))
}

fn pointer_button(button: MouseButton) -> Option<PointerButton> {
    match button {
        MouseButton::Left => Some(PointerButton::Left),
        MouseButton::Right => Some(PointerButton::Right),
        MouseButton::Middle => Some(PointerButton::Middle),
        _ => None,
    }
}

/// The system pasteboard. `pbcopy` is the whole of it: the host is the only
/// thing that touches the system, and a Spore cannot see what it wrote.
struct SystemClipboard;

impl ClipboardHost for SystemClipboard {
    fn set_text(&mut self, text: String) -> Result<(), String> {
        use std::io::Write as _;
        let mut child = std::process::Command::new("/usr/bin/pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| format!("pbcopy: {error}"))?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(text.as_bytes())
                .map_err(|error| format!("pbcopy stdin: {error}"))?;
        }
        child
            .wait()
            .map(|status| status.success())
            .map_err(|error| format!("pbcopy: {error}"))?
            .then_some(())
            .ok_or_else(|| "pbcopy reported failure".to_owned())
    }
}

/// Applies the pointer shape a Spore asks for. The guest never sees a cursor or
/// a window; it only says what it is pointing at.
struct WindowCursor(Arc<Window>);

impl CursorHost for WindowCursor {
    fn set_cursor(&mut self, style: CursorStyle) {
        self.0.set_cursor(cursor_icon(style));
    }
}

fn cursor_icon(style: CursorStyle) -> CursorIcon {
    match style {
        CursorStyle::Default => CursorIcon::Default,
        CursorStyle::Text => CursorIcon::Text,
        CursorStyle::Pointer => CursorIcon::Pointer,
        CursorStyle::Crosshair => CursorIcon::Crosshair,
        CursorStyle::Move => CursorIcon::Move,
        CursorStyle::Grab => CursorIcon::Grab,
        CursorStyle::Grabbing => CursorIcon::Grabbing,
        CursorStyle::NoDrop => CursorIcon::NoDrop,
        CursorStyle::Wait => CursorIcon::Wait,
        CursorStyle::Help => CursorIcon::Help,
    }
}

fn key_name(key: &WinitKey) -> String {
    match key {
        WinitKey::Named(NamedKey::Enter) => "enter".into(),
        WinitKey::Named(NamedKey::Escape) => "escape".into(),
        WinitKey::Named(NamedKey::ArrowDown) => "arrow-down".into(),
        WinitKey::Named(NamedKey::ArrowUp) => "arrow-up".into(),
        WinitKey::Named(NamedKey::ArrowLeft) => "arrow-left".into(),
        WinitKey::Named(NamedKey::ArrowRight) => "arrow-right".into(),
        WinitKey::Named(NamedKey::Space) => "space".into(),
        WinitKey::Named(NamedKey::Tab) => "tab".into(),
        WinitKey::Named(NamedKey::Backspace) => "backspace".into(),
        WinitKey::Character(text) => text.to_string(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn build_http(launch: &LaunchConfig) -> Result<Option<HttpAccess>> {
    match (launch.geode_socket.as_ref(), launch.geode_purpose.as_ref()) {
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("--geode-socket and --geode-purpose must be used together");
        }
        _ => {}
    }
    // The storefront is the one origin every presenter grants. A Spore that
    // needs another origin says so at launch; the launcher does not guess.
    let mut origins = launch.origins.clone();
    origins.push(soil::webgpu::LOCAL_STORE_ORIGIN.to_string());
    if launch.hue_bridge.is_some() {
        origins.push("http://hue.local".to_string());
    }
    let policy = HttpPolicy::new(
        origins,
        16 * 1024 * 1024,
        std::time::Duration::from_secs(30),
    )
    .context("http policy")?;
    let mut access = HttpAccess::new(policy).with_transport(Arc::new(HueTransport {
        bridge: launch.hue_bridge.clone(),
        inner: DirectTransport,
    }));
    if let (Some(socket), Some(purpose)) = (&launch.geode_socket, &launch.geode_purpose) {
        #[cfg(unix)]
        {
            access = access.with_credentials(Arc::new(GeodeCredentials::new(
                socket.clone(),
                purpose.clone(),
            )));
        }
        #[cfg(not(unix))]
        {
            let _ = (socket, purpose);
            anyhow::bail!("--geode-socket is only available on Unix hosts");
        }
    }
    Ok(Some(access))
}

/// One window: its own resolution, and the view it should open on.
///
/// A wall and a popup are different shapes. The wall is a phone because it is
/// read at a glance from a desk; a popup is a card because it exists to be
/// answered and then go away. Nothing in the Spore assumes one size — it lays
/// out in `surface::size()` — so the resolution belongs to the window, not to
/// the module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceSpec {
    /// The window's opening size in points.
    pub size: (u32, u32),
    /// The view to open on, as the Spore names it (`grid`, `popup`, ...).
    pub view: Option<String>,
    /// The agent to open on, when the surface is about one agent.
    pub agent: Option<String>,
}

impl SurfaceSpec {
    /// What to call this surface in a message: the view it opens on, or the
    /// wall when it was not told.
    pub fn label(&self) -> &str {
        self.view.as_deref().unwrap_or("wall")
    }

    /// `420x300:view=popup:agent=op-7f3a`, or `430x680` for a plain wall.
    pub fn parse(text: &str) -> Result<Self> {
        let mut parts = text.split(':');
        let size = parts.next().context("surface needs a WxH size")?;
        let (width, height) = size
            .split_once('x')
            .context("surface size is WxH, e.g. 430x680")?;
        let size = (
            width.parse::<u32>().context("surface width")?,
            height.parse::<u32>().context("surface height")?,
        );
        anyhow::ensure!(size.0 > 0 && size.1 > 0, "surface size must be positive");
        let mut view = None;
        let mut agent = None;
        for part in parts {
            match part.split_once('=') {
                Some(("view", value)) if known_view(value) => {
                    view = Some(value.to_string());
                }
                Some(("agent", value)) if !value.is_empty() => {
                    agent = Some(value.to_string());
                }
                _ => anyhow::bail!("surface setting {part:?} is not view= or agent="),
            }
        }
        Ok(Self { size, view, agent })
    }
}

/// Views a host may open a surface on. A typo is a startup error, not a window
/// that silently opens on the grid.
fn known_view(name: &str) -> bool {
    matches!(
        name,
        "grid" | "board" | "roster" | "stage" | "focus" | "thread" | "popup"
    )
}

#[cfg(test)]
mod surface_tests {
    use super::SurfaceSpec;

    #[test]
    fn a_size_alone_is_a_wall() {
        let spec = SurfaceSpec::parse("430x680").unwrap();
        assert_eq!(spec.size, (430, 680));
        assert_eq!(spec.view, None);
    }

    #[test]
    fn a_popup_carries_its_view_and_agent() {
        let spec = SurfaceSpec::parse("420x300:view=popup:agent=op-7f3a").unwrap();
        assert_eq!(spec.size, (420, 300));
        assert_eq!(spec.view.as_deref(), Some("popup"));
        assert_eq!(spec.agent.as_deref(), Some("op-7f3a"));
    }

    #[test]
    fn a_typo_is_refused_rather_than_opened() {
        assert!(SurfaceSpec::parse("420x300:view=popupp").is_err());
        assert!(SurfaceSpec::parse("420x300:volume=up").is_err());
        assert!(SurfaceSpec::parse("420").is_err());
        assert!(SurfaceSpec::parse("0x300").is_err());
    }
}

/// `dir/frame.rgba` -> `dir/frame.0007.rgba`.
fn sequence_path(path: &std::path::Path, index: usize) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "frame".to_string());
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rgba".to_string());
    path.with_file_name(format!("{stem}.{index:04}.{extension}"))
}

fn main() -> Result<()> {
    let mut module = None;
    let mut capture = None;
    let mut frames = None;
    let mut key = None;
    let mut keys = None;
    let mut surfaces: Vec<SurfaceSpec> = Vec::new();
    let mut agents = None;
    let mut acp_cmd = None;
    let mut edgerunner = None;
    let mut stats = false;
    let mut command = None;
    let mut pointer = None;
    let mut click = None;
    let mut drag = None;
    let mut after: Option<u64> = None;
    let mut splash = Splash::Flow;
    let mut prewarm = false;
    let mut spores = None;
    let mut logical_size = LOGICAL_SIZE;
    let mut size_given = false;
    let mut origins = Vec::new();
    let mut geode_socket = None;
    let mut geode_purpose = None;
    let mut hue_bridge = None;
    let mut records_root = None;
    let mut vfs_root: Option<PathBuf> = None;
    let mut records_book: Option<String> = None;
    let mut records_collection: Option<String> = None;
    let mut actor: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--capture" => {
                capture = Some(PathBuf::from(
                    args.next().context("--capture needs a path")?,
                ));
            }
            "--frames" => {
                frames = Some(
                    args.next()
                        .context("--frames needs a count")?
                        .parse()
                        .context("--frames needs a number")?,
                );
            }
            "--surface" => {
                let text = args.next().context("--surface needs WxH:view=..")?;
                surfaces.push(SurfaceSpec::parse(&text)?);
            }
            "--agents" => {
                let value = args.next().context("--agents needs a source")?;
                if !matches!(value.as_str(), "demo" | "acp" | "all") {
                    anyhow::bail!("--agents: unknown source {value:?} (demo, acp, all)");
                }
                agents = Some(value);
            }
            "--acp-cmd" => acp_cmd = Some(args.next().context("--acp-cmd needs a command")?),
            "--edgerunner" => {
                edgerunner = Some(args.next().context("--edgerunner needs a URL")?)
            }
            "--key" => key = Some(args.next().context("--key needs a key name")?),
            "--keys" => keys = Some(args.next().context("--keys needs text")?),
            "--command" => command = Some(args.next().context("--command needs a command line")?),
            "--pointer" => pointer = Some(point(args.next().context("--pointer needs X,Y")?)?),
            "--click" => click = Some(point(args.next().context("--click needs X,Y")?)?),
            "--drag" => {
                drag = Some(drag_point(
                    args.next().context("--drag needs X1,Y1,X2,Y2")?,
                )?)
            }
            "--prewarm" => prewarm = true,
            "--stats" => stats = true,
            "--splash" => {
                splash = match args.next().context("--splash needs logo or flow")?.as_str() {
                    "logo" => Splash::Logo,
                    "flow" => Splash::Flow,
                    other => anyhow::bail!("--splash takes logo or flow, not {other}"),
                }
            }
            "--after" => {
                after = Some(
                    args.next()
                        .context("--after needs milliseconds")?
                        .parse()
                        .context("--after needs a number")?,
                )
            }
            "--spores" => {
                spores = Some(PathBuf::from(
                    args.next().context("--spores needs a directory")?,
                ));
            }
            "--size" => {
                let value = args.next().context("--size needs WxH")?;
                let (width, height) = value.split_once('x').context("--size needs WxH")?;
                let width: f64 = width.parse().context("--size needs numbers")?;
                let height: f64 = height.parse().context("--size needs numbers")?;
                if width < 1.0 || height < 1.0 {
                    anyhow::bail!("--size needs positive numbers");
                }
                logical_size = (width, height);
                size_given = true;
            }
            "--allow-origin" | "--store-origin" => {
                origins.push(args.next().context("--allow-origin needs a URL")?);
            }
            "--geode-socket" => {
                geode_socket = Some(PathBuf::from(
                    args.next().context("--geode-socket needs a path")?,
                ));
            }
            "--geode-purpose" => {
                geode_purpose = Some(args.next().context("--geode-purpose needs a purpose")?);
            }
            "--hue-bridge" => {
                let value = args.next().context("--hue-bridge needs a URL")?;
                let url = Url::parse(&value).context("--hue-bridge needs an absolute URL")?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    anyhow::bail!("--hue-bridge needs an absolute http(s) URL");
                }
                hue_bridge = Some(url);
            }
            "--vfs-root" => {
                let path = args.next().context("--vfs-root needs a directory")?;
                vfs_root = Some(PathBuf::from(path));
            }
            "--records-book" => {
                let value = args.next().context("--records-book needs a book name")?;
                records_book = Some(value);
            }
            "--records-collection" => {
                let value = args
                    .next()
                    .context("--records-collection needs a collection name")?;
                records_collection = Some(value);
            }
            "--actor" => {
                let value = args.next().context("--actor needs a principal name")?;
                actor = Some(value);
            }
            "--records-root" => {
                records_root = Some(PathBuf::from(
                    args.next().context("--records-root needs a directory")?,
                ));
            }
            value if module.is_none() => module = Some(PathBuf::from(value)),
            _ => anyhow::bail!(
                "usage: canvas-winit <module.wasm> [--capture FILE.rgba] [--frames N] [--surface WxH:view=NAME] [--agents demo|acp|all] [--acp-cmd CMD] [--edgerunner URL] [--key KEY] [--keys TEXT] [--command LINE] [--pointer X,Y] [--click X,Y] [--drag X1,Y1,X2,Y2] [--after MS] [--spores DIR] [--size WxH] [--allow-origin URL]... [--geode-socket PATH --geode-purpose PURPOSE] [--hue-bridge URL] [--records-root DIR]"
            ),
        }
    }
    let module = match module {
        Some(module) => module,
        None => {
            // A .app bundle runs this binary directly; the launcher module sits
            // in Contents/Resources next to Contents/MacOS.
            let executable = std::env::current_exe().context("locate the executable")?;
            let resources = executable
                .parent()
                .and_then(|macos| macos.parent())
                .map(|contents| contents.join("Resources"))
                .context("locate the bundle Resources directory")?;
            // A per-Spore bundle carries its module as `spore.wasm`; the
            // launcher's own bundle carries `launcher.wasm`.
            let candidate = {
                let spore = resources.join("spore.wasm");
                if spore.is_file() {
                    spore
                } else {
                    resources.join("launcher.wasm")
                }
            };
            if !candidate.is_file() {
                anyhow::bail!(
                    "usage: canvas-winit <module.wasm> [--capture FILE.rgba] [--frames N] [--surface WxH:view=NAME] [--agents demo|acp|all] [--acp-cmd CMD] [--edgerunner URL] [--key KEY] [--keys TEXT] [--command LINE] [--pointer X,Y] [--click X,Y] [--drag X1,Y1,X2,Y2] [--after MS] [--spores DIR] [--size WxH] [--allow-origin URL]... [--geode-socket PATH --geode-purpose PURPOSE] [--hue-bridge URL] [--vfs-root DIR] [--records-root DIR] [--records-book NAME --records-collection NAME] [--actor WHO]"
                );
            }
            candidate
        }
    };
    // Compile and cache, then exit: no window, no device, no Spore.
    if prewarm {
        let bytes = std::fs::read(&module).with_context(|| format!("read {}", module.display()))?;
        let runtime = WebGpuRuntime::new()?.with_component_cache(component_cache(&module));
        runtime.prewarm(&bytes)?;
        println!("canvas-winit: warmed {}", module.display());
        return Ok(());
    }
    let input = Arc::new(Mutex::new(VecDeque::new()));
    if let Some(key) = key {
        let text = (key.chars().count() == 1).then(|| key.clone());
        input
            .lock()
            .expect("input queue poisoned")
            .push_back(InputEvent::Key(InputKey {
                key,
                text,
                pressed: true,
                shift: false,
                control: false,
                alt: false,
                meta: false,
            }));
    }
    // Input seeded before the window exists is delivered the moment the guest
    // starts, which is before the shell has painted anything. `--after` waits
    // for the app to be worth typing into.
    let seed_queue = input.clone();
    let seed = move || {
        let input = seed_queue;
        if let Some(text) = keys {
            let mut queue = input.lock().expect("input queue poisoned");
            for character in text.chars() {
                let character = character.to_string();
                queue.push_back(InputEvent::Key(InputKey {
                    key: character.clone(),
                    text: Some(character),
                    pressed: true,
                    shift: false,
                    control: false,
                    alt: false,
                    meta: false,
                }));
            }
        }
        if let Some(((x1, y1), (x2, y2))) = drag {
            let steps = 8;
            let mut queue = input.lock().expect("input queue poisoned");
            queue.push_back(InputEvent::Moved { x: x1, y: y1 });
            queue.push_back(InputEvent::Button {
                x: x1,
                y: y1,
                button: PointerButton::Left,
                pressed: true,
            });
            for step in 1..=steps {
                let t = step as f32 / steps as f32;
                queue.push_back(InputEvent::Moved {
                    x: x1 + (x2 - x1) * t,
                    y: y1 + (y2 - y1) * t,
                });
            }
            queue.push_back(InputEvent::Button {
                x: x2,
                y: y2,
                button: PointerButton::Left,
                pressed: false,
            });
        }
        if let Some((x, y)) = pointer {
            input
                .lock()
                .expect("input queue poisoned")
                .push_back(InputEvent::Moved { x, y });
        }
        if let Some((x, y)) = click {
            input.lock().expect("input queue poisoned").extend([
                InputEvent::Moved { x, y },
                InputEvent::Button {
                    x,
                    y,
                    button: PointerButton::Left,
                    pressed: true,
                },
                InputEvent::Button {
                    x,
                    y,
                    button: PointerButton::Left,
                    pressed: false,
                },
            ]);
        }
    };
    match after {
        Some(milliseconds) => {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(milliseconds));
                seed();
            });
        }
        None => seed(),
    }
    let frames = frames.unwrap_or(if capture.is_some() { 1 } else { usize::MAX });
    let launch = LaunchConfig {
        origins,
        geode_socket,
        geode_purpose,
        hue_bridge,
        records_root,
        vfs_root: vfs_root.clone(),
        records_book: match (records_book.clone(), records_collection.clone()) {
            (Some(book), Some(collection)) => Some(BookGrant { collection, book }),
            (None, None) => None,
            _ => anyhow::bail!("--records-book and --records-collection are given together"),
        },
        actor: actor.clone(),
    };
    let http = build_http(&launch)?;
    // A named book replaces the read-only vitals view: a Spore that was given a
    // book to write into reads and writes that collection, not somebody else's.
    let records = match (&launch.records_book, &launch.records_root) {
        (Some(grant), Some(root)) => Some(RecordsAccess::book(
            root,
            &grant.collection,
            &grant.book,
            &grant.collection,
        )),
        (None, Some(root)) => Some(RecordsAccess::vitals(root)),
        (Some(_), None) => anyhow::bail!("--records-book needs --records-root"),
        (None, None) => None,
    };
    // A `--surface` names a window; `--size` still fits the one-window case,
    // and wins over the first `--surface` when both were given.
    let mut specs = surfaces;
    if specs.is_empty() {
        specs.push(SurfaceSpec {
            size: (logical_size.0 as u32, logical_size.1 as u32),
            view: None,
            agent: None,
        });
    } else if size_given {
        specs[0].size = (logical_size.0 as u32, logical_size.1 as u32);
    }
    // One adapter for the process. A window's intent is its own, so the roster
    // is built without one; two windows then read one set of sessions.
    let roster = match agents.as_deref() {
        Some(source) => soil::webgpu::agent_backend(source, acp_cmd.as_deref(), None, None)
            .map_err(|error| anyhow::anyhow!("--agents: {error}"))?,
        None => Box::new(NoAgents),
    };
    let shared = Shared {
        module,
        edgerunner,
        command,
        spores: spores.map(|path| vec![path]).unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            // Every place a Spore can live: what the store installed, what the
            // shell ships, and any root the environment adds. Joining them is
            // what makes the launcher describe the machine instead of the app.
            let mut roots = vec![
                home.join(".local/share/fungos/spores"),
                home.join("Library/Application Support/fungOS/spores"),
            ];
            if let Some(extra) = std::env::var_os("SOIL_SPORES_ROOTS") {
                roots.extend(std::env::split_paths(&extra));
            }
            roots
        }),
        http,
        records,
        vfs_root,
        actor,
        launch,
        // One bus per process, shared by every window in it: the wall announces
        // a prompt and the popup it opens reads the same frame.
        bus: soil::webgpu::in_process_bus(),
        roster: SharedAgents::new(roster),
    };
    let mut app = App {
        shared,
        specs,
        surfaces: HashMap::new(),
        splash,
        modifiers: ModifiersState::default(),
        seed: Some(input),
        stats,
        capture,
        frames,
    };
    let event_loop = EventLoop::new()?;
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod native_launch_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn native_host_is_resolved_beside_the_package() {
        let root = std::env::temp_dir().join(format!(
            "canvas-winit-native-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(root.join("native")).expect("test package");
        let host = root.join("native/terminal-host");
        std::fs::write(&host, "#!/bin/sh\n").expect("test host");
        let mut permissions = std::fs::metadata(&host)
            .expect("host metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&host, permissions).expect("host permissions");
        let module = root.join("module.wasm");
        std::fs::write(&module, b"").expect("test module");

        let command = native_command(
            "native/terminal-host",
            &root,
            &module,
            "dev.fungos.terminal",
            "native",
        )
        .expect("a native host beside the package");
        let envs = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(command.get_program(), host.as_os_str());
        assert_eq!(
            envs.get("SOIL_SPORE_MODULE")
                .and_then(|value| value.as_deref()),
            module.to_str()
        );
        assert_eq!(
            envs.get("TERMINAL_WINDOW")
                .and_then(|value| value.as_deref()),
            Some("1")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn catalogue_launches_the_native_host() {
        let root = std::env::temp_dir().join(format!(
            "canvas-winit-terminal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let package = root.join("terminal");
        std::fs::create_dir_all(package.join("native")).expect("test package");
        std::fs::write(package.join("module.wasm"), b"").expect("test module");
        let host = package.join("native/terminal-host");
        std::fs::write(
            &host,
            "#!/bin/sh\nprintf '%s\\n%s\\n' \"$SOIL_SPORE_MODULE\" \"$SOIL_SPORE_SURFACE\" > \"$SOIL_SPORE_PACKAGE/launched\"\n",
        )
        .expect("test host");
        let mut permissions = std::fs::metadata(&host)
            .expect("host metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&host, permissions).expect("host permissions");
        std::fs::write(
            package.join("spore.toml"),
            "schema = 1\n\
             [spore]\n\
             id = \"dev.fungos.terminal\"\n\
             name = \"terminal\"\n\
             version = \"0.1.0\"\n\
             license = \"MIT\"\n\
             summary = \"a terminal\"\n\
             [engine]\n\
             soil = \">=0.1\"\n\
             target = \"wasm32-wasip2\"\n\
             [module]\n\
             path = \"module.wasm\"\n\
             [launch]\n\
             default = \"native\"\n\
             surfaces = [\"native\"]\n\
             native = \"native/terminal-host\"\n",
        )
        .expect("test manifest");

        let mut catalog = SporeCatalog::new(std::slice::from_ref(&root), LaunchConfig::default())
            .expect("test catalogue");
        let entries = catalog.entries().expect("catalogue entries");
        let terminal = entries
            .iter()
            .find(|entry| entry.id == "dev.fungos.terminal")
            .expect("terminal entry");
        assert_eq!(terminal.default_surface, "native");
        assert!(
            terminal
                .surfaces
                .iter()
                .any(|surface| surface.name == "native")
        );

        let launched = catalog
            .launch("dev.fungos.terminal", "native")
            .expect("native launch");
        assert!(launched.target.starts_with("native native/terminal-host"));

        let log = package.join("launched");
        for _ in 0..100 {
            if log.is_file() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let text = std::fs::read_to_string(&log).expect("native host ran");
        assert!(text.contains(&package.join("module.wasm").display().to_string()));
        assert!(text.contains("native"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
