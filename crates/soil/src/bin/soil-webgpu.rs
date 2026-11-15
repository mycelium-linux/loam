//! Run one component that imports `wasi:webgpu` and exports `run`.
//!
//! Usage:
//!   soil-webgpu <component.wasm>
//!   soil-webgpu --frame FILE.bgr [--size WxH] [--agents demo] <component.wasm>
//!   soil-webgpu --frame FILE.bgr --root VAULT --records DIR \
//!     --book BOOK --collection NAME --actor WHO <component.wasm>
//!   soil-webgpu --frame FILE.bgr --root VAULT --records DIR --collection NAME \
//!     --search <component.wasm>
//!   soil-webgpu --frame FILE.bgr --bus-seed FILE <component.wasm>
//!
//! `--root`, `--records`, `--book`, `--collection` and `--actor` are the grants
//! a document-shaped Spore asks for: one workspace to read, a book to write
//! pending entities into, and the identity the host attests for them.
//!
//! `--bus-seed` publishes `kind<TAB>body` lines on the in-process bus before the
//! guest runs. A popup surface is driven by what a wall announced, and a
//! one-shot capture has no wall: this is how a test says "someone is asking".

use std::{env, fs, path::Path, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

#[cfg(unix)]
use soil::http_host::GeodeCredentials;
use soil::http_host::HttpPolicy;
use soil::webgpu::{
    CanvasFrame, CatalogHost, FrameCapture, FramebufferPresenter, Grants, HostSetup, HttpAccess,
    InstallCatalog, RecordsAccess, SearchAccess, WebGpuRuntime,
};
use soil_agent_host::AgentBackend;

fn main() -> ExitCode {
    let mut frame_path = None;
    let mut fb_path: Option<String> = None;
    let mut records_dir: Option<PathBuf> = None;
    let mut vfs_root: Option<PathBuf> = None;
    let mut records_root: Option<PathBuf> = None;
    let mut book: Option<String> = None;
    let mut collection: Option<String> = None;
    let mut actor: Option<String> = None;
    let mut size = (480, 640);
    let mut spores_roots: Vec<PathBuf> = Vec::new();
    let mut origins = Vec::new();
    let mut store_origin: Option<String> = None;
    let mut geode_socket = None;
    let mut geode_purpose = None;
    let mut agents = None;
    let mut view = None;
    let mut agent = None;
    let mut search = false;
    let mut seed: Vec<(String, String)> = Vec::new();
    let mut component = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--frame" => {
                let Some(path) = args.next() else {
                    eprintln!("--frame requires FILE.bgr");
                    return ExitCode::FAILURE;
                };
                frame_path = Some(path);
            }
            "--fb" => {
                let Some(path) = args.next() else {
                    eprintln!("--fb requires a framebuffer device, e.g. /dev/fb0");
                    return ExitCode::FAILURE;
                };
                fb_path = Some(path);
            }
            "--records-dir" => {
                let Some(path) = args.next() else {
                    eprintln!("--records-dir requires a directory");
                    return ExitCode::FAILURE;
                };
                records_dir = Some(PathBuf::from(path));
            }
            "--root" => {
                let Some(path) = args.next() else {
                    eprintln!("--root requires a directory");
                    return ExitCode::FAILURE;
                };
                vfs_root = Some(PathBuf::from(path));
            }
            "--records" => {
                let Some(path) = args.next() else {
                    eprintln!("--records requires a directory");
                    return ExitCode::FAILURE;
                };
                records_root = Some(PathBuf::from(path));
            }
            "--book" => {
                let Some(value) = args.next() else {
                    eprintln!("--book requires a name");
                    return ExitCode::FAILURE;
                };
                book = Some(value);
            }
            "--collection" => {
                let Some(value) = args.next() else {
                    eprintln!("--collection requires a name");
                    return ExitCode::FAILURE;
                };
                collection = Some(value);
            }
            "--actor" => {
                let Some(value) = args.next() else {
                    eprintln!("--actor requires a principal name");
                    return ExitCode::FAILURE;
                };
                actor = Some(value);
            }
            "--size" => {
                let Some(value) = args.next() else {
                    eprintln!("--size requires WxH");
                    return ExitCode::FAILURE;
                };
                let Some((width, height)) = value.split_once('x') else {
                    eprintln!("--size requires WxH");
                    return ExitCode::FAILURE;
                };
                let (Ok(width), Ok(height)) = (width.parse(), height.parse()) else {
                    eprintln!("--size requires positive integers");
                    return ExitCode::FAILURE;
                };
                if width == 0 || height == 0 {
                    eprintln!("--size requires positive integers");
                    return ExitCode::FAILURE;
                }
                size = (width, height);
            }
            "--spores-root" => {
                let Some(path) = args.next() else {
                    eprintln!("--spores-root requires a directory");
                    return ExitCode::FAILURE;
                };
                spores_roots.push(PathBuf::from(path));
            }
            "--allow-origin" => {
                let Some(origin) = args.next() else {
                    eprintln!("--allow-origin requires a URL");
                    return ExitCode::FAILURE;
                };
                origins.push(origin);
            }
            "--store-origin" => {
                let Some(origin) = args.next() else {
                    eprintln!("--store-origin requires a URL");
                    return ExitCode::FAILURE;
                };
                store_origin = Some(origin);
            }
            "--agents" => {
                // Only one cast exists so far. Naming it explicitly keeps a
                // capture honest: a frame of the demo cast is not a frame of a
                // running Operator session.
                let Some(value) = args.next() else {
                    eprintln!("--agents requires a cast name (demo)");
                    return ExitCode::FAILURE;
                };
                if value != "demo" {
                    eprintln!("--agents: unknown cast {value:?} (demo)");
                    return ExitCode::FAILURE;
                }
                agents = Some(value);
            }
            "--bus-seed" => {
                let Some(path) = args.next() else {
                    eprintln!("--bus-seed needs a file of `kind<TAB>body` lines");
                    return ExitCode::FAILURE;
                };
                let text = match fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(error) => {
                        eprintln!("--bus-seed: {path}: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                for line in text.lines().filter(|line| !line.trim().is_empty()) {
                    match line.split_once('\t') {
                        Some((kind, body)) => {
                            seed.push((kind.to_string(), body.to_string()));
                        }
                        None => {
                            eprintln!("--bus-seed: {path}: line is not kind<TAB>body");
                            return ExitCode::FAILURE;
                        }
                    }
                }
            }
            "--search" => search = true,
            "--view" => {
                let Some(value) = args.next() else {
                    eprintln!("--view requires board|roster|stage|focus|popup");
                    return ExitCode::FAILURE;
                };
                view = Some(value);
            }
            "--agent" => {
                let Some(value) = args.next() else {
                    eprintln!("--agent requires an agent id");
                    return ExitCode::FAILURE;
                };
                agent = Some(value);
            }
            "--geode-socket" => {
                let Some(path) = args.next() else {
                    eprintln!("--geode-socket requires a path");
                    return ExitCode::FAILURE;
                };
                geode_socket = Some(PathBuf::from(path));
            }
            "--geode-purpose" => {
                let Some(purpose) = args.next() else {
                    eprintln!("--geode-purpose requires a purpose");
                    return ExitCode::FAILURE;
                };
                geode_purpose = Some(purpose);
            }
            _ => {
                if component.replace(arg).is_some() {
                    eprintln!("{USAGE}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    let Some(path) = component else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = if frame_path.is_some() || fb_path.is_some() {
        if records_dir.is_some()
            && (!origins.is_empty()
                || !spores_roots.is_empty()
                || store_origin.is_some()
                || agents.is_some())
        {
            eprintln!(
                "--records-dir alone cannot be combined with --allow-origin, --store-origin, --spores-root or --agents; pass --root or --actor to use the document grants"
            );
            return ExitCode::FAILURE;
        }
        let http = match build_http(origins, store_origin, geode_socket, geode_purpose) {
            Ok(http) => http,
            Err(error) => {
                eprintln!("webgpu: {error}");
                return ExitCode::FAILURE;
            }
        };
        let catalog: Option<Box<dyn CatalogHost>> = if spores_roots.is_empty() {
            None
        } else {
            Some(Box::new(InstallCatalog::with_roots(spores_roots)))
        };
        let cast: Option<Box<dyn AgentBackend>> = agents.map(|_| {
            Box::new(soil_agent_host::FixtureAgents::new().bind(agent.clone(), view.clone()))
                as Box<dyn AgentBackend>
        });
        let document_grants = vfs_root.is_some() || actor.is_some() || records_root.is_some();
        let search = if search {
            let Some(vfs) = vfs_root.clone() else {
                eprintln!("--search needs --root: search composes a workspace");
                return ExitCode::FAILURE;
            };
            let Some(records) = records_root.clone().or_else(|| records_dir.clone()) else {
                eprintln!("--search needs --records or --records-dir");
                return ExitCode::FAILURE;
            };
            Some(SearchAccess::open(
                vfs,
                records,
                collection.clone().into_iter().collect(),
            ))
        } else {
            None
        };
        let grants = if document_grants {
            let mut grants = Grants::new();
            grants.vfs = vfs_root;
            if let Some(root) = records_root {
                let Some(collection) = collection.clone() else {
                    eprintln!("--records requires --collection: the guest names a collection");
                    return ExitCode::FAILURE;
                };
                grants.records = Some(RecordsAccess::book(
                    &root,
                    collection.clone(),
                    book.unwrap_or_else(|| collection.clone()),
                    collection,
                ));
            }
            if let Some(actor) = actor {
                grants = grants.actor(actor);
            }
            Some(grants)
        } else {
            None
        };
        run_capture(Capture {
            bytes: &bytes,
            size,
            frame: frame_path.as_deref(),
            fb: fb_path.as_deref(),
            catalog,
            http,
            agents: cast,
            records_dir,
            grants,
            search,
            seed,
        })
    } else {
        soil::webgpu::run_component_blocking(&bytes).map(|status| {
            if status == 0 {
                println!("webgpu: ok");
            } else {
                eprintln!("webgpu: guest status {status}");
            }
        })
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("webgpu: {error:#}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "usage: soil-webgpu [--frame FILE.bgr] [--fb DEVICE] [--size WxH] [--records-dir DIR] [--spores-root DIR]... [--allow-origin URL]... [--store-origin URL] [--agents demo] [--view NAME] [--agent ID] [--search] [--bus-seed FILE] [--geode-socket PATH --geode-purpose PURPOSE] <component.wasm>";

/// Everything one capture run was asked for.
struct Capture<'a> {
    bytes: &'a [u8],
    size: (u32, u32),
    frame: Option<&'a str>,
    fb: Option<&'a str>,
    catalog: Option<Box<dyn CatalogHost>>,
    http: Option<HttpAccess>,
    agents: Option<Box<dyn AgentBackend>>,
    records_dir: Option<PathBuf>,
    grants: Option<Grants>,
    search: Option<SearchAccess>,
    seed: Vec<(String, String)>,
}

/// One frame, one guest, one store.
///
/// The presenter differs by destination (a file, a panel), everything else is
/// the same run. Building the setup here rather than in a library helper is
/// what lets `--bus-seed` publish on the very bus the guest will poll.
fn run_capture(capture: Capture<'_>) -> anyhow::Result<()> {
    let Capture {
        bytes,
        size,
        frame: frame_path,
        fb,
        catalog,
        http,
        agents,
        records_dir,
        grants,
        search,
        seed,
    } = capture;
    let (presenter, sink): (
        Box<dyn soil::webgpu::CanvasGpuPresenter>,
        Option<FrameCapture>,
    ) = match fb {
        Some(device) => (
            Box::new(FramebufferPresenter::open(device, size.0, size.1)?),
            None,
        ),
        None => {
            let capture = FrameCapture::new(size.0, size.1);
            (Box::new(capture.clone()), Some(capture))
        }
    };
    let runtime = match component_cache() {
        Some(cache) => WebGpuRuntime::new()?.with_component_cache(cache),
        None => WebGpuRuntime::new()?,
    };
    let mut setup = HostSetup::capture(presenter);
    if let Some(catalog) = catalog {
        setup.catalog = catalog;
    }
    setup.http = http;
    if let Some(agents) = agents {
        setup.agents = agents;
    }
    if let Some(root) = records_dir {
        setup.records = Some(RecordsAccess::vitals(root));
    }
    if let Some(grants) = grants {
        setup.vfs = grants.vfs;
        setup.records = grants.records.or(setup.records);
        setup.principal = grants.principal;
        setup.http = grants.http.or(setup.http);
    }
    setup.search = search;
    for (kind, body) in &seed {
        setup
            .bus
            .publish(kind, body.as_bytes(), None, 60 * 60 * 1000)
            .map_err(|error| anyhow::anyhow!("bus seed {kind}: {error}"))?;
    }
    let status = pollster::block_on(runtime.run_component_with(bytes, setup))?;
    if status != 0 {
        anyhow::bail!("guest status {status}");
    }
    match (fb, sink) {
        (Some(device), _) => {
            println!(
                "webgpu: ok fb={device} width={} height={} format=rgb565",
                size.0, size.1
            );
            Ok(())
        }
        (None, Some(capture)) => {
            let frame = capture
                .take()
                .ok_or_else(|| anyhow::anyhow!("guest did not present a Canvas frame"))?;
            let path = frame_path.ok_or_else(|| anyhow::anyhow!("--frame path"))?;
            write_bgr(path, &frame)?;
            println!(
                "webgpu: ok frame={path} width={} height={} format=bgr24",
                frame.width, frame.height
            );
            Ok(())
        }
        (None, None) => Ok(()),
    }
}

fn build_http(
    origins: Vec<String>,
    store_origin: Option<String>,
    geode_socket: Option<PathBuf>,
    geode_purpose: Option<String>,
) -> Result<Option<HttpAccess>, String> {
    match (geode_socket.as_ref(), geode_purpose.as_ref()) {
        (Some(_), None) | (None, Some(_)) => {
            return Err("--geode-socket and --geode-purpose must be used together".into());
        }
        _ => {}
    }
    if origins.is_empty() {
        return match store_origin {
            Some(origin) => HttpAccess::store(origin)
                .map(Some)
                .map_err(|error| format!("http policy: {error:#}")),
            None => Ok(None),
        };
    }
    let policy = HttpPolicy::new(origins, 16 * 1024 * 1024, Duration::from_secs(30))
        .map_err(|error| format!("http policy: {error:#}"))?;
    let mut access = HttpAccess::new(policy);
    if let (Some(socket), Some(purpose)) = (geode_socket, geode_purpose) {
        #[cfg(unix)]
        {
            access = access.with_credentials(Arc::new(GeodeCredentials::new(socket, purpose)));
        }
        #[cfg(not(unix))]
        {
            let _ = (socket, purpose);
            return Err("--geode-socket is only available on Unix hosts".into());
        }
    }
    Ok(Some(access))
}

/// Where compiled Spores are cached across capture runs. `SOIL_COMPONENT_CACHE`
/// overrides it; without a home directory every frame pays Cranelift again.
fn component_cache() -> Option<PathBuf> {
    if let Some(path) = env::var_os("SOIL_COMPONENT_CACHE") {
        return Some(PathBuf::from(path));
    }
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join("Library/Caches/soil/components"))
}

fn write_bgr(path: &str, frame: &CanvasFrame) -> anyhow::Result<()> {
    let path = Path::new(path);
    let mut bgr = Vec::with_capacity(frame.width as usize * frame.height as usize * 3);
    for pixel in frame.rgba.as_chunks::<4>().0 {
        bgr.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, bgr)?;
    fs::rename(temporary, path)?;
    Ok(())
}
