//! Soil BLE host for device protocol Spores.
//!
//! Loads one protocol guest and gives it exactly the `soil:ble` capability.
//! The radio transport (btleplug / BlueZ / CoreBluetooth) and the signed lease
//! stay in the host; framing and command semantics stay in the Spore.
//!
//! See `ajmwagar/spores/spores/colmi-ring/README.md` for the COLMI target matrix
//! and lease model, and `crates/soil-ble-sim` for the deterministic backend.

use anyhow::{Context, Result, anyhow, bail};
use soil_ble_api::{Failure, LinkId};
use soil_ble_host::{ble, links, session};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

wasmtime::component::bindgen!({ path: "wit", world: "host" });

use soil::ble::ble::Error as BleError;
use soil::ble::types::{Device, Event, Notification, Service};

const MAX_DEVICES: usize = 64;
const MAX_READ_BYTES: u32 = 512;
const MAX_WRITE_BYTES: usize = 512;
const MAX_EVENT_WAIT_MS: u32 = 1000;

/// One scan result. The platform address stays host-side, the handle is what
/// the guest sees, and a claimed id is what the guest may key a record on.
#[derive(Clone)]
struct Scanned {
    address: String,
    name: Option<String>,
}

struct State {
    ble: ble::Ble,
    links: links::Links,
    /// Scan handles are one-based indexes into this list. They are Soil-issued
    /// and deliberately not the platform peripheral id.
    devices: Vec<Scanned>,
    wasi: wasmtime_wasi::WasiCtx,
    table: wasmtime_wasi::ResourceTable,
}

impl wasmtime_wasi::WasiView for State {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Short advisory lease: a crashed host must lose the radio quickly.
const LEASE_TTL_MS: u64 = 60_000;

fn io_error(e: anyhow::Error) -> BleError {
    eprintln!("ble: {e:#}");
    BleError::Io
}

/// Map a typed failure to the guest's error enum. Anything untyped is `io`:
/// the guest cannot act on it, and the detail is already logged host-side.
fn wit_error(e: anyhow::Error) -> BleError {
    match e.downcast_ref::<Failure>() {
        Some(Failure::NotFound(_)) => BleError::NotFound,
        Some(Failure::NotConnected(_)) => BleError::NotConnected,
        Some(Failure::Invalid(_)) => BleError::Invalid,
        Some(Failure::OverBudget(_)) => BleError::OverBudget,
        Some(Failure::Leased { .. }) => BleError::Leased,
        None => io_error(e),
    }
}

impl soil::ble::ble::Host for State {
    fn scan(&mut self, timeout_ms: u32) -> std::result::Result<Vec<Device>, BleError> {
        let timeout_ms = u64::from(timeout_ms.min(30_000));
        let found = self.ble.scan(timeout_ms).map_err(io_error)?;
        let found: Vec<_> = found.into_iter().take(MAX_DEVICES).collect();
        self.devices.clear();
        let mut out = Vec::with_capacity(found.len());
        for (index, device) in found.into_iter().enumerate() {
            // A claimed peripheral already knows its identity; report it now
            // rather than after a connect, so a guest can select it.
            let device_id = self.links.device_id_for(&device.id);
            self.devices.push(Scanned {
                address: device.id,
                name: device.name.clone(),
            });
            out.push(Device {
                handle: index as u32 + 1,
                name: device.name,
                rssi: device.rssi,
                device_id,
            });
        }
        Ok(out)
    }

    fn max_links(&mut self) -> u32 {
        u32::try_from(self.ble.max_links()).unwrap_or(u32::MAX)
    }

    fn connect(&mut self, device: u32) -> std::result::Result<LinkId, BleError> {
        let index = usize::try_from(device)
            .ok()
            .and_then(|h| h.checked_sub(1))
            .ok_or(BleError::NotFound)?;
        let id = self
            .devices
            .get(index)
            .map(|device| device.address.clone())
            .ok_or(BleError::NotFound)?;

        // One lease per device: a second link must not release the first's claim.
        self.links.open(&mut self.ble, &id).map_err(wit_error)
    }

    fn disconnect(&mut self, link: LinkId) -> std::result::Result<(), BleError> {
        self.links.close(&mut self.ble, link).map_err(wit_error)
    }

