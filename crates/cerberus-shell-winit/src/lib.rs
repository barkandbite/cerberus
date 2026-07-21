//! Windowing adapter (ADR-0004): run a [`FrameApp`] in a real OS window using
//! `winit` (window/fullscreen/input + event loop) and `softbuffer` (CPU
//! framebuffer presentation).
//!
//! This crate is the *only* place that names winit/softbuffer types; it
//! translates window events into `FrameApp` calls and blits the returned
//! `Framebuffer`. The same `FrameApp` runs headlessly in tests and in the
//! headless render mode — no display required there.
//!
//! Keyboard: Enter submits, Backspace deletes, F11 toggles fullscreen, Esc
//! leaves fullscreen; other text goes to the app's URL box.
//!
//! HiDPI: the OS reports the surface in physical pixels plus a scale factor
//! (e.g. 2.0 at 200%). We hand the [`FrameApp`] the scale and the physical size;
//! it lays out in logical pixels and paints *crisp* at physical resolution
//! (re-outlined glyphs, not a bitmap upscale), so `blit_scaled` is a 1:1 copy.
//! Pointer coordinates are divided by the scale back into the app's logical
//! space. (The multi-window mirror path still renders logical + upscales.)

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;

use cerberus_paint::Framebuffer;
use cerberus_shell::{FrameApp, MultiSurfaceApp, Waker};
use cerberus_types::Size;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

/// Logical pixels scrolled per wheel notch / arrow-key press.
const WHEEL_LINE_PX: f32 = 48.0;
const ARROW_STEP_PX: i32 = 48;

/// Convert a winit wheel `delta` into a logical-pixel vertical offset, positive
/// = scroll toward the end of the document. Line deltas (mouse wheels) are
/// notches; pixel deltas (trackpads) are physical pixels divided back to
/// logical. winit reports positive `y` for scrolling *up*, so we negate.
fn wheel_dy(delta: MouseScrollDelta, scale: f64) -> i32 {
    let logical = match delta {
        MouseScrollDelta::LineDelta(_, y) => -y * WHEEL_LINE_PX,
        MouseScrollDelta::PixelDelta(pos) => -(pos.y as f32) / (scale.max(1.0) as f32),
    };
    logical.round() as i32
}

/// Wraps a winit proxy so a worker thread can wake the event loop.
struct ProxyWaker(EventLoopProxy<()>);

impl Waker for ProxyWaker {
    fn wake(&self) {
        let _ = self.0.send_event(());
    }
}

/// Errors from running the windowed event loop.
#[derive(Debug)]
pub enum WinitError {
    /// The event loop could not be created or run.
    EventLoop(String),
    /// A window or drawing surface could not be created.
    Surface(String),
}

impl std::fmt::Display for WinitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WinitError::EventLoop(m) => write!(f, "event loop error: {m}"),
            WinitError::Surface(m) => write!(f, "surface error: {m}"),
        }
    }
}

impl std::error::Error for WinitError {}

type WinitSurface = softbuffer::Surface<Rc<Window>, Rc<Window>>;

