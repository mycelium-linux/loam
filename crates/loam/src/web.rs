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

use crate::spore::{Anycast, Manifest, Surface, SurfaceKind, Ui, soil::SoilManifest};

#[derive(Debug, Clone)]
pub struct StaticSurface {
    pub package: PathBuf,
    pub spore_id: String,
    pub version: String,
    pub anycast: Anycast,
    pub entry: PathBuf,
    pub name: String,
    pub kind: String,
    pub ttl_secs: u64,
}

/// Find static web surfaces in a set of package roots.
///
/// `spore.soil` wins when a package carries both forms: it is the source
/// manifest the author edits, and a lease should follow the file being edited.
/// `spore.toml` remains the installed-package form.
pub fn discover(roots: &[PathBuf]) -> Result<Vec<StaticSurface>> {
    let mut found = Vec::new();
    for root in roots {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let package = entry.path();
            if !package.is_dir() {
                continue;
            }
            let soil = package.join("spore.soil");
            let toml = package.join("spore.toml");
            let surface = if soil.is_file() {
                fs::read_to_string(&soil)
                    .ok()
                    .and_then(|source| SoilManifest::parse(&source).ok())
                    .and_then(|manifest| {
                        let anycast = manifest.anycast.clone()?;
                        static_surface(
                            &package,
                            manifest.id,
                            manifest.version,
                            anycast,
                            &manifest.surfaces,
                            None,
                        )
                    })
            } else if toml.is_file() {
                fs::read_to_string(&toml)
                    .ok()
                    .and_then(|source| Manifest::parse(&source).ok())
                    .and_then(|manifest| {
                        let anycast = manifest.anycast.clone()?;
                        static_surface(
                            &package,
                            manifest.spore.id,
                            manifest.spore.version,
                            anycast,
                            &manifest.surfaces,
                            manifest.ui.as_ref(),
                        )
                    })
            } else {
                None
            };
            if let Some(surface) = surface {
                found.push(surface);
            }
        }
    }
    found.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(found)
}

fn static_surface(
    package: &Path,
    spore_id: String,
    version: String,
    anycast: Anycast,
    surfaces: &[Surface],
    ui: Option<&Ui>,
) -> Option<StaticSurface> {
    let surface = surfaces
        .iter()
        .find(|surface| surface.kind == SurfaceKind::Web)?;
    let entry = package.join(surface.entry(ui)?);
    if !entry.is_file() {
        return None;
    }
    Some(StaticSurface {
        package: package.to_path_buf(),
        spore_id,
        version,
        name: anycast.name.clone(),
        kind: anycast.kind.clone(),
        ttl_secs: anycast.ttl_secs,
        anycast,
        entry,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn package(root: &Path, manifest_name: &str, manifest: &str) {
        let package = root.join("site");
        fs::create_dir_all(&package).expect("package");
        fs::write(package.join(manifest_name), manifest).expect("manifest");
        fs::write(package.join("index.html"), "<h1>site</h1>").expect("page");
    }

    #[test]
    fn discovers_a_web_surface_from_spore_soil() {
        let temp = tempfile::tempdir().expect("temp");
        package(
            temp.path(),
            "spore.soil",
            r#"(spore
                 (id "org.mycelium.web")
                 (version "0.1.0")
                 (anycast (name "web") (kind "web") (alpns ("soil/http/1")))
                 (surfaces (surface "web" (kind "web") (entry "index.html"))))"#,
        );
        let surfaces = discover(&[temp.path().to_path_buf()]).expect("discover");
        assert_eq!(surfaces.len(), 1);
        let surface = &surfaces[0];
        assert_eq!(surface.spore_id, "org.mycelium.web");
        assert_eq!(surface.version, "0.1.0");
        assert_eq!(surface.name, "web");
        assert_eq!(surface.kind, "web");
        assert_eq!(surface.anycast.alpns, vec!["soil/http/1"]);
        assert_eq!(surface.entry, temp.path().join("site/index.html"));
    }

    #[test]
    fn a_soil_package_without_an_anycast_claim_is_not_leased() {
        let temp = tempfile::tempdir().expect("temp");
        package(
            temp.path(),
            "spore.soil",
            r#"(spore
                 (id "org.mycelium.web")
                 (version "0.1.0")
                 (surfaces (surface "web" (kind "web") (entry "index.html"))))"#,
        );
        assert!(
            discover(&[temp.path().to_path_buf()])
                .expect("discover")
                .is_empty()
        );
    }
}
