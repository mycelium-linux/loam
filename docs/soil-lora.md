# `soil:lora`

`soil:lora` is the radio transport, not the mesh protocol.

```text
soil:lora       scan / open / configure / transmit / receive / CAD
meshcore.soil   identity, contacts, routes, channels, ACK/retry
```

The host owns:

- the serial/SPI device
- the exclusive radio lease
- region and duty-cycle policy
- packet size and TX timeout
- RX queue and backpressure
- chip-specific timing

The guest sees opaque device/session handles and bounded packets. It never sees
a serial path, SPI device, GPIO line or modem command.

## Backends

`crates/soil-lora-api` defines the `Backend` trait and a loopback backend for
tests. It follows the `soil-ble-api` pattern:

```text
soil:lora WIT
      ↓
soil-lora-api Backend trait
      ↓
SX1262 serial | SX1276 SPI | simulator | remote modem
```

Adding a chip does not add a WIT.

## What is deliberately not here

- MeshCore framing, routes, contacts, channels, ACKs
- LoRaWAN MAC, join, uplink/downlink
- frequency plans and regulatory limits beyond host validation
- audio/codec or DSP

MeshCore should be a Spore over `soil:lora` + `soil:records`. If the existing
MeshCore sidecar remains authoritative, expose it through a narrow adapter
(`soil:http`/`soil:streams`) first; add a typed `soil:meshcore` WIT only when
multiple Spores need the same service contract.

The hardware adapter is the next slice. This commit defines the contract and
the provider seam needed to add one without touching the guest ABI.
