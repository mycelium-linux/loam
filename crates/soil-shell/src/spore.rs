//! `soil spore run`: host any `.spore` on the names it claims.
//!
//! One launcher, no product code. The Spore decides where it answers (its
//! `[http]` table) and what it may do (`[grants]`); this process decides nothing
//! except the listener, and refuses to start when two Spores want one name.
//!
//! The claim is routing, not privilege: a Spore that claims `contacts.myc` gets
//! requests routed to it and still cannot read a file, open a socket or touch a
//! record it was not granted. That split is why self-assertion is safe.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ::soil::{
    CommandService, GatewayRoute, GeodeCredentials, HttpGatewayBuilder, HttpService, OriginAuth,
    RecordsBookHost, StreamHub, UnibusBus, VfsHost, WasiHostBuilder, WasiHostPolicy,
};
use anyhow::{Context, Result, bail};
use soil_spore::{Manifest, OriginAuth as ClaimedAuth};
use tokio::net::TcpListener;

/// How a run is asked for.
#[derive(Debug, Clone)]
pub struct Run {
    pub listen: Option<SocketAddr>,
    pub unibus_config: Option<PathBuf>,
    pub hubs: Vec<String>,
    pub max_body: Option<usize>,
    pub trusted_terminators: Vec<IpAddr>,
    pub state: Option<PathBuf>,
    /// Mesh addresses this Spore's `[anycast]` lease advertises. Empty leaves
    /// the claim unpublished: a lease that names no endpoint is invisible to
    /// every client, so it must be this machine's answer, not the package's.
    pub announce_endpoints: Vec<String>,
    /// Publish the lease unsigned. The publisher-grant path does not exist yet,
    /// so this is the only way a lease is published today, and it says so.
    pub announce_unsigned_dev: bool,
    /// `soil-spore`, when it is not installed beside this binary.
    pub announce_binary: Option<PathBuf>,
    /// Where the announcer writes the observation document, when not default.
    pub announce_out: Option<PathBuf>,
    /// `geoded`'s socket. When set, the Spore resolves credentials through a
    /// lease instead of anything in this process.
    pub geode_socket: Option<PathBuf>,
    /// `geode://<realm>/<account>/`, prefixed to whatever name a Spore asks for.
    pub geode_prefix: String,
    pub geode_purpose: String,
    pub geode_ttl_secs: u64,
    /// Environment variable holding the bearer token `soil:http`'s `default`
    /// handle resolves to. Read by the presenter, never passed to the Spore;
    /// it is the alternative to a Geode lease.
    pub credential_env: Option<String>,
    /// Root of the private `soil:appdata` store. Each app gets one book under
    /// it, named by the manifest's id; `None` denies the interface.
    pub appdata_root: Option<PathBuf>,
    /// Serve a web bridge here, so a browser can offer `soil:http`.
    pub bridge: Option<SocketAddr>,
    /// How long to wait for a page to offer `soil:http` before starting anyway.
    pub bridge_wait: Duration,
    /// Origins this Spore may reach through `soil:http`.
    pub allow_origins: Vec<String>,
    /// Where and how to show it. A Spore is one component and the `soil`
    /// binary is one runner: which surfaces it lands on is a request, not a
    /// separate executable.
    pub present: crate::present::Request,
}

