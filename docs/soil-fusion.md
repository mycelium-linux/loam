# Offline sensor fusion

`examples/fusion` is the first calibrated judge over recorded episodes. It is
deliberately offline and shadow-only:

```text
episode features -> score -> calibration -> shadow decision
```

Inputs per episode:

```text
id, label, ble, imu, hr, gps
```

`fusion/score` is a transparent weighted model over normalized channel
evidence. `fusion/calibrate` reports Brier score and five-bin expected
calibration error (ECE). `fusion/shadow-events` emits decisions with
`mode: "shadow"` and never calls an actuator.

Run:

```sh
./examples/fusion/build.sh
```

The fixture is recorded JSON; the build step converts it to a generated Soil
data file. The judge is loaded by the Soil test suite so score/calibration/shadow
behaviour is tested without a browser or a live sensor.

The next step is not a bigger model: it is replacing the hand-weighted score
with a trained small model and keeping the same calibration/shadow contract.