    fn claim(&mut self, device: u32, device_id: String) -> std::result::Result<(), BleError> {
        let index = usize::try_from(device)
            .ok()
            .and_then(|h| h.checked_sub(1))
            .ok_or(BleError::NotFound)?;
        let scanned = self.devices.get(index).cloned().ok_or(BleError::NotFound)?;
        self.links
            .claim(&scanned.address, scanned.name.as_deref(), &device_id)
            .map_err(wit_error)
    }

    fn services(&mut self, link: LinkId) -> std::result::Result<Vec<Service>, BleError> {
        let services = self.ble.services(link).map_err(wit_error)?;
        Ok(services
            .into_iter()
            .map(|s| Service {
                uuid: s.uuid,
                characteristics: s
                    .characteristics
                    .into_iter()
                    .map(|c| soil::ble::types::Characteristic {
                        uuid: c.uuid,
                        readable: c.readable,
                        writable: c.writable,
                        notifiable: c.notifiable,
                    })
                    .collect(),
            })
            .collect())
    }

    fn read(
        &mut self,
        link: LinkId,
        char_uuid: String,
        max_bytes: u32,
    ) -> std::result::Result<Vec<u8>, BleError> {
        if char_uuid.is_empty() {
            return Err(BleError::Invalid);
        }
        if max_bytes == 0 || max_bytes > MAX_READ_BYTES {
            return Err(BleError::OverBudget);
        }
        let data = self.ble.read(link, &char_uuid).map_err(wit_error)?;
        Ok(data.into_iter().take(max_bytes as usize).collect())
    }

    fn write(
        &mut self,
        link: LinkId,
        char_uuid: String,
        data: Vec<u8>,
    ) -> std::result::Result<(), BleError> {
        if char_uuid.is_empty() {
            return Err(BleError::Invalid);
        }
        if data.len() > MAX_WRITE_BYTES {
            return Err(BleError::OverBudget);
        }
        self.ble.write(link, &char_uuid, &data).map_err(wit_error)
    }

    fn subscribe(&mut self, link: LinkId, char_uuid: String) -> std::result::Result<(), BleError> {
        if char_uuid.is_empty() {
            return Err(BleError::Invalid);
        }
        self.ble.subscribe(link, &char_uuid).map_err(wit_error)
    }

    fn next_event(
        &mut self,
        link: LinkId,
        timeout_ms: u32,
    ) -> std::result::Result<Option<Event>, BleError> {
        let timeout_ms = u64::from(timeout_ms.min(MAX_EVENT_WAIT_MS));
        // Waiting on a link is also the lease heartbeat: a guest that is only
        // listening must not lose its claim to a 60 s timer.
        self.links.heartbeat(link).map_err(wit_error)?;
        match self.ble.next_event(link, timeout_ms).map_err(wit_error)? {
            Some(ble::BleEvent::Notification { char_uuid, data }) => {
                Ok(Some(Event::Notification(Notification { char_uuid, data })))
            }
            Some(ble::BleEvent::Disconnected { reason }) => Ok(Some(Event::Disconnected(reason))),
            None => Ok(None),
        }
    }
}

fn wall_clock() -> std::result::Result<soil::time::time::DateTime, soil::time::time::Error> {
    #[cfg(unix)]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| soil::time::time::Error::NotReady)?;
        let seconds = libc::time_t::try_from(now.as_secs()).map_err(|_| soil::time::time::Error::NotReady)?;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let resolved = unsafe { libc::localtime_r(&seconds, &mut tm) };
        if resolved.is_null() { return Err(soil::time::time::Error::NotReady); }
        let zone = if tm.tm_zone.is_null() { None } else {
            Some(unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned())
        };
        Ok(soil::time::time::DateTime {
            unix_ms: u64::try_from(now.as_millis()).unwrap_or(u64::MAX),
            offset_minutes: (tm.tm_gmtoff / 60) as i32,
            year: (tm.tm_year + 1900) as u32, month: (tm.tm_mon + 1) as u8,
            day: tm.tm_mday as u8, hour: tm.tm_hour as u8, minute: tm.tm_min as u8,
            second: tm.tm_sec as u8, weekday: ((tm.tm_wday + 6) % 7 + 1) as u8, zone,
        })
    }
    #[cfg(not(unix))]
    { Err(soil::time::time::Error::NotReady) }
}

