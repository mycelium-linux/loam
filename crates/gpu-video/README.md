# gpu-video

Decoded frames in, resident `wgpu` textures out.

A video pipeline has three lifetimes that are easy to conflate: the decoder, the
frame it just wrote, and the texture the compositor samples. The bug that follows
from conflating the last two is an allocation and an upload per frame at 60 Hz.
This crate owns only the third: hand it a frame and it returns a texture from a
small pool, reused by shape, so steady-state playback allocates nothing.

```rust
let mut frames = FrameTextures::new();
let resident = frames.upload(&device, &queue, &frame);
let _ = resident.bind_entry();
```

## What it handles

| concern | how |
| --- | --- |
| formats | `Rgba8`, `Bgra8` (what decoders emit), `Nv12`, `I420` |
| stride | accepts any stride ≥ one row — a decoder pads to 64 or 256 bytes, and copying that as if it were tight is how a picture comes out sheared |
| 4:2:0 | luma plane + chroma plane(s) in `R8`/`Rg8`, which is what every decoder actually hands over |
| sRGB | `Rgba8UnormSrgb` for a display-referred frame, `…Unorm` for one the caller converts itself |
| pipelining | a ring of `depth` textures per shape (3 by default), so a queued frame does not overwrite the one being sampled |
| residency | `resident_bytes()` and `FrameShape::of(&frame)` so a caller can prune a pool after a re-size |

## What it does not

**Zero-copy import.** A platform decoder keeps its output in a GPU surface
(`CVMetalTextureCache` on macOS, CUDA or D3D11 elsewhere) and importing it beats
a download and re-upload. That needs `unsafe` to hand a native handle to `wgpu`,
which this workspace forbids outside reviewed boundaries, so `NativeImport` is a
seam: implement it in a crate that states its own safety contract, and the copying
path stays correct for everyone else.

**Decoding.** This takes frames, not files. A host that "opens" media owns the
decoder; what it needs from a crate like this one is a texture that outlives the
frame.
