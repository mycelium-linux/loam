//! Link bookkeeping shared by every `soil:ble` host.
//!
//! Two rules live here so the desktop dev host and the protocol host enforce
//! them identically:
//!   * the link budget is checked before a handshake starts, never after,
//!   * every link owns one device lease, and closing it releases only that one.

use std::collections::HashMap;

use anyhow::Result;
use soil_ble_api::{Failure, LinkId, fail};

use crate::ble::Ble;
use crate::session::{self, SessionStore};

pub struct Links {
    session: SessionStore,
    host_id: String,
    /// link -> device id, so closing a link releases exactly one lease.
    devices: HashMap<LinkId, String>,
    ttl_ms: u64,
}

impl Links {
    #[must_use]
    pub fn new(session: SessionStore, host_id: impl Into<String>, ttl_ms: u64) -> Self {
        Self {
            session,
            host_id: host_id.into(),
            devices: HashMap::new(),
            ttl_ms,
        }
    }

    /// Open a link (or return the existing one) and take that device's lease.
    pub fn open(&mut self, ble: &mut Ble, device_id: &str) -> Result<LinkId> {
        if let Some((link, _)) = self
            .devices
            .iter()
            .find(|(_, id)| id.eq_ignore_ascii_case(device_id))
        {
            return Ok(*link);
        }
        let max = ble.max_links();
        if self.devices.len() >= max {
            return fail(Failure::OverBudget(format!(
                "{device_id}: {} of {max} links in use",
                self.devices.len()
            )));
        }

        self.session
            .claim(device_id, &self.host_id, self.ttl_ms)
            .map_err(lease_error)?;
        let link = match ble.connect(device_id) {
            Ok(link) => link,
            Err(error) => {
                // The lease was taken for a link that never opened.
                if let Err(release) = self.session.release_device(device_id, &self.host_id) {
                    eprintln!("lease release failed for {device_id}: {release:#}");
                }
                return Err(error);
            }
        };
        self.devices.insert(link, device_id.to_string());
        Ok(link)
    }

    /// Close one link and give up its lease. A failed transport disconnect still
    /// drops the host's claim: this host no longer owns that link either way.
    pub fn close(&mut self, ble: &mut Ble, link: LinkId) -> Result<()> {
        let Some(device_id) = self.devices.remove(&link) else {
            return fail(Failure::NotConnected(link));
        };
        let result = ble.disconnect(link);
        if let Err(error) = self.session.release_device(&device_id, &self.host_id) {
            eprintln!("lease release failed for {device_id}: {error:#}");
        }
        result
    }

    /// Renew one link's lease. Cheap while the lease is healthy; `Err` means
    /// another host took the device while this host held it.
    pub fn heartbeat(&mut self, link: LinkId) -> Result<()> {
        let device_id = self
            .devices
            .get(&link)
            .ok_or_else(|| anyhow::Error::new(Failure::NotConnected(link)))?;
        self.session
            .renew(device_id, &self.host_id, self.ttl_ms)
            .map(|_| ())
            .map_err(lease_error)
    }

    /// The id a peripheral was claimed with, so a scan can report it before
    /// the guest reads anything.
    #[must_use]
    pub fn device_id_for(&self, address: &str) -> Option<String> {
        self.session.device_id_for(address)
    }

    /// Bind a guest-chosen id to this peripheral's platform address.
    pub fn claim(&mut self, address: &str, name: Option<&str>, device_id: &str) -> Result<()> {
        let device_id = device_id.trim();
        if device_id.is_empty() {
            return fail(Failure::Invalid("empty device id".to_string()));
        }
        self.session.claim_device(address, device_id, name)
    }

    /// Device behind a link, for guest-visible identity.
    #[must_use]
    pub fn device(&self, link: LinkId) -> Option<&str> {
        self.devices.get(&link).map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Drop every link on shutdown so the next host does not wait out a lease.
    pub fn close_all(&mut self, ble: &mut Ble) {
        for link in self.devices.keys().copied().collect::<Vec<_>>() {
            if let Err(error) = self.close(ble, link) {
                eprintln!("link {link} close failed: {error:#}");
            }
        }
    }
}

/// Promote a session refusal to the guest-visible `Leased` failure.
fn lease_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<session::Leased>() {
        Some(leased) => anyhow::Error::new(Failure::Leased {
            device_id: leased.device_id.clone(),
            host_id: leased.host_id.clone(),
            remaining_ms: leased.remaining_ms,
        }),
        None => error,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStore;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_session(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "soil-ble-links-{tag}-{}-{stamp}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn host(path: &PathBuf, host_id: &str) -> Links {
        Links::new(
            SessionStore::open(path).expect("open session"),
            host_id,
            60_000,
        )
    }

    #[test]
    fn the_budget_is_spent_before_a_handshake_and_freed_per_link() {
        let path = temp_session("budget");
        let mut ble = Ble::sim().expect("sim backend");
        let mut links = host(&path, "host-a");

        for id in ["sim-pinetime", "sim-ring", "sim-ring-2"] {
            links.open(&mut ble, id).expect("within budget");
        }
        assert_eq!(links.len(), ble.max_links());

        let error = links
            .open(&mut ble, "sim-omi")
            .expect_err("the budget is spent");
        assert!(matches!(
            error.downcast_ref::<Failure>(),
            Some(Failure::OverBudget(_))
        ));
        assert_eq!(links.len(), 3, "a refused link is not half-opened");
        assert_eq!(
            links.session.get().leases.len(),
            3,
            "no lease was taken for the refused device"
        );

        let watch = links.device(1).map(str::to_string).expect("link 1");
        assert_eq!(watch, "sim-pinetime");
        links.close(&mut ble, 1).expect("close the watch");
        links.open(&mut ble, "sim-omi").expect("the slot came back");

        let held: Vec<_> = links
            .session
            .get()
            .leases
            .iter()
            .map(|lease| lease.device_id.clone())
            .collect();
        assert_eq!(held.len(), 3);
        assert!(!held.contains(&watch), "the closed link's lease is gone");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_second_host_is_refused_with_leased_and_leaves_nothing_behind() {
        let path = temp_session("leased");
        let mut first = host(&path, "host-a");
        let mut first_ble = Ble::sim().expect("sim backend");
        first
            .open(&mut first_ble, "sim-pinetime")
            .expect("first host");

        let mut second = host(&path, "host-b");
        let mut second_ble = Ble::sim().expect("sim backend");
        let error = second
            .open(&mut second_ble, "sim-pinetime")
            .expect_err("a leased device cannot be taken");
        assert!(matches!(
            error.downcast_ref::<Failure>(),
            Some(Failure::Leased { host_id, .. }) if host_id == "host-a"
        ));
        assert!(
            second
                .session
                .get()
                .leases
                .iter()
                .all(|lease| lease.host_id != "host-b"),
            "a refused claim must not be recorded"
        );
        assert!(second.device(1).is_none(), "no link handle was handed out");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn listening_keeps_the_lease_alive() {
        let path = temp_session("heartbeat");
        let mut links = host(&path, "host-a");
        let mut ble = Ble::sim().expect("sim backend");
        let link = links.open(&mut ble, "sim-pinetime").expect("open");
        for _ in 0..3 {
            links.heartbeat(link).expect("heartbeat");
        }
        links.close(&mut ble, link).expect("close");
        assert!(
            links.session.get().leases.is_empty(),
            "closing the only link releases the only lease"
        );
        let _ = std::fs::remove_file(&path);
    }
}