impl soil::time::time::Host for State {
    fn monotonic_ms(&mut self) -> std::result::Result<u64, soil::time::time::Error> {
        Ok(soil_ble_host::monotonic_ms())
    }

    fn wall_clock(&mut self) -> std::result::Result<soil::time::time::DateTime, soil::time::time::Error> {
        wall_clock()
    }

    fn tick(&mut self, timeout_ms: u32) -> std::result::Result<Option<u64>, soil::time::time::Error> {
        if timeout_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(u64::from(timeout_ms.min(50))));
        }
        Ok(Some(soil_ble_host::monotonic_ms()))
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let guest_path = args
        .next()
        .context("usage: soil-ble-host [--scan [timeout-ms]] <guest.wasm> [guest args...]")?;
    if guest_path == "--scan" {
        let timeout_ms = args
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(3_000);
        let mut ble = ble::Ble::from_env().context("init BLE backend")?;
        let devices = ble.scan(timeout_ms).context("scan BLE")?;
        for device in devices {
            println!(
                "{}\t{}\t{}",
                device.id,
                device
                    .rssi
                    .map_or_else(|| "-".to_string(), |r| r.to_string()),
                device.name.unwrap_or_default()
            );
        }
        return Ok(());
    }
    let guest_args: Vec<String> = args.collect();
    let host_id = std::env::var("SOIL_HOST_ID")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "host".to_string());
    let session_path =
        std::env::var("SOIL_BLE_SESSION").unwrap_or_else(|_| "soil-ble-session.json".to_string());
    if std::env::var_os("SOIL_BLE_SIM").is_some()
        && std::env::var_os("SOIL_BLE_SIM_CONTENDER").is_some()
    {
        // Prove the host-level lease rule with the simulator: a second host
        // claims the first virtual device before this host opens the store.
        let mut contender = session::SessionStore::open(&session_path)
            .with_context(|| format!("open contender session {session_path}"))?;
        contender
            .claim("sim-pinetime", "sim-contender", LEASE_TTL_MS)
            .context("claim contender lease")?;
        eprintln!("sim: sim-contender holds the lease for sim-pinetime");
    }
    let session = session::SessionStore::open(&session_path)
        .with_context(|| format!("open session {session_path}"))?;
    eprintln!("host id {host_id}, session {session_path}");

    let engine = Engine::default();
    let component = Component::from_file(&engine, &guest_path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("load component {guest_path}"))?;

    let mut linker = Linker::<State>::new(&engine);
    // The guest is a wasip2 component (std pulls in wasi:io/cli), so WASI must
    // be present even though the device protocol itself never touches it.
    wasmtime_wasi::p2::add_to_linker_sync::<State>(&mut linker)
        .map_err(anyhow::Error::from)
        .context("link wasi:cli")?;

    soil::time::time::add_to_linker::<State, wasmtime::component::HasSelf<State>>(&mut linker, |state| state)?;
    soil::ble::ble::add_to_linker::<State, wasmtime::component::HasSelf<State>>(
        &mut linker,
        |state| state,
    )
    .map_err(anyhow::Error::from)
    .context("link soil:ble/ble")?;

    let state = State {
        ble: ble::Ble::from_env().context("init BLE backend")?,
        links: links::Links::new(session, host_id, LEASE_TTL_MS),
        devices: Vec::new(),
        wasi: wasmtime_wasi::WasiCtxBuilder::new().inherit_stdio().build(),
        table: wasmtime_wasi::ResourceTable::new(),
    };
    let mut store = Store::new(&engine, state);

    let bindings = Host_::instantiate(&mut store, &component, &linker)
        .map_err(anyhow::Error::from)
        .context("instantiate guest")?;

    let result = bindings.call_run(&mut store, &guest_args);
    // A guest that exits without disconnecting must not leave the next host
    // waiting out a stale claim.
    let state = store.data_mut();
    state.links.close_all(&mut state.ble);

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(msg)) => Err(anyhow!("guest failed: {msg}")),
        Err(e) => bail!("guest trapped: {e:#}"),
    }
}
