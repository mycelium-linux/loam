//! The presenter seam: a frame on its way out, and input on its way in.
//!
//! A Spore never owns a window. It hands the host a completed frame and reads
//! one ordered stream of normalized input events; a presenter is whatever turns
//! that frame into a window blit, a framebuffer write, or a file. The types here
//! are deliberately plain: `canvas-winit` implements the trait with softbuffer,
//! a panel host with [`crate::webgpu::fb`], and a test with [`FrameCapture`].

use std::sync::{Arc, Mutex};

use anyhow::Result;

/// One composed Canvas frame, tightly packed top-to-bottom RGBA8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Host-side destination for a guest `soil:present` frame.
///
/// Canvas owns the actual window and final composition; this trait only lets
/// the WebGPU host hand over a completed frame. It is never called from inside
/// a guest callback.
pub trait CanvasGpuPresenter: Send {
    fn size(&self) -> (u32, u32);

    /// Device pixels per layout pixel. A capture host renders at 1:1.
    fn scale(&self) -> f64 {
        1.0
    }

    fn present(&mut self, frame: CanvasFrame) -> Result<()>;
}

/// A presenter that accepts a frame and drops it.
///
/// The bare `run` entry points need a surface extent and nothing else: a
/// workload that only computes has no pixels worth keeping.
pub struct DiscardPresenter {
    size: (u32, u32),
}

impl DiscardPresenter {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            size: (width, height),
        }
    }
}

impl CanvasGpuPresenter for DiscardPresenter {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn present(&mut self, _frame: CanvasFrame) -> Result<()> {
        Ok(())
    }
}

/// A presenter that retains exactly one frame for tests and CLI output.
#[derive(Clone, Default)]
pub struct FrameCapture {
    frame: Arc<Mutex<Option<CanvasFrame>>>,
    size: (u32, u32),
}

impl FrameCapture {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            frame: Arc::new(Mutex::new(None)),
            size: (width, height),
        }
    }

    pub fn take(&self) -> Option<CanvasFrame> {
        self.frame.lock().expect("frame capture poisoned").take()
    }
}

impl CanvasGpuPresenter for FrameCapture {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn present(&mut self, frame: CanvasFrame) -> Result<()> {
        let mut slot = self.frame.lock().expect("frame capture poisoned");
        anyhow::ensure!(slot.is_none(), "present budget");
        *slot = Some(frame);
        Ok(())
    }
}

/// The pointer shape a Spore asks for. A presenter maps it to a native cursor;
/// a headless host drops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStyle {
    Default,
    Text,
    Pointer,
    Crosshair,
    Move,
    Grab,
    Grabbing,
    NoDrop,
    Wait,
    Help,
}

/// The system clipboard, when the presenter has one. A host without a pasteboard
/// leaves it unset and `soil:present/clipboard` refuses loudly.
pub trait ClipboardHost: Send {
    fn set_text(&mut self, text: String) -> Result<(), String>;
}

/// The window's pointer shape. Presenters that own a window implement it;
/// a capture or headless host leaves it unset and `set` succeeds doing nothing.
pub trait CursorHost: Send {
    fn set_cursor(&mut self, style: CursorStyle);
}

/// One normalized key waiting for the guest. The WIT type is built at the
/// boundary; presenters only need this plain shape.
#[derive(Clone, Debug)]
pub struct InputKey {
    pub key: String,
    pub text: Option<String>,
    pub pressed: bool,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Presenter-independent pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

/// One event waiting for the guest, in the order the presenter produced it.
/// Keys and pointer actions share a queue, so a click cannot overtake a
/// keystroke that arrived before it.
#[derive(Clone, Debug)]
pub enum InputEvent {
    Key(InputKey),
    /// Surface-space position: the units `soil:present/surface.size` reports.
    Moved {
        x: f32,
        y: f32,
    },
    Button {
        x: f32,
        y: f32,
        button: PointerButton,
        pressed: bool,
    },
    /// Positive `lines` scrolls towards earlier content.
    Scrolled {
        x: f32,
        y: f32,
        lines: f32,
    },
}
