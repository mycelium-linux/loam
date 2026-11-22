//! Minimal but real session state for roaming between hosts.
//!
//! Three things have to survive a host switch:
//!   * credentials  — so a second host never re-pairs or re-authenticates
//!   * device identity — address + the strings the vendor API keys on
//!   * cursors      — where each bulk stream was drained to
//!
//! The lease is deliberately short-lived and advisory: the radio can only be
//! owned by one host at a time, and a crashed host must lose the device quickly.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Creds {
    /// Vendor REST token (`token` header). Shared between hosts.
    pub token: Option<String>,
    pub uid: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Stable key for this device: the BLE address is platform-specific on macOS
    /// (CoreBluetooth UUID), so prefer hardware_version + serial when known.
    #[serde(alias = "ring_id")]
    pub device_id: String,
    pub address: String,
    pub name: Option<String>,
    pub hardware_version: Option<String>,
    pub firmware_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub host_id: String,
    #[serde(alias = "ring_id")]
    pub device_id: String,
    pub claimed_at_ms: u64,
    pub expires_at_ms: u64,
}

impl Lease {
    pub fn is_live(&self, at_ms: u64) -> bool {
        at_ms < self.expires_at_ms
    }
}

/// Another host holds this device's lease. Typed so a host maps it to the
/// guest's `leased` error instead of matching on message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leased {
    pub device_id: String,
    pub host_id: String,
    pub remaining_ms: u64,
}

impl std::fmt::Display for Leased {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "device {} is leased by {} for another {} ms",
            self.device_id, self.host_id, self.remaining_ms
        )
    }
}

impl std::error::Error for Leased {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(alias = "ring_id")]
    pub device_id: String,
    pub stream: String,
    /// Inclusive lower bound already drained.
    pub since_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub creds: Creds,
    #[serde(default, alias = "rings")]
    pub devices: Vec<DeviceIdentity>,
    /// One live claim per device. A host may hold several at once; two hosts
    /// may not hold the same device. Old session files carried a single
    /// `lease`; it is dropped on read, and leases expire in minutes anyway.
    #[serde(default)]
    pub leases: Vec<Lease>,
    pub cursors: Vec<Cursor>,
}

impl Session {
    /// The first live claim, for callers that only ever hold one device.
    /// Multi-device hosts use [`Self::leases`] directly.
    #[must_use]
    pub fn lease(&self) -> Option<&Lease> {
        self.leases.first()
    }
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    session: Session,
}

