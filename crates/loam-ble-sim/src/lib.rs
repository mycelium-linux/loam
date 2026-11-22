//! Deterministic virtual BLE backend for `soil:ble`.
//!
//! The simulator is not a Bluetooth emulator. It is a GATT-shaped source of
//! truth with a virtual clock, battery drain, scripted packet loss, disconnects,
//! stale reads, and a lease contender. That is enough to develop the host and
//! the first Spores without a radio, and to reproduce a bad day on demand.
//!
//! Every operation advances or observes `tick`; nothing reads the wall clock.
//! A test can therefore assert exact values instead of sleeping.

pub mod signal;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use soil_ble_api::{Backend, BleEvent, CharInfo, DeviceInfo, Failure, LinkId, ServiceInfo, fail};

const DIS: &str = "0000180a-0000-1000-8000-00805f9b34fb";
const HRS: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const BAS: &str = "0000180f-0000-1000-8000-00805f9b34fb";
const ANS: &str = "00001811-0000-1000-8000-00805f9b34fb";
const IAS: &str = "00001802-0000-1000-8000-00805f9b34fb";
const CTS: &str = "00001805-0000-1000-8000-00805f9b34fb";

const DIS_MODEL: &str = "00002a24-0000-1000-8000-00805f9b34fb";
const DIS_SERIAL: &str = "00002a25-0000-1000-8000-00805f9b34fb";
const DIS_FIRMWARE: &str = "00002a26-0000-1000-8000-00805f9b34fb";
const DIS_MANUFACTURER: &str = "00002a29-0000-1000-8000-00805f9b34fb";
const HR_MEASUREMENT: &str = "00002a37-0000-1000-8000-00805f9b34fb";
const BATTERY_LEVEL: &str = "00002a19-0000-1000-8000-00805f9b34fb";
const ALERT_NEW: &str = "00002a46-0000-1000-8000-00805f9b34fb";
const ALERT_LEVEL: &str = "00002a06-0000-1000-8000-00805f9b34fb";

const PINETIME_MOTION_SERVICE: &str = "00030000-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_STEPS: &str = "00030001-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MOTION: &str = "00030002-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_SERVICE: &str = "00000000-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_EVENT: &str = "00000001-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_STATUS: &str = "00000002-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_ARTIST: &str = "00000003-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_TRACK: &str = "00000004-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_ALBUM: &str = "00000005-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_POSITION: &str = "00000006-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_LENGTH: &str = "00000007-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_TRACK_NUMBER: &str = "00000008-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_TRACK_TOTAL: &str = "00000009-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_SPEED: &str = "0000000a-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_REPEAT: &str = "0000000b-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_MUSIC_SHUFFLE: &str = "0000000c-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_NOTIFICATION_EVENT: &str = "00020001-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_NAV_SERVICE: &str = "00010000-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_NAV_FLAGS: &str = "00010001-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_NAV_NARRATIVE: &str = "00010002-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_NAV_MAN_DIST: &str = "00010003-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_NAV_PROGRESS: &str = "00010004-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_WEATHER_SERVICE: &str = "00050000-78fc-48fe-8e23-433b3a1942d0";
const PINETIME_WEATHER_DATA: &str = "00050001-78fc-48fe-8e23-433b3a1942d0";
/// COLMI ring (QRing) UART service: the central writes `QRING_WRITE` and the
/// ring answers on `QRING_NOTIFY` in fixed 16-byte packets.
const QRING_SERVICE: &str = "6e40fff0-b5a3-f393-e0a9-e50e24dcca9e";
const QRING_WRITE: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e";
const QRING_NOTIFY: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e";
const OMI_AUDIO_SERVICE: &str = "19b10000-e8f2-537e-4f6c-d104768a1214";
const OMI_AUDIO_DATA: &str = "19b10001-e8f2-537e-4f6c-d104768a1214";
const OMI_CODEC: &str = "19b10002-e8f2-537e-4f6c-d104768a1214";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Static,
    HeartRate,
    Battery,
    Steps,
    Motion,
    MusicEvent,
    CallResponse,
    RingUart,
    OmiAudio,
}

#[derive(Debug, Clone)]
struct SimCharacteristic {
    uuid: String,
    readable: bool,
    writable: bool,
    notifiable: bool,
    value: Vec<u8>,
    source: Source,
}

#[derive(Debug, Clone)]
struct SimService {
    uuid: String,
    characteristics: Vec<SimCharacteristic>,
}

#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// Drop all notifications on ticks where `tick % drop_every == 0`.
    pub drop_every: Option<u64>,
    /// Emit a disconnect once the link reaches this tick.
    pub disconnect_at: Option<u64>,
    /// Reads after this tick fail with a stale error.
    pub stale_after: Option<u64>,
    /// Characteristic UUIDs whose reads are denied.
    pub denied: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SimDevice {
    pub id: String,
    pub name: String,
    pub rssi: i16,
    pub battery_percent: f32,
    pub drain_per_tick: f32,
    pub faults: Faults,
    services: Vec<SimService>,
}

/// Default concurrent links. Chosen to match the conservative controller
/// ceiling the real backend states, so a guest that plans against the
/// simulator does not discover a smaller budget on hardware.
pub const DEFAULT_CAPACITY: usize = 3;

/// One live link. Its clock, subscriptions, queue and command writes are its
/// own: two links in one guest are as independent as two separate centrals.
struct SimLink {
    device: usize,
    subscribed: HashSet<String>,
    events: VecDeque<BleEvent>,
    writes: HashMap<String, Vec<u8>>,
    tick: u64,
    /// The virtual radio dropped this link; queued events remain drainable,
    /// then the handle is refused.
    closed: bool,
    /// Which real-time reading the ring is streaming, if any. Set by the
    /// start/stop commands the guest writes, exactly like the hardware.
    real_time: Option<u8>,
}

struct SimState {
    devices: Vec<SimDevice>,
    links: HashMap<LinkId, SimLink>,
    next_link: LinkId,
    capacity: usize,
    /// Optional real-time delay per generated tick. Tests use 0; the desktop
    /// dev host uses a small value so a 1 s guest run does not produce hundreds
    /// of samples.
    pacing: Duration,
}

