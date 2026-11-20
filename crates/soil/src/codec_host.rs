//! Host implementation of `soil:codec`.

use std::collections::HashMap;

use wasmtime::component::{HasSelf, Linker};

use crate::host::WasiHost;

wasmtime::component::bindgen!({
    path: "../../wit/soil-codec.wit",
    world: "host",
});

use self::soil::codec::codec as api;

const MAX_PACKET: usize = 1275;
const MAX_PCM: usize = 11520 * 2;

#[derive(Default)]
pub struct CodecHost {
    decoders: HashMap<String, soil_codec::OpusDecoder>,
    encoders: HashMap<String, soil_codec::OpusEncoder>,
}

fn error(error: soil_codec::CodecError) -> api::Error {
    match error {
        soil_codec::CodecError::SampleRate(_)
        | soil_codec::CodecError::Channels(_)
        | soil_codec::CodecError::Opus(_) => api::Error::Unsupported,
        soil_codec::CodecError::Closed => api::Error::Invalid,
    }
}

impl api::Host for CodecHost {
    fn decode(
        &mut self,
        session: String,
        packet: Vec<u8>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Vec<u8>, api::Error> {
        if packet.is_empty() || packet.len() > MAX_PACKET {
            return Err(api::Error::OverBudget);
        }
        let decoder = if let Some(decoder) = self.decoders.get_mut(&session) {
            decoder
        } else {
            let decoder = soil_codec::OpusDecoder::new(sample_rate as i32, channels)
                .map_err(error)?;
            self.decoders.entry(session).or_insert(decoder)
        };
        let pcm = decoder.decode(&packet).map_err(error)?;
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(bytes)
    }

    fn encode(
        &mut self,
        session: String,
        pcm: Vec<u8>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Vec<u8>, api::Error> {
        if pcm.is_empty() || pcm.len() > MAX_PCM || !pcm.len().is_multiple_of(2) {
            return Err(api::Error::OverBudget);
        }
        let encoder = if let Some(encoder) = self.encoders.get_mut(&session) {
            encoder
        } else {
            let encoder = soil_codec::OpusEncoder::new(sample_rate as i32, channels)
                .map_err(error)?;
            self.encoders.entry(session).or_insert(encoder)
        };
        let (pairs, _) = pcm.as_chunks::<2>();
        let samples: Vec<i16> = pairs
            .iter()
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        encoder.encode(&samples).map_err(error)
    }

    fn reset(&mut self, session: String) -> Result<(), api::Error> {
        self.decoders.remove(&session);
        self.encoders.remove(&session);
        Ok(())
    }
}

impl api::Host for WasiHost {
    fn decode(
        &mut self,
        session: String,
        packet: Vec<u8>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Vec<u8>, api::Error> {
        let codec = self.codec.as_ref().ok_or(api::Error::Denied)?;
        let mut codec = codec.lock().map_err(|_| api::Error::Io)?;
        codec.decode(session, packet, sample_rate, channels)
    }

    fn encode(
        &mut self,
        session: String,
        pcm: Vec<u8>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Vec<u8>, api::Error> {
        let codec = self.codec.as_ref().ok_or(api::Error::Denied)?;
        let mut codec = codec.lock().map_err(|_| api::Error::Io)?;
        codec.encode(session, pcm, sample_rate, channels)
    }

    fn reset(&mut self, session: String) -> Result<(), api::Error> {
        let codec = self.codec.as_ref().ok_or(api::Error::Denied)?;
        let mut codec = codec.lock().map_err(|_| api::Error::Io)?;
        codec.reset(session)
    }
}

pub fn add_to_linker<T>(linker: &mut Linker<T>) -> anyhow::Result<()>
where
    T: api::Host + 'static,
{
    api::add_to_linker::<T, HasSelf<T>>(linker, |state| state)?;
    Ok(())
}
