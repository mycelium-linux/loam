//! Per-user `loamd` supervisor.
//!
//! The daemon owns one process, a command registry, a plugin registry and the
//! command instances it starts. It is deliberately per-user: the socket is
//! created in the user's runtime directory and a peer is accepted only when its
//! Unix uid matches the daemon's effective uid. There is no root mode, group
//! ACL or remote control surface in this slice.
//!
//! The control protocol is a single newline-delimited JSON request and response
//! over a Unix stream. It is a local admin surface, not a wasm capability and
//! not a second package format.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wasmtime::Engine;

use crate::{
    Bus,
    command::{
        CommandRegistry, CommandRunner, RecordsBinding, RunLimits, RunOutput, RunRequest,
        plugin::PluginRegistry,
    },
    log::LogStore,
};

/// How many live instances one daemon may own.
pub const DEFAULT_MAX_INSTANCES: usize = 8;

/// A request from `loamd`'s local control client.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    Ping,
    List,
    Inspect {
        id: String,
    },
    Logs {
        id: String,
    },
    Start {
        id: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        stdin: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        max_stdout_bytes: Option<usize>,
        #[serde(default)]
        max_stderr_bytes: Option<usize>,
        #[serde(default)]
        max_memory_bytes: Option<usize>,
    },
    Stop {
        id: String,
    },
    ImageList,
    ImagePlan {
        spore: String,
    },
    ImageStart {
        id: String,
        spore: String,
    },
    ImageEval {
        id: String,
        source: String,
    },
    ImageStatus {
        id: String,
    },
    ImageStop {
        id: String,
    },
    WindowList,
    WindowStart {
        id: String,
        url: String,
        title: String,
        width: u32,
        height: u32,
    },
    WindowStop {
        id: String,
    },
    Reload,
    Shutdown,
}

/// What the daemon needs to start.
pub struct Options {
    pub socket: PathBuf,
    pub roots: Vec<PathBuf>,
    pub records: Option<RecordsBinding>,
    pub bus: Option<Arc<dyn Bus>>,
    pub max_instances: usize,
    /// The live Soil image runtime. `None` means use `soil-image` on PATH.
    pub image_runtime: Option<PathBuf>,
    /// The native webview host. `None` means use `loam-window` on PATH.
    pub window_runtime: Option<PathBuf>,
    /// Static web surface listener. `None` leaves manifest web surfaces unserved.
    pub web_listen: Option<String>,
    /// Durable instance logs. `None` uses the per-user state directory.
    pub log_root: Option<PathBuf>,
    /// Rotate an instance log before it grows past this many bytes.
    pub log_max_bytes: u64,
    /// Generations kept beside the current file.
    pub log_keep: usize,
    /// The Mycelium home used to publish manifest web leases. `None` uses
    /// `$MYCELIUM_HOME` or `~/.mycelium`.
    pub mycelium_home: Option<PathBuf>,
    /// Whether manifest web surfaces publish a Mycelium service lease.
    pub publish_leases: bool,
}

impl Options {
    pub fn new(socket: impl Into<PathBuf>, roots: Vec<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            roots,
            records: None,
            bus: None,
            max_instances: DEFAULT_MAX_INSTANCES,
            image_runtime: None,
            window_runtime: None,
            web_listen: None,
            log_root: None,
            log_max_bytes: crate::log::DEFAULT_MAX_BYTES,
            log_keep: crate::log::DEFAULT_KEEP,
            mycelium_home: None,
            publish_leases: true,
        }
    }
}

/// The default per-user socket.
pub fn default_socket() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(runtime).join("loam/loamd.sock");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/share/loam/loamd.sock")
}

/// Send one request to a running daemon.
pub fn request(socket: &Path, request: &Request) -> Result<Value> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    let line = serde_json::to_string(request)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    if response.trim().is_empty() {
        bail!("daemon closed the connection without a response");
    }
    Ok(serde_json::from_str(&response)?)
}

