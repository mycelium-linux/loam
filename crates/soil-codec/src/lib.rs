//! Native audio codec plumbing for host capabilities.
//!
//! The guest never sees a libopus handle. A resident host owns decoder/encoder
//! state and exposes bounded PCM in, bounded packets out.

use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Channels, MutSignals, SampleRate};
use audiopus::packet::Packet;
use thiserror::Error;

pub const MAX_OPUS_PACKET: usize = 1275;
pub const MAX_PCM_SAMPLES: usize = 5760;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("invalid sample rate {0}")]
    SampleRate(i32),
    #[error("invalid channels {0}")]
    Channels(u16),
    #[error("opus: {0}")]
    Opus(String),
    #[error("codec is closed")]
    Closed,
}

fn rate(value: i32) -> Result<SampleRate, CodecError> {
    SampleRate::try_from(value).map_err(|_| CodecError::SampleRate(value))
}

fn channels(value: u16) -> Result<Channels, CodecError> {
    match value {
        1 => Ok(Channels::Mono),
        2 => Ok(Channels::Stereo),
        other => Err(CodecError::Channels(other)),
    }
}

/// One stateful Opus decoder. Keep it for the life of a stream: packet loss and
/// PLC state are part of the codec, not a per-packet helper.
pub struct OpusDecoder {
    inner: Decoder,
    channels: usize,
}

impl OpusDecoder {
    pub fn new(sample_rate: i32, channel_count: u16) -> Result<Self, CodecError> {
        let inner = Decoder::new(rate(sample_rate)?, channels(channel_count)?)
            .map_err(|error| CodecError::Opus(error.to_string()))?;
        Ok(Self {
            inner,
            channels: channel_count as usize,
        })
    }

    /// Decode one Opus packet into interleaved i16 PCM.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, CodecError> {
        if packet.is_empty() || packet.len() > MAX_OPUS_PACKET {
            return Err(CodecError::Closed);
        }
        let mut output = vec![0i16; MAX_PCM_SAMPLES * self.channels];
        let packet = Packet::try_from(packet)
            .map_err(|error| CodecError::Opus(error.to_string()))?;
        let signals = MutSignals::try_from(output.as_mut_slice())
            .map_err(|error| CodecError::Opus(error.to_string()))?;
        let samples = self
            .inner
            .decode(Some(packet), signals, false)
            .map_err(|error| CodecError::Opus(error.to_string()))?;
        output.truncate(samples * self.channels);
        Ok(output)
    }
}

/// One stateful Opus encoder at a fixed rate/channel count.
pub struct OpusEncoder {
    inner: Encoder,
}

impl OpusEncoder {
    pub fn new(sample_rate: i32, channel_count: u16) -> Result<Self, CodecError> {
        let inner = Encoder::new(
            rate(sample_rate)?,
            channels(channel_count)?,
            Application::Voip,
        )
        .map_err(|error| CodecError::Opus(error.to_string()))?;
        Ok(Self { inner })
    }

    /// Encode interleaved i16 PCM into one Opus packet.
    pub fn encode(&self, pcm: &[i16]) -> Result<Vec<u8>, CodecError> {
        let mut output = vec![0u8; MAX_OPUS_PACKET];
        let len = self
            .inner
            .encode(pcm, &mut output)
            .map_err(|error| CodecError::Opus(error.to_string()))?;
        output.truncate(len);
        Ok(output)
    }
}

/// Encode PCM16 into a mono WAV body. The STT upstream accepts WAV, not raw
/// PCM, and the host owns the container just as it owns the codec.
pub fn wav_mono(pcm: &[u8], sample_rate: u32) -> Result<Vec<u8>, CodecError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    let mut writer = hound::WavWriter::new(&mut cursor, spec)
        .map_err(|error| CodecError::Opus(error.to_string()))?;
    let (samples, _) = pcm.as_chunks::<2>();
    for sample in samples {
        writer
            .write_sample(i16::from_le_bytes([sample[0], sample[1]]))
            .map_err(|error| CodecError::Opus(error.to_string()))?;
    }
    writer
        .finalize()
        .map_err(|error| CodecError::Opus(error.to_string()))?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_round_trip_has_audio() {
        let encoder = OpusEncoder::new(48_000, 1).unwrap();
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16)
            .collect();
        let packet = encoder.encode(&pcm).unwrap();
        assert!(!packet.is_empty());
        let mut decoder = OpusDecoder::new(48_000, 1).unwrap();
        let decoded = decoder.decode(&packet).unwrap();
        assert!(!decoded.is_empty());
    }

    #[test]
    fn wav_has_riff_header() {
        let wav = wav_mono(&[0, 0, 1, 0], 16_000).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
    }
}
