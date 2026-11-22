//! Native surface host: winit owns the window, `wasi:webgpu` owns the GPU
//! device, and `soil:surface` carries a finished frame or texture to the glass.
//!
//! There is no scene graph, no layout engine, and no UI framework here. A guest
//! renders however it likes and hands over pixels or a WebGPU texture.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use wasi_webgpu_wasmtime::reexports::{wgpu_core, wgpu_types};
use wasi_webgpu_wasmtime::{WasiWebGpuCtx, WasiWebGpuCtxView, WasiWebGpuOptions};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key as WinitKey, ModifiersState, NamedKey},
    window::{CursorIcon, Window, WindowId},
};

mod wit {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "runner",
        exports: {
            "run": async,
        },
        with: {
            "wasi:webgpu/webgpu@0.3.0-rc.2.gpu-texture": wasi_webgpu_wasmtime::Texture,
        },
        require_store_data_send: true,
    });
}

use wit::soil::surface::{clipboard, cursor, input, surface, types};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const DEFAULT_SIZE: (u32, u32) = (1000, 700);

/// One tightly packed RGBA8 frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Host destination for a presented frame.
pub trait Presenter: Send {
    fn size(&self) -> (u32, u32);
    fn present(&mut self, frame: Frame) -> Result<()>;
}

/// A capture presenter for tests and `--capture`.
#[derive(Clone)]
pub struct FrameCapture {
    frame: Arc<Mutex<Option<Frame>>>,
    size: (u32, u32),
}

impl FrameCapture {
    pub fn new(size: (u32, u32)) -> Self {
        Self {
            frame: Arc::new(Mutex::new(None)),
            size,
        }
    }

    pub fn take(&self) -> Option<Frame> {
        self.frame.lock().expect("frame capture poisoned").take()
    }
}

impl Presenter for FrameCapture {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn present(&mut self, frame: Frame) -> Result<()> {
        *self.frame.lock().expect("frame capture poisoned") = Some(frame);
        Ok(())
    }
}

/// Input the presenter queues for the guest.
#[derive(Clone, Debug)]
pub enum InputEvent {
    Key {
        key: String,
        text: Option<String>,
        pressed: bool,
        shift: bool,
        control: bool,
        alt: bool,
        meta: bool,
    },
    Moved {
        x: f32,
        y: f32,
    },
    Button {
        x: f32,
        y: f32,
        button: u8,
        pressed: bool,
    },
    Scrolled {
        x: f32,
        y: f32,
        lines: f32,
    },
}

pub struct SurfaceSetup {
    pub presenter: Box<dyn Presenter>,
    pub input: Arc<Mutex<VecDeque<InputEvent>>>,
    pub interactive: bool,
}

impl SurfaceSetup {
    pub fn new(presenter: Box<dyn Presenter>, input: Arc<Mutex<VecDeque<InputEvent>>>) -> Self {
        Self {
            presenter,
            input,
            interactive: true,
        }
    }
}

struct Host {
    instance: Arc<wgpu_core::global::Global>,
    table: ResourceTable,
    options: WasiWebGpuOptions,
    wasi: WasiCtx,
    presenter: Box<dyn Presenter>,
    input: Arc<Mutex<VecDeque<InputEvent>>>,
    interactive: bool,
    last_frame: Option<Instant>,
}

impl Host {
    fn new(setup: SurfaceSetup) -> Result<Self> {
        let descriptor = wgpu_types::InstanceDescriptor {
            backends: wgpu_types::Backends::all(),
            flags: wgpu_types::InstanceFlags::from_build_config(),
            backend_options: Default::default(),
            memory_budget_thresholds: Default::default(),
            display: None,
        };
        Ok(Self {
            instance: Arc::new(wgpu_core::global::Global::new("webgpu", descriptor, None)),
            table: ResourceTable::new(),
            options: WasiWebGpuOptions::default(),
            wasi: WasiCtxBuilder::new().inherit_stderr().build(),
            presenter: setup.presenter,
            input: setup.input,
            interactive: setup.interactive,
            last_frame: None,
        })
    }

    fn pace(&mut self) {
        if let Some(last) = self.last_frame {
            let elapsed = last.elapsed();
            if elapsed < FRAME_INTERVAL {
                thread::sleep(FRAME_INTERVAL - elapsed);
            }
        }
        self.last_frame = Some(Instant::now());
    }
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiWebGpuCtxView for Host {
    fn webgpu_ctx(&mut self) -> WasiWebGpuCtx<'_> {
        WasiWebGpuCtx {
            instance: &self.instance,
            table: &mut self.table,
            options: &self.options,
        }
    }
}

impl surface::Host for Host {
    fn size(&mut self) -> (u32, u32) {
        self.presenter.size()
    }

