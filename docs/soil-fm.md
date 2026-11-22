# Offline FM broadcast scan

`examples/fm` is the first complex-IQ tuning proof:

```text
interleaved IQ -> IQ windowed FFT -> FFT-shifted magnitudes -> station peaks
```

The fixture is a synthetic 2.4 MS/s recording centered at 100 MHz with three
carriers at roughly -1.1 MHz, -0.35 MHz, and +0.8 MHz. The example detects
local maxima above 20% of the strongest bin and emits:

```json
{"kind":"fm.station","frequency-hz":99649609.375,"offset-hz":-350390.625,...}
```

Run:

```sh
./examples/fm/build.sh
```

This proves:

- complex IQ ordering and DC-centered bin mapping
- signed offset calculation
- peak detection in Soil over the FFT output
- a tuning-shaped event stream

It does not demodulate audio yet. The next FM step would be an `fm-demod`
kernel (phase discriminator + de-emphasis) or a scan across multiple tuner
centers. A live `soil:streams` IQ source comes after the offline scan is stable.
