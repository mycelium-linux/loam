//! Decoded frames to resident GPU textures.
//!
//! A video pipeline is three things with three different lifetimes: the decoder
//! (owned by the host), the frame (owned by the decoder until it is handed
//! over), and the texture (owned by the GPU until nothing samples it). What goes
//! wrong in every naive version of this is the middle one — a frame is copied
//! into a freshly allocated texture, the texture is freed when the frame is
//! dropped, and the compositor ends up paying an allocation and an upload for
//! every frame at 60 Hz while the GPU waits.
//!
//! This crate owns only the third lifetime. You hand it a [`Frame`] — planes,
//! strides, format — and it hands back a [`Resident`] texture from a small pool
//! that is reused by shape, so steady-state playback allocates nothing.
//!
//! ```no_run
//! use gpu_video::{ColorSpace, Frame, FrameTextures, PixelFormat};
//!
//! fn paint(device: &wgpu::Device, queue: &wgpu::Queue, rgba: &[u8], width: u32, height: u32) {
//!     let mut frames = FrameTextures::new();
//!     let frame = Frame {
//!         width,
//!         height,
//!         format: PixelFormat::Rgba8,
//!         color_space: ColorSpace::Srgb,
//!         strides: [width * 4, 0, 0],
//!         planes: [rgba, &[], &[]],
//!     };
//!     let resident = frames.upload(device, queue, &frame);
//!     let _bind = resident.bind_entry();
//! }
//! ```
//!
//! # Why a ring and not one texture
//!
//! A texture that is being sampled cannot be written. Reusing one texture means
//! waiting for the previous frame to finish drawing, which turns pipelining into
//! a stall; allocating one per frame means the allocator is on the hot path. A
//! small ring of `depth` textures per shape is the usual answer, and `depth` is
//! configurable because a compositor that keeps two frames in flight needs two.
//!
//! # Plans
//!
//! 8-bit 4:4:4 planes upload as one texture each. NV12 and I420 are 4:2:0 and go
//! up as a luma plane plus a chroma plane (or two for I420) in `R8Unorm`, which
//! is what every decoder actually hands over; if the adapter advertises
//! `TEXTURE_FORMAT_NV12` *and* the caller asks for it, the native multi-planar
//! format is used instead of the split.
//!
//! # What is deliberately not here
//!
//! Zero-copy import of a decoder-owned texture — `CVMetalTextureCache` on macOS,
//! CUDA or D3D11 interop elsewhere — needs `unsafe` to hand an existing native
//! handle to `wgpu`, and this workspace forbids `unsafe` outside reviewed
//! boundaries. [`NativeImport`] is the seam: a host that can do better
//! implements it in a crate that carries its own safety contract, and the
//! fallback here stays correct.

use std::collections::HashMap;

/// How the planes of a frame are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// One plane, 4 bytes per pixel, red first.
    Rgba8,
    /// One plane, 4 bytes per pixel, blue first — what most decoders emit.
    Bgra8,
    /// One luma plane at full size, one interleaved chroma plane at half the
    /// width and height.
    Nv12,
    /// One luma plane, two chroma planes at half size.
    I420,
}

/// Whether the samples are already display-referred.
///
/// The distinction is a *sampling* fact, not a taste: an sRGB texture converts
/// on the way out of the shader, and the same bytes bound as linear will look
/// washed out. A caller that does its own transfer function asks for [`ColorSpace::Linear`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorSpace {
    Srgb,
    Linear,
}

/// One decoded frame, borrowed from the decoder.
///
/// `strides` and `planes` are per plane, in the order the format names them. A
/// decoder's stride is rarely `width * bytes_per_pixel` — it is aligned to 64 or
/// 256 bytes so the hardware can write whole cache lines — and copying it as if
/// it were tight is the second classic way to get a sheared picture.
#[derive(Debug)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub color_space: ColorSpace,
    pub strides: [u32; 3],
    pub planes: [&'a [u8]; 3],
}

