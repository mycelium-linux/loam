//! Bluetooth BLE backend via btleplug.
//!
//! The host calls these from synchronous wasm import calls, so everything is
//! driven from one `tokio` runtime owned by the backend rather than the caller.
//!
//! Links are keyed by a Soil-issued handle, and each link owns its notification
//! queue and pump. A guest draining one link therefore never reads another
//! device's traffic, and a device that floods notifications cannot fill a
//! shared queue that a quiet device then has to wait behind.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use soil_ble_api::{Backend, BleEvent, CharInfo, DeviceInfo, Failure, LinkId, ServiceInfo, fail};
use tokio::runtime::Runtime;

/// Adapters do not report their connection ceiling. Four concurrent links is
/// the conservative number that holds across the controllers this runs on; the
/// host refuses the fifth before any handshake starts.
const MAX_LINKS: usize = 4;

/// Per-link queue depth. A guest that stops draining a chatty device blocks
/// that link's pump — radio-side backpressure — instead of growing host memory
/// or dropping notifications silently.
const LINK_QUEUE: usize = 1024;

/// Identity note: on macOS `Peripheral::address()` is hardcoded to all zeros,
/// so `Peripheral::id()` is the only stable handle. Scanned peripherals are
/// cached here so `connect` never depends on a second enumeration.
#[derive(Clone)]
struct Scanned {
    id: String,
    periph: Peripheral,
}

struct Link {
    device_id: String,
    periph: Peripheral,
    chars: HashMap<String, Characteristic>,
    rx: Receiver<BleEvent>,
    pump: Option<tokio::task::JoinHandle<()>>,
}

pub struct BtleplugBackend {
    rt: Runtime,
    adapter: Option<Adapter>,
    scanned: Vec<Scanned>,
    links: HashMap<LinkId, Link>,
    next_link: LinkId,
}

/// Every transport call gets a deadline: btleplug awaits can hang forever on
/// macOS, and a hung host is worse than a failed one.
async fn bound<F: std::future::Future>(secs: u64, fut: F) -> Result<F::Output> {
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(v) => Ok(v),
        Err(_) => Err(anyhow!("BLE operation timed out after {secs}s")),
    }
}

fn lower(u: impl ToString) -> String {
    u.to_string().to_ascii_lowercase()
}