    fn scale(&mut self) -> f64 {
        1.0
    }

    fn delivery(&mut self) -> Vec<types::DeliveryKind> {
        vec![types::DeliveryKind::Texture, types::DeliveryKind::Rgba]
    }

    fn present(
        &mut self,
        texture: Resource<wasi_webgpu_wasmtime::Texture>,
    ) -> std::result::Result<(), String> {
        self.pace();
        let image = wasi_webgpu_wasmtime::read_texture_rgba(&self.instance, &self.table, texture)
            .map_err(|error| error.to_string())?;
        self.presenter
            .present(Frame {
                width: image.width,
                height: image.height,
                rgba: image.rgba,
            })
            .map_err(|error| error.to_string())
    }

    fn present_rgba(&mut self, frame: types::RgbaFrame) -> std::result::Result<(), String> {
        self.pace();
        let (width, height) = self.presenter.size();
        if frame.width != width || frame.height != height {
            return Err(format!(
                "frame is {}x{}, the granted surface is {width}x{height}",
                frame.width, frame.height
            ));
        }
        if frame.pixels.len() != width as usize * height as usize * 4 {
            return Err("frame is not tightly packed RGBA8".into());
        }
        if frame
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0xFF)
        {
            return Err("every pixel must be opaque".into());
        }
        self.presenter
            .present(Frame {
                width,
                height,
                rgba: frame.pixels,
            })
            .map_err(|error| error.to_string())
    }
}

impl input::Host for Host {
    fn interactive(&mut self) -> bool {
        self.interactive
    }

    fn poll(&mut self, timeout_ms: u32) -> Option<input::Event> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            if let Some(event) = self.input.lock().expect("input queue poisoned").pop_front() {
                return Some(match event {
                    InputEvent::Key {
                        key,
                        text,
                        pressed,
                        shift,
                        control,
                        alt,
                        meta,
                    } => input::Event::KeyInput(input::Key {
                        key,
                        text,
                        pressed,
                        shift,
                        control,
                        alt,
                        meta,
                    }),
                    InputEvent::Moved { x, y } => input::Event::Moved(input::PointerMove { x, y }),
                    InputEvent::Button {
                        x,
                        y,
                        button,
                        pressed,
                    } => input::Event::Button(input::PointerPress {
                        x,
                        y,
                        button: match button {
                            0 => input::PointerButton::Left,
                            1 => input::PointerButton::Right,
                            _ => input::PointerButton::Middle,
                        },
                        pressed,
                    }),
                    InputEvent::Scrolled { x, y, lines } => {
                        input::Event::Scrolled(input::PointerScroll { x, y, lines })
                    }
                });
            }
            if !self.interactive || timeout_ms == 0 || Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl cursor::Host for Host {
    fn set(&mut self, _style: cursor::CursorStyle) -> std::result::Result<(), String> {
        Ok(())
    }
}

impl clipboard::Host for Host {
    fn set_text(&mut self, text: String) -> std::result::Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            use std::io::Write as _;
            let mut child = std::process::Command::new("/usr/bin/pbcopy")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|error| format!("pbcopy: {error}"))?;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin
                    .write_all(text.as_bytes())
                    .map_err(|error| format!("pbcopy stdin: {error}"))?;
            }
            let status = child.wait().map_err(|error| format!("pbcopy: {error}"))?;
            if status.success() {
                Ok(())
            } else {
                Err("pbcopy reported failure".into())
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = text;
            Err("clipboard is not available on this platform".into())
        }
    }
}

/// Reusable engine + linker. Each run gets a fresh store and resource table.
pub struct SurfaceRuntime {
    engine: Engine,
    linker: Linker<Host>,
}

