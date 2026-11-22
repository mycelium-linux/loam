// SPDX-License-Identifier: MIT OR Apache-2.0
//! Transport-agnostic LoRa backend API.
//!
//! The host adapter owns the serial/SPI device, exclusive lease, timing and
//! duty-cycle policy. This trait is the seam the same `soil:lora` WIT path uses
//! for a real radio, a simulator, or a remote modem.

use std::collections::VecDeque;
use std::fmt;

pub type DeviceHandle = u32;
pub type Session = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Denied,
    NotFound(String),
    Leased,
    Invalid(String),
    OverBudget(String),
    Timeout,
    Unavailable,
    Io(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Denied => write!(f, "denied"),
            Failure::NotFound(what) => write!(f, "not found: {what}"),
            Failure::Leased => write!(f, "radio is leased"),
            Failure::Invalid(why) => write!(f, "invalid: {why}"),
            Failure::OverBudget(why) => write!(f, "over budget: {why}"),
            Failure::Timeout => write!(f, "timeout"),
            Failure::Unavailable => write!(f, "unavailable"),
            Failure::Io(why) => write!(f, "io: {why}"),
        }
    }
}

impl std::error::Error for Failure {}

#[derive(Debug, Clone, PartialEq)]
pub struct Capability {
    pub handle: DeviceHandle,
    pub name: String,
    pub min_hz: u64,
    pub max_hz: u64,
    pub max_tx_power_dbm: i8,
    pub max_packet_bytes: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub frequency_hz: u64,
    pub bandwidth_hz: u32,
    pub spreading_factor: u8,
    pub coding_rate: u8,
    pub preamble_symbols: u16,
    pub sync_word: u16,
    pub tx_power_dbm: i8,
    pub crc: bool,
    pub invert_iq: bool,
}