pub struct SimBackend {
    state: Mutex<SimState>,
}

impl SimBackend {
    pub fn builtin() -> Result<Self> {
        Self::builtin_paced(Duration::ZERO)
    }

    pub fn builtin_paced(pacing: Duration) -> Result<Self> {
        // The wearer carries two rings, a watch and a necklace; four devices and
        // three links is the smallest set that exercises the budget honestly.
        Ok(Self::with_capacity(
            vec![
                pine_time(),
                ring("sim-ring", "COLMI Ring 2", 74.0, -62),
                ring("sim-ring-2", "COLMI Ring 2", 68.0, -71),
                // A second person's ring in the same room advertises a
                // different model name, which is how a claim narrows to one.
                ring("sim-r02", "COLMI R02_3101", 81.0, -58),
                omi(),
            ],
            DEFAULT_CAPACITY,
            pacing,
        ))
    }

    pub fn with_devices(devices: Vec<SimDevice>) -> Self {
        Self::with_pacing(devices, Duration::ZERO)
    }

    pub fn with_pacing(devices: Vec<SimDevice>, pacing: Duration) -> Self {
        Self::with_capacity(devices, DEFAULT_CAPACITY, pacing)
    }

    pub fn with_capacity(devices: Vec<SimDevice>, capacity: usize, pacing: Duration) -> Self {
        Self {
            state: Mutex::new(SimState {
                devices,
                links: HashMap::new(),
                next_link: 1,
                capacity,
                pacing,
            }),
        }
    }

    /// Newest virtual tick across live links. Per-link clocks are exposed by
    /// [`Self::link_tick`]; tests with two links should use that.
    pub fn tick(&self) -> u64 {
        self.state
            .lock()
            .expect("sim state")
            .links
            .values()
            .map(|link| link.tick)
            .max()
            .unwrap_or(0)
    }

    pub fn link_tick(&self, link: LinkId) -> Option<u64> {
        self.state
            .lock()
            .expect("sim state")
            .links
            .get(&link)
            .map(|entry| entry.tick)
    }

    pub fn link_count(&self) -> usize {
        self.state.lock().expect("sim state").links.len()
    }

    /// Battery of one virtual device, for assertions.
    pub fn battery(&self, id: &str) -> Option<f32> {
        self.state
            .lock()
            .expect("sim state")
            .devices
            .iter()
            .find(|device| device.id == id)
            .map(|device| device.battery_percent)
    }

    /// Last value written to a characteristic on this link.
    pub fn last_write(&self, link: LinkId, uuid: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .expect("sim state")
            .links
            .get(&link)
            .and_then(|entry| entry.writes.get(uuid))
            .cloned()
    }
}

impl Backend for SimBackend {
    fn scan(&mut self, _timeout_ms: u64) -> Result<Vec<DeviceInfo>> {
        let state = self.state.lock().expect("sim state");
        Ok(state
            .devices
            .iter()
            .map(|device| DeviceInfo {
                id: device.id.clone(),
                name: Some(device.name.clone()),
                rssi: Some(device.rssi),
            })
            .collect())
    }

    fn max_links(&self) -> usize {
        self.state.lock().expect("sim state").capacity
    }

    fn connect(&mut self, id: &str) -> Result<LinkId> {
        let mut state = self.state.lock().expect("sim state");
        if let Some((link, _)) = state
            .links
            .iter()
            .find(|(_, entry)| state.devices[entry.device].id.eq_ignore_ascii_case(id))
        {
            return Ok(*link);
        }
        let index = state
            .devices
            .iter()
            .position(|device| device.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| anyhow::Error::new(Failure::NotFound(format!("device {id}"))))?;
        if state.links.len() >= state.capacity {
            let (used, capacity) = (state.links.len(), state.capacity);
            return fail(Failure::OverBudget(format!(
                "{id}: {used} of {capacity} links in use"
            )));
        }
        let link = state.next_link;
        state.next_link = state.next_link.wrapping_add(1).max(1);
        state.links.insert(
            link,
            SimLink {
                device: index,
                subscribed: HashSet::new(),
                events: VecDeque::new(),
                writes: HashMap::new(),
                tick: 0,
                closed: false,
                real_time: None,
            },
        );
        Ok(link)
    }

    fn disconnect(&mut self, link: LinkId) -> Result<()> {
        let mut state = self.state.lock().expect("sim state");
        state
            .links
            .remove(&link)
            .map(|_| ())
            .ok_or_else(|| anyhow::Error::new(Failure::NotConnected(link)))
    }

    fn services(&self, link: LinkId) -> Result<Vec<ServiceInfo>> {
        let state = self.state.lock().expect("sim state");
        let device = &state.devices[linked(&state, link)?.device];
        Ok(device
            .services
            .iter()
            .map(|service| ServiceInfo {
                uuid: service.uuid.clone(),
                characteristics: service
                    .characteristics
                    .iter()
                    .map(|characteristic| CharInfo {
                        uuid: characteristic.uuid.clone(),
                        readable: characteristic.readable,
                        writable: characteristic.writable,
                        notifiable: characteristic.notifiable,
                    })
                    .collect(),
            })
            .collect())
    }

    fn read(&self, link: LinkId, uuid: &str) -> Result<Vec<u8>> {
        let state = self.state.lock().expect("sim state");
        let entry = linked(&state, link)?;
        let device = &state.devices[entry.device];
        let characteristic = find_characteristic(device, uuid)?;
        if !characteristic.readable {
            bail!("characteristic {uuid} is not readable");
        }
        if device
            .faults
            .denied
            .iter()
            .any(|denied| denied.eq_ignore_ascii_case(uuid))
        {
            bail!("characteristic {uuid} is denied");
        }
        if device
            .faults
            .stale_after
            .is_some_and(|after| entry.tick > after)
        {
            bail!("characteristic {uuid} is stale");
        }
        Ok(current_value(device, characteristic, entry.tick))
    }