#[allow(dead_code)]
impl SessionStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let session = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse session {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Session::default(),
            Err(e) => return Err(e).with_context(|| format!("read session {}", path.display())),
        };
        Ok(Self { path, session })
    }

    pub fn get(&self) -> &Session {
        &self.session
    }

    /// Re-read the shared file before a lease decision. The lease is advisory
    /// and file-backed, so a host that opened hours ago must not decide from a
    /// stale copy. Cross-machine leases still need the signed Mycelium lease.
    fn reload(&mut self) -> Result<()> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                self.session = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse session {}", self.path.display()))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.session = Session::default();
            }
            Err(e) => {
                return Err(e).with_context(|| format!("read session {}", self.path.display()));
            }
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(&self.session)?;
        std::fs::write(&self.path, bytes)
            .with_context(|| format!("write session {}", self.path.display()))
    }

    pub fn upsert_device(&mut self, device: DeviceIdentity) -> Result<()> {
        // Reload before writing: credentials and cursors share this file with the
        // lease, and a stale copy must not erase another host's claim.
        self.reload()?;
        match self
            .session
            .devices
            .iter_mut()
            .find(|existing| existing.device_id == device.device_id)
        {
            Some(existing) => *existing = device,
            None => self.session.devices.push(device),
        }
        self.save()
    }

    /// Claim the radio for `host_id`. Fails loudly if another host holds a live
    /// lease — that is the "don't fight over the link" rule, enforced.
    pub fn claim(&mut self, device_id: &str, host_id: &str, ttl_ms: u64) -> Result<Lease> {
        self.claim_at(device_id, host_id, ttl_ms, now_ms())
    }

    /// Deterministic form of [`claim`] for tests and simulated clocks. The
    /// lease rule is the same; only the clock is injected.
    pub fn claim_at(
        &mut self,
        device_id: &str,
        host_id: &str,
        ttl_ms: u64,
        at: u64,
    ) -> Result<Lease> {
        self.reload()?;
        self.session.leases.retain(|lease| lease.is_live(at));
        if let Some(current) = self
            .session
            .leases
            .iter()
            .find(|lease| lease.device_id == device_id && lease.host_id != host_id)
        {
            return Err(anyhow::Error::new(Leased {
                device_id: device_id.to_string(),
                host_id: current.host_id.clone(),
                remaining_ms: current.expires_at_ms.saturating_sub(at),
            }));
        }
        let lease = Lease {
            host_id: host_id.to_string(),
            device_id: device_id.to_string(),
            claimed_at_ms: at,
            expires_at_ms: at.saturating_add(ttl_ms),
        };
        // Replace this host's own claim for the device rather than stacking a
        // second one; another host's claim was refused above.
        self.session
            .leases
            .retain(|existing| existing.device_id != device_id || existing.host_id != host_id);
        self.session.leases.push(lease.clone());
        self.save()?;
        Ok(lease)
    }

    pub fn release(&mut self, host_id: &str) -> Result<()> {
        self.release_where(|lease| lease.host_id == host_id)
    }

    /// Give up one device's lease without touching this host's other links.
    pub fn release_device(&mut self, device_id: &str, host_id: &str) -> Result<()> {
        self.release_where(|lease| lease.device_id == device_id && lease.host_id == host_id)
    }

    /// Re-read first: a host that lost the lease must not wipe the winner's
    /// claim with a stale in-memory copy.
    fn release_where(&mut self, matches: impl Fn(&Lease) -> bool) -> Result<()> {
        self.reload()?;
        let before = self.session.leases.len();
        self.session.leases.retain(|lease| !matches(lease));
        if self.session.leases.len() != before {
            self.save()?;
        }
        Ok(())
    }

    /// Heartbeat a live link's lease. The lease is a rolling claim, not a lock:
    /// a connected host keeps renewing, a crashed one stops and expires.
    ///
    /// Writes only once half the TTL is gone, or re-claims when the lease
    /// lapsed. `Err` means another host took the device while this one held it.
    pub fn renew(&mut self, device_id: &str, host_id: &str, ttl_ms: u64) -> Result<Lease> {
        self.renew_at(device_id, host_id, ttl_ms, now_ms())
    }

    /// Deterministic form of [`renew`] for tests and simulated clocks.
    pub fn renew_at(
        &mut self,
        device_id: &str,
        host_id: &str,
        ttl_ms: u64,
        at: u64,
    ) -> Result<Lease> {
        self.reload()?;
        if let Some(current) = self
            .session
            .leases
            .iter()
            .find(|lease| lease.device_id == device_id && lease.host_id == host_id)
        {
            // Renew at the halfway point, not after the lease is gone.
            if current.expires_at_ms.saturating_sub(at) > ttl_ms / 2 {
                return Ok(current.clone());
            }
        }
        self.claim_at(device_id, host_id, ttl_ms, at)
    }

    /// The id this peripheral was claimed with, if any.
    #[must_use]
    pub fn device_id_for(&self, address: &str) -> Option<String> {
        self.session
            .devices
            .iter()
            .find(|device| device.address.eq_ignore_ascii_case(address))
            .map(|device| device.device_id.clone())
    }

    /// Bind an id to a peripheral's platform address. One id owns one address:
    /// claiming an address that belonged to another id replaces that claim, and
    /// re-claiming an id moves it to the current peripheral. That keeps a
    /// replaced ring from leaving a second address under the same identity.
    pub fn claim_device(
        &mut self,
        address: &str,
        device_id: &str,
        name: Option<&str>,
    ) -> Result<()> {
        self.reload()?;
        self.session.devices.retain(|device| {
            !device.address.eq_ignore_ascii_case(address) && device.device_id != device_id
        });
        self.session.devices.push(DeviceIdentity {
            device_id: device_id.to_string(),
            address: address.to_string(),
            name: name.map(str::to_string),
            hardware_version: None,
            firmware_version: None,
        });
        self.save()
    }

    pub fn cursor_for(&self, device_id: &str, stream: &str) -> Option<u64> {
        self.session
            .cursors
            .iter()
            .find(|c| c.device_id == device_id && c.stream == stream)
            .map(|c| c.since_ms)
    }

    pub fn set_cursor(&mut self, device_id: &str, stream: &str, since_ms: u64) -> Result<()> {
        self.reload()?;
        match self
            .session
            .cursors
            .iter_mut()
            .find(|c| c.device_id == device_id && c.stream == stream)
        {
            Some(c) => c.since_ms = since_ms,
            None => self.session.cursors.push(Cursor {
                device_id: device_id.to_string(),
                stream: stream.to_string(),
                since_ms,
            }),
        }
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_host_cannot_take_a_live_lease() {
        let path = std::env::temp_dir().join(format!(
            "soil-ble-session-test-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&path);

        let mut first = SessionStore::open(&path).expect("first store");
        let mut second = SessionStore::open(&path).expect("second store");
        first
            .claim_at("sim-pinetime", "host-a", 60_000, 1_000)
            .expect("first claim");

        let error = second
            .claim_at("sim-pinetime", "host-b", 60_000, 1_001)
            .expect_err("second claim must be refused");
        assert!(error.to_string().contains("is leased by"));

        // The contender can take the link after the first lease expires; a
        // lease is advisory, not a permanent lock.
        second
            .claim_at("sim-pinetime", "host-b", 60_000, 61_001)
            .expect("claim after expiry");

        first
            .release("host-a")
            .expect("release is a no-op for a lost lease");
        assert_eq!(
            first.get().lease().map(|lease| lease.host_id.as_str()),
            Some("host-b"),
            "a stale host must not clear the new holder's lease"
        );
        first.release("host-b").expect("winner releases");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_stale_host_write_does_not_erase_a_live_lease() {
        let path = std::env::temp_dir().join(format!(
            "soil-ble-session-write-test-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&path);

        let mut owner = SessionStore::open(&path).expect("owner store");
        let mut other = SessionStore::open(&path).expect("other store");
        owner
            .claim_at("sim-pinetime", "host-a", 60_000, 1_000)
            .expect("claim");

        other
            .upsert_device(DeviceIdentity {
                device_id: "sim-pinetime".to_string(),
                address: "platform-id".to_string(),
                name: Some("PineTime".to_string()),
                hardware_version: None,
                firmware_version: None,
            })
            .expect("upsert");

        assert_eq!(
            other.get().lease().map(|lease| lease.host_id.as_str()),
            Some("host-a"),
            "a credential write must not erase the live lease"
        );
        let _ = std::fs::remove_file(&path);
    }

    fn temp_session(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "soil-ble-session-{tag}-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_claim_binds_one_id_to_one_address() {
        let path = temp_session("claim");
        let mut store = SessionStore::open(&path).expect("store");
        assert_eq!(store.device_id_for("addr-a"), None);

        store
            .claim_device("addr-a", "ring-left", Some("COLMI Ring 2"))
            .expect("claim");
        assert_eq!(store.device_id_for("addr-a").as_deref(), Some("ring-left"));
        assert_eq!(store.device_id_for("addr-b"), None);

        // The same id moving to a different ring drops the old address, so a
        // replaced unit cannot keep answering under the same identity.
        store
            .claim_device("addr-b", "ring-left", Some("COLMI Ring 2"))
            .expect("re-claim");
        assert_eq!(store.device_id_for("addr-a"), None);
        assert_eq!(store.device_id_for("addr-b").as_deref(), Some("ring-left"));
        assert_eq!(store.get().devices.len(), 1);

        // Claiming an address for a second id replaces the first id's claim.
        store
            .claim_device("addr-b", "ring-right", None)
            .expect("reassign");
        assert_eq!(store.device_id_for("addr-b").as_deref(), Some("ring-right"));
        assert_eq!(store.get().devices.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn closing_one_link_keeps_the_others_lease() {
        let path = temp_session("multi");
        let mut store = SessionStore::open(&path).expect("store");
        store
            .claim_at("sim-pinetime", "host-a", 60_000, 1_000)
            .expect("claim pinetime");
        store
            .claim_at("sim-ring", "host-a", 60_000, 1_000)
            .expect("claim ring");
        assert_eq!(store.get().leases.len(), 2, "one lease per device");

        store
            .release_device("sim-pinetime", "host-a")
            .expect("release pinetime");
        let held: Vec<_> = store
            .get()
            .leases
            .iter()
            .map(|lease| lease.device_id.as_str())
            .collect();
        assert_eq!(
            held,
            vec!["sim-ring"],
            "closing one link must not drop another link's lease"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lease_refusal_is_typed_and_renewable() {
        let path = temp_session("renew");
        let mut host_a = SessionStore::open(&path).expect("host a");
        let mut host_b = SessionStore::open(&path).expect("host b");
        host_a
            .claim_at("sim-pinetime", "host-a", 10_000, 1_000)
            .expect("claim");

        // Half the TTL is gone, so the heartbeat re-claims from `at`.
        let renewed = host_a
            .renew_at("sim-pinetime", "host-a", 10_000, 6_000)
            .expect("renew");
        assert_eq!(renewed.expires_at_ms, 16_000);

        // A healthy lease is not rewritten on every heartbeat.
        let untouched = host_a
            .renew_at("sim-pinetime", "host-a", 10_000, 7_000)
            .expect("renew");
        assert_eq!(untouched.expires_at_ms, 16_000);

        host_b
            .claim_at("sim-pinetime", "host-b", 10_000, 20_000)
            .expect("claim after expiry");
        let error = host_a
            .renew_at("sim-pinetime", "host-a", 10_000, 20_500)
            .expect_err("a lost lease must not be renewed silently");
        let leased = error.downcast_ref::<Leased>().expect("typed lease refusal");
        assert_eq!(leased.host_id, "host-b");
        let _ = std::fs::remove_file(&path);
    }
}
