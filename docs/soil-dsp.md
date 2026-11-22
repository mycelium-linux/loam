# `soil:dsp`

The first signal-processing capability is deliberately one kernel:

```wit
windowed-fft: func(request: fft-request) -> result<spectrum, error>;
```

The host owns the window function, radix-2 FFT, bounds checks, and the eventual
SIMD/GPU placement. The guest sends bounded `f32` samples, a sample rate, and a
window name; it receives magnitudes for bins `0..n/2`, the bin width, the peak
bin/frequency, and the RMS.

Constraints:

- power-of-two length
- `32..=65536` samples
- non-zero sample rate
- explicit window: `rectangular`, `hann`, `hamming`, or `blackman`

The implementation lives in `crates/soil-dsp-host`; it is a pure kernel, not a
stream owner. A graph runner will own windows, clocks, backpressure, and
placement later.