pub fn parse(args: &[String]) -> Result<(PathBuf, Run)> {
    // `soil spore run <path>`: the verb is `run`, and the path follows it.
    let args = match args.first().map(String::as_str) {
        Some("run") => &args[1..],
        _ => args,
    };
    let Some(spore) = args.first().map(PathBuf::from) else {
        bail!("usage: soil spore run <spore-dir|package.spore> [options]");
    };
    let mut run = Run {
        listen: None,
        unibus_config: None,
        hubs: Vec::new(),
        max_body: None,
        trusted_terminators: Vec::new(),
        state: None,
        announce_endpoints: Vec::new(),
        announce_unsigned_dev: false,
        announce_binary: None,
        announce_out: None,
        geode_socket: None,
        geode_prefix: "geode://local/default/".into(),
        // Verification is what an ingress does with a credential; signing is a
        // separate capability and must be asked for explicitly.
        geode_purpose: "verify".into(),
        geode_ttl_secs: 600,
        credential_env: None,
        appdata_root: std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local/share/fungos/appdata")),
        bridge: None,
        bridge_wait: Duration::ZERO,
        allow_origins: Vec::new(),
        present: crate::present::Request::default(),
    };
    let mut index = 1;
    while index < args.len() {
        let value = |index: usize| {
            args.get(index)
                .cloned()
                .with_context(|| format!("{} needs a value", args[index - 1]))
        };
        match args[index].as_str() {
            "--listen" => {
                run.listen = Some(value(index + 1)?.parse().context("--listen is addr:port")?);
                index += 2;
            }
            "--unibus-config" => {
                run.unibus_config = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--hub" => {
                run.hubs.push(value(index + 1)?);
                index += 2;
            }
            "--max-body" => {
                run.max_body = Some(value(index + 1)?.parse().context("--max-body is bytes")?);
                index += 2;
            }
            "--trusted-terminator" => {
                run.trusted_terminators.push(
                    value(index + 1)?
                        .parse()
                        .context("--trusted-terminator is an IP")?,
                );
                index += 2;
            }
            "--state" => {
                run.state = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--geode-socket" => {
                run.geode_socket = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--geode-prefix" => {
                run.geode_prefix = value(index + 1)?;
                index += 2;
            }
            "--geode-purpose" => {
                run.geode_purpose = value(index + 1)?;
                index += 2;
            }
            "--geode-ttl-secs" => {
                run.geode_ttl_secs = value(index + 1)?
                    .parse()
                    .context("--geode-ttl-secs is seconds")?;
                index += 2;
            }
            "--credential-env" => {
                run.credential_env = Some(value(index + 1)?);
                index += 2;
            }
            "--appdata-root" => {
                run.appdata_root = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--bridge" => {
                run.bridge = Some(value(index + 1)?.parse().context("--bridge is addr:port")?);
                index += 2;
            }
            "--bridge-wait" => {
                run.bridge_wait = Duration::from_secs(
                    value(index + 1)?
                        .parse()
                        .context("--bridge-wait is seconds")?,
                );
                index += 2;
            }
            "--allow-origin" => {
                run.allow_origins.push(value(index + 1)?);
                index += 2;
            }
            "--surface" => {
                run.present
                    .surfaces
                    .push(crate::present::Surface::parse(&value(index + 1)?)?);
                index += 2;
            }
            "--frame" => {
                run.present.frame = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--fb" => {
                run.present.framebuffer = Some(value(index + 1)?);
                index += 2;
            }
            "--input" => {
                run.present.input = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--cache" => {
                run.present.cache = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--size" => {
                let size = value(index + 1)?;
                let (width, height) = size.split_once('x').context("--size is WxH")?;
                run.present.size = (
                    width.parse().context("--size is WxH")?,
                    height.parse().context("--size is WxH")?,
                );
                index += 2;
            }
            "--keys" => {
                run.present.keys = Some(value(index + 1)?);
                index += 2;
            }
            "--vfs-root" => {
                run.present.vfs_root = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--records" => {
                run.present.records_root = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--book" => {
                run.present.book = Some(value(index + 1)?);
                index += 2;
            }
            "--collection" => {
                run.present.collection = Some(value(index + 1)?);
                index += 2;
            }
            "--namespace" => {
                run.present.namespace = Some(value(index + 1)?);
                index += 2;
            }
            "--whole-book" => {
                run.present.whole_book = true;
                index += 1;
            }
            "--records-book" => {
                run.present.records_books.push(value(index + 1)?);
                index += 2;
            }
            "--actor" => {
                run.present.actor = Some(value(index + 1)?);
                index += 2;
            }
            "--spores" => {
                run.present.spores = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            // Present one recorded transcript as the agent set: the book a
            // harness wrote (`fungos/agents/<session>`), read back through
            // `soil:agents`. Needs `--records ROOT` beside it.
            "--agent-session" => {
                run.present.agent_session = Some(value(index + 1)?);
                index += 2;
            }
            "--bind" => {
                run.present.bind = Some(value(index + 1)?);
                index += 2;
            }
            "--hue-bridge" => {
                run.present.hue_bridge = Some(value(index + 1)?);
                index += 2;
            }
            "--token" => {
                run.present.token = Some(value(index + 1)?);
                index += 2;
            }
            "--actor-secret-env" => {
                run.present.actor_secret_env = Some(value(index + 1)?);
                index += 2;
            }
            "--per-principal" => {
                run.present.per_principal = true;
                index += 1;
            }
            // An `[anycast]` claim becomes a lease only when this host names an
            // address a client can dial. That is one machine's fact, so it is a
            // flag here rather than part of the package.
            "--announce-endpoint" => {
                run.announce_endpoints.push(value(index + 1)?);
                index += 2;
            }
            "--announce-unsigned-dev" => {
                run.announce_unsigned_dev = true;
                index += 1;
            }
            "--announce-binary" => {
                run.announce_binary = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--announce-out" => {
                run.announce_out = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            other => bail!("unknown option `{other}`"),
        }
    }
    Ok((spore, run))
}

/// Run a Spore until the listener closes.
pub fn run(args: &[String]) -> Result<()> {
    // One verb, four ways to run the same component. `--mode command` is the
    // `run(args)` export; the other modes are the surfaces `present` already
    // knows. A mode is a request, not a different executable.
    let (mode, args) = split_mode(args)?;
    if mode.as_deref() == Some("command") {
        let mut exec_args = args;
        if exec_args.first().map(String::as_str) == Some("run") {
            exec_args[0] = "exec".into();
        }
        return exec(&exec_args);
    }
    // `soil spore warm <path>`: compile it into the window host's AOT cache and
    // stop. This is the wait an install pays so the first click does not.
    if args.first().map(String::as_str) == Some("exec") {
        return exec(&args);
    }
    if args.first().map(String::as_str) == Some("warm") {
        let path = args
            .get(1)
            .context("usage: soil spore warm <spore-dir|package.spore>")?;
        return warm(Path::new(path));
    }
    // A host that cannot say what it is doing is a host nobody can operate. The
    // default is `info` for the host crates, so a plain launch is not silent; an
    // operator can widen or narrow it with `RUST_LOG`, exactly as they can for
    // every other soil service.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,soil=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let (path, mut run) = parse(&args)?;
    if mode.as_deref() == Some("http") {
        let spore = Spore::resolve(&path)?;
        return serve_http(&spore.root, run);
    }
    if let Some(surface) = mode_surface(mode.as_deref())? {
        if !run.present.surfaces.contains(&surface) {
            run.present.surfaces.push(surface);
        }
    }
    let spore = Spore::resolve(&path)?;
    let surfaces = crate::present::plan(&run.present, &spore.manifest)?;
    if surfaces.is_empty() {
        bail!("nothing to do: no surface was planned");
    }
    tracing::info!(
        spore = %spore.manifest.spore.id,
        surfaces = %surfaces
            .iter()
            .map(|surface| surface.name())
            .collect::<Vec<_>>()
            .join(","),
        "presenting"
    );
    crate::present::show(
        &spore.root,
        &spore.manifest,
        &run.present,
        &surfaces,
        Some(&run),
    )
}

/// Split `--mode NAME` from the rest, leaving everything after `--` alone.
fn split_mode(args: &[String]) -> Result<(Option<String>, Vec<String>)> {
    let mut mode = None;
    let mut out = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--" {
            out.extend(args[index..].iter().cloned());
            break;
        }
        if args[index] == "--mode" {
            let value = args
                .get(index + 1)
                .with_context(|| "--mode needs command, http, ui, web or bridge")?;
            if mode.replace(value.clone()).is_some() {
                bail!("--mode was given more than once");
            }
            index += 2;
            continue;
        }
        out.push(args[index].clone());
        index += 1;
    }
    Ok((mode, out))
}

