//! Off-screen windows ([`gpui::App::open_offscreen_window`]): a [`PlatformWindow`] with no
//! display-server surface. It renders with the wgpu headless renderer into an owned texture,
//! reads each frame back and hands the pixels to a callback. It never talks to the display
//! server, so it works the same under Wayland, X11 and the headless platform. Frames are
//! demand-driven: GPUI wakes the window when it becomes dirty, and the window asks for a
//! frame from the foreground executor, at most once per `min_frame_interval`.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    BackgroundExecutor, Bounds, Capslock, DevicePixels, DispatchEventResult, ForegroundExecutor,
    GpuSpecs, Modifiers, OffscreenFrame, OffscreenFrameCallback, OffscreenParams, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, PromptLevel, RequestFrameOptions, Scene, Size, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams, WindowVisibility,
};
use gpui_wgpu::{HeadlessRendererOptions, WgpuHeadlessRenderer};

type RequestFrameCallback = Box<dyn FnMut(RequestFrameOptions)>;

struct OffscreenWindowState {
    size: Size<Pixels>,
    scale_factor: f32,
    appearance: WindowAppearance,
    renderer: WgpuHeadlessRenderer,
    on_frame: OffscreenFrameCallback,
    input_handler: Option<PlatformInputHandler>,
    resize_callback: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    title: String,
    /// Quiver: frames are packed side by side for two eyes, so the image is twice as wide.
    stereo: bool,
}

/// What the frame scheduler shares with the window.
struct FrameSource {
    request_frame: RefCell<Option<RequestFrameCallback>>,
    /// A frame request is queued on the foreground executor.
    pending: Cell<bool>,
    last_frame: Cell<Option<Instant>>,
    min_frame_interval: Option<Duration>,
    foreground: ForegroundExecutor,
    background: BackgroundExecutor,
}

impl FrameSource {
    /// Queues one frame request, unless one is already queued.
    fn schedule(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let delay = match (self.min_frame_interval, self.last_frame.get()) {
            (Some(interval), Some(last)) => (last + interval).checked_duration_since(Instant::now()),
            _ => None,
        };
        let this: Weak<Self> = Rc::downgrade(self);
        let background = self.background.clone();
        self.foreground
            .spawn(async move {
                if let Some(delay) = delay {
                    background.timer(delay).await;
                }
                let Some(this) = this.upgrade() else {
                    return;
                };
                // Cleared first: a wake while the frame runs (still dirty) queues the next one.
                this.pending.set(false);
                // Taken out so the callback can re-enter the window freely.
                let callback = this.request_frame.borrow_mut().take();
                if let Some(mut callback) = callback {
                    callback(RequestFrameOptions::default());
                    let mut slot = this.request_frame.borrow_mut();
                    if slot.is_none() {
                        *slot = Some(callback);
                    }
                }
            })
            .detach();
    }
}

pub(crate) struct OffscreenWindow {
    state: Rc<RefCell<OffscreenWindowState>>,
    frames: Rc<FrameSource>,
}

fn device_size(size: Size<Pixels>, scale_factor: f32) -> Size<DevicePixels> {
    Size {
        width: DevicePixels(((f32::from(size.width) * scale_factor).ceil() as i32).max(1)),
        height: DevicePixels(((f32::from(size.height) * scale_factor).ceil() as i32).max(1)),
    }
}

impl OffscreenWindow {
    pub(crate) fn new(
        params: WindowParams,
        foreground: ForegroundExecutor,
        background: BackgroundExecutor,
    ) -> anyhow::Result<Self> {
        let OffscreenParams { options, on_frame } = params
            .offscreen
            .ok_or_else(|| anyhow::anyhow!("not an off-screen window"))?;
        let renderer = WgpuHeadlessRenderer::with_options(HeadlessRendererOptions {
            transparent: true,
            prefer_low_power_gpu: options.prefer_low_power_gpu,
        })?;
        let state = Rc::new(RefCell::new(OffscreenWindowState {
            size: options.size,
            scale_factor: options.scale_factor,
            appearance: options.appearance,
            renderer,
            on_frame,
            input_handler: None,
            resize_callback: None,
            title: String::new(),
            stereo: false,
        }));
        let frames = Rc::new(FrameSource {
            request_frame: RefCell::new(None),
            pending: Cell::new(false),
            last_frame: Cell::new(None),
            min_frame_interval: options.min_frame_interval,
            foreground,
            background,
        });
        Ok(Self { state, frames })
    }
}

