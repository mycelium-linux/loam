//! Host side of `fungos:mycelium/fleet`.
//!
//! The Mycelium control plane owns the fleet. This adapter owns only the call:
//! it runs the stable CLI contract (`mycelium fleet status --json` and
//! `mycelium topology --json`) and returns a bounded projection. It never links
//! Mycelium crates, opens the daemon socket, or hands a Spore the topology
//! document.
//!
//! Loose coupling is the same shape `palisade` and `soil-principal-host` use: a
//! configured binary, a documented JSON contract, and no shared internal type.
//! The read is live; a short cache keeps a wall that renders every second from
//! forking the daemon every second.

use std::{
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;

wasmtime::component::bindgen!({ path: "wit", world: "fleet-host" });

pub use fungos::mycelium::fleet as api;

/// One row of `mycelium fleet status --json`.
///
/// Every field is optional because the CLI is a projection too. A missing field
/// is unknown, never a zero.
#[derive(Debug, Clone, Default, Deserialize)]
struct StatusRow {
    node_id: Option<String>,
    hostname: Option<String>,
    site: Option<String>,
    platform: Option<String>,
    architecture: Option<String>,
    daemon_version: Option<String>,
    release_version: Option<String>,
    last_seen: Option<i64>,
    ssh_listening: Option<bool>,
    update_state: Option<String>,
}

impl StatusRow {
    fn into_peer(self) -> api::Peer {
        let id = self
            .node_id
            .or_else(|| self.hostname.clone())
            .unwrap_or_else(|| "unknown".to_string());
        api::Peer {
            id,
            hostname: self.hostname,
            site: self.site,
            platform: self.platform,
            architecture: self.architecture,
            daemon_version: self.daemon_version,
            release_version: self.release_version,
            last_seen: self.last_seen.map(|seen| seen.max(0) as u64),
            ssh_listening: self.ssh_listening,
            update_state: self.update_state,
        }
    }
}

/// One live fleet reader, shared by the stores of one host process.
///
/// Named `MyceliumFleet`, not `FleetHost`: the generated world host already owns
/// that name, and two types with one name is how a host adapter ends up calling
/// itself.
#[derive(Clone)]
pub struct MyceliumFleet {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    bin: PathBuf,
    refresh: Duration,
    /// A live read that outlives this is a dead daemon, not a slow one. A wall
    /// must not block a render waiting for it.
    timeout: Duration,
    cache: Option<(Instant, api::Snapshot)>,
}

impl MyceliumFleet {
    #[must_use]
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                bin: bin.into(),
                refresh: Duration::from_secs(15),
                timeout: Duration::from_secs(4),
                cache: None,
            })),
        }
    }

    /// How long one read is reused. A render loop should not fork the daemon
    /// per frame; a person looking at the wall should not wait longer than a
    /// refresh to see a peer leave.
    #[must_use]
    pub fn with_refresh(self, refresh: Duration) -> Self {
        self.inner.lock().expect("fleet host poisoned").refresh = refresh;
        self
    }

    /// Bound one daemon read. The default is four seconds.
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.inner.lock().expect("fleet host poisoned").timeout = timeout;
        self
    }

    pub fn status(&self) -> Result<api::Snapshot, api::Error> {
        let mut inner = self.inner.lock().expect("fleet host poisoned");
        if let Some((read_at, snapshot)) = &inner.cache
            && read_at.elapsed() < inner.refresh
        {
            return Ok(snapshot.clone());
        }
        let snapshot = inner.read()?;
        inner.cache = Some((Instant::now(), snapshot.clone()));
        Ok(snapshot)
    }
}

impl Inner {
    fn read(&self) -> Result<api::Snapshot, api::Error> {
        let peers = read_peers(&self.bin, self.timeout)?;
        Ok(api::Snapshot {
            captured_ms: now_ms(),
            peers,
            topology: read_topology(&self.bin, self.timeout),
        })
    }
}

impl api::Host for MyceliumFleet {
    fn status(&mut self) -> Result<api::Snapshot, api::Error> {
        MyceliumFleet::status(self)
    }
}

/// Link `fungos:mycelium/fleet` for a store that implements [`api::Host`].
pub fn add_to_linker<T>(linker: &mut wasmtime::component::Linker<T>) -> anyhow::Result<()>
where
    T: api::Host + 'static,
{
    api::add_to_linker::<T, wasmtime::component::HasSelf<T>>(linker, |state| state)?;
    Ok(())
}