impl BtleplugBackend {
    pub fn new() -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build tokio runtime")?;
        let adapter = rt.block_on(async {
            let manager = Manager::new().await.context("BLE manager")?;
            let adapters = manager.adapters().await.context("list adapters")?;
            adapters
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no BLE adapter"))
        })?;
        Ok(Self {
            rt,
            adapter: Some(adapter),
            scanned: Vec::new(),
            links: HashMap::new(),
            next_link: 1,
        })
    }

    pub fn scan(&mut self, timeout_ms: u64) -> Result<Vec<DeviceInfo>> {
        let adapter = self.adapter.as_ref().ok_or_else(|| anyhow!("no adapter"))?;
        self.rt.block_on(async {
            bound(5, adapter.start_scan(ScanFilter::default()))
                .await?
                .context("start scan")?;
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
            let found = bound(5, adapter.peripherals())
                .await?
                .context("list peripherals")?;
            let mut out = Vec::new();
            let mut cache = Vec::new();
            for p in found {
                let id = p.id().to_string();
                match p.properties().await {
                    Ok(Some(props)) => {
                        if props.local_name.is_some() || props.rssi.is_some() {
                            out.push(DeviceInfo {
                                id: id.clone(),
                                name: props.local_name.clone(),
                                rssi: props.rssi,
                            });
                        }
                        cache.push(Scanned { id, periph: p });
                    }
                    Ok(None) => cache.push(Scanned { id, periph: p }),
                    Err(e) => eprintln!("scan: properties failed: {e}"),
                }
            }
            let _ = adapter.stop_scan().await;
            self.scanned = cache;
            Ok(out)
        })
    }

    pub fn connect(&mut self, id: &str) -> Result<LinkId> {
        if let Some((link, _)) = self
            .links
            .iter()
            .find(|(_, existing)| existing.device_id.eq_ignore_ascii_case(id))
        {
            return Ok(*link);
        }
        if self.links.len() >= MAX_LINKS {
            return fail(Failure::OverBudget(format!(
                "{id}: {} of {MAX_LINKS} links in use",
                self.links.len()
            )));
        }
        let adapter = self.adapter.as_ref().ok_or_else(|| anyhow!("no adapter"))?;
        let (tx, rx): (SyncSender<BleEvent>, Receiver<BleEvent>) = sync_channel(LINK_QUEUE);

        // resolve from the scan cache first (macOS addresses are zeros), then
        // fall back to a fresh enumeration
        let cached = self
            .scanned
            .iter()
            .find(|s| s.id.eq_ignore_ascii_case(id))
            .map(|s| s.periph.clone());
        let cached = match cached {
            Some(p) => Some(p),
            None => {
                let found = self.rt.block_on(async {
                    let r = bound(5, adapter.peripherals()).await?;
                    r.map_err(|e| anyhow!("{e}"))
                })?;
                found
                    .into_iter()
                    .find(|p| p.id().to_string().eq_ignore_ascii_case(id))
            }
        };

        let (periph, chars) = self.rt.block_on(async {
            let periph =
                cached.ok_or_else(|| anyhow!("peripheral id not in scan results: {id}"))?;
            bound(15, periph.connect()).await?.context("connect")?;
            // btleplug returns before the link is usable; CoreBluetooth needs a
            // moment or the first GATT op tears the connection down.
            for _ in 0..40 {
                if periph.is_connected().await.unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            bound(10, periph.discover_services())
                .await?
                .context("discover services")?;
            let chars = periph
                .characteristics()
                .into_iter()
                .map(|c| (lower(c.uuid), c))
                .collect::<HashMap<_, _>>();
            if chars.is_empty() {
                anyhow::bail!("connected but no GATT characteristics were discovered");
            }
            // CoreBluetooth can answer discovery from its cache, so a dead
            // link still lists services. Say which state the next read is in.
            eprintln!(
                "link: {id} connected={} services={} characteristics={}",
                periph.is_connected().await.unwrap_or(false),
                periph.services().len(),
                periph.characteristics().len()
            );
            Ok::<_, anyhow::Error>((periph, chars))
        })?;

        // one pump per link; the guest drains it by link handle
        let pump = self.rt.spawn({
            let periph = periph.clone();
            async move {
                match periph.notifications().await {
                    Ok(mut stream) => {
                        use futures_util::StreamExt;
                        while let Some(n) = stream.next().await {
                            let event = BleEvent::Notification {
                                char_uuid: lower(n.uuid),
                                data: n.value,
                            };
                            // A full queue blocks this link only.
                            if tx.send(event).is_err() {
                                return;
                            }
                        }
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: "notification stream ended".to_string(),
                        });
                    }
                    Err(e) => {
                        eprintln!("notify stream ended: {e}");
                        let _ = tx.send(BleEvent::Disconnected {
                            reason: e.to_string(),
                        });
                    }
                }
            }
        });

        let link = self.next_link;
        self.next_link = self.next_link.wrapping_add(1).max(1);
        self.links.insert(
            link,
            Link {
                device_id: id.to_string(),
                periph,
                chars,
                rx,
                pump: Some(pump),
            },
        );
        Ok(link)
    }

    pub fn disconnect(&mut self, link: LinkId) -> Result<()> {
        let Some(mut entry) = self.links.remove(&link) else {
            return fail(Failure::NotConnected(link));
        };
        if let Some(handle) = entry.pump.take() {
            handle.abort();
        }
        // Dropping `rx` also unblocks a pump that is waiting on a full queue.
        self.rt
            .block_on(async {
                let r = bound(5, entry.periph.disconnect()).await?;
                r.map_err(|e| anyhow!("{e}"))
            })
            .context("disconnect")
    }

    pub fn services(&self, link: LinkId) -> Result<Vec<ServiceInfo>> {
        let link = self.linked(link)?;
        let periph = link.periph.clone();
        self.rt.block_on(async {
            bound(10, periph.discover_services())
                .await?
                .context("discover services")?;
            let mut out = Vec::new();
            for svc in periph.services() {
                let chars = svc
                    .characteristics
                    .iter()
                    .map(|c| CharInfo {
                        uuid: lower(c.uuid),
                        readable: c.properties.contains(btleplug::api::CharPropFlags::READ),
                        writable: c.properties.contains(btleplug::api::CharPropFlags::WRITE)
                            || c.properties
                                .contains(btleplug::api::CharPropFlags::WRITE_WITHOUT_RESPONSE),
                        notifiable: c.properties.contains(btleplug::api::CharPropFlags::NOTIFY),
                    })
                    .collect();
                out.push(ServiceInfo {
                    uuid: lower(svc.uuid),
                    characteristics: chars,
                });
            }
            Ok(out)
        })
    }

    pub fn read(&self, link: LinkId, uuid: &str) -> Result<Vec<u8>> {
        let (ch, periph) = self.char(link, uuid)?;
        self.rt.block_on(async {
            let mut last = None;
            for _ in 0..3 {
                if !periph.is_connected().await.unwrap_or(false) {
                    if let Err(e) = periph.connect().await {
                        last = Some(anyhow!("reconnect failed: {e}"));
                        tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
                        continue;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                }
                match bound(5, periph.read(&ch)).await? {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        last = Some(anyhow!("{e}"));
                        tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
                    }
                }
            }
            Err(last.unwrap_or_else(|| anyhow!("read failed"))).context("read characteristic")
        })
    }

    fn linked(&self, link: LinkId) -> Result<&Link> {
        self.links
            .get(&link)
            .ok_or_else(|| anyhow::Error::new(Failure::NotConnected(link)))
    }

    fn char(&self, link: LinkId, uuid: &str) -> Result<(Characteristic, Peripheral)> {
        let link = self.linked(link)?;
        let ch = link.chars.get(uuid).cloned().ok_or_else(|| {
            anyhow::Error::new(Failure::NotFound(format!(
                "characteristic {uuid} on link {}",
                link.device_id
            )))
        })?;
        Ok((ch, link.periph.clone()))
    }

    pub fn write(&self, link: LinkId, uuid: &str, data: &[u8]) -> Result<()> {
        let (ch, periph) = self.char(link, uuid)?;
        self.rt.block_on(async {
            let mut last = None;
            for _ in 0..3 {
                if !periph.is_connected().await.unwrap_or(false) {
                    if let Err(e) = periph.connect().await {
                        last = Some(anyhow!("reconnect failed: {e}"));
                        tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
                        continue;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                }
                match bound(5, periph.write(&ch, data, WriteType::WithoutResponse)).await? {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        last = Some(anyhow!("{e}"));
                        tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
                    }
                }
            }
            Err(last.unwrap_or_else(|| anyhow!("write failed"))).context("write characteristic")
        })
    }

    pub fn subscribe(&self, link: LinkId, uuid: &str) -> Result<()> {
        let (ch, periph) = self.char(link, uuid)?;
        self.rt
            .block_on(async move {
                let r = bound(10, periph.subscribe(&ch)).await?;
                r.map_err(|e| anyhow!("{e}"))
            })
            .context("subscribe")
    }

    /// `Ok(None)` = deadline expired, not a failure.
    pub fn next_event(&self, link: LinkId, timeout_ms: u64) -> Result<Option<BleEvent>> {
        let entry = self.linked(link)?;
        match entry.rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(event) => Ok(Some(event)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                fail(Failure::NotConnected(link))
            }
        }
    }
}