    fn write(&self, link: LinkId, uuid: &str, data: &[u8]) -> Result<()> {
        if data.len() > 512 {
            bail!("write exceeds 512 bytes");
        }
        let mut state = self.state.lock().expect("sim state");
        let index = linked(&state, link)?.device;
        let (key, is_static) = {
            let characteristic = find_characteristic(&state.devices[index], uuid)?;
            if !characteristic.writable {
                bail!("characteristic {uuid} is not writable");
            }
            (
                characteristic.uuid.clone(),
                characteristic.source == Source::Static,
            )
        };
        if is_static {
            if let Some(characteristic) = state.devices[index]
                .services
                .iter_mut()
                .flat_map(|service| service.characteristics.iter_mut())
                .find(|characteristic| characteristic.uuid == key)
            {
                characteristic.value = data.to_vec();
            }
        }
        let is_qring = key == QRING_WRITE;
        linked_mut(&mut state, link)?
            .writes
            .insert(key, data.to_vec());
        if is_qring {
            ring_command(&mut state, link, data)?;
        }
        Ok(())
    }

    fn subscribe(&self, link: LinkId, uuid: &str) -> Result<()> {
        let mut state = self.state.lock().expect("sim state");
        let index = linked(&state, link)?.device;
        let canonical = {
            let characteristic = find_characteristic(&state.devices[index], uuid)?;
            if !characteristic.notifiable {
                bail!("characteristic {uuid} is not notifiable");
            }
            characteristic.uuid.clone()
        };
        // A real peripheral does not replay its current value merely because a
        // central subscribed. Notifications arrive on change; tests drive the
        // virtual clock with `next_event`.
        linked_mut(&mut state, link)?.subscribed.insert(canonical);
        Ok(())
    }

    fn next_event(&self, link: LinkId, _timeout_ms: u64) -> Result<Option<BleEvent>> {
        let mut state = self.state.lock().expect("sim state");
        {
            let entry = linked_mut(&mut state, link)?;
            if let Some(event) = entry.events.pop_front() {
                return Ok(Some(event));
            }
            if entry.closed {
                return fail(Failure::NotConnected(link));
            }
            entry.tick = entry.tick.saturating_add(1);
        }

        let index = linked(&state, link)?.device;
        let tick = linked(&state, link)?.tick;
        {
            let device = &mut state.devices[index];
            device.battery_percent = (device.battery_percent - device.drain_per_tick).max(0.0);
        }
        let (disconnect_at, drop_every) = {
            let device = &state.devices[index];
            (device.faults.disconnect_at, device.faults.drop_every)
        };
        if disconnect_at.is_some_and(|at| tick >= at) {
            let entry = linked_mut(&mut state, link)?;
            entry.closed = true;
            entry.subscribed.clear();
            entry.events.clear();
            entry.events.push_back(BleEvent::Disconnected {
                reason: format!("simulated disconnect at tick {tick}"),
            });
            return Ok(entry.events.pop_front());
        }
        if drop_every.is_some_and(|every| every > 0 && tick % every == 0) {
            return Ok(None);
        }

        let mut generated = Vec::new();
        {
            let device = &state.devices[index];
            let subscribed = &linked(&state, link)?.subscribed;
            let real_time = linked(&state, link)?.real_time;
            for service in &device.services {
                for characteristic in &service.characteristics {
                    if !subscribed.contains(&characteristic.uuid) {
                        continue;
                    }
                    if characteristic.source == Source::Battery && tick % 10 != 0 {
                        continue;
                    }
                    // Button-driven events are one-shot scripts, not a value that
                    // changes every tick. A music event at tick 5 and a call
                    // response at tick 10 are enough to exercise both paths.
                    if characteristic.source == Source::MusicEvent && tick != 5 {
                        continue;
                    }
                    if characteristic.source == Source::CallResponse && tick != 10 {
                        continue;
                    }
                    let data = match characteristic.source {
                        // The ring streams whatever reading was started; it
                        // says nothing while no reading is running.
                        Source::RingUart => match real_time {
                            Some(kind) => ring_reading(kind, tick),
                            None => continue,
                        },
                        _ => notify_value(device, characteristic, tick),
                    };
                    generated.push(BleEvent::Notification {
                        char_uuid: characteristic.uuid.clone(),
                        data,
                    });
                }
            }
        }
        let mut generated = generated.into_iter();
        let first = generated.next();
        linked_mut(&mut state, link)?.events.extend(generated);
        if state.pacing > Duration::ZERO {
            std::thread::sleep(state.pacing);
        }
        Ok(first)
    }
}

/// Handle one QRing request the guest wrote to the ring's write
/// characteristic. The ring answers on its notify characteristic, and a
/// real-time reading turns into a per-tick stream instead of a reply.
fn ring_command(state: &mut SimState, link: LinkId, request: &[u8]) -> Result<()> {
    if request.len() != 16 {
        bail!("qring request is {} bytes, expected 16", request.len());
    }
    let sum = request[..15]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    if sum != request[15] {
        bail!("qring request checksum does not match its contents");
    }
    let index = linked(state, link)?.device;
    let percent = battery(&state.devices[index]);
    match request[0] {
        3 => battery_page(state, link, percent)?,
        // 1 = start, 4 = stop, matching the reference command set.
        105 if request[2] == 1 => linked_mut(state, link)?.real_time = Some(request[1]),
        106 => linked_mut(state, link)?.real_time = None,
        67 => steps_page(state, link)?,
        other => eprintln!("sim-ring: ignoring unknown command {other}"),
    }
    Ok(())
}

/// A battery reply: `[3, percent, charging, ..]`.
fn battery_page(state: &mut SimState, link: LinkId, percent: u8) -> Result<()> {
    let mut packet = [0u8; 16];
    packet[0] = 3;
    packet[1] = percent;
    packet[2] = 0;
    queue_ring_packet(state, link, packet)
}

/// One dated activity bucket on the simulator's fixed virtual day. A real ring
/// pages through several; one complete page is enough to exercise the parser.
fn steps_page(state: &mut SimState, link: LinkId) -> Result<()> {
    let mut packet = [0u8; 16];
    packet[0] = 67;
    packet[1] = 0x24; // 2024, BCD
    packet[2] = 0x01; // January
    packet[3] = 0x01; // the 1st
    packet[4] = 0; // first quarter-hour bucket
    packet[5] = 0; // page 0
    packet[6] = 1; // of 1
    packet[7] = 12; // 12 kcal
    packet[9] = 0x59;
    packet[10] = 0x01; // 345 steps
    packet[11] = 0xfa; // 250 m
    queue_ring_packet(state, link, packet)
}