fn read_peers(bin: &PathBuf, timeout: Duration) -> Result<Vec<api::Peer>, api::Error> {
    let bytes = run(bin, &["fleet", "status", "--json"], timeout)?;
    let rows: Vec<StatusRow> = serde_json::from_slice(&bytes).map_err(|_| api::Error::Invalid)?;
    Ok(rows.into_iter().map(StatusRow::into_peer).collect())
}

/// Run one bounded CLI read. A child that does not exit by the deadline is
/// killed: a live capability must fail visibly, not hang the render.
fn run(bin: &PathBuf, args: &[&str], timeout: Duration) -> Result<Vec<u8>, api::Error> {
    let mut child = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            tracing::debug!(%error, bin = %bin.display(), "mycelium read failed");
            api::Error::Unavailable
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|_| api::Error::Io)? {
            Some(status) => {
                if !status.success() {
                    return Err(api::Error::Io);
                }
                let mut bytes = Vec::new();
                child
                    .stdout
                    .take()
                    .ok_or(api::Error::Io)?
                    .read_to_end(&mut bytes)
                    .map_err(|_| api::Error::Io)?;
                return Ok(bytes);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::debug!(bin = %bin.display(), "mycelium read timed out");
                return Err(api::Error::Unavailable);
            }
            None => thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// Topology is optional: a host with an older daemon still has peers, and a
/// wall without four counts is better than no wall. The document itself never
/// crosses the boundary.
fn read_topology(bin: &PathBuf, timeout: Duration) -> Option<api::Topology> {
    let bytes = run(bin, &["topology", "--json"], timeout).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let objects = |key: &str| {
        value
            .get(key)
            .and_then(|value| value.as_object())
            .map_or(0, serde_json::Map::len) as u32
    };
    let arrays = |key: &str| {
        value
            .get(key)
            .and_then(|value| value.as_array())
            .map_or(0, Vec::len) as u32
    };
    Some(api::Topology {
        nodes: objects("nodes"),
        segments: objects("segments"),
        conflicts: arrays("conflicts"),
        leases: arrays("leases"),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("mycelium");
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    const STATUS: &str = r#"[
      {"node_id":"a","hostname":"batty","site":"fpl","platform":"darwin","architecture":"aarch64","daemon_version":"0.1.0","release_version":"0.2.27","last_seen":100,"ssh_listening":true,"update_state":"current"},
      {"node_id":null,"hostname":null,"site":null,"last_seen":null}
    ]"#;

    const TOPOLOGY: &str =
        r#"{"nodes":{"a":{},"b":{}},"segments":{"s":{}},"conflicts":[{},{},{}],"leases":[{}]}"#;

    #[test]
    fn a_live_read_maps_peers_and_summarizes_topology() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(
            dir.path(),
            &format!(
                "#!/bin/sh\ncase \"$1\" in fleet) cat <<'JSON'\n{STATUS}\nJSON\n;; topology) cat <<'JSON'\n{TOPOLOGY}\nJSON\n;; esac\n"
            ),
        );
        let snapshot = MyceliumFleet::new(bin).status().unwrap();
        assert_eq!(snapshot.peers.len(), 2);
        assert_eq!(snapshot.peers[0].hostname.as_deref(), Some("batty"));
        assert_eq!(snapshot.peers[0].last_seen, Some(100));
        assert_eq!(snapshot.peers[1].id, "unknown");
        let topology = snapshot.topology.unwrap();
        assert_eq!(topology.nodes, 2);
        assert_eq!(topology.segments, 1);
        assert_eq!(topology.conflicts, 3);
        assert_eq!(topology.leases, 1);
    }

    #[test]
    fn one_read_is_shared_until_the_refresh_window() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("count");
        let bin = script(
            dir.path(),
            &format!(
                "#!/bin/sh\necho x >> {}\ncat <<'JSON'\n{STATUS}\nJSON\n",
                counter.display()
            ),
        );
        let host = MyceliumFleet::new(bin);
        host.status().unwrap();
        host.status().unwrap();
        // One read is two CLI calls (status + topology), and the second `status()`
        // must not add two more.
        assert_eq!(std::fs::read_to_string(counter).unwrap().lines().count(), 2);
    }

    #[test]
    fn a_missing_binary_is_unavailable_not_empty() {
        let host = MyceliumFleet::new("/nonexistent/mycelium");
        assert!(matches!(host.status(), Err(api::Error::Unavailable)));
    }
}