fn mode_surface(mode: Option<&str>) -> Result<Option<crate::present::Surface>> {
    Ok(match mode {
        None => None,
        Some("http") => Some(crate::present::Surface::Http),
        Some("ui") | Some("window") => Some(crate::present::Surface::Window),
        Some("web") | Some("bridge") => Some(crate::present::Surface::Web),
        Some("command") => None,
        Some(other) => bail!("unknown --mode {other:?} (command, http, ui, web, bridge)"),
    })
}

/// Run a command Spore in the same host as the HTTP and window runners.
///
/// `soil spore exec <spore-dir|package.spore> [options] -- <args...>`
pub fn exec(args: &[String]) -> Result<()> {
    let path = args
        .get(1)
        .map(PathBuf::from)
        .context("usage: soil spore exec <spore-dir|package.spore> [options] -- <args...>")?;
    let mut vfs_root = None;
    let mut records_root = None;
    let mut book = "fungos".to_string();
    let mut unibus_config = None;
    let mut hubs = Vec::new();
    let mut allow_origins = Vec::new();
    let mut wss_listen = None;
    let mut wss_token = None;
    let mut guest_args = Vec::new();
    let mut index = 2;
    while index < args.len() {
        if args[index] == "--" {
            guest_args.extend(args[index + 1..].iter().cloned());
            break;
        }
        let value = |index: usize| {
            args.get(index)
                .cloned()
                .with_context(|| format!("{} needs a value", args[index - 1]))
        };
        match args[index].as_str() {
            "--vfs-root" => {
                vfs_root = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--records-root" => {
                records_root = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--book" => {
                book = value(index + 1)?;
                index += 2;
            }
            "--unibus-config" => {
                unibus_config = Some(PathBuf::from(value(index + 1)?));
                index += 2;
            }
            "--hub" => {
                hubs.push(value(index + 1)?);
                index += 2;
            }
            "--allow-origin" => {
                allow_origins.push(value(index + 1)?);
                index += 2;
            }
            "--wss-listen" => {
                wss_listen = Some(value(index + 1)?.parse().context("--wss-listen is addr:port")?);
                index += 2;
            }
            "--wss-token" => {
                wss_token = Some(value(index + 1)?);
                index += 2;
            }
            other => bail!("unknown exec option `{other}` (separate guest args with --)"),
        }
    }

    let spore = Spore::resolve(&path)?;
    let module = spore
        .manifest
        .module
        .as_ref()
        .map(|module| module.path.clone())
        .with_context(|| format!("{} has no [module] to run", spore.manifest.spore.id))?;
    let wasm =
        std::fs::read(spore.root.join(&module)).with_context(|| format!("read module {module}"))?;

    let vfs_root = vfs_root
        .or_else(|| std::env::var_os("SOIL_VFS_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| crate::expand_home("~/.local/share/fungos/vfs"));
    let records_root = records_root
        .or_else(|| std::env::var_os("SOIL_RECORDS_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| crate::expand_home("~/.local/share/fungos/records"));

    std::fs::create_dir_all(&vfs_root).with_context(|| format!("create {}", vfs_root.display()))?;
    std::fs::create_dir_all(&records_root)
        .with_context(|| format!("create {}", records_root.display()))?;

    let mut builder = WasiHostBuilder::new(WasiHostPolicy::default())
        .vfs(VfsHost::open(&vfs_root).context("open soil:vfs")?);
    if spore.manifest.grants.contains_key("soil:codec") {
        builder = builder.codec(::soil::codec_host::CodecHost::default());
    }
    if let Some(listen) = wss_listen {
        let hub = StreamHub::new();
        let token = wss_token
            .or_else(|| std::env::var("SOIL_STREAM_TOKEN").ok())
            .or_else(|| std::env::var("OMI_TOKEN").ok())
            .unwrap_or_default();
        crate::wss::spawn(hub.clone(), listen, token.into())?;
        builder = builder.stream_hub(hub);
    }
    if spore
        .manifest
        .grants
        .keys()
        .any(|grant| grant == "soil:records")
    {
        builder = builder.records_book(
            RecordsBookHost::open(&records_root, book, "capture").context("open soil:records")?,
        );
    }
    if spore.manifest.grants.contains_key("soil:codec") {
        builder = builder.codec(::soil::codec_host::CodecHost::default());
    }
    if let Some(inference) = inference_host() {
        builder = builder.inference(inference);
    }
    if unibus_config.is_some() {
        if let Some(bus) = bus(&spore.manifest, unibus_config.as_deref(), &hubs)? {
            builder = builder.bus(bus);
        }
    }
    if !allow_origins.is_empty() {
        let policy = ::soil::http_host::HttpPolicy::new(
            allow_origins,
            8 * 1024 * 1024,
            Duration::from_secs(30),
        )
        .context("soil:http policy")?;
        builder = builder.with_http(::soil::HttpCapability::new(
            policy,
            Arc::new(::soil::http_host::DirectTransport),
        ));
    }

    let service = CommandService::from_bytes_named(spore.manifest.spore.id.clone(), &wasm, builder)
        .with_context(|| format!("load {}", spore.manifest.spore.id))?;
    let outcome = service.run(guest_args)?;
    if let Some(snapshot) = outcome.snapshot {
        println!("{}", String::from_utf8_lossy(&snapshot));
    }
    Ok(())
}

/// `soil install --id <id>`: fetch, verify, write and compile a listing.
///
/// The window that asked names an id and nothing else: which store, which cache,
/// and which root are the machine's, so they default here and each can be
/// pointed somewhere else when it is not.
pub fn install(args: &[String]) -> Result<()> {
    let value = |name: &str| {
        args.iter()
            .position(|argument| argument == name)
            .and_then(|index| args.get(index + 1))
            .cloned()
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set, so the store paths cannot be found")?;
    let id = value("--id").context("usage: soil install --id SPORE [--json]")?;
    let root = value("--root")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share/fungos/spores"));
    let index = value("--index")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share/fungos/store/index.json"));
    let packages = value("--packages")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share/fungos/store/packages"));
    let origin = value("--origin").unwrap_or_else(|| "http://127.0.0.1:8800".into());

    let state = soil_install::install_id(&id, &index, &packages, Some(&origin), &root)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    // Installing and starting are the same promise: a package that was just
    // written is compiled now, so the click that follows does not pay for it.
    let package = root.join(state.tree_name());
    if let Err(error) = canvas_winit::prewarm(&package) {
        eprintln!(
            "soil: installed {} but could not compile it: {error:#}",
            state.id
        );
    }
    if args.iter().any(|argument| argument == "--json") {
        println!(
            "{}",
            serde_json::json!({
                "id": state.id,
                "version": state.version,
                "digest": state.digest,
            })
        );
    } else {
        println!(
            "installed {} {} ({})",
            state.id, state.version, state.digest
        );
    }
    Ok(())
}

/// Compile a package for the window that will open it.
///
/// The cache is the window host's, keyed by the module's digest, and
/// [`canvas_winit::prewarm`] is the only thing that writes it — the same call
/// the launcher makes for the Spores it has not warmed yet. A composition has no
/// module, which is a fact about the package rather than a failure.
fn warm(path: &Path) -> Result<()> {
    let spore = Spore::resolve(path)?;
    if spore.manifest.module.is_none() {
        println!(
            "soil: {} ships no module; nothing to compile",
            spore.manifest.spore.id
        );
        return Ok(());
    }
    let file = canvas_winit::prewarm(&spore.root)?;
    println!(
        "soil: warmed {} ({})",
        spore.manifest.spore.id,
        file.display()
    );
    Ok(())
}

/// Serve the hosts a Spore claims in its `[http]` table, until the listener
/// closes.///
/// It takes the package directory rather than a resolved `Spore` because a
/// composite run serves these hosts on their own thread while a window is open
/// on this one.
pub(crate) fn serve_http(root: &Path, run: Run) -> Result<()> {
    let manifest = Manifest::load(root).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start the async runtime")?;
    runtime.block_on(serve(
        Spore {
            root: root.to_path_buf(),
            manifest,
            transient: None,
        },
        run,
    ))
}

/// A Spore unpacked and ready to host.
struct Spore {
    root: PathBuf,
    manifest: Manifest,
    /// Set when the package was unpacked to a temporary directory, so it is
    /// cleaned up when the process ends rather than accumulating.
    transient: Option<PathBuf>,
}

impl Spore {
    /// A directory, or a `.spore` package verified and unpacked.
    fn resolve(path: &Path) -> Result<Self> {
        if path.is_dir() {
            let manifest =
                Manifest::load(path).map_err(|error| anyhow::anyhow!(error.to_string()))?;
            return Ok(Self {
                root: path.to_path_buf(),
                manifest,
                transient: None,
            });
        }
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        // Verify before unpacking: a package that fails its own digest check
        // must never reach a temp directory, let alone a listener.
        soil_spore::verify(&bytes).map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let transient = temp_root(path);
        let _ = std::fs::remove_dir_all(&transient);
        std::fs::create_dir_all(&transient)
            .with_context(|| format!("create {}", transient.display()))?;
        soil_spore::unpack(&bytes, &transient)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let manifest =
            Manifest::load(&transient).map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(Self {
            root: transient.clone(),
            manifest,
            transient: Some(transient),
        })
    }
}

impl Drop for Spore {
    fn drop(&mut self) {
        if let Some(transient) = &self.transient {
            let _ = std::fs::remove_dir_all(transient);
        }
    }
}

async fn serve(spore: Spore, run: Run) -> Result<()> {
    let claimed = spore.manifest.http.as_ref().with_context(|| {
        format!(
            "{} claims no [http], so there is nothing for the launcher to host",
            spore.manifest.spore.id
        )
    })?;
    claim_hosts(
        &spore.manifest.spore.id,
        &claimed.hosts,
        run.state.as_deref(),
    )?;

    // The face that answers is the one the `[http]` table names, falling back
    // to the package's own module. A Spore whose window and endpoint are two
    // artifacts says so here; the gateway instantiates only what it calls.
    let module = claimed
        .module
        .clone()
        .or_else(|| {
            spore
                .manifest
                .module
                .as_ref()
                .map(|module| module.path.clone())
        })
        .with_context(|| {
            format!(
                "{} claims a name but has no module to answer with",
                spore.manifest.spore.id
            )
        })?;
    let wasm =
        std::fs::read(spore.root.join(&module)).with_context(|| format!("read module {module}"))?;

    let vfs_root = run
        .present
        .vfs_root
        .clone()
        .or_else(|| std::env::var_os("SOIL_VFS_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| crate::expand_home("~/.local/share/fungos/vfs"));
    std::fs::create_dir_all(&vfs_root).with_context(|| format!("create {}", vfs_root.display()))?;
    let mut builder = WasiHostBuilder::new(WasiHostPolicy::default())
        .vfs(VfsHost::open(&vfs_root).context("open soil:vfs")?);
    if spore.manifest.grants.contains_key("soil:codec") {
        builder = builder.codec(::soil::codec_host::CodecHost::default());
    }
    if let Some(inference) = inference_host() {
        builder = builder.inference(inference);
    }
    if let Some(socket) = &run.geode_socket {
        builder = builder.credential_provider(Arc::new(GeodeCredentials::new(
            socket,
            run.geode_prefix.clone(),
            run.geode_purpose.clone(),
            Duration::from_secs(run.geode_ttl_secs),
        )));
    }
    if let Some(bus) = bus(&spore.manifest, run.unibus_config.as_deref(), &run.hubs)? {
        builder = builder.bus(bus);
    }
    // The manifest is the whole grant decision for metrics: the import always
    // links, so a Spore that declares it but was not granted it starts and is
    // told `denied`, instead of failing to instantiate with only the import
    // named. The registry is rendered at `/metrics` either way; ungranted it
    // stays empty.
    builder = builder.metrics_granted(spore.manifest.grants.contains_key("soil:metrics"));

    // The browser, when one connects. The Spore still runs here; `soil:http`
    // reaches the network through a page that volunteered the interface, and
    // falls back to this process's own socket when none did.
    let mut _bridge_server = None;
    if let Some(listen) = run.bridge {
        let policy = ::soil::http_host::HttpPolicy::new(
            run.allow_origins.iter().cloned(),
            8 * 1024 * 1024,
            Duration::from_secs(30),
        )
        .context("soil:http policy")?;
        let grants = soil_web_bridge::GrantSet::from_manifest(&spore.manifest);
        let bridge = Arc::new(
            soil_web_bridge::Bridge::new(
                spore.manifest.spore.id.clone(),
                grants,
                soil_web_bridge::Registry::new(),
            )
            .with_auth(soil_web_bridge::Auth::loopback_only()),
        );
        let server = bridge
            .clone()
            .serve(listen)
            .context("start the web bridge")?;
        println!("soil: bridge on http://{}", server.addr());
        if run.allow_origins.is_empty() {
            println!("soil: no --allow-origin, so soil:http reaches nothing");
        } else {
            println!("soil: soil:http may reach {}", run.allow_origins.join(", "));
        }
        if !run.bridge_wait.is_zero() {
            let deadline = tokio::time::Instant::now() + run.bridge_wait;
            while !bridge.provides("soil:http") && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !bridge.provides("soil:http") {
                println!("soil: no page offered soil:http; fetches use this process's network");
            }
        }
        let transport = Arc::new(::soil::http_host::PreferProvider::new(
            Arc::new(soil_web_bridge::SessionTransport::new(bridge)),
            Arc::new(::soil::http_host::DirectTransport),
        ));
        builder = builder.with_http(::soil::HttpCapability::new(policy, transport));
        _bridge_server = Some(server);
    }
    // A Spore granted `soil:records` must have a book linked before it can
    // instantiate. The HTTP surface is a host surface like the window, and the
    // grant is the same one; before this, `--surface http` linked every other
    // capability and left the records import unresolved. The operator names
    // the collection (or the package declares it), and the book defaults to
    // the collection's own name.
    // `soil:records.read` is the least-privilege spelling a read-only Spore
    // uses; the older `soil:records` names the whole capability. Both open a
    // book here, matching `present::grants`.
    if spore
        .manifest
        .grants
        .keys()
        .any(|grant| grant.starts_with("soil:records"))
    {
        let root = run
            .present
            .records_root
            .clone()
            .or_else(crate::present::default_records_root)
            .context("no records store: pass --records DIR")?;
        let declared = spore.manifest.records.as_ref();
        let collection = run
            .present
            .collection
            .clone()
            .or_else(|| declared.and_then(|records| records.collection.clone()))
            .or_else(|| {
                declared
                    .filter(|records| records.collections.len() == 1)
                    .and_then(|records| records.collections.first().cloned())
            })
            .context(
                "the Spore is granted soil:records, so --collection NAME says which collection \
                 the browser may address",
            )?;
        let book = run
            .present
            .book
            .clone()
            .or_else(|| declared.and_then(|records| records.book.clone()))
            .unwrap_or_else(|| collection.clone());
        builder = builder.records_book(
            ::soil::RecordsBookHost::open(&root, book.clone(), collection.clone()).with_context(
                || {
                    format!(
                        "open records collection {collection} in book {book} at {}",
                        root.display()
                    )
                },
            )?,
        );
    }
    let service = Arc::new(
        HttpService::from_bytes_named(spore.manifest.spore.id.clone(), &wasm, builder)
            .with_context(|| format!("load {}", spore.manifest.spore.id))?,
    );

    let max_body = run.max_body.unwrap_or(claimed.max_body);
    let mut gateway = HttpGatewayBuilder::new()
        .max_body_bytes(max_body)
        .origin_auth(origin_auth(&claimed.origin_auth, &run.trusted_terminators)?)
        .request_timeout(Duration::from_secs(30));
    for host in &claimed.hosts {
        let mut routes = Vec::new();
        for route in &claimed.routes {
            routes.push(
                GatewayRoute::new(
                    route.methods.iter().map(|method| match method.as_str() {
                        "GET" => http::Method::GET,
                        "HEAD" => http::Method::HEAD,
                        "POST" => http::Method::POST,
                        "PUT" => http::Method::PUT,
                        "PATCH" => http::Method::PATCH,
                        "DELETE" => http::Method::DELETE,
                        "OPTIONS" => http::Method::OPTIONS,
                        // Validation already refused anything else.
                        _ => http::Method::GET,
                    }),
                    &route.path,
                )
                .with_context(|| format!("grant {} {}", route.methods.join(","), route.path))?
                .max_body_bytes(max_body),
            );
        }
        gateway = gateway
            .vhost(
                host.clone(),
                spore.manifest.spore.id.clone(),
                Arc::clone(&service),
                routes,
            )
            .with_context(|| format!("grant the vhost {host}"))?;
    }
    let gateway = gateway.build().context("build the HTTP gateway")?;

    let listen = run
        .listen
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8787)));
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    tracing::info!(
        addr = %listen,
        spore = %spore.manifest.spore.id,
        hosts = %claimed.hosts.join(","),
        "spore listening"
    );
    // Listening first, then announced: a lease is a promise that a request will
    // be answered here, so it must not exist before the socket does.
    announce(&spore.manifest, &spore.root, &run)?;
    Arc::new(gateway).serve(listener).await
}

/// Publish this Spore's `[anycast]` claim, now that it is listening.
///
/// The announcer is a child process rather than a thread: it owns `SIGTERM` so
/// it can withdraw the lease before it exits, and that drain belongs to the
/// process that publishes, not to a listener that is shutting down. A crash
/// here also cannot take the listener with it, and a lease nobody renews
/// expires on its own TTL — which is the whole reason the lease is short.
fn announce(manifest: &Manifest, root: &Path, run: &Run) -> Result<()> {
    let Some(anycast) = &manifest.anycast else {
        // Asking for an announcement the package cannot make is worth naming:
        // silence here reads as a replica that is quietly in the pool.
        if !run.announce_endpoints.is_empty() {
            bail!(
                "{} declares no [anycast] claim, so --announce-endpoint has nothing to publish",
                manifest.spore.id
            );
        }
        return Ok(());
    };
    if run.announce_endpoints.is_empty() {
        tracing::warn!(
            name = %anycast.name,
            "no --announce-endpoint, so this replica is not in the anycast pool"
        );
        return Ok(());
    }
    if !run.announce_unsigned_dev {
        bail!(
            "{} claims the anycast name {} but no publisher grant is configured for this host;              pass --announce-unsigned-dev to publish an unsigned lease for a local test",
            manifest.spore.id,
            anycast.name
        );
    }
    let binary = match &run.announce_binary {
        Some(path) => path.clone(),
        None => std::env::current_exe()
            .context("locate this binary")?
            .parent()
            .map(|directory| directory.join("soil-spore"))
            .context("this binary has no parent directory")?,
    };
    let mut command = std::process::Command::new(&binary);
    command.arg("announce").arg(root);
    for endpoint in &run.announce_endpoints {
        command.arg("--endpoint").arg(endpoint);
    }
    if let Some(out) = &run.announce_out {
        command.arg("--out").arg(out);
    }
    command.arg("--unsigned-dev");
    // A killed launcher would otherwise leave a renewer behind, and a renewer
    // that was never stopped keeps advertising a node that is gone: the lease
    // TTL covers a process that stopped beating, not one that is still alive.
    // SIGTERM is what the announcer already drains on.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: prctl is async-signal-safe and the closure allocates nothing.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("run {}", binary.display()))?;
    tracing::info!(
        name = %anycast.name,
        pid = child.id(),
        endpoints = %run.announce_endpoints.join(","),
        "announcing this replica"
    );
    // Nothing waits on it: the listener is the run, and the announcer is meant
    // to outlive every request. When it does exit, say so once — a replica that
    // stopped renewing is out of the pool, not merely quiet.
    std::thread::spawn(move || match child.wait() {
        Ok(status) => {
            tracing::warn!(%status, "the announcer exited; this replica is out of the pool")
        }
        Err(error) => tracing::warn!(%error, "lost track of the announcer"),
    });
    Ok(())
}

/// Optional `soil:inference` provider, configured from the environment.
///
/// `SOIL_INFERENCE_URL` is the OpenAI-compatible base URL, e.g.
/// `http://agora-one:8717/v1`. `SOIL_INFERENCE_MODEL` defaults to
/// `scribe/summarize`. The key comes from `SOIL_INFERENCE_API_KEY` or
/// `SOIL_INFERENCE_KEY_FILE`.
fn inference_host() -> Option<::soil::InferenceHost> {
    let url = std::env::var("SOIL_INFERENCE_URL").ok()?;
    if url.trim().is_empty() {
        return None;
    }
    let model =
        std::env::var("SOIL_INFERENCE_MODEL").unwrap_or_else(|_| "scribe/summarize".to_string());
    let api_key = std::env::var("SOIL_INFERENCE_API_KEY").ok().or_else(|| {
        std::env::var("SOIL_INFERENCE_KEY_FILE")
            .ok()
            .and_then(|path| {
                std::fs::read_to_string(crate::expand_home(&path))
                    .ok()
                    .map(|value| value.trim().to_string())
            })
    });
    let session = std::env::var("SOIL_INFERENCE_SESSION").unwrap_or_else(|_| {
        format!(
            "soil-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        )
    });
    let host = ::soil::InferenceHost::new(url, api_key, "soil", model).with_session(session);
    let host = match std::env::var("SOIL_VOICEPRINT_URL") {
        Ok(url) if !url.trim().is_empty() => host.with_voice_url(url),
        _ => host,
    };
    let host = match std::env::var("SOIL_STT_URL") {
        Ok(url) if !url.trim().is_empty() => host.with_stt_url(url),
        _ => host,
    };
    let host = match std::env::var("SOIL_STT_MODEL") {
        Ok(model) if !model.trim().is_empty() => host.with_stt_model(model),
        _ => host,
    };
    Some(host)
}

/// The bus, when the Spore is granted one.
///
/// A Spore that observes or publishes needs a hub; one that does not must not
/// be handed a connection it never asked for.
fn bus(
    manifest: &Manifest,
    config: Option<&Path>,
    hubs: &[String],
) -> Result<Option<Arc<dyn ::soil::Bus>>> {
    let wants = manifest
        .grants
        .keys()
        .any(|grant| grant.starts_with("soil:unibus"));
    if !wants {
        return Ok(None);
    }
    let config = config.with_context(|| {
        format!(
            "{} is granted soil:unibus but no --unibus-config was given",
            manifest.spore.id
        )
    })?;
    let mut base: unibus::client::ClientConfig = serde_json::from_slice(
        &std::fs::read(config).with_context(|| format!("read {}", config.display()))?,
    )
    .with_context(|| format!("parse {}", config.display()))?;
    base.resolve_token_file(config);
    let mut endpoints = vec![base.hub_endpoint.clone()];
    endpoints.extend(hubs.iter().cloned());
    let bus = UnibusBus::connect_any(&base, &endpoints)?;
    Ok(Some(Arc::new(bus)))
}

fn origin_auth(claimed: &ClaimedAuth, terminators: &[IpAddr]) -> Result<OriginAuth> {
    match claimed {
        ClaimedAuth::ProviderSignature => Ok(OriginAuth::ProviderSignature),
        ClaimedAuth::TrustedTerminator if terminators.is_empty() => bail!(
            "this Spore trusts a terminator, so --trusted-terminator must name it; \
             otherwise every peer can assert an identity nobody verified"
        ),
        ClaimedAuth::TrustedTerminator => {
            Ok(OriginAuth::trusted_terminator(terminators.iter().copied()))
        }
    }
}

/// Where a claim lives between processes, so two Spores cannot share a name.
fn claim_dir(state: Option<&Path>) -> PathBuf {
    state
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            std::env::var_os("SOIL_STATE")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                        .join(".local/state/soil")
                })
        })
        .join("hosts")
}

/// Take the claimed names, or refuse.
///
/// A name is exclusive. Two Spores on one vhost do not round-robin: whichever
/// bound last would answer, and the other would look deployed while serving
/// nothing - the failure that is hardest to see from outside.
fn claim_hosts(spore: &str, hosts: &[String], state: Option<&Path>) -> Result<()> {
    let dir = claim_dir(state);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let overrides = overrides();
    for host in hosts {
        if let Some(pinned) = overrides.get(host)
            && pinned != spore
        {
            bail!(
                "{host} is pinned to {pinned} by the host override file; refusing to let {spore} take it"
            );
        }
        let claim = dir.join(claim_name(host));
        if let Ok(body) = std::fs::read_to_string(&claim) {
            let mut parts = body.split_whitespace();
            let owner = parts.next().unwrap_or_default();
            let pid = parts.next().and_then(|pid| pid.parse::<u32>().ok());
            if owner != spore && pid.is_some_and(alive) {
                bail!(
                    "{host} is already served by {owner} (pid {}); a name answers from one Spore at a time",
                    pid.unwrap_or_default()
                );
            }
        }
        std::fs::write(&claim, format!("{spore} {}\n", std::process::id()))
            .with_context(|| format!("claim {host}"))?;
    }
    Ok(())
}

fn claim_name(host: &str) -> String {
    host.chars()
        .map(|character| {
            if character == '.' || character == '*' {
                '_'
            } else {
                character
            }
        })
        .collect()
}

/// Whether a pid is still running, without trusting the claim file's age.
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
            || std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Host-level overrides: `[hosts]` in `~/.config/soil/hosts.toml`.
fn overrides() -> BTreeMap<String, String> {
    let path = std::env::var_os("SOIL_HOSTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".config/soil/hosts.toml")
        });
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        tracing::warn!(
            path = %path.display(),
            "host override file is not valid TOML; ignoring it"
        );
        return BTreeMap::new();
    };
    value
        .get("hosts")
        .and_then(|hosts| hosts.as_table())
        .map(|table| {
            table
                .iter()
                .filter_map(|(host, owner)| {
                    owner.as_str().map(|owner| (host.clone(), owner.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn temp_root(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "spore".into());
    std::env::temp_dir().join(format!("soil-spore-{}-{name}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(http: &str) -> String {
        format!(
            "schema = 1\n[spore]\nid = \"dev.fungos.test\"\nname = \"test\"\nversion = \"0.1.0\"\nlicense = \"MIT\"\nsummary = \"s\"\n[engine]\nsoil = \">=0.1\"\ntarget = \"wasm32-wasip2\"\n[module]\npath = \"module.wasm\"\n[ui]\nentry = \"ui/main.lua\"\n[launch]\ndefault = \"canvas\"\nsurfaces = [\"canvas\"]\n{http}"
        )
    }

    #[test]
    fn a_bridge_travels_with_the_run() {
        let (package, run) = parse(&[
            "run".into(),
            "/tmp/example".into(),
            "--hue-bridge".into(),
            "http://192.168.1.9".into(),
        ])
        .expect("parse");
        assert_eq!(package, PathBuf::from("/tmp/example"));
        assert_eq!(
            run.present.hue_bridge.as_deref(),
            Some("http://192.168.1.9"),
            "the window a Spore opens inherits the bridge the run was given"
        );
    }

    #[test]
    fn the_claim_is_the_manifest_and_nothing_else() {
        let parsed = Manifest::parse(&manifest(
            "[http]\nhosts = [\"contacts.myc\"]\nroutes = [{ methods = [\"GET\"], path = \"/\" }]\n",
        ))
        .expect("manifest");
        let http = parsed.http.expect("a claim");
        assert_eq!(http.hosts, vec!["contacts.myc"]);
    }

    #[test]
    fn a_second_spore_cannot_take_a_live_name() {
        let state = std::env::temp_dir().join(format!(
            "soil-claims-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        claim_hosts(
            "dev.fungos.contacts",
            &["contacts.myc".into()],
            Some(&state),
        )
        .expect("first claim wins");
        // Same Spore re-claiming is fine: a restart must not be a conflict.
        claim_hosts(
            "dev.fungos.contacts",
            &["contacts.myc".into()],
            Some(&state),
        )
        .expect("re-claim by the same Spore");
        let stolen = claim_hosts("dev.fungos.other", &["contacts.myc".into()], Some(&state));
        assert!(stolen.is_err(), "a name answers from one Spore at a time");
        let _ = std::fs::remove_dir_all(state);
    }
}

#[cfg(test)]
mod announce_tests {
    use super::*;

    /// The endpoint is this machine's answer, so it collects: one host can be
    /// reachable over the mesh and the LAN at once, and the lease carries both.
    #[test]
    fn announce_endpoints_are_per_host_and_collected() {
        let args: Vec<String> = "run /tmp/spore --announce-endpoint http://10.253.0.6:8790              --announce-endpoint tcp://192.168.1.39:8790 --announce-unsigned-dev"
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let (_, run) = parse(&args).expect("parse");
        assert_eq!(
            run.announce_endpoints,
            ["http://10.253.0.6:8790", "tcp://192.168.1.39:8790"]
        );
        assert!(run.announce_unsigned_dev);
    }
}
