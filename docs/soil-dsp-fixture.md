# Offline DSP fixture

`examples/dsp` is the first record-to-event run:

```text
recorded IQ JSON -> windows -> windowed FFT -> dsp.window events
```

The fixture is a recorded 6 kHz tone in 48 kHz IQ samples. The pipeline is
declared as records and validated before execution; the kernel is
`soil:dsp/windowed-fft`.

Run:

```sh
./examples/dsp/build.sh
```

It prints one JSONL event per window and a final `dsp.done` record. No live
radio, stream, or clock is involved yet.

Next: replace the fixture source with a `soil:streams`/DSP stream session and
keep the same pipeline and event schema.
