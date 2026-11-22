//! Desktop development host: BLE + Records + Unibus + HTTP + monotonic time.
//!
//! This is deliberately a composition, not a new capability. The BLE backend
//! and lease are the same ones `soil-ble-host` uses; Records, Unibus and HTTP are
//! the canonical host adapters. It exists so a device or CLI Spore can run end to
//! end against the simulator (or a real radio with `--real`) on a laptop.
//!
//! `soil:http` is linked with an empty allowlist unless `--allow-origin` is
//! passed: a guest that needs the network says which origin, and the host
//! refuses everything else rather than falling back to a default.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ::soil::http_host::{self, HttpHost, HttpPolicy};
use ::soil::records_host::{self, Namespace, NamespacedRecordsHost, records};
use ::soil::{
    MemoryBus, WasiHost, WasiHostBuilder, WasiHostPolicy,
    unibus::{Bus, UnibusBus, bus},
};
use anyhow::{Context, Result, bail};
use soil_ble_host::ble as ble_host;
use soil_ble_host::links;
use soil_ble_host::session;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit",
    world: "runner",
    // The notify host already implements these interfaces; map them to its
    // types so this host forwards instead of re-deriving the tray.
    with: {
        "soil:notify/types": soil_notify_host::api::types,
        "soil:notify/publisher": soil_notify_host::api::publisher,
        "soil:notify/focus": soil_notify_host::api::focus,
    },
});

use crate::soil::ble::ble::Error as BleError;
use crate::soil::ble::types::{Characteristic, Device, Event, Notification, Service};

const MAX_DEVICES: usize = 64;
const MAX_READ_BYTES: u32 = 512;
const MAX_WRITE_BYTES: usize = 512;
const MAX_EVENT_WAIT_MS: u32 = 1_000;
const LEASE_TTL_MS: u64 = 60_000;
const MAX_HTTP_BYTES: usize = 16 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// One scan result. The platform address stays host-side, the handle is what
/// the guest sees, and a claimed id is what the guest may key a record on.
#[derive(Clone)]
struct Scanned {
    address: String,
    name: Option<String>,
}

struct DevHost {
    ble: ble_host::Ble,
    /// Optional platform-id filter, so a bench host cannot accidentally write
    /// to the second watch in radio range.
    ble_device: Option<String>,
    links: links::Links,
    /// Tray broker for Spores that publish notifications. The host owns focus
    /// and holding policy; a Spore only classifies and publishes.
    notify: soil_notify_host::NotifyComponent,
    /// One-based handles into the last scan result.
    devices: Vec<Scanned>,
    records: NamespacedRecordsHost,
    http: HttpHost,
    wasi: WasiHost,
    start: Instant,
    /// Dev-only stable epoch. Records written by consecutive dev-host runs must
    /// be comparable; a bare `Instant` resets every process.
    time_origin_ms: u64,
}

impl WasiView for DevHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.wasi.ctx()
    }
}

impl records::Host for DevHost {
    fn get(&mut self, key: records::Key) -> std::result::Result<records::Entry, records::Error> {
        records::Host::get(&mut self.records, key)
    }