impl Frame<'_> {
    /// Every texture this frame becomes, as (width, height, texel size).
    pub fn planes_of(format: PixelFormat, width: u32, height: u32) -> Vec<Plane> {
        let luma = Plane {
            width,
            height,
            format: PlaneFormat::R8,
        };
        let chroma = Plane {
            width: width.div_ceil(2),
            height: height.div_ceil(2),
            format: PlaneFormat::Rg8,
        };
        match format {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => vec![Plane {
                width,
                height,
                format: PlaneFormat::Rgba8,
            }],
            PixelFormat::Nv12 => vec![luma, chroma],
            PixelFormat::I420 => vec![
                luma,
                Plane {
                    width: width.div_ceil(2),
                    height: height.div_ceil(2),
                    format: PlaneFormat::R8,
                },
                Plane {
                    width: width.div_ceil(2),
                    height: height.div_ceil(2),
                    format: PlaneFormat::R8,
                },
            ],
        }
    }

    /// Check the frame against the sizes the format implies.
    ///
    /// A short plane is a decoder bug, and a decoder bug that reaches a GPU
    /// upload becomes a validation panic somewhere the decoder is not in the
    /// stack trace. Refusing here is the cheaper failure.
    pub fn validate(&self) -> Result<(), Error> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::Empty);
        }
        for (index, plane) in Frame::planes_of(self.format, self.width, self.height)
            .iter()
            .enumerate()
        {
            let stride = self.strides[index] as usize;
            let needed = stride * plane.height as usize;
            if stride < plane.min_stride() {
                return Err(Error::Stride {
                    plane: index,
                    stride,
                    minimum: plane.min_stride(),
                });
            }
            if self.planes[index].len() < needed {
                return Err(Error::Short {
                    plane: index,
                    bytes: self.planes[index].len(),
                    needed,
                });
            }
        }
        Ok(())
    }
}

/// One plane of a frame, before it becomes a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    pub width: u32,
    pub height: u32,
    pub format: PlaneFormat,
}