fn queue_ring_packet(state: &mut SimState, link: LinkId, mut packet: [u8; 16]) -> Result<()> {
    packet[15] = packet[..15]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    linked_mut(state, link)?
        .events
        .push_back(BleEvent::Notification {
            char_uuid: QRING_NOTIFY.to_string(),
            data: packet.to_vec(),
        });
    Ok(())
}

fn linked(state: &SimState, link: LinkId) -> Result<&SimLink> {
    state
        .links
        .get(&link)
        .ok_or_else(|| anyhow::Error::new(Failure::NotConnected(link)))
}

fn linked_mut(state: &mut SimState, link: LinkId) -> Result<&mut SimLink> {
    state
        .links
        .get_mut(&link)
        .ok_or_else(|| anyhow::Error::new(Failure::NotConnected(link)))
}

fn find_characteristic<'a>(device: &'a SimDevice, uuid: &str) -> Result<&'a SimCharacteristic> {
    device
        .services
        .iter()
        .flat_map(|service| service.characteristics.iter())
        .find(|characteristic| characteristic.uuid.eq_ignore_ascii_case(uuid))
        .ok_or_else(|| anyhow!("unknown characteristic {uuid}"))
}

fn current_value(device: &SimDevice, characteristic: &SimCharacteristic, tick: u64) -> Vec<u8> {
    match characteristic.source {
        Source::Static => characteristic.value.clone(),
        Source::HeartRate => heart_rate(tick),
        Source::Battery => vec![battery(device)],
        Source::Steps => steps(tick),
        Source::Motion => motion(tick),
        Source::MusicEvent => music_event(tick),
        Source::CallResponse => call_response(tick),
        // The real-time stream depends on the link's active reading, not on
        // the characteristic, so it is generated in `next_event` instead.
        Source::RingUart => Vec::new(),
        Source::OmiAudio => omi_audio(tick),
    }
}

fn notify_value(device: &SimDevice, characteristic: &SimCharacteristic, tick: u64) -> Vec<u8> {
    current_value(device, characteristic, tick)
}

fn battery(device: &SimDevice) -> u8 {
    device.battery_percent.round().clamp(0.0, 100.0) as u8
}

fn heart_rate(tick: u64) -> Vec<u8> {
    // InfiniTime HRS: flags=0 means the measurement is the following uint8.
    let bpm: u8 = 60 + (tick % 30) as u8;
    vec![0x00, bpm]
}

fn steps(tick: u64) -> Vec<u8> {
    // InfiniTime motion service: uint32 little-endian step count.
    ((tick / 2) as u32).to_le_bytes().to_vec()
}

fn motion(tick: u64) -> Vec<u8> {
    // InfiniTime motion service: three int16 little-endian axes.
    let phase = (tick % 12) as i16;
    let mut out = Vec::with_capacity(6);
    for value in [phase * 7, -phase * 3, 1000 - phase * 2] {
        out.extend(value.to_le_bytes());
    }
    out
}

fn music_event(tick: u64) -> Vec<u8> {
    // InfiniTime event codes; the watch sends these to the phone.
    const EVENTS: [u8; 5] = [0x00, 0x01, 0x03, 0x04, 0x05];
    vec![EVENTS[(tick as usize) % EVENTS.len()]]
}

fn call_response(tick: u64) -> Vec<u8> {
    // Reject / answer / mute, matching InfiniTime's IncomingCallResponses.
    vec![(tick % 3) as u8]
}

/// The ring's answer to "start real-time <kind>": one 16-byte packet per
/// tick, the same shape the hardware sends.
fn ring_reading(kind: u8, tick: u64) -> Vec<u8> {
    let value = match kind {
        1 => (60 + tick % 30) as u8,
        3 => 97,
        // Any other real-time kind is acknowledged but carries no reading.
        _ => 0,
    };
    let mut packet = [0u8; 16];
    packet[0] = 105;
    packet[1] = kind;
    packet[2] = 0;
    packet[3] = value;
    packet[15] = packet[..15]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    packet.to_vec()
}

fn omi_audio(tick: u64) -> Vec<u8> {
    // 3-byte packet header + 160 bytes of payload, matching the OMI framing.
    let packet = tick as u16;
    let mut out = Vec::with_capacity(163);
    out.push(packet as u8);
    out.push((packet >> 8) as u8);
    out.push(0);
    out.extend((0..160).map(|index| ((tick as usize + index) % 251) as u8));
    out
}

fn service(uuid: &str, characteristics: Vec<SimCharacteristic>) -> SimService {
    SimService {
        uuid: uuid.to_ascii_lowercase(),
        characteristics,
    }
}

fn characteristic(
    uuid: &str,
    readable: bool,
    writable: bool,
    notifiable: bool,
    value: Vec<u8>,
    source: Source,
) -> SimCharacteristic {
    SimCharacteristic {
        uuid: uuid.to_ascii_lowercase(),
        readable,
        writable,
        notifiable,
        value,
        source,
    }
}