/// Run the per-user daemon until a `shutdown` request.
pub fn serve(options: Options) -> Result<()> {
    if options.max_instances == 0 {
        bail!("--max-instances must be greater than zero");
    }
    let logs = LogStore::new(
        options
            .log_root
            .clone()
            .unwrap_or_else(crate::log::default_root),
        options.log_max_bytes,
        options.log_keep,
    )
    .context("open instance log directory")?;
    let _ = logs.append(
        "loamd",
        format!("{} loamd started roots={:?}\n", now_millis(), options.roots).as_bytes(),
    );
    start_web_surfaces(
        &options.roots,
        options.web_listen.as_deref(),
        options.mycelium_home.as_deref(),
        options.publish_leases,
    )?;
    let registry = Arc::new(CommandRegistry::scan_roots(&options.roots)?);
    let plugins = PluginRegistry::scan_roots(&options.roots)?;
    let daemon = Arc::new(Daemon {
        roots: options.roots,
        registry: RwLock::new(registry),
        plugins: RwLock::new(plugins),
        records: options.records,
        bus: options.bus,
        max_instances: options.max_instances,
        image_runtime: options.image_runtime,
        window_runtime: options.window_runtime,
        logs: logs.clone(),
        instances: Mutex::new(BTreeMap::new()),
        images: Mutex::new(BTreeMap::new()),
        windows: Mutex::new(BTreeMap::new()),
        shutdown: AtomicBool::new(false),
    });
    let listener = bind_socket(&options.socket)?;
    listener.set_nonblocking(true)?;

    while !daemon.shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let daemon = Arc::clone(&daemon);
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, &daemon) {
                        eprintln!("loamd: control connection: {error:#}");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("accept loamd control connection"),
        }
    }

    let _ = logs.append(
        "loamd",
        format!("{} loamd stopped\n", now_millis()).as_bytes(),
    );
    let _ = fs::remove_file(&options.socket);
    Ok(())
}

fn start_web_surfaces(
    roots: &[PathBuf],
    listen: Option<&str>,
    mycelium_home: Option<&Path>,
    publish_leases: bool,
) -> Result<()> {
    let surfaces = crate::web::discover(roots)?;
    if surfaces.is_empty() {
        return Ok(());
    }
    let address = listen.unwrap_or("127.0.0.1:8799");
    let listener = std::net::TcpListener::bind(address)
        .with_context(|| format!("bind static web surface {address}"))?;
    let endpoint = crate::web::loopback_endpoint(&listener)?;
    if publish_leases {
        let lease_surfaces = surfaces.clone();
        let lease_endpoint = endpoint.clone();
        let lease_home = mycelium_home.map(Path::to_path_buf);
        let renew = surfaces
            .iter()
            .map(|surface| surface.ttl_secs)
            .min()
            .unwrap_or(240)
            / 3;
        publish_web_leases(&lease_surfaces, &lease_endpoint, lease_home.as_deref());
        let _ = thread::Builder::new()
            .name("loamd-web-lease".into())
            .spawn(move || {
                loop {
                    thread::sleep(Duration::from_secs(renew.max(1)));
                    publish_web_leases(&lease_surfaces, &lease_endpoint, lease_home.as_deref());
                }
            });
    }
    let _ = thread::Builder::new()
        .name("loamd-web".into())
        .spawn(move || {
            if let Err(error) = crate::web::serve(surfaces, listener) {
                eprintln!("loamd: static web surface: {error:#}");
            }
        });
    Ok(())
}