/// Present a logical-pixel `frame` onto a physical-pixel surface `buffer`
/// (`0x00RRGGBB`) by nearest-neighbour upscaling. At scale 1.0 (`frame` already
/// the surface size) this is a straight copy. Nearest-neighbour keeps rectangle
/// and rule edges crisp; glyph edges soften a little above 1× — native hi-dpi
/// glyph rendering is a separate, later refinement.
fn blit_scaled(frame: &Framebuffer, buffer: &mut [u32], pw: u32, ph: u32) {
    let lw = (frame.size.w.max(1)) as usize;
    let lh = (frame.size.h.max(1)) as usize;
    let (pw, ph) = (pw as usize, ph as usize);
    for y in 0..ph {
        let sy = (y * lh / ph).min(lh - 1);
        let src_row = sy * lw;
        let dst_row = y * pw;
        for x in 0..pw {
            let sx = (x * lw / pw).min(lw - 1);
            let s = (src_row + sx) * 4;
            let px = &frame.rgba[s..s + 4];
            buffer[dst_row + x] = (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
        }
    }
}

/// Holds the running app plus its window and drawing surface.
struct State<A: FrameApp> {
    app: A,
    window: Option<Rc<Window>>,
    surface: Option<WinitSurface>,
    cursor: (f64, f64),
    fullscreen: bool,
    start_fullscreen: bool,
    error: Option<WinitError>,
}

impl<A: FrameApp> State<A> {
    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let (pw, ph) = (size.width.max(1), size.height.max(1));
        // Hand the app the scale factor; it lays out in logical pixels and paints
        // crisp at the physical size, so no upscaling of the result is needed.
        let scale = window.scale_factor().max(1.0);
        self.app.set_scale_factor(scale as f32);

        // Render before borrowing the surface (disjoint field borrows).
        let frame = self.app.render_frame(Size::new(pw, ph));

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let (Some(nw), Some(nh)) = (NonZeroU32::new(pw), NonZeroU32::new(ph)) else {
            return;
        };
        if surface.resize(nw, nh).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        // `frame` is already physical-sized; blit_scaled is a 1:1 copy here.
        blit_scaled(&frame, &mut buffer, pw, ph);
        let _ = buffer.present();
    }

    fn set_fullscreen(&mut self, on: bool) {
        self.fullscreen = on;
        if let Some(window) = &self.window {
            let mode = on.then(|| Fullscreen::Borderless(None));
            window.set_fullscreen(mode);
        }
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn handle_key(&mut self, event: KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let redraw = match &event.logical_key {
            Key::Named(NamedKey::Enter) => self.app.submit(),
            Key::Named(NamedKey::Backspace) => self.app.backspace(),
            Key::Named(NamedKey::F11) => {
                self.set_fullscreen(!self.fullscreen);
                true
            }
            Key::Named(NamedKey::Escape) if self.fullscreen => {
                self.set_fullscreen(false);
                true
            }
            Key::Named(NamedKey::F12) => self.app.dev_console_toggle(),
            Key::Named(NamedKey::ArrowDown) => self.app.scroll_by(ARROW_STEP_PX),
            Key::Named(NamedKey::ArrowUp) => self.app.scroll_by(-ARROW_STEP_PX),
            Key::Named(NamedKey::PageDown) => self.app.scroll_page(true),
            Key::Named(NamedKey::PageUp) => self.app.scroll_page(false),
            Key::Named(NamedKey::Home) => self.app.scroll_to_end(false),
            Key::Named(NamedKey::End) => self.app.scroll_to_end(true),
            _ => match event.text {
                Some(text) => text
                    .chars()
                    .fold(false, |acc, c| self.app.text_input(c) || acc),
                None => false,
            },
        };
        if redraw {
            self.request_redraw();
        }
    }
}

impl<A: FrameApp> ApplicationHandler for State<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes().with_title(self.app.title());
        if self.start_fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
            self.fullscreen = true;
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                self.error = Some(WinitError::Surface(e.to_string()));
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Context::new(window.clone())
            .and_then(|ctx| softbuffer::Surface::new(&ctx, window.clone()))
        {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(WinitError::Surface(e.to_string()));
                event_loop.exit();
                return;
            }
        };
        self.surface = Some(surface);
        self.window = Some(window);
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.request_redraw(),
            // A DPI change (moving to another monitor) repaints at the new scale.
            WindowEvent::ScaleFactorChanged { .. } => self.request_redraw(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed && button == MouseButton::Left {
                    // Map physical cursor coords to the logical space the app
                    // laid out and hit-tests in.
                    let scale = self
                        .window
                        .as_ref()
                        .map(|w| w.scale_factor())
                        .unwrap_or(1.0)
                        .max(1.0);
                    let (x, y) = (
                        (self.cursor.0 / scale) as i32,
                        (self.cursor.1 / scale) as i32,
                    );
                    if self.app.pointer_down(x, y) {
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scale = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor())
                    .unwrap_or(1.0);
                if self.app.scroll_by(wheel_dy(delta, scale)) {
                    self.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(event),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // A worker woke us; let the app drain its results and redraw if needed.
        if self.app.poll() {
            self.request_redraw();
        }
    }
}

/// Run `app` in a window until the user closes it. `fullscreen` starts the
/// window borderless-fullscreen (toggle later with F11). Requires a display
/// server; the headless path is used in CI/tests instead.
pub fn run(app: impl FrameApp + 'static, fullscreen: bool) -> Result<(), WinitError> {
    let event_loop = EventLoop::<()>::with_user_event()
        .build()
        .map_err(|e| WinitError::EventLoop(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut state = State {
        app,
        window: None,
        surface: None,
        cursor: (0.0, 0.0),
        fullscreen: false,
        start_fullscreen: fullscreen,
        error: None,
    };
    // Hand the app a waker so its network worker can wake the loop.
    state
        .app
        .set_waker(Arc::new(ProxyWaker(event_loop.create_proxy())));

    event_loop
        .run_app(&mut state)
        .map_err(|e| WinitError::EventLoop(e.to_string()))?;

    match state.error.take() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Multi-window: drive a `MultiSurfaceApp` across N OS windows (ADR-0017/0018).
// ---------------------------------------------------------------------------

/// One window + its drawing surface, tagged with the app's window index.
struct WindowSlot {
    window: Rc<Window>,
    surface: WinitSurface,
}

/// Drives a [`MultiSurfaceApp`] across N windows. Window slots are created in
/// app-index order, so `slots[idx]` is the surface for app window `idx`.
struct MultiState<A: MultiSurfaceApp> {
    app: A,
    slots: Vec<WindowSlot>,
    cursors: Vec<(f64, f64)>,
    error: Option<WinitError>,
}

impl<A: MultiSurfaceApp> MultiState<A> {
    fn idx_of(&self, id: WindowId) -> Option<usize> {
        self.slots.iter().position(|s| s.window.id() == id)
    }

    fn request_redraw(&self, idx: usize) {
        if let Some(slot) = self.slots.get(idx) {
            slot.window.request_redraw();
        }
    }

    fn redraw(&mut self, idx: usize) {
        let Some(slot) = self.slots.get(idx) else {
            return;
        };
        let window = slot.window.clone();
        let size = window.inner_size();
        let (pw, ph) = (size.width.max(1), size.height.max(1));
        let scale = window.scale_factor().max(1.0);
        let lw = ((pw as f64 / scale).round() as u32).max(1);
        let lh = ((ph as f64 / scale).round() as u32).max(1);

        // Render before borrowing the surface (disjoint field borrows).
        let frame = self.app.render(idx, Size::new(lw, lh));

        let Some(slot) = self.slots.get_mut(idx) else {
            return;
        };
        let (Some(nw), Some(nh)) = (NonZeroU32::new(pw), NonZeroU32::new(ph)) else {
            return;
        };
        if slot.surface.resize(nw, nh).is_err() {
            return;
        }
        let Ok(mut buffer) = slot.surface.buffer_mut() else {
            return;
        };
        blit_scaled(&frame, &mut buffer, pw, ph);
        let _ = buffer.present();
    }
}

impl<A: MultiSurfaceApp> ApplicationHandler for MultiState<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.slots.is_empty() {
            return;
        }
        let count = self.app.window_count();
        for idx in 0..count {
            let attrs = Window::default_attributes().with_title(self.app.title(idx));
            let window = match event_loop.create_window(attrs) {
                Ok(w) => Rc::new(w),
                Err(e) => {
                    self.error = Some(WinitError::Surface(e.to_string()));
                    event_loop.exit();
                    return;
                }
            };
            let surface = match softbuffer::Context::new(window.clone())
                .and_then(|ctx| softbuffer::Surface::new(&ctx, window.clone()))
            {
                Ok(s) => s,
                Err(e) => {
                    self.error = Some(WinitError::Surface(e.to_string()));
                    event_loop.exit();
                    return;
                }
            };
            self.slots.push(WindowSlot { window, surface });
        }
        self.cursors = vec![(0.0, 0.0); self.slots.len()];
        for idx in 0..self.slots.len() {
            self.request_redraw(idx);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(idx) = self.idx_of(id) else {
            return;
        };
        match event {
            // Closing any window tears the whole mirror group down.
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.request_redraw(idx),
            WindowEvent::ScaleFactorChanged { .. } => self.request_redraw(idx),
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(c) = self.cursors.get_mut(idx) {
                    *c = (position.x, position.y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed && button == MouseButton::Left {
                    let scale = self
                        .slots
                        .get(idx)
                        .map(|s| s.window.scale_factor())
                        .unwrap_or(1.0)
                        .max(1.0);
                    let (x, y) = self.cursors.get(idx).copied().unwrap_or((0.0, 0.0));
                    let (x, y) = ((x / scale) as i32, (y / scale) as i32);
                    for w in self.app.pointer_down(idx, x, y) {
                        self.request_redraw(w);
                    }
                }
            }
            // Raising a follower window is its chance to catch up to the master.
            WindowEvent::Focused(true) => {
                for w in self.app.focus(idx) {
                    self.request_redraw(w);
                }
            }
            // Hidden/minimized: let the app release that instance's memory.
            WindowEvent::Occluded(true) => self.app.surface_hidden(idx),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Some(text) = event.text {
                        for c in text.chars() {
                            for w in self.app.text_input(idx, c) {
                                self.request_redraw(w);
                            }
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => self.redraw(idx),
            _ => {}
        }
    }
}

/// Run a [`MultiSurfaceApp`] in N OS windows until one is closed. Requires a
/// display server; the multi-surface driving logic itself is exercised
/// headlessly in `cerberus-app`'s `MirrorShell` tests.
pub fn run_multi(app: impl MultiSurfaceApp + 'static) -> Result<(), WinitError> {
    let event_loop = EventLoop::new().map_err(|e| WinitError::EventLoop(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut state = MultiState {
        app,
        slots: Vec::new(),
        cursors: Vec::new(),
        error: None,
    };
    event_loop
        .run_app(&mut state)
        .map_err(|e| WinitError::EventLoop(e.to_string()))?;

    match state.error.take() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_scaled_doubles_pixels_at_2x() {
        // 1×2 logical frame (red over green) upscaled to a 2×4 physical surface.
        let mut frame = Framebuffer::new(Size::new(1, 2));
        frame.rgba = vec![
            255, 0, 0, 255, /* red */ 0, 255, 0, 255, /* green */
        ];
        let mut buf = vec![0u32; 2 * 4];
        blit_scaled(&frame, &mut buf, 2, 4);
        // Physical rows 0,1 ← logical row 0 (red); rows 2,3 ← logical row 1 (green).
        assert_eq!(buf[0], 0x00FF_0000);
        assert_eq!(buf[1], 0x00FF_0000);
        assert_eq!(buf[2 * 2], 0x0000_FF00);
        assert_eq!(buf[2 * 3 + 1], 0x0000_FF00);
    }

    #[test]
    fn blit_scaled_is_identity_at_1x() {
        let mut frame = Framebuffer::new(Size::new(2, 1));
        frame.rgba = vec![1, 2, 3, 255, 4, 5, 6, 255];
        let mut buf = vec![0u32; 2];
        blit_scaled(&frame, &mut buf, 2, 1);
        assert_eq!(buf[0], (1 << 16) | (2 << 8) | 3);
        assert_eq!(buf[1], (4 << 16) | (5 << 8) | 6);
    }
}