fn pine_time() -> SimDevice {
    SimDevice {
        id: "sim-pinetime".to_string(),
        name: "PineTime".to_string(),
        rssi: -55,
        battery_percent: 82.0,
        drain_per_tick: 0.01,
        faults: Faults::default(),
        services: vec![
            service(
                DIS,
                vec![
                    characteristic(
                        DIS_MANUFACTURER,
                        true,
                        false,
                        false,
                        b"Pine64".to_vec(),
                        Source::Static,
                    ),
                    characteristic(
                        DIS_MODEL,
                        true,
                        false,
                        false,
                        b"PineTime".to_vec(),
                        Source::Static,
                    ),
                    characteristic(
                        DIS_SERIAL,
                        true,
                        false,
                        false,
                        b"sim-pinetime-0001".to_vec(),
                        Source::Static,
                    ),
                    characteristic(
                        DIS_FIRMWARE,
                        true,
                        false,
                        false,
                        b"InfiniTime 1.16.0".to_vec(),
                        Source::Static,
                    ),
                ],
            ),
            service(
                HRS,
                vec![characteristic(
                    HR_MEASUREMENT,
                    true,
                    false,
                    true,
                    Vec::new(),
                    Source::HeartRate,
                )],
            ),
            service(
                BAS,
                vec![characteristic(
                    BATTERY_LEVEL,
                    true,
                    false,
                    true,
                    Vec::new(),
                    Source::Battery,
                )],
            ),
            service(
                ANS,
                vec![
                    characteristic(ALERT_NEW, false, true, false, Vec::new(), Source::Static),
                    characteristic(
                        PINETIME_NOTIFICATION_EVENT,
                        false,
                        false,
                        true,
                        Vec::new(),
                        Source::CallResponse,
                    ),
                ],
            ),
            service(
                IAS,
                vec![characteristic(
                    ALERT_LEVEL,
                    false,
                    true,
                    false,
                    Vec::new(),
                    Source::Static,
                )],
            ),
            service(
                CTS,
                vec![characteristic(
                    "00002a2b-0000-1000-8000-00805f9b34fb",
                    true,
                    true,
                    false,
                    Vec::new(),
                    Source::Static,
                )],
            ),
            service(
                PINETIME_NAV_SERVICE,
                vec![
                    characteristic(
                        PINETIME_NAV_FLAGS,
                        true,
                        true,
                        false,
                        Vec::new(),
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_NAV_NARRATIVE,
                        true,
                        true,
                        false,
                        Vec::new(),
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_NAV_MAN_DIST,
                        true,
                        true,
                        false,
                        Vec::new(),
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_NAV_PROGRESS,
                        true,
                        true,
                        false,
                        Vec::new(),
                        Source::Static,
                    ),
                ],
            ),
            service(
                PINETIME_WEATHER_SERVICE,
                vec![characteristic(
                    PINETIME_WEATHER_DATA,
                    true,
                    true,
                    false,
                    Vec::new(),
                    Source::Static,
                )],
            ),
            service(
                PINETIME_MOTION_SERVICE,
                vec![
                    characteristic(PINETIME_STEPS, true, false, true, Vec::new(), Source::Steps),
                    characteristic(
                        PINETIME_MOTION,
                        true,
                        false,
                        true,
                        Vec::new(),
                        Source::Motion,
                    ),
                ],
            ),
            service(
                PINETIME_MUSIC_SERVICE,
                vec![
                    characteristic(
                        PINETIME_MUSIC_EVENT,
                        false,
                        false,
                        true,
                        Vec::new(),
                        Source::MusicEvent,
                    ),
                    characteristic(
                        PINETIME_MUSIC_STATUS,
                        true,
                        true,
                        false,
                        vec![0],
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_ARTIST,
                        true,
                        true,
                        false,
                        Vec::new(),
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_TRACK,
                        true,
                        true,
                        false,
                        Vec::new(),
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_ALBUM,
                        true,
                        true,
                        false,
                        Vec::new(),
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_POSITION,
                        true,
                        true,
                        false,
                        vec![0; 4],
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_LENGTH,
                        true,
                        true,
                        false,
                        vec![0; 4],
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_TRACK_NUMBER,
                        true,
                        true,
                        false,
                        vec![0; 4],
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_TRACK_TOTAL,
                        true,
                        true,
                        false,
                        vec![0; 4],
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_SPEED,
                        true,
                        true,
                        false,
                        vec![0; 4],
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_REPEAT,
                        true,
                        true,
                        false,
                        vec![0],
                        Source::Static,
                    ),
                    characteristic(
                        PINETIME_MUSIC_SHUFFLE,
                        true,
                        true,
                        false,
                        vec![0],
                        Source::Static,
                    ),
                ],
            ),
        ],
    }
}

fn ring(id: &str, name: &str, battery_percent: f32, rssi: i16) -> SimDevice {
    // A QRing exposes the standard Device Information service plus one UART
    // service; there is no vendor-specific measurement characteristic, and no
    // standard heart-rate or battery service on the rings this targets.
    SimDevice {
        id: id.to_string(),
        name: name.to_string(),
        rssi,
        battery_percent,
        drain_per_tick: 0.004,
        faults: Faults::default(),
        services: vec![
            service(
                DIS,
                vec![
                    characteristic(
                        DIS_MANUFACTURER,
                        true,
                        false,
                        false,
                        b"COLMI".to_vec(),
                        Source::Static,
                    ),
                    characteristic(
                        DIS_MODEL,
                        true,
                        false,
                        false,
                        b"COLMI Ring 2".to_vec(),
                        Source::Static,
                    ),
                    // The real Ring 2 exposes 2A25 and answers with zero
                    // bytes, so the simulator does the same: a ring with no
                    // serial is the case the Spore has to handle.
                    characteristic(DIS_SERIAL, true, false, false, Vec::new(), Source::Static),
                    characteristic(
                        DIS_FIRMWARE,
                        true,
                        false,
                        false,
                        b"sim-qring-1.0.0".to_vec(),
                        Source::Static,
                    ),
                ],
            ),
            service(
                QRING_SERVICE,
                vec![
                    characteristic(QRING_WRITE, false, true, false, Vec::new(), Source::Static),
                    characteristic(
                        QRING_NOTIFY,
                        false,
                        false,
                        true,
                        Vec::new(),
                        Source::RingUart,
                    ),
                ],
            ),
        ],
    }
}