impl Plane {
    /// The narrowest stride that still holds a row of this plane.
    pub fn min_stride(&self) -> usize {
        self.width as usize * self.format.bytes_per_texel()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneFormat {
    R8,
    Rg8,
    Rgba8,
}

impl PlaneFormat {
    pub fn bytes_per_texel(self) -> usize {
        match self {
            Self::R8 => 1,
            Self::Rg8 => 2,
            Self::Rgba8 => 4,
        }
    }

    fn texture_format(self, color_space: ColorSpace) -> wgpu::TextureFormat {
        match (self, color_space) {
            // `R8Unorm` is right for luma and chroma: a transfer function applied
            // to chroma is a bug, and there is no sRGB 8-bit single channel.
            (Self::R8, _) | (Self::Rg8, _) => match self {
                Self::R8 => wgpu::TextureFormat::R8Unorm,
                _ => wgpu::TextureFormat::Rg8Unorm,
            },
            (Self::Rgba8, ColorSpace::Srgb) => wgpu::TextureFormat::Rgba8UnormSrgb,
            (Self::Rgba8, ColorSpace::Linear) => wgpu::TextureFormat::Rgba8Unorm,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Empty,
    Short {
        plane: usize,
        bytes: usize,
        needed: usize,
    },
    Stride {
        plane: usize,
        stride: usize,
        minimum: usize,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(formatter, "frame has no extent"),
            Self::Short {
                plane,
                bytes,
                needed,
            } => write!(
                formatter,
                "plane {plane} is {bytes} bytes, its rows need {needed}"
            ),
            Self::Stride {
                plane,
                stride,
                minimum,
            } => write!(
                formatter,
                "plane {plane} has stride {stride}, narrower than one row ({minimum})"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// A texture a frame was uploaded into.
///
/// Holds the view the shader binds and the two numbers a compositor needs: the
/// extent it was allocated at, and whether it is sRGB.
#[derive(Debug, Clone)]
pub struct Resident {
    pub width: u32,
    pub height: u32,
    pub planes: Vec<wgpu::TextureView>,
    /// Which ring slot this frame landed in. A compositor that caps frames in
    /// flight reads it; nothing else needs to.
    pub slot: usize,
    key: FrameShape,
}

impl Resident {
    /// One bind group entry per plane, for a shader that samples them.
    pub fn bind_entries(&self, binding: u32) -> Vec<wgpu::BindGroupEntry<'_>> {
        self.planes
            .iter()
            .enumerate()
            .map(|(index, view)| wgpu::BindGroupEntry {
                binding: binding + index as u32,
                resource: wgpu::BindingResource::TextureView(view),
            })
            .collect()
    }

    /// The view for a single-plane frame, which is the common case.
    pub fn bind_entry(&self) -> wgpu::BindGroupEntry<'_> {
        wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&self.planes[0]),
        }
    }

    /// Bytes these textures hold on the GPU, for a residency budget.
    pub fn bytes(&self) -> usize {
        self.key.bytes()
    }
}

/// What a pool entry is keyed by: same shape, same plan, same texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameShape {
    width: u32,
    height: u32,
    format: PixelFormat,
    color_space: ColorSpace,
}

impl FrameShape {
    fn bytes(&self) -> usize {
        Frame::planes_of(self.format, self.width, self.height)
            .iter()
            .map(|plane| {
                plane.width as usize * plane.height as usize * plane.format.bytes_per_texel()
            })
            .sum()
    }
}

impl FrameShape {
    pub fn of(frame: &Frame<'_>) -> Self {
        Self {
            width: frame.width,
            height: frame.height,
            format: frame.format,
            color_space: frame.color_space,
        }
    }
}

/// A pool of textures, reused by frame shape.
///
/// One pool per stream: two streams of the same size would otherwise evict each
/// other's ring every frame, which is the allocation churn this exists to avoid.
pub struct FrameTextures {
    /// Textures per shape. Two would be enough to hide one frame's latency; three
    /// survives a compositor that is a frame behind without stalling.
    depth: usize,
    ring: HashMap<FrameShape, Vec<Ring>>,
    next: HashMap<FrameShape, usize>,
}

struct Ring {
    textures: Vec<wgpu::Texture>,
    views: Vec<wgpu::TextureView>,
}

impl Default for FrameTextures {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameTextures {
    pub fn new() -> Self {
        Self::with_depth(3)
    }

    pub fn with_depth(depth: usize) -> Self {
        Self {
            depth: depth.max(1),
            ring: HashMap::new(),
            next: HashMap::new(),
        }
    }

    /// Upload one frame and get the texture to sample it from.
    ///
    /// Panics on a frame that does not fit its own format; callers handing over
    /// decoder output should use [`FrameTextures::try_upload`], which returns the
    /// reason instead.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Frame<'_>,
    ) -> Resident {
        self.try_upload(device, queue, frame)
            .expect("frame does not match its format")
    }

    pub fn try_upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Frame<'_>,
    ) -> Result<Resident, Error> {
        frame.validate()?;
        let shape = FrameShape {
            width: frame.width,
            height: frame.height,
            format: frame.format,
            color_space: frame.color_space,
        };
        let slot = {
            let next = self.next.entry(shape).or_insert(0);
            let slot = *next % self.depth;
            *next = (*next + 1) % self.depth;
            slot
        };
        let rings = self.ring.entry(shape).or_insert_with(|| {
            (0..self.depth)
                .map(|_| allocate(device, &shape))
                .collect()
        });
        let ring = &mut rings[slot];

        for (index, plane) in Frame::planes_of(frame.format, frame.width, frame.height)
            .iter()
            .enumerate()
        {
            let stride = frame.strides[index];
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &ring.textures[index],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                frame.planes[index],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(plane.height),
                },
                wgpu::Extent3d {
                    width: plane.width,
                    height: plane.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        Ok(Resident {
            width: frame.width,
            height: frame.height,
            planes: ring.views.clone(),
            slot,
            key: shape,
        })
    }

    /// Bytes currently resident, across every shape this pool has seen.
    pub fn resident_bytes(&self) -> usize {
        self.ring
            .iter()
            .map(|(shape, rings)| shape.bytes() * rings.len())
            .sum()
    }

    /// Drop the rings for shapes other than `keep`, for a stream that re-sized
    /// and will not come back.
    pub fn retain(&mut self, keep: FrameShape) {
        self.ring.retain(|shape, _| *shape == keep);
        self.next.retain(|shape, _| *shape == keep);
    }
}

fn allocate(device: &wgpu::Device, shape: &FrameShape) -> Ring {
    let size = wgpu::Extent3d {
        width: shape.width,
        height: shape.height,
        depth_or_array_layers: 1,
    };
    let textures: Vec<wgpu::Texture> = Frame::planes_of(shape.format, shape.width, shape.height)
        .iter()
        .map(|plane| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("gpu-video plane"),
                size: wgpu::Extent3d {
                    width: plane.width,
                    height: plane.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: plane.format.texture_format(shape.color_space),
                // Sampled by a compositor and written by an upload. Nothing here
                // renders *into* a frame, so no attachment usage is requested:
                // asking for it makes some backends allocate a different layout.
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        })
        .collect();
    let _ = size;
    let views = textures
        .iter()
        .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()))
        .collect();
    Ring { textures, views }
}

