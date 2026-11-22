//! Native media primitives: host-owned PCM output and codec state.
//!
//! `audio` serves `wasi:audio`; `codec` serves `soil:codec`. Both are native
//! plugin implementations. The guest never sees a device, a codec handle, or a
//! native pointer.

pub mod audio;
pub mod codec;