impl Backend for BtleplugBackend {
    fn scan(&mut self, timeout_ms: u64) -> Result<Vec<DeviceInfo>> {
        BtleplugBackend::scan(self, timeout_ms)
    }

    fn max_links(&self) -> usize {
        MAX_LINKS
    }

    fn connect(&mut self, id: &str) -> Result<LinkId> {
        BtleplugBackend::connect(self, id)
    }

    fn disconnect(&mut self, link: LinkId) -> Result<()> {
        BtleplugBackend::disconnect(self, link)
    }

    fn services(&self, link: LinkId) -> Result<Vec<ServiceInfo>> {
        BtleplugBackend::services(self, link)
    }

    fn read(&self, link: LinkId, uuid: &str) -> Result<Vec<u8>> {
        BtleplugBackend::read(self, link, uuid)
    }

    fn write(&self, link: LinkId, uuid: &str, data: &[u8]) -> Result<()> {
        BtleplugBackend::write(self, link, uuid, data)
    }

    fn subscribe(&self, link: LinkId, uuid: &str) -> Result<()> {
        BtleplugBackend::subscribe(self, link, uuid)
    }

    fn next_event(&self, link: LinkId, timeout_ms: u64) -> Result<Option<BleEvent>> {
        BtleplugBackend::next_event(self, link, timeout_ms)
    }
}
