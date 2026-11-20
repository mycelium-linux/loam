//! Reusable BLE host pieces for the protocol host and the desktop dev host.
//!
//! The `soil:ble` trait implementation itself stays in the binaries because
//! each host composes a different state. The backend selector and session
//! store are shared so real and simulated links use one path.

pub mod ble;
mod btleplug;
pub mod links;
pub mod session;

/// Monotonic milliseconds for the soil:time host link.
pub fn monotonic_ms() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(std::time::Instant::now);
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
}