impl raw_window_handle::HasWindowHandle for OffscreenWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl raw_window_handle::HasDisplayHandle for OffscreenWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl PlatformWindow for OffscreenWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: Point::default(),
            size: self.state.borrow().size,
        }
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.state.borrow().size
    }

    fn resize(&mut self, size: Size<Pixels>) {
        // The new size applies to the next frame at once. GPUI's resize handling runs later,
        // on the foreground executor: `Window::resize` is called from inside an update of
        // this very window, where GPUI can't re-enter it (callers that render right away call
        // `Window::bounds_changed` themselves first).
        let changed = {
            let mut state = self.state.borrow_mut();
            let changed = state.size != size;
            state.size = size;
            changed
        };
        if !changed {
            return;
        }
        let state = Rc::downgrade(&self.state);
        self.frames
            .foreground
            .spawn(async move {
                let Some(state) = state.upgrade() else {
                    return;
                };
                let (size, scale_factor, callback) = {
                    let mut s = state.borrow_mut();
                    (s.size, s.scale_factor, s.resize_callback.take())
                };
                if let Some(mut callback) = callback {
                    callback(size, scale_factor);
                    let mut s = state.borrow_mut();
                    if s.resize_callback.is_none() {
                        s.resize_callback = Some(callback);
                    }
                }
            })
            .detach();
    }

    fn scale_factor(&self) -> f32 {
        self.state.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        self.state.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        None
    }

    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        false
    }

    fn visibility(&self) -> WindowVisibility {
        // Every frame drawn is delivered to the frame callback.
        WindowVisibility::Visible
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Transparent
    }

    fn set_title(&mut self, title: &str) {
        self.state.borrow_mut().title = title.to_owned();
    }

    fn get_title(&self) -> String {
        self.state.borrow().title.clone()
    }

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        false
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        let frames = Rc::downgrade(&self.frames);
        Some(Rc::new(move || {
            if let Some(frames) = frames.upgrade() {
                frames.schedule();
            }
        }))
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.frames.request_frame.borrow_mut() = Some(callback);
    }

    fn schedule_frame(&self) {
        self.frames.schedule();
    }

    fn on_input(&self, _callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {}

    fn on_active_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_visibility_change(&self, _callback: Box<dyn FnMut(WindowVisibility)>) {}

    fn on_hover_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.state.borrow_mut().resize_callback = Some(callback);
    }

    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}

    fn on_display_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn on_should_close(&self, _callback: Box<dyn FnMut() -> bool>) {}

    fn on_close(&self, _callback: Box<dyn FnOnce()>) {}

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    fn draw(&self, scene: &Scene) {
        self.frames.last_frame.set(Some(Instant::now()));
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        let scale_factor = state.scale_factor;
        let mut size = device_size(state.size, scale_factor);
        if state.stereo {
            // Each eye is the window's size, rounded as `Window::present` rounds it.
            let eye = (f32::from(state.size.width) * scale_factor).round() as i32;
            size.width = DevicePixels((eye * 2).max(2));
        }
        let on_frame = &mut state.on_frame;
        let started = Instant::now();
        let result = state.renderer.render_and_read(scene, size, |frame| {
            on_frame(&OffscreenFrame {
                width: frame.width,
                height: frame.height,
                stride: frame.stride,
                scale_factor,
                render_duration: started.elapsed(),
                data: frame.data,
            })
        });
        if let Err(error) = result {
            log::error!("off-screen window: {error:#}");
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.state.borrow().renderer.atlas()
    }

    fn set_stereo_side_by_side(&self, enabled: bool) -> bool {
        self.state.borrow_mut().stereo = enabled;
        true
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        // Subpixel coverage can't be stored in a single alpha channel.
        false
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        Some(self.state.borrow().renderer.gpu_specs())
    }
}