    fn scan(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<records::Page, records::Error> {
        records::Host::scan(&mut self.records, collection, cursor, limit)
    }

    fn query(
        &mut self,
        spec: records::QuerySpec,
    ) -> std::result::Result<records::Page, records::Error> {
        records::Host::query(&mut self.records, spec)
    }

    fn aggregate(
        &mut self,
        collection: String,
        spec: records::AggregateSpec,
    ) -> std::result::Result<Vec<records::AggregateGroup>, records::Error> {
        records::Host::aggregate(&mut self.records, collection, spec)
    }

    fn scan_packed(
        &mut self,
        collection: String,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<records::PackedPage, records::Error> {
        records::Host::scan_packed(&mut self.records, collection, cursor, limit)
    }

    fn upsert(
        &mut self,
        entry: records::Entry,
    ) -> std::result::Result<records::Entry, records::Error> {
        records::Host::upsert(&mut self.records, entry)
    }

    fn compare_and_swap(
        &mut self,
        key: records::Key,
        expected: Option<records::Version>,
        entry: records::Entry,
    ) -> std::result::Result<records::CasResult, records::Error> {
        records::Host::compare_and_swap(&mut self.records, key, expected, entry)
    }

    fn delete(&mut self, key: records::Key) -> std::result::Result<(), records::Error> {
        records::Host::delete(&mut self.records, key)
    }

    fn changes(
        &mut self,
        since: Option<String>,
        limit: u32,
    ) -> std::result::Result<records::Page, records::Error> {
        records::Host::changes(&mut self.records, since, limit)
    }

    fn apply(
        &mut self,
        entries: Vec<records::Entry>,
    ) -> std::result::Result<records::ApplyResult, records::Error> {
        records::Host::apply(&mut self.records, entries)
    }
}

/// `soil:http` is `soil-host`'s own implementation, unchanged. The dev host
/// chooses the allowlist; the policy and the transport stay in one place.
impl http_host::api::Host for DevHost {
    fn fetch(
        &mut self,
        request: http_host::api::Request,
    ) -> std::result::Result<http_host::api::Response, http_host::api::Error> {
        <HttpHost as http_host::api::Host>::fetch(&mut self.http, request)
    }
}

impl bus::Host for DevHost {
    fn poll(
        &mut self,
        kinds: Vec<String>,
        cursor: Option<String>,
        limit: u32,
    ) -> std::result::Result<bus::Page, bus::Error> {
        <WasiHost as bus::Host>::poll(&mut self.wasi, kinds, cursor, limit)
    }

    fn publish(
        &mut self,
        kind: String,
        body: Vec<u8>,
        destination: Option<String>,
        ttl_ms: u64,
    ) -> std::result::Result<bus::Receipt, bus::Error> {
        <WasiHost as bus::Host>::publish(&mut self.wasi, kind, body, destination, ttl_ms)
    }
}

impl soil::time::time::Host for DevHost {
    fn monotonic_ms(&mut self) -> std::result::Result<u64, soil::time::time::Error> {
        Ok(self.now_ms())
    }

    fn tick(
        &mut self,
        timeout_ms: u32,
    ) -> std::result::Result<Option<u64>, soil::time::time::Error> {
        let wait = timeout_ms.min(1_000);
        if wait > 0 {
            std::thread::sleep(Duration::from_millis(u64::from(wait)));
        }
        Ok(Some(self.now_ms()))
    }
}

impl DevHost {
    fn now_ms(&self) -> u64 {
        self.time_origin_ms
            .saturating_add(u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX))
    }
}

impl soil_notify_host::api::publisher::Host for DevHost {
    fn publish(
        &mut self,
        notification: soil_notify_host::api::publisher::Notification,
    ) -> Result<String, soil_notify_host::api::types::Error> {
        <soil_notify_host::NotifyComponent as soil_notify_host::api::publisher::Host>::publish(
            &mut self.notify,
            notification,
        )
    }

    fn update(
        &mut self,
        id: String,
        notification: soil_notify_host::api::publisher::Notification,
    ) -> Result<(), soil_notify_host::api::types::Error> {
        <soil_notify_host::NotifyComponent as soil_notify_host::api::publisher::Host>::update(
            &mut self.notify,
            id,
            notification,
        )
    }

    fn withdraw(&mut self, id: String) -> Result<(), soil_notify_host::api::types::Error> {
        <soil_notify_host::NotifyComponent as soil_notify_host::api::publisher::Host>::withdraw(
            &mut self.notify,
            id,
        )
    }
}

impl soil_notify_host::api::focus::Host for DevHost {
    fn get(
        &mut self,
    ) -> Result<Option<soil_notify_host::api::focus::Profile>, soil_notify_host::api::types::Error>
    {
        <soil_notify_host::NotifyComponent as soil_notify_host::api::focus::Host>::get(&mut self.notify)
    }

    fn set(
        &mut self,
        profile: soil_notify_host::api::focus::Profile,
    ) -> Result<(), soil_notify_host::api::types::Error> {
        <soil_notify_host::NotifyComponent as soil_notify_host::api::focus::Host>::set(
            &mut self.notify,
            profile,
        )
    }

