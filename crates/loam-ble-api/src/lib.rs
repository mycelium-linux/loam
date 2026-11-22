//! Transport-agnostic BLE backend API.
//!
//! The wasmtime host and the simulator implement the same trait, so the
//! `soil:ble` WIT path is identical whether a radio is present or not. The
//! trait is deliberately small: scan, link lifecycle, GATT read/write,
//! notifications, and a bounded event wait. Everything above it is the host.
//!
//! Every link-scoped call names its link, and every link owns its own event
//! queue. One device's traffic therefore cannot delay another's.

use std::fmt;

use anyhow::Result;

/// Soil-issued handle for one live link. Never an address; never reused.
pub type LinkId = u32;

/// The failures a guest can act on, carried through `anyhow` so a host can
/// recover the distinction without matching on message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No live link for this handle: disconnected, or never issued.
    NotConnected(LinkId),
    /// Unknown device, handle, or characteristic.
    NotFound(String),
    /// A malformed argument, e.g. an empty UUID.
    Invalid(String),
    /// The host's link or byte budget is spent.
    OverBudget(String),
    /// Another Soil host holds this device's radio lease.
    Leased {
        device_id: String,
        host_id: String,
        remaining_ms: u64,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::NotConnected(link) => write!(f, "link {link} is not connected"),
            Failure::NotFound(what) => write!(f, "{what} not found"),
            Failure::Invalid(why) => write!(f, "invalid argument: {why}"),
            Failure::OverBudget(why) => write!(f, "over budget: {why}"),
            Failure::Leased {
                device_id,
                host_id,
                remaining_ms,
            } => write!(
                f,
                "device {device_id} is leased by {host_id} for another {remaining_ms} ms"
            ),
        }
    }
}

impl std::error::Error for Failure {}

/// Build the `anyhow` error a backend returns for an actionable failure.
pub fn fail<T>(failure: Failure) -> Result<T> {
    Err(anyhow::Error::new(failure))
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Platform peripheral id. On macOS this is a CoreBluetooth UUID, not a MAC.
    pub id: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
}

/// One event drained from the notification pump. The characteristic UUID is
/// preserved so the guest can distinguish the command channel from DFU.
#[derive(Debug, Clone)]
pub enum BleEvent {
    Notification { char_uuid: String, data: Vec<u8> },
    Disconnected { reason: String },
}

#[derive(Debug, Clone)]
pub struct CharInfo {
    pub uuid: String,
    pub readable: bool,
    pub writable: bool,
    pub notifiable: bool,
}

#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub uuid: String,
    pub characteristics: Vec<CharInfo>,
}

/// A BLE transport. Implementations own their runtime or virtual clock; the
/// host calls these methods synchronously from wasm imports. `Send` is
/// required because the wasmtime store owns the backend.
pub trait Backend: Send {
    fn scan(&mut self, timeout_ms: u64) -> Result<Vec<DeviceInfo>>;

    /// Concurrent links this transport will carry. Adapters do not report it,
    /// so each backend states its own ceiling.
    fn max_links(&self) -> usize;

    fn connect(&mut self, id: &str) -> Result<LinkId>;
    fn disconnect(&mut self, link: LinkId) -> Result<()>;
    fn services(&self, link: LinkId) -> Result<Vec<ServiceInfo>>;
    fn read(&self, link: LinkId, uuid: &str) -> Result<Vec<u8>>;
    fn write(&self, link: LinkId, uuid: &str, data: &[u8]) -> Result<()>;
    fn subscribe(&self, link: LinkId, uuid: &str) -> Result<()>;

    /// One event from that link's queue. `Ok(None)` means its deadline expired.
    fn next_event(&self, link: LinkId, timeout_ms: u64) -> Result<Option<BleEvent>>;
}
