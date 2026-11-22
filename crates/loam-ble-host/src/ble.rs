//! Backend selector for the BLE host.
//!
//! The `soil:ble` WIT path is identical whether a real radio or the simulator
//! is behind it. `SOIL_BLE_SIM=1` selects the deterministic backend; every
//! caller above this file uses the same methods.

use anyhow::Result;
use soil_ble_api::{Backend, DeviceInfo, ServiceInfo};
pub use soil_ble_api::{BleEvent, Failure, LinkId};

pub struct Ble {
    backend: Box<dyn Backend>,
}

impl Ble {
    pub fn new() -> Result<Self> {
        Ok(Self {
            backend: Box::new(crate::btleplug::BtleplugBackend::new()?),
        })
    }

    pub fn sim() -> Result<Self> {
        Ok(Self {
            backend: Box::new(soil_ble_sim::SimBackend::builtin()?),
        })
    }

    /// Simulated backend with a real-time delay per generated tick. The dev
    /// host uses this so a one-second guest run produces a sane sample count
    /// instead of spinning the virtual clock as fast as the CPU allows.
    pub fn sim_paced(pacing: std::time::Duration) -> Result<Self> {
        Ok(Self {
            backend: Box::new(soil_ble_sim::SimBackend::builtin_paced(pacing)?),
        })
    }

    pub fn from_env() -> Result<Self> {
        if std::env::var_os("SOIL_BLE_SIM").is_some() {
            Self::sim()
        } else {
            Self::new()
        }
    }

    pub fn scan(&mut self, timeout_ms: u64) -> Result<Vec<DeviceInfo>> {
        self.backend.scan(timeout_ms)
    }

    pub fn max_links(&self) -> usize {
        self.backend.max_links()
    }

    pub fn connect(&mut self, id: &str) -> Result<LinkId> {
        self.backend.connect(id)
    }

    pub fn disconnect(&mut self, link: LinkId) -> Result<()> {
        self.backend.disconnect(link)
    }

    pub fn services(&self, link: LinkId) -> Result<Vec<ServiceInfo>> {
        self.backend.services(link)
    }

    pub fn read(&self, link: LinkId, uuid: &str) -> Result<Vec<u8>> {
        self.backend.read(link, uuid)
    }

    pub fn write(&self, link: LinkId, uuid: &str, data: &[u8]) -> Result<()> {
        self.backend.write(link, uuid, data)
    }

    pub fn subscribe(&self, link: LinkId, uuid: &str) -> Result<()> {
        self.backend.subscribe(link, uuid)
    }

    pub fn next_event(&self, link: LinkId, timeout_ms: u64) -> Result<Option<BleEvent>> {
        self.backend.next_event(link, timeout_ms)
    }
}