impl SurfaceRuntime {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        let engine = Engine::new(&config).map_err(anyhow::Error::from)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(anyhow::Error::from)?;
        wasi_webgpu_wasmtime::add_to_linker(&mut linker)
            .map_err(anyhow::Error::from)
            .context("link wasi:webgpu")?;
        surface::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:surface/surface")?;
        input::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:surface/input")?;
        cursor::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:surface/cursor")?;
        clipboard::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host)
            .map_err(anyhow::Error::from)
            .context("link soil:surface/clipboard")?;
        Ok(Self { engine, linker })
    }

    pub fn run(&self, bytes: &[u8], setup: SurfaceSetup) -> Result<u32> {
        pollster::block_on(self.run_async(bytes, setup))
    }

    /// Run a sync guest export. The RGBA capture fixture is sync on purpose:
    /// a frame handoff has no asynchronous operation, and keeping the proof
    /// path free of the async ABI makes it a smaller, more useful test.
    fn run_sync(&self, bytes: &[u8], setup: SurfaceSetup) -> Result<u32> {
        let component = Component::new(&self.engine, bytes)
            .map_err(anyhow::Error::from)
            .context("compile surface component")?;
        let mut store = Store::new(&self.engine, Host::new(setup)?);
        let instance = self
            .linker
            .instantiate(&mut store, &component)
            .map_err(anyhow::Error::from)
            .context("instantiate surface component")?;
        let run = instance
            .get_func(&mut store, "run")
            .context("surface component does not export run")?
            .typed::<(), (u32,)>(&mut store)
            .map_err(anyhow::Error::from)
            .context("run: func() -> u32")?;
        let (status,) = run.call(&mut store, ()).map_err(anyhow::Error::from)?;
        Ok(status)
    }

    async fn run_async(&self, bytes: &[u8], setup: SurfaceSetup) -> Result<u32> {
        let component = Component::new(&self.engine, bytes)
            .map_err(anyhow::Error::from)
            .context("compile surface component")?;
        let mut store = Store::new(&self.engine, Host::new(setup)?);
        let instance = wit::Runner::instantiate_async(&mut store, &component, &self.linker)
            .await
            .map_err(anyhow::Error::from)
            .context("instantiate surface component")?;
        let (status,) = instance
            .func_run()
            .call_async(&mut store, ())
            .await
            .map_err(anyhow::Error::from)
            .context("run surface component")?;
        Ok(status)
    }
}

enum WindowMessage {
    Frame(Frame),
    Finished(Result<u32>),
}

struct WindowPresenter {
    size: (u32, u32),
    sender: mpsc::Sender<WindowMessage>,
}

impl Presenter for WindowPresenter {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn present(&mut self, frame: Frame) -> Result<()> {
        self.sender
            .send(WindowMessage::Frame(frame))
            .map_err(|_| anyhow::anyhow!("window closed"))
    }
}

struct App {
    receiver: mpsc::Receiver<WindowMessage>,
    input: Arc<Mutex<VecDeque<InputEvent>>>,
    size: (u32, u32),
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    frame: Option<Frame>,
    modifiers: ModifiersState,
    pointer: (f32, f32),
    scale: f32,
    finished: bool,
}

impl App {
    fn new(
        receiver: mpsc::Receiver<WindowMessage>,
        input: Arc<Mutex<VecDeque<InputEvent>>>,
        size: (u32, u32),
    ) -> Self {
        Self {
            receiver,
            input,
            size,
            window: None,
            context: None,
            surface: None,
            frame: None,
            modifiers: ModifiersState::empty(),
            pointer: (0.0, 0.0),
            scale: 1.0,
            finished: false,
        }
    }

    fn drain(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                WindowMessage::Frame(frame) => self.frame = Some(frame),
                WindowMessage::Finished(result) => {
                    if let Err(error) = result {
                        eprintln!("loam surface: {error:#}");
                    }
                    self.finished = true;
                    event_loop.exit();
                }
            }
        }
    }

    fn paint(&mut self) {
        let (Some(surface), Some(frame), Some(window)) = (
            self.surface.as_mut(),
            self.frame.as_ref(),
            self.window.as_ref(),
        ) else {
            return;
        };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        // Nearest-neighbour only: the guest owns its pixels; the window does not
        // silently resample them.
        for y in 0..size.height {
            let sy = (u64::from(y) * u64::from(frame.height) / u64::from(size.height)) as u32;
            for x in 0..size.width {
                let sx = (u64::from(x) * u64::from(frame.width) / u64::from(size.width)) as u32;
                let offset = ((sy * frame.width + sx) * 4) as usize;
                let pixel = &frame.rgba[offset..offset + 4];
                buffer[(y * size.width + x) as usize] =
                    (u32::from(pixel[0]) << 16) | (u32::from(pixel[1]) << 8) | u32::from(pixel[2]);
            }
        }
        let _ = buffer.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attributes = Window::default_attributes()
            .with_title("loam surface")
            .with_inner_size(LogicalSize::new(
                f64::from(self.size.0),
                f64::from(self.size.1),
            ));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                eprintln!("loam surface: create window: {error}");
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("loam surface: softbuffer context: {error}");
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(surface) => surface,
            Err(error) => {
                eprintln!("loam surface: softbuffer surface: {error}");
                event_loop.exit();
                return;
            }
        };
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                self.drain(event_loop);
                self.paint();
            }
            WindowEvent::Resized(size) => {
                self.size = (size.width, size.height);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    self.input
                        .lock()
                        .expect("input queue")
                        .push_back(InputEvent::Key {
                            key: key_name(&event.logical_key),
                            text: event.text.as_ref().map(ToString::to_string),
                            pressed: true,
                            shift: self.modifiers.shift_key(),
                            control: self.modifiers.control_key(),
                            alt: self.modifiers.alt_key(),
                            meta: self.modifiers.super_key(),
                        });
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (
                    position.x as f32 / self.scale,
                    position.y as f32 / self.scale,
                );
                self.input
                    .lock()
                    .expect("input queue")
                    .push_back(InputEvent::Moved {
                        x: self.pointer.0,
                        y: self.pointer.1,
                    });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    _ => return,
                };
                self.input
                    .lock()
                    .expect("input queue")
                    .push_back(InputEvent::Button {
                        x: self.pointer.0,
                        y: self.pointer.1,
                        button,
                        pressed: state == ElementState::Pressed,
                    });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, vertical) => vertical,
                    MouseScrollDelta::PixelDelta(position) => position.y as f32 / 20.0,
                };
                self.input
                    .lock()
                    .expect("input queue")
                    .push_back(InputEvent::Scrolled {
                        x: self.pointer.0,
                        y: self.pointer.1,
                        lines,
                    });
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain(event_loop);
        if self.finished {
            event_loop.exit();
            return;
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME_INTERVAL));
    }
}