impl Config {
    pub fn validate(&self, capability: &Capability) -> Result<(), Failure> {
        if self.frequency_hz < capability.min_hz || self.frequency_hz > capability.max_hz {
            return Err(Failure::Invalid("frequency outside radio range".into()));
        }
        if self.spreading_factor < 6 || self.spreading_factor > 12 {
            return Err(Failure::Invalid("spreading factor must be 6..=12".into()));
        }
        if self.bandwidth_hz == 0 {
            return Err(Failure::Invalid("bandwidth must be non-zero".into()));
        }
        if self.tx_power_dbm > capability.max_tx_power_dbm {
            return Err(Failure::Invalid("tx power above radio maximum".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub payload: Vec<u8>,
    pub rssi_dbm: Option<i16>,
    pub snr_db: Option<f32>,
    pub time_on_air_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub bytes: u32,
    pub time_on_air_ms: u32,
}

pub trait Backend: Send {
    fn scan(&mut self, timeout_ms: u64) -> Result<Vec<Capability>, Failure>;
    fn open(&mut self, device: DeviceHandle) -> Result<Session, Failure>;
    fn configure(&mut self, session: Session, config: Config) -> Result<Config, Failure>;
    fn transmit(
        &mut self,
        session: Session,
        payload: &[u8],
        timeout_ms: u64,
    ) -> Result<Receipt, Failure>;
    fn receive(&mut self, session: Session, timeout_ms: u64) -> Result<Option<Packet>, Failure>;
    fn cad(&mut self, session: Session, timeout_ms: u64) -> Result<bool, Failure>;
    fn state(&mut self, session: Session) -> Result<Config, Failure>;
    fn close(&mut self, session: Session) -> Result<(), Failure>;
}

/// An in-memory backend for tests and for a Spore that has not been given a
/// radio yet. TX is looped back into the RX queue.
#[derive(Debug, Clone)]
pub struct Loopback {
    capability: Capability,
    session: Option<Session>,
    config: Option<Config>,
    rx: VecDeque<Packet>,
}

impl Loopback {
    pub fn new() -> Self {
        Self {
            capability: Capability {
                handle: 1,
                name: "loopback-lora".into(),
                min_hz: 902_000_000,
                max_hz: 928_000_000,
                max_tx_power_dbm: 20,
                max_packet_bytes: 255,
            },
            session: None,
            config: None,
            rx: VecDeque::new(),
        }
    }

    fn check(&self, session: Session) -> Result<(), Failure> {
        if self.session != Some(session) {
            return Err(Failure::NotFound(format!("session {session}")));
        }
        Ok(())
    }
}

impl Backend for Loopback {
    fn scan(&mut self, _timeout_ms: u64) -> Result<Vec<Capability>, Failure> {
        Ok(vec![self.capability.clone()])
    }

    fn open(&mut self, device: DeviceHandle) -> Result<Session, Failure> {
        if device != self.capability.handle {
            return Err(Failure::NotFound(format!("device {device}")));
        }
        if self.session.is_some() {
            return Err(Failure::Leased);
        }
        self.session = Some(7);
        Ok(7)
    }

    fn configure(&mut self, session: Session, config: Config) -> Result<Config, Failure> {
        self.check(session)?;
        config.validate(&self.capability)?;
        self.config = Some(config.clone());
        Ok(config)
    }

    fn transmit(
        &mut self,
        session: Session,
        payload: &[u8],
        _timeout_ms: u64,
    ) -> Result<Receipt, Failure> {
        self.check(session)?;
        if payload.len() > self.capability.max_packet_bytes as usize {
            return Err(Failure::OverBudget("payload larger than max packet".into()));
        }
        self.rx.push_back(Packet {
            payload: payload.to_vec(),
            rssi_dbm: Some(-72),
            snr_db: Some(8.5),
            time_on_air_ms: 42,
        });
        Ok(Receipt {
            bytes: payload.len() as u32,
            time_on_air_ms: 42,
        })
    }

    fn receive(&mut self, session: Session, _timeout_ms: u64) -> Result<Option<Packet>, Failure> {
        self.check(session)?;
        Ok(self.rx.pop_front())
    }

    fn cad(&mut self, session: Session, _timeout_ms: u64) -> Result<bool, Failure> {
        self.check(session)?;
        Ok(false)
    }

    fn state(&mut self, session: Session) -> Result<Config, Failure> {
        self.check(session)?;
        self.config
            .clone()
            .ok_or_else(|| Failure::Unavailable)
    }

    fn close(&mut self, session: Session) -> Result<(), Failure> {
        self.check(session)?;
        self.session = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            frequency_hz: 915_000_000,
            bandwidth_hz: 125_000,
            spreading_factor: 10,
            coding_rate: 5,
            preamble_symbols: 8,
            sync_word: 0x12,
            tx_power_dbm: 14,
            crc: true,
            invert_iq: false,
        }
    }

    #[test]
    fn loopback_round_trips_packets() {
        let mut radio = Loopback::new();
        let device = radio.scan(100).unwrap().remove(0);
        let session = radio.open(device.handle).unwrap();
        radio.configure(session, config()).unwrap();
        let receipt = radio.transmit(session, b"mesh", 100).unwrap();
        assert_eq!(receipt.bytes, 4);
        let packet = radio.receive(session, 100).unwrap().unwrap();
        assert_eq!(packet.payload, b"mesh");
        assert_eq!(packet.rssi_dbm, Some(-72));
    }

    #[test]
    fn rejects_out_of_range_frequency() {
        let capability = Loopback::new().scan(100).unwrap().remove(0);
        let mut config = config();
        config.frequency_hz = 100_000_000;
        assert!(matches!(config.validate(&capability), Err(Failure::Invalid(_))));
    }

    #[test]
    fn one_lease_at_a_time() {
        let mut radio = Loopback::new();
        radio.open(1).unwrap();
        assert_eq!(radio.open(1), Err(Failure::Leased));
    }
}