    fn clear(&mut self) -> Result<(), soil_notify_host::api::types::Error> {
        <soil_notify_host::NotifyComponent as soil_notify_host::api::focus::Host>::clear(
            &mut self.notify,
        )
    }
}

impl crate::soil::ble::ble::Host for DevHost {
    fn scan(&mut self, timeout_ms: u32) -> std::result::Result<Vec<Device>, BleError> {
        let timeout_ms = u64::from(timeout_ms.min(30_000));
        let found = self.ble.scan(timeout_ms).map_err(io_error)?;
        let found: Vec<_> = found
            .into_iter()
            .filter(|device| {
                self.ble_device
                    .as_ref()
                    .is_none_or(|wanted| device.id.eq_ignore_ascii_case(wanted))
            })
            .take(MAX_DEVICES)
            .collect();
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

    fn connect(&mut self, device: u32) -> std::result::Result<ble_host::LinkId, BleError> {
        let index = usize::try_from(device)
            .ok()
            .and_then(|handle| handle.checked_sub(1))
            .ok_or(BleError::NotFound)?;
        let id = self
            .devices
            .get(index)
            .map(|device| device.address.clone())
            .ok_or(BleError::NotFound)?;
        // The budget is checked and one device lease is taken before the
        // handshake starts; opening a second link never releases the first.
        self.links.open(&mut self.ble, &id).map_err(wit_error)
    }

    fn disconnect(&mut self, link: ble_host::LinkId) -> std::result::Result<(), BleError> {
        self.links.close(&mut self.ble, link).map_err(wit_error)
    }

    fn claim(&mut self, device: u32, device_id: String) -> std::result::Result<(), BleError> {
        let index = usize::try_from(device)
            .ok()
            .and_then(|handle| handle.checked_sub(1))
            .ok_or(BleError::NotFound)?;
        let scanned = self.devices.get(index).cloned().ok_or(BleError::NotFound)?;
        self.links
            .claim(&scanned.address, scanned.name.as_deref(), &device_id)
            .map_err(wit_error)
    }

    fn services(&mut self, link: ble_host::LinkId) -> std::result::Result<Vec<Service>, BleError> {
        let services = self.ble.services(link).map_err(wit_error)?;
        Ok(services
            .into_iter()
            .map(|service| Service {
                uuid: service.uuid,
                characteristics: service
                    .characteristics
                    .into_iter()
                    .map(|characteristic| Characteristic {
                        uuid: characteristic.uuid,
                        readable: characteristic.readable,
                        writable: characteristic.writable,
                        notifiable: characteristic.notifiable,
                    })
                    .collect(),
            })
            .collect())
    }

    fn read(
        &mut self,
        link: ble_host::LinkId,
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
        link: ble_host::LinkId,
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

    fn subscribe(
        &mut self,
        link: ble_host::LinkId,
        char_uuid: String,
    ) -> std::result::Result<(), BleError> {
        if char_uuid.is_empty() {
            return Err(BleError::Invalid);
        }
        self.ble.subscribe(link, &char_uuid).map_err(wit_error)
    }

    fn next_event(
        &mut self,
        link: ble_host::LinkId,
        timeout_ms: u32,
    ) -> std::result::Result<Option<Event>, BleError> {
        let timeout_ms = u64::from(timeout_ms.min(MAX_EVENT_WAIT_MS));
        // Listening is this host's lease heartbeat for the link.
        self.links.heartbeat(link).map_err(wit_error)?;
        match self.ble.next_event(link, timeout_ms).map_err(wit_error)? {
            Some(ble_host::BleEvent::Notification { char_uuid, data }) => {
                Ok(Some(Event::Notification(Notification { char_uuid, data })))
            }
            Some(ble_host::BleEvent::Disconnected { reason }) => {
                Ok(Some(Event::Disconnected(reason)))
            }
            None => Ok(None),
        }
    }
}

/// Persist the dev host's epoch in the records directory. Each run starts at
/// the later of the stored value and the current wall time, so records from
/// consecutive runs share one forward-moving timeline. This is a development
/// concession, not the production `soil:time` contract.
fn load_or_create_origin(path: &Path) -> Result<u64> {
    let persisted = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let value = persisted.max(wall);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, value.to_string()).with_context(|| format!("write {}", path.display()))?;
    Ok(value)
}

/// Wearer names become book paths, so they are validated here rather than
/// trusted from a command line or an environment variable.
fn parse_wearers(raw: &str) -> Result<Vec<String>> {
    let mut wearers: Vec<String> = Vec::new();
    for token in raw
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        if !token
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(anyhow::anyhow!(
                "--wearers: {token:?} must be lowercase letters, digits, '-' or '_'"
            ));
        }
        if !wearers.iter().any(|wearer| wearer == token) {
            wearers.push(token.to_string());
        }
    }
    if wearers.is_empty() {
        return Err(anyhow::anyhow!(
            "--wearers: at least one wearer is required"
        ));
    }
    Ok(wearers)
}

