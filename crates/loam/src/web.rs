//! Tiny local web surface for manifest-declared static Spores.
//!
//! This is deliberately not the HTTP component gateway. It is the static
//! `[[surfaces]] kind = "web"` path: a package ships `web/index.html` and its
//! assets, the host serves them on a loopback listener, and `loamd` publishes
//! the matching Mycelium service lease.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::spore::{Manifest, SurfaceKind};

#[derive(Debug, Clone)]
pub struct StaticSurface {
    pub package: PathBuf,
    pub manifest: Manifest,
    pub entry: PathBuf,
    pub name: String,
    pub kind: String,
    pub ttl_secs: u64,
}

/// Find static web surfaces in a set of package roots.
pub fn discover(roots: &[PathBuf]) -> Result<Vec<StaticSurface>> {
    let mut found = Vec::new();
    for root in roots {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let package = entry.path();
            if !package.is_dir() || !package.join("spore.toml").is_file() {
                continue;
            }
            let source = match fs::read_to_string(package.join("spore.toml")) {
                Ok(source) => source,
                Err(_) => continue,
            };
            let manifest = match Manifest::parse(&source) {
                Ok(manifest) => manifest,
                Err(_) => continue,
            };
            let Some(anycast) = &manifest.anycast else {
                continue;
            };
            let Some(surface) = manifest
                .surfaces
                .iter()
                .find(|surface| surface.kind == SurfaceKind::Web)
            else {
                continue;
            };
            let Some(entry) = surface.entry(manifest.ui.as_ref()) else {
                continue;
            };
            let entry = package.join(entry);
            if !entry.is_file() {
                continue;
            }
            let name = anycast.name.clone();
            let kind = anycast.kind.clone();
            let ttl_secs = anycast.ttl_secs;
            found.push(StaticSurface {
                package,
                manifest,
                entry,
                name,
                kind,
                ttl_secs,
            });
        }
    }
    found.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(found)
}

/// Serve one or more static surfaces. The first surface is the fallback when
/// the request Host does not name a package.
pub fn serve(surfaces: Vec<StaticSurface>, listener: TcpListener) -> Result<()> {
    let sites = surfaces
        .into_iter()
        .map(|surface| {
            let root = surface
                .entry
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| surface.package.clone());
            (surface, root)
        })
        .collect::<Vec<_>>();
    if sites.is_empty() {
        return Ok(());
    }
    loop {
        let (stream, _) = listener.accept()?;
        let sites = sites.clone();
        std::thread::spawn(move || {
            let _ = handle(stream, &sites);
        });
    }
}

fn handle(mut stream: TcpStream, sites: &[(StaticSurface, PathBuf)]) -> Result<()> {
    let mut buffer = [0u8; 8192];
    let read = stream.read(&mut buffer)?;
    if read == 0 {
        return Ok(());
    }
    let request = String::from_utf8_lossy(&buffer[..read]);
    let mut lines = request.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let raw_path = parts.next().unwrap_or("/");
    if !matches!(method, "GET" | "HEAD") {
        return respond(&mut stream, 405, "text/plain", b"method not allowed");
    }
    let host = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("host")
                .then(|| value.trim().to_ascii_lowercase())
        })
        .unwrap_or_default();
    let (_, root) = sites
        .iter()
        .find(|(site, _)| {
            host == format!("{}.myc", site.name) || host.starts_with(&format!("{}.", site.name))
        })
        .or_else(|| sites.first())
        .expect("sites is non-empty");

    let path = raw_path.split('?').next().unwrap_or("/");
    let mut relative = path.trim_start_matches('/');
    if relative.is_empty() {
        relative = "index.html";
    }
    if relative.split('/').any(|part| part == ".." || part == ".") {
        return respond(&mut stream, 400, "text/plain", b"invalid path");
    }
    let file = root.join(relative);
    match fs::read(&file) {
        Ok(bytes) => {
            let mime = media_type(&file);
            respond(&mut stream, 200, mime, &bytes)
        }
        Err(_) => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn respond(stream: &mut TcpStream, status: u16, mime: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    if status == 200 {
        stream.write_all(body)?;
    }
    stream.flush()?;
    Ok(())
}

fn media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The lease endpoint's bind address is not a route; it is a local hint.
pub fn loopback_endpoint(listener: &TcpListener) -> Result<String> {
    let addr = listener.local_addr().context("web listener address")?;
    Ok(format!("http://{addr}"))
}