fn publish_web_leases(
    surfaces: &[crate::web::StaticSurface],
    endpoint: &str,
    mycelium_home: Option<&Path>,
) {
    let home = mycelium_home
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("MYCELIUM_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".mycelium")));
    let Some(home) = home else { return };
    let mut identity = match crate::spore::identity::Identity::read(&home) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("loamd: static web surface has no Mycelium identity: {error}");
            return;
        }
    };
    // `Identity` carries the short certificate digest; Mycelium leases use the
    // full peer key. Prefer the daemon's published id, then an explicit env var.
    if let Some(node_id) = std::env::var("MYCELIUM_NODE_ID")
        .ok()
        .filter(|id| id.len() == 64)
    {
        identity.node_id = node_id;
    } else if let Ok(bytes) = fs::read(home.join("application-services.json"))
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        && let Some(id) = value["observations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["provenance"]["observer"].as_str())
            .find(|id| id.len() == 64)
    {
        identity.node_id = id.to_owned();
    }
    let publisher = crate::spore::publisher::TrustedPublisher {
        principal: "loam-local".into(),
        grant_id: "loam-local".into(),
        key_id: "loam-local".into(),
    };
    let path = home.join("application-services.json");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let mut document: Value = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| json!({"schema_version": 1, "observations": []}));
    let Some(observations) = document
        .get_mut("observations")
        .and_then(Value::as_array_mut)
    else {
        eprintln!("loamd: application-services.json is not a catalog");
        return;
    };
    for surface in surfaces {
        let lock = fs::read_to_string(surface.package.join("spore.lock"))
            .ok()
            .and_then(|source| crate::spore::Lock::parse(&source).ok())
            .unwrap_or_else(|| crate::spore::Lock {
                id: surface.spore_id.clone(),
                version: surface.version.clone(),
                files: Vec::new(),
                digest: "sha256:local".into(),
            });
        let rendered = match crate::spore::lease::document_for(
            &surface.spore_id,
            &surface.version,
            &surface.anycast,
            &lock,
            &publisher,
            &identity,
            &[endpoint.to_owned()],
            now,
        ) {
            Ok(document) => document,
            Err(error) => {
                eprintln!("loamd: static web lease {}: {error}", surface.name);
                continue;
            }
        };
        let service_id = match crate::spore::lease::service_id_for(&surface.anycast, &identity) {
            Ok(service_id) => service_id,
            Err(_) => continue,
        };
        observations.retain(|entry| {
            entry["service_id"] != service_id
                && !(entry["value"]["kind"] == surface.kind
                    && entry["value"]["names"]
                        .as_array()
                        .is_some_and(|names| names.iter().any(|name| name == &surface.name)))
        });
        if let Some(observation) = rendered["observations"].get(0).cloned() {
            observations.push(observation);
        }
    }
    if let Ok(text) = serde_json::to_vec_pretty(&document)
        && let Err(error) = fs::write(&path, text)
    {
        eprintln!("loamd: write {}: {error}", path.display());
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn bind_socket(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", parent.display()))?;
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("loamd is already listening on {}", path.display());
        }
        fs::remove_file(path).with_context(|| format!("remove stale {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", path.display()))?;
    Ok(listener)
}