fn omi() -> SimDevice {
    SimDevice {
        id: "sim-omi".to_string(),
        name: "Omi".to_string(),
        rssi: -58,
        battery_percent: 63.0,
        drain_per_tick: 0.05,
        faults: Faults::default(),
        services: vec![
            service(
                DIS,
                vec![
                    characteristic(
                        DIS_MANUFACTURER,
                        true,
                        false,
                        false,
                        b"Based Hardware".to_vec(),
                        Source::Static,
                    ),
                    characteristic(
                        DIS_MODEL,
                        true,
                        false,
                        false,
                        b"Omi".to_vec(),
                        Source::Static,
                    ),
                    characteristic(
                        DIS_FIRMWARE,
                        true,
                        false,
                        false,
                        b"1.0.3".to_vec(),
                        Source::Static,
                    ),
                ],
            ),
            service(
                BAS,
                vec![characteristic(
                    BATTERY_LEVEL,
                    true,
                    false,
                    true,
                    Vec::new(),
                    Source::Battery,
                )],
            ),
            service(
                OMI_AUDIO_SERVICE,
                vec![
                    characteristic(
                        OMI_AUDIO_DATA,
                        false,
                        false,
                        true,
                        Vec::new(),
                        Source::OmiAudio,
                    ),
                    characteristic(OMI_CODEC, true, true, false, vec![20], Source::Static),
                ],
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> SimBackend {
        SimBackend::builtin().expect("sim")
    }

    #[test]
    fn scan_finds_builtin_devices() {
        let mut sim = backend();
        let devices = sim.scan(100).expect("scan");
        let names: Vec<_> = devices
            .iter()
            .filter_map(|device| device.name.as_deref())
            .collect();
        assert!(names.contains(&"PineTime"));
        assert!(names.contains(&"Omi"));
        assert_eq!(
            names.iter().filter(|name| **name == "COLMI Ring 2").count(),
            2,
            "the wearer carries two rings; they share a name and differ by id"
        );
        let ids: Vec<_> = devices.iter().map(|device| device.id.as_str()).collect();
        assert!(ids.contains(&"sim-ring"));
        assert!(ids.contains(&"sim-ring-2"));
    }

    #[test]
    fn connect_read_and_subscribe() {
        let mut sim = backend();
        let link = sim.connect("sim-pinetime").expect("connect");
        let services = sim.services(link).expect("services");
        assert!(services.iter().any(|service| service.uuid == HRS));
        let serial = sim.read(link, DIS_SERIAL).expect("serial");
        assert_eq!(serial, b"sim-pinetime-0001");
        let battery = sim.read(link, BATTERY_LEVEL).expect("battery");
        assert_eq!(battery.len(), 1);
        sim.subscribe(link, HR_MEASUREMENT).expect("subscribe");
        let event = sim.next_event(link, 100).expect("event").expect("some");
        match event {
            BleEvent::Notification { char_uuid, data } => {
                assert_eq!(char_uuid, HR_MEASUREMENT);
                assert_eq!(data.len(), 2, "InfiniTime sends flags + uint8 HR");
                assert_eq!(data[1], 61);
            }
            BleEvent::Disconnected { .. } => panic!("unexpected disconnect"),
        }
    }

    #[test]
    fn packet_loss_skips_a_tick() {
        let mut sim = SimBackend::with_devices(vec![{
            let mut device = pine_time();
            device.faults.drop_every = Some(2);
            device
        }]);
        let link = sim.connect("sim-pinetime").expect("connect");
        sim.subscribe(link, HR_MEASUREMENT).expect("subscribe");
        // Tick 1 succeeds, tick 2 drops, tick 3 succeeds.
        assert!(sim.next_event(link, 100).expect("tick 1").is_some());
        assert!(sim.next_event(link, 100).expect("tick 2").is_none());
        assert!(sim.next_event(link, 100).expect("tick 3").is_some());
    }

    #[test]
    fn disconnect_is_an_event() {
        let mut sim = SimBackend::with_devices(vec![{
            let mut device = pine_time();
            device.faults.disconnect_at = Some(2);
            device
        }]);
        let link = sim.connect("sim-pinetime").expect("connect");
        sim.subscribe(link, HR_MEASUREMENT).expect("subscribe");
        let _ = sim.next_event(link, 100).expect("tick 1");
        let event = sim.next_event(link, 100).expect("tick 2").expect("event");
        assert!(matches!(event, BleEvent::Disconnected { .. }));
        assert!(sim.next_event(link, 100).is_err());
    }

    #[test]
    fn stale_and_denied_reads_fail() {
        let mut sim = SimBackend::with_devices(vec![{
            let mut device = pine_time();
            device.faults.stale_after = Some(0);
            device.faults.denied.push(DIS_MODEL.to_string());
            device
        }]);
        let link = sim.connect("sim-pinetime").expect("connect");
        // Denial takes precedence and is an error, never a zero.
        assert!(sim.read(link, DIS_MODEL).is_err());
        // Advance the virtual clock past the stale bound.
        let _ = sim.next_event(link, 100);
        assert!(sim.read(link, DIS_SERIAL).is_err());
    }

    #[test]
    fn omi_audio_has_header_and_payload() {
        let mut sim = backend();
        let link = sim.connect("sim-omi").expect("connect");
        sim.subscribe(link, OMI_AUDIO_DATA).expect("subscribe");
        let event = sim.next_event(link, 100).expect("event").expect("some");
        match event {
            BleEvent::Notification { char_uuid, data } => {
                assert_eq!(char_uuid, OMI_AUDIO_DATA);
                assert_eq!(data.len(), 163);
                assert_eq!(data[2], 0);
            }
            BleEvent::Disconnected { .. } => panic!("unexpected disconnect"),
        }
    }

    #[test]
    fn pine_time_exposes_the_real_infinitime_services() {
        let mut sim = backend();
        let link = sim.connect("sim-pinetime").expect("connect");
        let services = sim.services(link).expect("services");
        let uuids: Vec<_> = services
            .iter()
            .map(|service| service.uuid.as_str())
            .collect();
        for expected in [
            HRS,
            BAS,
            ANS,
            IAS,
            CTS,
            PINETIME_MOTION_SERVICE,
            PINETIME_MUSIC_SERVICE,
        ] {
            assert!(uuids.contains(&expected), "missing service {expected}");
        }
        let motion = services
            .iter()
            .find(|service| service.uuid == PINETIME_MOTION_SERVICE)
            .expect("motion service");
        assert!(
            motion
                .characteristics
                .iter()
                .any(|characteristic| characteristic.uuid == PINETIME_STEPS)
        );
        assert!(
            motion
                .characteristics
                .iter()
                .any(|characteristic| characteristic.uuid == PINETIME_MOTION)
        );
    }

    #[test]
    fn steps_and_motion_are_distinct_values() {
        let mut sim = backend();
        let link = sim.connect("sim-pinetime").expect("connect");
        let steps = sim.read(link, PINETIME_STEPS).expect("steps");
        assert_eq!(steps.len(), 4, "uint32 step count");
        assert_eq!(u32::from_le_bytes(steps.as_slice().try_into().unwrap()), 0);
        let motion = sim.read(link, PINETIME_MOTION).expect("motion");
        assert_eq!(motion.len(), 6, "three int16 axes");
        // A step count is not an axis sum and the simulator never fabricates a
        // combined total.
        assert_ne!(steps.len(), motion.len());
    }

    #[test]
    fn notification_and_music_writes_reach_the_device() {
        let mut sim = backend();
        let link = sim.connect("sim-pinetime").expect("connect");

        sim.write(link, ALERT_NEW, &[0x03, 0x01, b'h', b'i'])
            .expect("ANS write");
        assert_eq!(
            sim.last_write(link, ALERT_NEW),
            Some(vec![0x03, 0x01, b'h', b'i'])
        );

        sim.write(link, PINETIME_MUSIC_STATUS, &[1])
            .expect("music status");
        assert_eq!(
            sim.read(link, PINETIME_MUSIC_STATUS).expect("status"),
            vec![1]
        );
        sim.write(link, PINETIME_MUSIC_ARTIST, b"Autechre")
            .expect("artist");
        assert_eq!(
            sim.read(link, PINETIME_MUSIC_ARTIST).expect("artist"),
            b"Autechre"
        );
    }

    #[test]
    fn replay_is_deterministic_and_golden() {
        fn trace() -> Vec<(u64, String, Vec<u8>)> {
            let mut sim = backend();
            let link = sim.connect("sim-pinetime").expect("connect");
            for uuid in [HR_MEASUREMENT, PINETIME_STEPS, PINETIME_MOTION] {
                sim.subscribe(link, uuid).expect("subscribe");
            }
            let mut out = Vec::new();
            for _ in 0..6 {
                match sim.next_event(link, 100).expect("event") {
                    Some(BleEvent::Notification { char_uuid, data }) => {
                        out.push((sim.tick(), char_uuid, data));
                    }
                    Some(BleEvent::Disconnected { reason }) => panic!("disconnect: {reason}"),
                    None => panic!("missing event"),
                }
            }
            out
        }

        let first = trace();
        assert_eq!(first, trace(), "same script must replay byte-for-byte");
        assert_eq!(
            first,
            vec![
                (1, HR_MEASUREMENT.to_string(), vec![0x00, 61]),
                (1, PINETIME_STEPS.to_string(), vec![0, 0, 0, 0]),
                (
                    1,
                    PINETIME_MOTION.to_string(),
                    vec![7, 0, 0xfd, 0xff, 0xe6, 0x03]
                ),
                (2, HR_MEASUREMENT.to_string(), vec![0x00, 62]),
                (2, PINETIME_STEPS.to_string(), vec![1, 0, 0, 0]),
                (
                    2,
                    PINETIME_MOTION.to_string(),
                    vec![14, 0, 0xfa, 0xff, 0xe4, 0x03]
                ),
            ]
        );
    }

    #[test]
    fn music_and_call_events_are_one_shot_scripts() {
        for (uuid, expected_tick) in [(PINETIME_MUSIC_EVENT, 5), (PINETIME_NOTIFICATION_EVENT, 10)]
        {
            let mut sim = backend();
            let link = sim.connect("sim-pinetime").expect("connect");
            sim.subscribe(link, uuid).expect("subscribe");
            let mut seen = 0;
            for _ in 0..12 {
                if let Some(BleEvent::Notification { char_uuid, .. }) =
                    sim.next_event(link, 100).expect("event")
                {
                    assert_eq!(char_uuid, uuid);
                    assert_eq!(sim.tick(), expected_tick);
                    seen += 1;
                }
            }
            assert_eq!(seen, 1, "{uuid} must fire once, not every tick");
        }
    }

    #[test]
    fn battery_drain_clamps_at_zero() {
        let mut sim = SimBackend::with_devices(vec![{
            let mut device = pine_time();
            device.battery_percent = 0.02;
            device.drain_per_tick = 0.01;
            device
        }]);
        let link = sim.connect("sim-pinetime").expect("connect");
        for _ in 0..5 {
            let _ = sim.next_event(link, 100);
        }
        assert_eq!(sim.battery("sim-pinetime"), Some(0.0));
    }

    #[test]
    fn battery_drains_deterministically() {
        let mut sim = backend();
        let link = sim.connect("sim-pinetime").expect("connect");
        let before = sim.battery("sim-pinetime").expect("battery");
        let _ = sim.next_event(link, 100);
        let after = sim.battery("sim-pinetime").expect("battery");
        assert!(after < before);
        assert_eq!(sim.tick(), 1);
    }

    #[test]
    fn stale_reads_are_unknown_never_zero() {
        let mut sim = SimBackend::with_devices(vec![{
            let mut device = pine_time();
            device.faults.stale_after = Some(0);
            device
        }]);
        let link = sim.connect("sim-pinetime").expect("connect");
        let _ = sim.next_event(link, 100);
        let result = sim.read(link, PINETIME_STEPS);
        assert!(result.is_err(), "stale must not become a zero reading");
        assert!(result.unwrap_err().to_string().contains("stale"));
    }

    #[test]
    fn disconnect_releases_the_gatt_tree() {
        let mut sim = backend();
        let link = sim.connect("sim-pinetime").expect("connect");
        assert!(sim.services(link).is_ok());
        sim.disconnect(link).expect("release");
        assert!(
            sim.services(link).is_err(),
            "a released link has no GATT tree"
        );
    }

    #[test]
    fn writes_are_recorded() {
        let mut sim = backend();
        let link = sim.connect("sim-pinetime").expect("connect");
        sim.write(link, ALERT_LEVEL, &[2]).expect("write");
        assert_eq!(sim.last_write(link, ALERT_LEVEL), Some(vec![2]));
    }

    /// Build a QRing request: command, sub-data, additive checksum.
    fn ring_packet(command: u8, sub: &[u8]) -> [u8; 16] {
        let mut packet = [0u8; 16];
        packet[0] = command;
        packet[1..1 + sub.len()].copy_from_slice(sub);
        packet[15] = packet[..15]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        packet
    }

    /// The point of link handles: one device's traffic cannot appear on, or
    /// delay, another device's queue. The ring here is driven the way the Spore
    /// drives it: a start command, then one packet per tick.
    #[test]
    fn two_links_keep_separate_queues_and_clocks() {
        let mut sim = backend();
        let watch = sim.connect("sim-pinetime").expect("watch link");
        let ring = sim.connect("sim-ring").expect("ring link");
        sim.subscribe(watch, HR_MEASUREMENT).expect("watch hr");
        sim.subscribe(ring, QRING_NOTIFY).expect("ring uart");
        sim.write(ring, QRING_WRITE, &ring_packet(105, &[1, 1]))
            .expect("start real-time hr");

        // Drain the watch hard; the ring's clock must not move.
        for _ in 0..6 {
            assert!(sim.next_event(watch, 100).expect("watch").is_some());
        }
        assert_eq!(sim.link_tick(watch), Some(6));
        assert_eq!(sim.link_tick(ring), Some(0));

        match sim
            .next_event(ring, 100)
            .expect("ring")
            .expect("ring event")
        {
            BleEvent::Notification { char_uuid, data } => {
                assert_eq!(
                    char_uuid, QRING_NOTIFY,
                    "the watch's notifications must not appear on the ring's queue"
                );
                assert_eq!(data[0], 105);
                assert_eq!(data[1], 1, "real-time heart rate");
                assert_eq!(data[3], 61, "the ring's first sample");
            }
            BleEvent::Disconnected { .. } => panic!("unexpected disconnect"),
        }
        assert_eq!(sim.link_tick(ring), Some(1));
        assert_eq!(sim.link_tick(watch), Some(6));
    }

    #[test]
    fn ring_battery_and_steps_are_request_response() {
        let mut sim = backend();
        let link = sim.connect("sim-ring").expect("connect");
        sim.subscribe(link, QRING_NOTIFY).expect("subscribe");

        // Battery: the reply carries the device's own level.
        sim.write(link, QRING_WRITE, &ring_packet(3, &[]))
            .expect("battery request");
        match sim.next_event(link, 100).expect("battery").expect("reply") {
            BleEvent::Notification { data, .. } => {
                assert_eq!(data.len(), 16);
                assert_eq!(data[0], 3);
                assert_eq!(data[1], 74, "the simulated ring's battery");
                assert_eq!(
                    data[..15]
                        .iter()
                        .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
                    data[15],
                    "the ring's reply carries its own checksum"
                );
            }
            BleEvent::Disconnected { .. } => panic!("unexpected disconnect"),
        }

        // Steps: one complete page, dated on the simulator's virtual day.
        sim.write(
            link,
            QRING_WRITE,
            &ring_packet(67, &[0, 0x0f, 0x00, 0x5f, 0x01]),
        )
        .expect("steps request");
        match sim.next_event(link, 100).expect("steps").expect("page") {
            BleEvent::Notification { data, .. } => {
                assert_eq!(data[0], 67);
                assert_eq!(data[6], 1, "the page is complete in one packet");
                assert_eq!(u16::from_le_bytes([data[9], data[10]]), 345);
            }
            BleEvent::Disconnected { .. } => panic!("unexpected disconnect"),
        }

        // Nothing streams until a reading is started, and stopping ends it.
        assert!(sim.next_event(link, 100).expect("idle").is_none());
        sim.write(link, QRING_WRITE, &ring_packet(105, &[1, 1]))
            .expect("start");
        assert!(sim.next_event(link, 100).expect("streaming").is_some());
        sim.write(link, QRING_WRITE, &ring_packet(106, &[1, 0, 0]))
            .expect("stop");
        assert!(sim.next_event(link, 100).expect("stopped").is_none());
    }

    #[test]
    fn a_corrupt_ring_request_is_refused() {
        let mut sim = backend();
        let link = sim.connect("sim-ring").expect("connect");
        sim.subscribe(link, QRING_NOTIFY).expect("subscribe");
        let mut request = ring_packet(3, &[]);
        request[15] ^= 0xff;
        let error = sim
            .write(link, QRING_WRITE, &request)
            .expect_err("bad checksum");
        assert!(error.to_string().contains("checksum"), "{error}");
        assert!(
            sim.next_event(link, 100).expect("quiet").is_none(),
            "a refused request produces no reply"
        );
    }

    #[test]
    fn fourth_link_is_refused_before_a_handshake() {
        let mut sim = backend();
        for id in ["sim-pinetime", "sim-ring", "sim-ring-2"] {
            sim.connect(id).expect("within budget");
        }
        assert_eq!(sim.max_links(), 3);
        assert_eq!(sim.link_count(), 3);

        let error = sim.connect("sim-omi").expect_err("budget is spent");
        match error.downcast_ref::<Failure>() {
            Some(Failure::OverBudget(text)) => assert!(
                text.contains("3 of 3 links"),
                "the refusal names the budget: {text}"
            ),
            other => panic!("expected over-budget, got {other:?}"),
        }
        assert_eq!(sim.link_count(), 3, "a refused link is not half-opened");
        assert!(
            sim.read(1, HR_MEASUREMENT).is_ok(),
            "existing links survive"
        );
    }

    #[test]
    fn closing_a_link_frees_the_budget() {
        let mut sim = backend();
        let watch = sim.connect("sim-pinetime").expect("watch");
        sim.connect("sim-ring").expect("ring");
        sim.connect("sim-ring-2").expect("second ring");
        sim.disconnect(watch).expect("close the watch");
        sim.connect("sim-omi")
            .expect("a closed link gives its slot back");
        assert_eq!(sim.link_count(), 3);
    }

    #[test]
    fn a_stale_link_handle_is_refused() {
        let mut sim = backend();
        let link = sim.connect("sim-pinetime").expect("connect");
        sim.disconnect(link).expect("close");
        let error = sim.read(link, DIS_SERIAL).expect_err("closed link");
        assert!(matches!(
            error.downcast_ref::<Failure>(),
            Some(Failure::NotConnected(stale)) if *stale == link
        ));
    }

    #[test]
    fn reconnecting_to_one_device_returns_the_same_link() {
        let mut sim = backend();
        let first = sim.connect("sim-pinetime").expect("connect");
        let second = sim.connect("sim-pinetime").expect("reconnect");
        assert_eq!(first, second, "one device, one link");
        assert_eq!(sim.link_count(), 1);
    }
}