fn io_error(error: anyhow::Error) -> BleError {
    eprintln!("ble: {error:#}");
    BleError::Io
}

/// Map a typed failure to the guest's error enum; anything untyped is `io`.
fn wit_error(error: anyhow::Error) -> BleError {
    match error.downcast_ref::<ble_host::Failure>() {
        Some(ble_host::Failure::NotFound(_)) => BleError::NotFound,
        Some(ble_host::Failure::NotConnected(_)) => BleError::NotConnected,
        Some(ble_host::Failure::Invalid(_)) => BleError::Invalid,
        Some(ble_host::Failure::OverBudget(_)) => BleError::OverBudget,
        Some(ble_host::Failure::Leased { .. }) => BleError::Leased,
        None => io_error(error),
    }
}

/// Preload the in-memory bus from JSON Lines. This is how a device Spore is
/// exercised without a live Unibus: one frame per line, `{kind, body}`.
fn load_bus_fixture(bus: &MemoryBus, path: &str) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read bus fixture {path}"))?;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("bus fixture {path}:{}: JSON", index + 1))?;
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("bus fixture {path}:{}: kind", index + 1))?;
        let body = match value.get("body") {
            Some(serde_json::Value::String(text)) => text.clone().into_bytes(),
            Some(body) => serde_json::to_vec(body)
                .with_context(|| format!("bus fixture {path}:{}: body JSON", index + 1))?,
            None => bail!("bus fixture {path}:{}: body", index + 1),
        };
        bus.publish(kind, &body, None, 60_000)
            .map_err(|error| anyhow::anyhow!("bus fixture {path}:{}: {error}", index + 1))?;
    }
    Ok(())
}

