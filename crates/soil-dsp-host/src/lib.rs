// SPDX-License-Identifier: MIT OR Apache-2.0
//! First real DSP kernel: windowed radix-2 FFT.
//!
//! This is intentionally a pure Rust kernel with a WIT contract. It does not
//! hold a stream, own a clock, or touch a device; those belong to the graph
//! runner and host adapters that will call it.

pub const MIN_FFT: usize = 32;
pub const MAX_FFT: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    Rectangular,
    Hann,
    Hamming,
    Blackman,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Spectrum {
    pub magnitudes: Vec<f32>,
    pub bin_hz: f32,
    pub peak_bin: u32,
    pub peak_hz: f32,
    pub rms: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DspError {
    Invalid,
    OverBudget,
}

impl Window {
    fn coefficient(self, index: usize, length: usize) -> f64 {
        if length <= 1 {
            return 1.0;
        }
        let n = (length - 1) as f64;
        let x = index as f64;
        let tau = std::f64::consts::TAU;
        match self {
            Window::Rectangular => 1.0,
            Window::Hann => 0.5 - 0.5 * (tau * x / n).cos(),
            Window::Hamming => 0.54 - 0.46 * (tau * x / n).cos(),
            Window::Blackman => 0.42 - 0.5 * (tau * x / n).cos() + 0.08 * (2.0 * tau * x / n).cos(),
        }
    }
}

/// Window `samples`, run a radix-2 FFT, and return magnitudes for `0..n/2`.
pub fn windowed_fft(
    samples: &[f32],
    sample_rate_hz: u32,
    window: Window,
) -> Result<Spectrum, DspError> {
    let n = samples.len();
    if n < MIN_FFT {
        return Err(DspError::Invalid);
    }
    if n > MAX_FFT {
        return Err(DspError::OverBudget);
    }
    if !n.is_power_of_two() || sample_rate_hz == 0 {
        return Err(DspError::Invalid);
    }

    let mut real = vec![0.0f64; n];
    let mut imag = vec![0.0f64; n];
    let mut energy = 0.0f64;
    for (index, sample) in samples.iter().enumerate() {
        let value = f64::from(*sample) * window.coefficient(index, n);
        real[index] = value;
        energy += value * value;
    }
    let rms = (energy / n as f64).sqrt() as f32;
    fft_in_place(&mut real, &mut imag);

    // A real signal is conjugate-symmetric; keep non-negative bins only.
    let bins = n / 2;
    let scale = 2.0 / n as f64;
    let mut magnitudes = Vec::with_capacity(bins);
    let mut peak_bin = 0usize;
    let mut peak = -1.0f64;
    for (bin, (re, im)) in real.iter().zip(&imag).take(bins).enumerate() {
        let magnitude = if bin == 0 {
            re.abs() / n as f64
        } else {
            (re * re + im * im).sqrt() * scale
        };
        if magnitude > peak {
            peak = magnitude;
            peak_bin = bin;
        }
        magnitudes.push(magnitude as f32);
    }
    let bin_hz = sample_rate_hz as f32 / n as f32;
    Ok(Spectrum {
        magnitudes,
        bin_hz,
        peak_bin: peak_bin as u32,
        peak_hz: peak_bin as f32 * bin_hz,
        rms,
    })
}

fn fft_in_place(real: &mut [f64], imag: &mut [f64]) {
    let n = real.len();
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            real.swap(i, j);
            imag.swap(i, j);
        }
    }
    let mut len = 2usize;
    while len <= n {
        let angle = -std::f64::consts::TAU / len as f64;
        let wlen_re = angle.cos();
        let wlen_im = angle.sin();
        let half = len / 2;
        let mut start = 0usize;
        while start < n {
            let mut w_re = 1.0f64;
            let mut w_im = 0.0f64;
            for offset in 0..half {
                let a = start + offset;
                let b = a + half;
                let v_re = real[b] * w_re - imag[b] * w_im;
                let v_im = real[b] * w_im + imag[b] * w_re;
                real[b] = real[a] - v_re;
                imag[b] = imag[a] - v_im;
                real[a] += v_re;
                imag[a] += v_im;
                let next_re = w_re * wlen_re - w_im * wlen_im;
                w_im = w_re * wlen_im + w_im * wlen_re;
                w_re = next_re;
            }
            start += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, sample_rate: f32, hz: f32) -> Vec<f32> {
        (0..n)
            .map(|index| (std::f32::consts::TAU * hz * index as f32 / sample_rate).sin())
            .collect()
    }

    #[test]
    fn finds_a_sine_bin() {
        let n = 1024;
        let rate = 48_000;
        let hz = 6_000.0;
        let spectrum = windowed_fft(&sine(n, rate as f32, hz), rate, Window::Hann).unwrap();
        let expected = (hz / (rate as f32 / n as f32)).round() as u32;
        assert!((spectrum.peak_bin as i64 - expected as i64).abs() <= 1);
        assert!((spectrum.peak_hz - hz).abs() <= spectrum.bin_hz);
        assert_eq!(spectrum.magnitudes.len(), n / 2);
    }

    #[test]
    fn rejects_bad_sizes() {
        assert_eq!(
            windowed_fft(&[], 48_000, Window::Hann),
            Err(DspError::Invalid)
        );
        assert_eq!(
            windowed_fft(&vec![0.0; 100], 48_000, Window::Hann),
            Err(DspError::Invalid)
        );
        assert_eq!(
            windowed_fft(&vec![0.0; MAX_FFT * 2], 48_000, Window::Hann),
            Err(DspError::OverBudget)
        );
    }

    #[test]
    fn windows_have_the_expected_endpoints() {
        assert_eq!(Window::Hann.coefficient(0, 64), 0.0);
        assert_eq!(Window::Hann.coefficient(63, 64), 0.0);
        assert_eq!(Window::Rectangular.coefficient(17, 64), 1.0);
    }
}