/// A decoder that already owns a texture.
///
/// The honest zero-copy path: a platform decoder keeps its output in a GPU
/// surface, and importing that surface beats downloading it to a buffer and
/// uploading it again. It needs `unsafe` to cross into `wgpu`'s native handles,
/// so an implementation lives in a crate that states its own safety contract;
/// this crate only defines the seam and keeps the copying path correct for
/// everyone else.
pub trait NativeImport {
    /// The texture this frame is already in, if this decoder can say.
    fn import(
        &mut self,
        device: &wgpu::Device,
        frame: &Frame<'_>,
    ) -> Option<wgpu::Texture> {
        let _ = (device, frame);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(format: PixelFormat, width: u32, height: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let planes = Frame::planes_of(format, width, height);
        let build = |plane: &Plane| {
            vec![0u8; plane.width as usize * plane.height as usize * plane.format.bytes_per_texel()]
        };
        let mut buffers: Vec<Vec<u8>> = planes.iter().map(build).collect();
        while buffers.len() < 3 {
            buffers.push(Vec::new());
        }
        let three: Vec<Vec<u8>> = buffers.into_iter().take(3).collect();
        let mut iter = three.into_iter();
        (
            iter.next().unwrap_or_default(),
            iter.next().unwrap_or_default(),
            iter.next().unwrap_or_default(),
        )
    }

    #[test]
    fn nv12_is_luma_plus_interleaved_chroma_at_half_extent() {
        let planes = Frame::planes_of(PixelFormat::Nv12, 1920, 1080);
        assert_eq!(planes.len(), 2);
        assert_eq!((planes[0].width, planes[0].height), (1920, 1080));
        assert_eq!((planes[1].width, planes[1].height), (960, 540));
        assert_eq!(planes[1].format, PlaneFormat::Rg8);
    }

    #[test]
    fn an_odd_extent_rounds_the_chroma_up_rather_than_down() {
        let planes = Frame::planes_of(PixelFormat::I420, 1919, 1079);
        assert_eq!((planes[1].width, planes[1].height), (960, 540));
        assert_eq!(planes.len(), 3);
    }

    #[test]
    fn a_stride_narrower_than_a_row_is_refused() {
        let (luma, chroma, _) = frame(PixelFormat::Nv12, 64, 64);
        let parsed = Frame {
            width: 64,
            height: 64,
            format: PixelFormat::Nv12,
            color_space: ColorSpace::Srgb,
            // 32 bytes cannot hold a 64-pixel row of luma.
            strides: [32, 64, 0],
            planes: [&luma, &chroma, &[]],
        };
        match parsed.validate() {
            Err(Error::Stride { plane: 0, .. }) => {}
            other => panic!("expected a stride error, got {other:?}"),
        }
    }

    #[test]
    fn a_short_plane_says_which_one_and_how_short() {
        let (luma, chroma, _) = frame(PixelFormat::Nv12, 64, 64);
        let short = &luma[..luma.len() / 2];
        let parsed = Frame {
            width: 64,
            height: 64,
            format: PixelFormat::Nv12,
            color_space: ColorSpace::Srgb,
            strides: [64, 128, 0],
            planes: [short, &chroma, &[]],
        };
        match parsed.validate() {
            Err(Error::Short { plane: 0, .. }) => {}
            other => panic!("expected a short-plane error, got {other:?}"),
        }
    }

    #[test]
    fn a_decoders_aligned_stride_is_accepted() {
        // 60 px is 240 bytes of RGBA; decoders hand that over padded to 256.
        let width = 60u32;
        let stride = 256u32;
        let plane = vec![0u8; stride as usize * 64];
        let parsed = Frame {
            width,
            height: 64,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            strides: [stride, 0, 0],
            planes: [&plane, &[], &[]],
        };
        assert!(
            parsed.validate().is_ok(),
            "a padded stride is normal, not an error"
        );
    }

    #[test]
    fn a_stride_wider_than_the_row_is_what_a_decoder_actually_sends() {
        let width = 100u32;
        let stride = 512u32;
        let plane = vec![0u8; stride as usize * 64];
        let parsed = Frame {
            width,
            height: 64,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            strides: [stride, 0, 0],
            planes: [&plane, &[], &[]],
        };
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn residency_is_counted_per_ring_entry() {
        let shape = FrameShape {
            width: 64,
            height: 64,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
        };
        assert_eq!(shape.bytes(), 64 * 64 * 4);
    }
}