pub fn run(args: Vec<String>) -> Result<()> {
    let mut real = false;
    let mut records_dir =
        std::env::var("SOIL_RECORDS_ROOT").unwrap_or_else(|_| ".soil-dev/records".to_string());
    let mut session_path = std::env::var("SOIL_BLE_SESSION")
        .unwrap_or_else(|_| ".soil-dev/soil-ble-session.json".to_string());
    let mut host_id = std::env::var("SOIL_HOST_ID")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "dev-host".to_string());
    // Every wearer gets their own records book. A Spore selects one by naming
    // a `<collection>:<wearer>` namespace; a wearer not listed here is denied.
    let mut wearers_raw = std::env::var("SOIL_WEARERS").unwrap_or_else(|_| "owner".to_string());
    let mut focus_mode: Option<String> = None;
    let mut bus_fixture = None;
    let mut ble_device = std::env::var("SOIL_BLE_DEVICE").ok();

    let mut origins: Vec<String> = Vec::new();
    let mut unibus_config: Option<String> = None;
    let mut args = args.into_iter();
    let mut guest_path = None;
    let mut guest_args = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--real" => real = true,
            "--allow-origin" => origins.push(args.next().context("--allow-origin needs a URL")?),
            "--records-dir" => records_dir = args.next().context("--records-dir needs a path")?,
            "--session" => session_path = args.next().context("--session needs a path")?,
            "--host-id" => host_id = args.next().context("--host-id needs a value")?,
            "--wearers" => wearers_raw = args.next().context("--wearers needs a list")?,
            "--focus" => focus_mode = Some(args.next().context("--focus needs a mode")?),
            "--bus-fixture" => {
                bus_fixture = Some(args.next().context("--bus-fixture needs a path")?)
            }
            "--unibus-config" => {
                unibus_config = Some(args.next().context("--unibus-config needs a path")?)
            }
            "--ble-device" => {
                ble_device = Some(args.next().context("--ble-device needs a platform id")?)
            }
            "--help" | "-h" => {
                println!(
                    "usage: soil host [--real] [--allow-origin URL]... [--records-dir DIR] [--session FILE] [--host-id ID] [--wearers LIST] [--focus MODE] [--bus-fixture FILE] [--ble-device ID] <guest.wasm> [guest args...]"
                );
                return Ok(());
            }
            _ if guest_path.is_none() => guest_path = Some(arg),
            _ => {
                guest_args.push(arg);
                guest_args.extend(args);
                break;
            }
        }
    }
    let guest_path = guest_path.context(
        "usage: soil host [--real] [--allow-origin URL]... [--records-dir DIR] [--session FILE] [--host-id ID] [--wearers LIST] [--focus MODE] [--bus-fixture FILE] [--ble-device ID] <guest.wasm> [guest args...]",
    )?;

    let ble = if real {
        ble_host::Ble::new().context("init real BLE backend")?
    } else {
        ble_host::Ble::sim_paced(Duration::from_millis(50)).context("init simulated BLE backend")?
    };
    let session = session::SessionStore::open(&session_path)
        .with_context(|| format!("open session {session_path}"))?;
    let wearers = parse_wearers(&wearers_raw)?;
    let mut namespaces = Vec::new();
    for wearer in &wearers {
        // One book per wearer, so two people's rings cannot land in one store
        // even if a Spore asks for the wrong namespace: the grant simply does
        // not exist and the write is denied.
        let book = format!("personal/{wearer}/telemetry");
        // The Omi Spore keys its device record by pendant id and uses the
        // unnamespaced collection; expose that on the owner book only.
        if wearer == &wearers[0] {
            namespaces.push(Namespace::write("device.state", &book, "device.state"));
            namespaces.push(Namespace::write("capture", &book, "capture"));
        }
        for collection in [
            "signal",
            "device.state",
            "vitals.summary",
            "vitals.latest",
            "vitals.view",
            // Inbox triage: raw messages in, one decision per message out.
            "inbox.raw",
            "inbox",
            "brief",
            // The watch bridge's active decision and dedupe set.
            "bridge",
        ] {
            namespaces.push(Namespace::write(
                format!("{collection}:{wearer}"),
                &book,
                collection,
            ));
        }
    }
    // Weather is where the wearer is, not who they are.
    namespaces.push(Namespace::write(
        "weather.current",
        "dev/personal/weather",
        "weather.current",
    ));
    eprintln!("records: wearers {}", wearers.join(", "));
    let records = NamespacedRecordsHost::open(&records_dir, namespaces)
        .with_context(|| format!("open records {records_dir}"))?;
    let time_origin_ms = load_or_create_origin(&Path::new(&records_dir).join("time-origin-ms"))?;
    let policy = HttpPolicy::new(origins, MAX_HTTP_BYTES, HTTP_TIMEOUT).context("http policy")?;
    let allowed: Vec<&str> = policy.origins().collect();
    if allowed.is_empty() {
        eprintln!("http: no origins allowed; pass --allow-origin URL to grant one");
    } else {
        eprintln!("http: allowing {}", allowed.join(", "));
    }
    let bus: Arc<dyn Bus> = if let Some(path) = &unibus_config {
        Arc::new(UnibusBus::from_config_file(Path::new(path))?)
    } else {
        let memory = Arc::new(MemoryBus::default());
        if let Some(path) = &bus_fixture {
            load_bus_fixture(&memory, path)?;
        }
        memory
    };
    let wasi = WasiHostBuilder::new(WasiHostPolicy::default())
        .inherit_stdio(true)
        .bus(bus.clone())
        .build()
        .context("build WASI state")?;

    let notify = soil_notify_host::NotifyComponent::new(
        soil_notify_host::NotifyBroker::in_memory("dev-host")
            .map_err(|error| anyhow::anyhow!("notify broker: {error:?}"))?,
        "dev.host",
    );
    if let Some(mode) = focus_mode {
        notify
            .broker()
            .set_focus(soil_notify_host::FocusProfile {
                mode: mode.clone(),
                until_ms: None,
                allow_apps: Vec::new(),
                allow_categories: Vec::new(),
                allow_threads: Vec::new(),
                breakthrough: Vec::new(),
                rollup_held: true,
            })
            .map_err(|error| anyhow::anyhow!("focus: {error:?}"))?;
        eprintln!("notify: focus {mode}, held items roll up");
    }
    let state = DevHost {
        ble,
        ble_device,
        links: links::Links::new(session, host_id, LEASE_TTL_MS),
        notify,
        devices: Vec::new(),
        records,
        http: HttpHost::new(policy),
        wasi,
        start: Instant::now(),
        time_origin_ms,
    };

    let engine = Engine::default();
    let component = Component::from_file(&engine, &guest_path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("load component {guest_path}"))?;
    let mut linker = Linker::<DevHost>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(anyhow::Error::from)
        .context("link wasi:cli")?;
    crate::soil::ble::ble::add_to_linker::<DevHost, HasSelf<DevHost>>(&mut linker, |state| state)
        .map_err(anyhow::Error::from)
        .context("link soil:ble")?;
    soil::time::time::add_to_linker::<DevHost, HasSelf<DevHost>>(&mut linker, |state| state)
        .map_err(anyhow::Error::from)
        .context("link soil:time")?;
    records_host::add_to_linker::<DevHost>(&mut linker).context("link soil:records")?;
    soil_notify_host::api::publisher::add_to_linker::<DevHost, HasSelf<DevHost>>(
        &mut linker,
        |state| state,
    )
    .map_err(anyhow::Error::from)
    .context("link soil:notify/publisher")?;
    soil_notify_host::api::focus::add_to_linker::<DevHost, HasSelf<DevHost>>(&mut linker, |state| {
        state
    })
    .map_err(anyhow::Error::from)
    .context("link soil:notify/focus")?;
    http_host::api::add_to_linker::<DevHost, HasSelf<DevHost>>(&mut linker, |state| state)
        .map_err(anyhow::Error::from)
        .context("link soil:http")?;
    ::soil::unibus::add_to_linker::<DevHost>(&mut linker).context("link soil:unibus")?;

    let mut store = Store::new(&engine, state);
    let bindings = Runner::instantiate(&mut store, &component, &linker)
        .map_err(anyhow::Error::from)
        .context("instantiate guest")?;
    let run_result = bindings.call_run(&mut store, &guest_args);
    // Release every link and lease before snapshotting or exiting.
    let state = store.data_mut();
    state.links.close_all(&mut state.ble);
    match run_result {
        Ok(Ok(())) => {}
        Ok(Err(message)) => bail!("guest failed: {message}"),
        Err(error) => bail!("guest trapped: {error:#}"),
    }
    match bindings.call_snapshot(&mut store) {
        Ok(Ok(bytes)) => println!("{}", String::from_utf8_lossy(&bytes)),
        Ok(Err(message)) => eprintln!("snapshot failed: {message}"),
        Err(error) => eprintln!("snapshot trapped: {error:#}"),
    }
    let filter = soil_notify_host::Filter {
        include_held: true,
        include_read: true,
        limit: 50,
        ..Default::default()
    };
    match store.data().notify.broker().inbox(&filter) {
        Ok(items) if !items.is_empty() => {
            for item in items {
                println!(
                    "notify [{:?}] {}: {} — {}{}",
                    item.envelope.priority,
                    item.envelope.category,
                    item.envelope.title,
                    item.envelope.body,
                    if item.envelope.sensitive { " (sensitive)" } else { "" }
                );
            }
        }
        Ok(_) => {}
        Err(error) => eprintln!("notify: {error:?}"),
    }
    Ok(())
}
