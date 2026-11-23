//! `soil:inference` host wiring.
//!
//! The inference implementation lives in the moved-in `soil-inference-host`
//! crate. Loam owns the store/table integration so a Spore sees one typed WIT
//! interface and never a provider route or credential.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use wasmtime::component::{Linker, Resource};

pub use soil_inference_host::{InferenceHost, api};

/// Register `soil:inference` on a linker whose store data implements the
/// generated host traits.
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> Result<()>
where
    T: api::Host + 'static,
{
    soil_inference_host::add_to_linker(linker)
}

pub(crate) fn host(
    inference: &Option<Arc<Mutex<InferenceHost>>>,
) -> std::result::Result<std::sync::MutexGuard<'_, InferenceHost>, api::Error> {
    inference
        .as_ref()
        .ok_or(api::Error::Unavailable)?
        .lock()
        .map_err(|_| api::Error::Unavailable)
}

pub(crate) fn next_stream(
    inference: &Option<Arc<Mutex<InferenceHost>>>,
    stream: Resource<soil_inference_host::CompletionStream>,
    timeout_ms: u32,
) -> std::result::Result<Option<api::Chunk>, api::Error> {
    let mut host = host(inference)?;
    api::HostCompletionStream::next(&mut *host, stream, timeout_ms)
}

pub(crate) fn cancel_stream(
    inference: &Option<Arc<Mutex<InferenceHost>>>,
    stream: Resource<soil_inference_host::CompletionStream>,
) -> std::result::Result<(), api::Error> {
    let mut host = host(inference)?;
    api::HostCompletionStream::cancel(&mut *host, stream)
}

pub(crate) fn drop_stream(
    inference: &Option<Arc<Mutex<InferenceHost>>>,
    stream: Resource<soil_inference_host::CompletionStream>,
) -> std::result::Result<(), wasmtime::Error> {
    let Ok(mut host) = host(inference) else {
        return Ok(());
    };
    api::HostCompletionStream::drop(&mut *host, stream)
}