fn handle_client(stream: UnixStream, daemon: &Arc<Daemon>) -> Result<()> {
    let peer = peer_uid(&stream)?;
    let me = unsafe { libc::geteuid() };
    if peer != me {
        bail!("peer uid {peer} is not this daemon's uid {me}");
    }

    let mut line = String::new();
    let mut reader = BufReader::new(&stream);
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        bail!("empty control request");
    }
    let request: Request = serde_json::from_str(&line)?;
    let response = daemon.handle(request);
    let mut stream = stream;
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Result<u32> {
    use std::{mem::size_of, os::fd::AsRawFd};

    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut _ as *mut libc::c_void,
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("read peer credentials");
    }
    Ok(credentials.uid)
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> Result<u32> {
    use std::os::fd::AsRawFd;

    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("read peer credentials");
    }
    Ok(uid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_uid(_stream: &UnixStream) -> Result<u32> {
    bail!("peer credential checks are not implemented on this Unix")
}

struct Daemon {
    roots: Vec<PathBuf>,
    registry: RwLock<Arc<CommandRegistry>>,
    plugins: RwLock<PluginRegistry>,
    records: Option<RecordsBinding>,
    bus: Option<Arc<dyn Bus>>,
    max_instances: usize,
    image_runtime: Option<PathBuf>,
    window_runtime: Option<PathBuf>,
    logs: LogStore,
    instances: Mutex<BTreeMap<String, InstanceRecord>>,
    images: Mutex<BTreeMap<String, ImageRecord>>,
    windows: Mutex<BTreeMap<String, WindowRecord>>,
    shutdown: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum State {
    Running,
    Stopping,
    Exited,
    Failed,
    Stopped,
}

#[derive(Debug)]
struct InstanceRecord {
    id: String,
    command: String,
    args: Vec<String>,
    started_ms: u64,
    limits: RunLimits,
    state: State,
    cancel: Option<Engine>,
    output: Option<RunOutput>,
    error: Option<String>,
}

struct ImageRecord {
    id: String,
    spore: String,
    plan: Value,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    started_ms: u64,
}

struct WindowRecord {
    id: String,
    url: String,
    title: String,
    width: u32,
    height: u32,
    child: Child,
    started_ms: u64,
    state: State,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WindowView {
    id: String,
    url: String,
    title: String,
    width: u32,
    height: u32,
    state: State,
    started_ms: u64,
    pid: u32,
    error: Option<String>,
}

impl WindowRecord {
    fn refresh(&mut self) {
        if self.state != State::Running {
            return;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.state = State::Exited;
                if !status.success() {
                    self.error = Some(format!("window exited: {status}"));
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.state = State::Failed;
                self.error = Some(format!("wait for window: {error}"));
            }
        }
    }

    fn view(&self) -> WindowView {
        WindowView {
            id: self.id.clone(),
            url: self.url.clone(),
            title: self.title.clone(),
            width: self.width,
            height: self.height,
            state: self.state,
            started_ms: self.started_ms,
            pid: self.child.id(),
            error: self.error.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ImageView {
    id: String,
    spore: String,
    started_ms: u64,
    imports: Vec<String>,
    refused: Vec<String>,
}

impl ImageRecord {
    fn view(&self) -> ImageView {
        ImageView {
            id: self.id.clone(),
            spore: self.spore.clone(),
            started_ms: self.started_ms,
            imports: self
                .plan
                .get("imports")
                .and_then(Value::as_array)
                .map(|imports| {
                    imports
                        .iter()
                        .filter_map(|import| import.get("interface").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            refused: self
                .plan
                .get("refused")
                .and_then(Value::as_array)
                .map(|refused| {
                    refused
                        .iter()
                        .filter_map(|item| item.get("interface").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn request(&mut self, request: Value) -> Result<Value> {
        let line = serde_json::to_string(&request)?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut response = String::new();
        if self.stdout.read_line(&mut response)? == 0 {
            bail!("image runtime closed stdout");
        }
        Ok(serde_json::from_str(&response)?)
    }
}

#[derive(Debug, Serialize)]
struct InstanceView {
    id: String,
    command: String,
    args: Vec<String>,
    state: State,
    started_ms: u64,
    timeout_ms: u64,
    max_memory_bytes: usize,
    exit_code: Option<i32>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    error: Option<String>,
}

impl InstanceRecord {
    fn view(&self) -> InstanceView {
        InstanceView {
            id: self.id.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            state: self.state,
            started_ms: self.started_ms,
            timeout_ms: self.limits.timeout_ms,
            max_memory_bytes: self.limits.max_memory_bytes,
            exit_code: self.output.as_ref().map(|output| output.code),
            stdout_bytes: self.output.as_ref().map_or(0, |output| output.stdout.len()),
            stderr_bytes: self.output.as_ref().map_or(0, |output| output.stderr.len()),
            error: self.error.clone(),
        }
    }

    fn logs(&self) -> Value {
        json!({
            "id": self.id,
            "stdout": self.output.as_ref().map(|output| String::from_utf8_lossy(&output.stdout).into_owned()),
            "stderr": self.output.as_ref().map(|output| String::from_utf8_lossy(&output.stderr).into_owned()),
            "error": self.error,
        })
    }
}

fn instance_log(record: &InstanceRecord, result: &Result<RunOutput>) -> Vec<u8> {
    let mut text = format!(
        "=== {} {} {} ===\n",
        record.started_ms,
        record.command,
        record.args.join(" ")
    )
    .into_bytes();
    match result {
        Ok(output) => {
            text.extend_from_slice(b"--- stdout ---\n");
            text.extend_from_slice(&output.stdout);
            if !output.stdout.ends_with(b"\n") {
                text.push(b'\n');
            }
            text.extend_from_slice(b"--- stderr ---\n");
            text.extend_from_slice(&output.stderr);
            if !output.stderr.ends_with(b"\n") {
                text.push(b'\n');
            }
            text.extend_from_slice(format!("--- exit {} ---\n", output.code).as_bytes());
        }
        Err(failure) => {
            text.extend_from_slice(format!("--- error: {failure:#} ---\n").as_bytes());
        }
    }
    text
}

impl Daemon {
    fn handle(self: &Arc<Self>, request: Request) -> Value {
        match request {
            Request::Ping => ok(json!({"pong": true})),
            Request::List => {
                let instances: Vec<_> = self
                    .instances
                    .lock()
                    .expect("instance lock poisoned")
                    .values()
                    .map(InstanceRecord::view)
                    .collect();
                ok(json!({"instances": instances}))
            }
            Request::Inspect { id } => match self
                .instances
                .lock()
                .expect("instance lock poisoned")
                .get(&id)
            {
                Some(record) => ok(json!(record.view())),
                None => error(format!("unknown instance {id:?}")),
            },
            Request::Logs { id } => match self
                .instances
                .lock()
                .expect("instance lock poisoned")
                .get(&id)
            {
                Some(record) => ok(record.logs()),
                None => match self.logs.read(&id) {
                    Ok(Some(text)) => ok(json!({"id": id, "persisted": true, "text": text})),
                    Ok(None) => error(format!("unknown instance {id:?}")),
                    Err(failure) => error(format!("read instance {id:?} log: {failure}")),
                },
            },
            Request::Start {
                id,
                command,
                args,
                env,
                cwd,
                stdin,
                timeout_ms,
                max_stdout_bytes,
                max_stderr_bytes,
                max_memory_bytes,
            } => self.start(
                id,
                command,
                args,
                env,
                cwd,
                stdin,
                timeout_ms,
                max_stdout_bytes,
                max_stderr_bytes,
                max_memory_bytes,
            ),
            Request::Stop { id } => self.stop(&id),
            Request::ImageList => self.image_list(),
            Request::ImagePlan { spore } => self.image_plan(&spore),
            Request::ImageStart { id, spore } => self.image_start(id, spore),
            Request::ImageEval { id, source } => self.image_eval(&id, &source),
            Request::ImageStatus { id } => self.image_status(&id),
            Request::ImageStop { id } => self.image_stop(&id),
            Request::WindowList => self.window_list(),
            Request::WindowStart {
                id,
                url,
                title,
                width,
                height,
            } => self.window_start(id, url, title, width, height),
            Request::WindowStop { id } => self.window_stop(&id),
            Request::Reload => self.reload_request(),
            Request::Shutdown => self.shutdown(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        self: &Arc<Self>,
        id: String,
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
        stdin: Option<String>,
        timeout_ms: Option<u64>,
        max_stdout_bytes: Option<usize>,
        max_stderr_bytes: Option<usize>,
        max_memory_bytes: Option<usize>,
    ) -> Value {
        if id.trim().is_empty() {
            return error("instance id must not be empty");
        }
        let mut entry = self
            .registry
            .read()
            .expect("registry lock poisoned")
            .get(&command)
            .cloned();
        if entry.is_none() {
            if let Err(failure) = self.reload() {
                return error(format!("reload command roots: {failure:#}"));
            }
            entry = self
                .registry
                .read()
                .expect("registry lock poisoned")
                .get(&command)
                .cloned();
        }
        let Some(entry) = entry else {
            return error(format!("unknown command {command:?}"));
        };
        let limits = RunLimits {
            timeout_ms: timeout_ms.unwrap_or(entry.timeout_ms),
            max_stdout_bytes: max_stdout_bytes.unwrap_or(entry.max_stdout_bytes),
            max_stderr_bytes: max_stderr_bytes.unwrap_or(entry.max_stderr_bytes),
            max_memory_bytes: max_memory_bytes.unwrap_or(entry.max_memory_bytes),
        };
        if limits.timeout_ms == 0 {
            return error("timeout_ms must be greater than zero");
        }
        if limits.max_memory_bytes == 0 {
            return error("max_memory_bytes must be greater than zero");
        }

        let instances = self.instances.lock().expect("instance lock poisoned");
        if instances.contains_key(&id) {
            return error(format!("instance {id:?} already exists"));
        }
        let live = instances
            .values()
            .filter(|record| matches!(record.state, State::Running | State::Stopping))
            .count();
        if live >= self.max_instances {
            return error(format!("instance limit reached ({})", self.max_instances));
        }
        drop(instances);

        let plugins = self.plugins.read().expect("plugin lock poisoned").clone();
        let runner = match CommandRunner::new(self.records.clone(), self.bus.clone()) {
            Ok(runner) => runner.with_plugins(plugins),
            Err(failure) => return error(format!("{failure:#}")),
        };
        let cancel = runner.engine().clone();
        let request = RunRequest {
            args: args.clone(),
            stdin: stdin.unwrap_or_default().into_bytes(),
            env: env.into_iter().collect(),
            cwd,
        };
        let started_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64);

        self.instances
            .lock()
            .expect("instance lock poisoned")
            .insert(
                id.clone(),
                InstanceRecord {
                    id: id.clone(),
                    command,
                    args,
                    started_ms,
                    limits,
                    state: State::Running,
                    cancel: Some(cancel),
                    output: None,
                    error: None,
                },
            );

        let daemon = Arc::clone(self);
        let instance_id = id.clone();
        thread::spawn(move || {
            let result = runner.run_with_limits(&entry, &request, limits);
            let mut instances = daemon.instances.lock().expect("instance lock poisoned");
            let Some(record) = instances.get_mut(&instance_id) else {
                return;
            };
            let report = instance_log(record, &result);
            record.cancel = None;
            match result {
                Ok(output) => {
                    record.state = State::Exited;
                    record.output = Some(output);
                }
                Err(failure) => {
                    record.state = if record.state == State::Stopping {
                        State::Stopped
                    } else {
                        State::Failed
                    };
                    record.error = Some(format!("{failure:#}"));
                }
            }
            drop(instances);
            if let Err(failure) = daemon.logs.append(&instance_id, &report) {
                eprintln!("loamd: write instance {instance_id:?} log: {failure}");
            }
        });

        ok(json!({"id": id, "state": "running"}))
    }

    fn image_list(&self) -> Value {
        let images = self.images.lock().expect("image lock poisoned");
        ok(json!({
            "images": images.values().map(ImageRecord::view).collect::<Vec<_>>(),
        }))
    }

    fn plan_for(&self, spore: &str) -> Result<crate::image::ImageHostPlan> {
        let plugins = self.plugins.read().expect("plugin lock poisoned");
        crate::image::ImageHostPlan::build(Path::new(spore), &plugins)
    }

    fn image_plan(&self, spore: &str) -> Value {
        match self.plan_for(spore) {
            Ok(plan) => match serde_json::to_value(plan) {
                Ok(plan) => ok(plan),
                Err(failure) => error(format!("serialize image plan: {failure}")),
            },
            Err(failure) => error(format!("{failure:#}")),
        }
    }

    fn image_start(&self, id: String, spore: String) -> Value {
        if id.trim().is_empty() {
            return error("image id must not be empty");
        }
        let plan = match self.plan_for(&spore) {
            Ok(plan) => plan,
            Err(failure) => return error(format!("{failure:#}")),
        };
        let plan = match serde_json::to_value(&plan) {
            Ok(plan) => plan,
            Err(failure) => return error(format!("serialize image plan: {failure}")),
        };
        if !plan["refused"].as_array().is_none_or(Vec::is_empty) {
            return error(format!(
                "image has refused imports: {}",
                plan["refused"]
                    .as_array()
                    .map(|refused| refused
                        .iter()
                        .filter_map(|item| item["interface"].as_str())
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default()
            ));
        }
        let runtime = self
            .image_runtime
            .clone()
            .or_else(|| std::env::var_os("LOAM_IMAGE_RUNTIME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("soil-image"));
        let mut images = self.images.lock().expect("image lock poisoned");
        if images.contains_key(&id) {
            return error(format!("image {id:?} already exists"));
        }
        let started_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64);
        let mut child = match Command::new(&runtime)
            .env("LOAM_IMAGE_SPORE", &spore)
            .env("LOAM_IMAGE_PLAN", plan.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(failure) => {
                return error(format!(
                    "start image runtime {}: {failure}",
                    runtime.display()
                ));
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.kill();
                return error("image runtime has no stdin");
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                return error("image runtime has no stdout");
            }
        };
        let mut record = ImageRecord {
            id: id.clone(),
            spore,
            plan,
            child,
            stdin,
            stdout: BufReader::new(stdout),
            started_ms,
        };
        let response = record.request(json!({
            "op": "start",
            "id": id,
            "plan": record.plan,
        }));
        match response {
            Ok(response) => {
                images.insert(id.clone(), record);
                ok(json!({"id": id, "response": response, "view": images[&id].view()}))
            }
            Err(failure) => {
                let _ = record.child.kill();
                error(format!("start image: {failure:#}"))
            }
        }
    }

    fn image_eval(&self, id: &str, source: &str) -> Value {
        let mut images = self.images.lock().expect("image lock poisoned");
        let Some(image) = images.get_mut(id) else {
            return error(format!("unknown image {id:?}"));
        };
        match image.request(json!({"op": "eval", "source": source})) {
            Ok(response) => ok(response),
            Err(failure) => error(format!("{failure:#}")),
        }
    }

    fn image_status(&self, id: &str) -> Value {
        let mut images = self.images.lock().expect("image lock poisoned");
        let Some(image) = images.get_mut(id) else {
            return error(format!("unknown image {id:?}"));
        };
        match image.request(json!({"op": "status"})) {
            Ok(response) => ok(json!({"view": image.view(), "runtime": response})),
            Err(failure) => error(format!("{failure:#}")),
        }
    }

    fn image_stop(&self, id: &str) -> Value {
        let mut images = self.images.lock().expect("image lock poisoned");
        let Some(mut image) = images.remove(id) else {
            return error(format!("unknown image {id:?}"));
        };
        let response = image.request(json!({"op": "stop"}));
        let _ = image.child.kill();
        let _ = image.child.wait();
        match response {
            Ok(response) => ok(json!({"id": id, "response": response})),
            Err(failure) => error(format!("{failure:#}")),
        }
    }

    fn window_list(&self) -> Value {
        let mut windows = self.windows.lock().expect("window lock poisoned");
        for window in windows.values_mut() {
            window.refresh();
        }
        ok(json!({
            "windows": windows.values().map(WindowRecord::view).collect::<Vec<_>>(),
        }))
    }

    fn window_start(
        &self,
        id: String,
        url: String,
        title: String,
        width: u32,
        height: u32,
    ) -> Value {
        if id.trim().is_empty() {
            return error("window id must not be empty");
        }
        if url.trim().is_empty() {
            return error("window url must not be empty");
        }
        if width == 0 || height == 0 {
            return error("window size must be greater than zero");
        }
        let runtime = self
            .window_runtime
            .clone()
            .or_else(|| std::env::var_os("LOAM_WINDOW_RUNTIME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("loam-window"));
        let mut windows = self.windows.lock().expect("window lock poisoned");
        if windows.contains_key(&id) {
            return error(format!("window {id:?} already exists"));
        }
        let started_ms = now_millis();
        let child = match Command::new(&runtime)
            .arg("--url")
            .arg(&url)
            .arg("--title")
            .arg(&title)
            .arg("--size")
            .arg(format!("{width}x{height}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(failure) => {
                return error(format!(
                    "start window runtime {}: {failure}",
                    runtime.display()
                ));
            }
        };
        windows.insert(
            id.clone(),
            WindowRecord {
                id: id.clone(),
                url,
                title,
                width,
                height,
                child,
                started_ms,
                state: State::Running,
                error: None,
            },
        );
        ok(json!({"id": id, "state": "running"}))
    }

    fn window_stop(&self, id: &str) -> Value {
        let mut windows = self.windows.lock().expect("window lock poisoned");
        let Some(mut window) = windows.remove(id) else {
            return error(format!("unknown window {id:?}"));
        };
        let _ = window.child.kill();
        let _ = window.child.wait();
        ok(json!({"id": id, "state": "stopped"}))
    }

    fn reload(&self) -> Result<()> {
        let registry = Arc::new(CommandRegistry::scan_roots(&self.roots)?);
        let plugins = PluginRegistry::scan_roots(&self.roots)?;
        *self.registry.write().expect("registry lock poisoned") = registry;
        *self.plugins.write().expect("plugin lock poisoned") = plugins;
        Ok(())
    }

    fn reload_request(&self) -> Value {
        match self.reload() {
            Ok(()) => {
                let registry = self.registry.read().expect("registry lock poisoned");
                let plugins = self.plugins.read().expect("plugin lock poisoned");
                ok(json!({
                    "commands": registry.path(),
                    "providers": plugins.interfaces().count(),
                    "refused": plugins.refused().iter().map(|refusal| {
                        json!({"name": refusal.name, "reason": refusal.reason})
                    }).collect::<Vec<_>>(),
                }))
            }
            Err(failure) => error(format!("{failure:#}")),
        }
    }

    fn stop(&self, id: &str) -> Value {
        let engine = {
            let mut instances = self.instances.lock().expect("instance lock poisoned");
            let Some(record) = instances.get_mut(id) else {
                return error(format!("unknown instance {id:?}"));
            };
            if record.state != State::Running {
                return error(format!("instance {id:?} is not running"));
            }
            record.state = State::Stopping;
            record.cancel.clone()
        };
        if let Some(engine) = engine {
            engine.increment_epoch();
        }
        ok(json!({"id": id, "state": "stopping"}))
    }

    fn shutdown(&self) -> Value {
        let engines: Vec<_> = {
            let mut instances = self.instances.lock().expect("instance lock poisoned");
            instances
                .values_mut()
                .filter_map(|record| {
                    if record.state == State::Running {
                        record.state = State::Stopping;
                        record.cancel.clone()
                    } else {
                        None
                    }
                })
                .collect()
        };
        for engine in engines {
            engine.increment_epoch();
        }
        let mut images = self.images.lock().expect("image lock poisoned");
        for image in images.values_mut() {
            let _ = image.request(json!({"op": "stop"}));
            let _ = image.child.kill();
            let _ = image.child.wait();
        }
        images.clear();
        let mut windows = self.windows.lock().expect("window lock poisoned");
        for window in windows.values_mut() {
            let _ = window.child.kill();
            let _ = window.child.wait();
        }
        windows.clear();
        self.shutdown.store(true, Ordering::Relaxed);
        ok(json!({"shutdown": true}))
    }
}

fn ok(data: Value) -> Value {
    json!({"ok": true, "data": data})
}

fn error(message: impl Into<String>) -> Value {
    json!({"ok": false, "error": message.into()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_is_per_user() {
        let socket = default_socket();
        assert!(socket.ends_with("loam/loamd.sock"));
    }

    #[test]
    fn peer_credentials_are_the_local_uid() {
        let (left, right) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&left).unwrap(), unsafe { libc::geteuid() });
        assert_eq!(peer_uid(&right).unwrap(), unsafe { libc::geteuid() });
    }

    #[test]
    fn reload_picks_up_a_new_command_package() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("loamd.sock");
        let mut options = Options::new(&socket, vec![dir.path().to_path_buf()]);
        options.log_root = Some(dir.path().join("logs"));
        let server = thread::spawn(move || serve(options));
        for _ in 0..40 {
            if socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let package = dir.path().join("hello");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("module.wasm"), b"\0asm").unwrap();
        fs::write(
            package.join("command.toml"),
            "schema = 1\n[command]\nname = \"hello\"\nversion = \"0.1.0\"\nsummary = \"test\"\nmodule = \"module.wasm\"\n",
        )
        .unwrap();
        let reload = request(&socket, &Request::Reload).unwrap();
        assert_eq!(reload["ok"], true);
        assert!(
            reload["data"]["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "hello")
        );
        let shutdown = request(&socket, &Request::Shutdown).unwrap();
        assert_eq!(shutdown["ok"], true);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn supervises_a_window_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("loamd.sock");
        let runtime = dir.path().join("window-runtime");
        fs::write(&runtime, "#!/bin/sh\nexec sleep 30\n").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        let mut options = Options::new(&socket, vec![dir.path().to_path_buf()]);
        options.window_runtime = Some(runtime);
        let server = thread::spawn(move || serve(options));
        for _ in 0..40 {
            if socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }

        let started = request(
            &socket,
            &Request::WindowStart {
                id: "window".into(),
                url: "http://127.0.0.1:1/".into(),
                title: "test".into(),
                width: 800,
                height: 600,
            },
        )
        .unwrap();
        assert_eq!(started["ok"], true);

        let list = request(&socket, &Request::WindowList).unwrap();
        assert_eq!(list["data"]["windows"][0]["state"], "running");

        let stopped = request(
            &socket,
            &Request::WindowStop {
                id: "window".into(),
            },
        )
        .unwrap();
        assert_eq!(stopped["ok"], true);
        assert_eq!(
            request(&socket, &Request::WindowList).unwrap()["data"]["windows"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        assert_eq!(request(&socket, &Request::Shutdown).unwrap()["ok"], true);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn serves_ping_list_and_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("loamd.sock");
        let mut options = Options::new(&socket, vec![dir.path().to_path_buf()]);
        options.log_root = Some(dir.path().join("logs"));
        let server = thread::spawn(move || serve(options));

        for _ in 0..40 {
            if socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        assert!(socket.exists(), "daemon did not create its socket");

        let pong = request(&socket, &Request::Ping).unwrap();
        assert_eq!(pong["ok"], true);
        assert_eq!(pong["data"]["pong"], true);

        let list = request(&socket, &Request::List).unwrap();
        assert_eq!(list["ok"], true);
        assert!(list["data"]["instances"].as_array().unwrap().is_empty());

        let start = request(
            &socket,
            &Request::Start {
                id: "missing".into(),
                command: "missing".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                stdin: None,
                timeout_ms: None,
                max_stdout_bytes: None,
                max_stderr_bytes: None,
                max_memory_bytes: None,
            },
        )
        .unwrap();
        assert_eq!(start["ok"], false);

        let shutdown = request(&socket, &Request::Shutdown).unwrap();
        assert_eq!(shutdown["ok"], true);
        server.join().unwrap().unwrap();
        assert!(!socket.exists(), "daemon left a stale socket");
    }
}