fn key_name(key: &WinitKey) -> String {
    match key {
        WinitKey::Named(NamedKey::Enter) => "enter".into(),
        WinitKey::Named(NamedKey::Escape) => "escape".into(),
        WinitKey::Named(NamedKey::ArrowDown) => "arrow-down".into(),
        WinitKey::Named(NamedKey::ArrowUp) => "arrow-up".into(),
        WinitKey::Named(NamedKey::ArrowLeft) => "arrow-left".into(),
        WinitKey::Named(NamedKey::ArrowRight) => "arrow-right".into(),
        WinitKey::Named(NamedKey::Space) => "space".into(),
        WinitKey::Named(NamedKey::Tab) => "tab".into(),
        WinitKey::Named(NamedKey::Backspace) => "backspace".into(),
        WinitKey::Character(text) => text.to_string(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Open a native window and run one component.
pub fn run_window(module: &Path, size: Option<(u32, u32)>) -> Result<()> {
    let bytes = std::fs::read(module).with_context(|| format!("read {}", module.display()))?;
    let size = size.unwrap_or(DEFAULT_SIZE);
    let input = Arc::new(Mutex::new(VecDeque::new()));
    let (sender, receiver) = mpsc::channel();
    let presenter = WindowPresenter {
        size,
        sender: sender.clone(),
    };
    let guest_input = Arc::clone(&input);
    let guest = thread::spawn(move || {
        let runtime = SurfaceRuntime::new();
        let result = runtime.and_then(|runtime| {
            runtime.run(
                &bytes,
                SurfaceSetup {
                    presenter: Box::new(presenter),
                    input: guest_input,
                    interactive: true,
                },
            )
        });
        let _ = sender.send(WindowMessage::Finished(result));
    });

    let event_loop = EventLoop::new().context("create winit event loop")?;
    let mut app = App::new(receiver, input, size);
    event_loop
        .run_app(&mut app)
        .context("run winit event loop")?;
    guest
        .join()
        .map_err(|_| anyhow::anyhow!("surface guest panicked"))?;
    Ok(())
}

/// Capture one frame and return it.
pub fn capture(module: &Path, size: (u32, u32)) -> Result<Option<Frame>> {
    let bytes = std::fs::read(module).with_context(|| format!("read {}", module.display()))?;
    let capture = FrameCapture::new(size);
    let sink = capture.clone();
    let runtime = SurfaceRuntime::new()?;
    runtime.run_sync(
        &bytes,
        SurfaceSetup {
            presenter: Box::new(sink),
            input: Arc::new(Mutex::new(VecDeque::new())),
            interactive: false,
        },
    )?;
    Ok(capture.take())
}

#[allow(dead_code)]
fn _cursor_icon(style: cursor::CursorStyle) -> CursorIcon {
    match style {
        cursor::CursorStyle::Default => CursorIcon::Default,
        cursor::CursorStyle::Text => CursorIcon::Text,
        cursor::CursorStyle::Pointer => CursorIcon::Pointer,
        cursor::CursorStyle::Crosshair => CursorIcon::Crosshair,
        cursor::CursorStyle::Move => CursorIcon::Move,
        cursor::CursorStyle::Grab => CursorIcon::Grab,
        cursor::CursorStyle::Grabbing => CursorIcon::Grabbing,
        cursor::CursorStyle::NoDrop => CursorIcon::NoDrop,
        cursor::CursorStyle::Wait => CursorIcon::Wait,
        cursor::CursorStyle::Help => CursorIcon::Help,
    }
}
