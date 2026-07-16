//! The platform surface seam.
//!
//! `PlatformSurface` abstracts "somewhere to present a framebuffer". The
//! scaffold ships only `HeadlessSurface` (captures frames in memory), which is
//! all the M0 trivial render and CI need and which also serves the headless
//! rendering mode (M8).
//!
//! A real windowed surface (e.g. via a `winit` + `softbuffer` adapter) is a
//! future adapter behind this same trait. That windowing dependency is **not
//! yet approved** — it needs its own ADR (see PLAN.md "Open decisions"); until
//! then we deliberately do not pull a GUI stack into the tree.

use cerberus_paint::Framebuffer;
use cerberus_types::Size;

/// Errors from a platform surface.
#[derive(Clone, Debug)]
pub enum ShellError {
    /// Presenting the frame failed.
    Present(String),
}

/// Somewhere a rendered frame can be presented (a window, or a headless
/// capture). Callers depend only on this trait, never on a windowing library.
pub trait PlatformSurface {
    /// The surface size in device pixels.
    fn size(&self) -> Size;

    /// Present a frame. The framebuffer's size should match [`size`](Self::size).
    fn present(&mut self, frame: &Framebuffer) -> Result<(), ShellError>;
}

/// A surface that keeps the most recently presented frame in memory. Used for
/// the headless render path, tests, and the memory gate.
#[derive(Debug)]
pub struct HeadlessSurface {
    size: Size,
    last: Option<Framebuffer>,
}

impl HeadlessSurface {
    /// Create a headless surface of the given size.
    pub fn new(size: Size) -> Self {
        Self { size, last: None }
    }

    /// The most recently presented frame, if any.
    pub fn last_frame(&self) -> Option<&Framebuffer> {
        self.last.as_ref()
    }
}

impl PlatformSurface for HeadlessSurface {
    fn size(&self) -> Size {
        self.size
    }

    fn present(&mut self, frame: &Framebuffer) -> Result<(), ShellError> {
        self.last = Some(frame.clone());
        Ok(())
    }
}

/// Wakes the platform event loop from another thread (e.g. a network worker) so
/// the app's [`FrameApp::poll`] runs promptly, without busy-waiting.
pub trait Waker: Send + Sync {
    /// Request that the event loop wake and poll the app.
    fn wake(&self);
}

/// An interactive application the platform layer drives.
///
/// It renders a frame for a given size and reacts to input. The browser
/// implements this; the windowing adapter (`cerberus-shell-winit`) calls it from
/// the event loop, and tests can drive it headlessly. Each input method returns
/// whether a redraw is needed. No windowing type ever appears here.
pub trait FrameApp {
    /// Window title.
    fn title(&self) -> String;

    /// Receive a waker the app can hand to background workers. Default: ignored.
    fn set_waker(&mut self, _waker: std::sync::Arc<dyn Waker>) {}

    /// Tell the app the display's HiDPI scale factor (physical / logical pixels,
    /// e.g. 2.0 at 200%). The app lays out in logical pixels and scales its paint
    /// up so the surface renders crisp. Default: ignored (scale 1.0).
    fn set_scale_factor(&mut self, _scale: f32) {}

    /// Advance background work (e.g. drain a network worker) when the loop is
    /// woken; return true if a redraw is needed. Default: nothing to do.
    fn poll(&mut self) -> bool {
        false
    }

    /// Render a frame at the given size.
    fn render_frame(&mut self, size: Size) -> Framebuffer;

    /// Pointer press at device coordinates.
    fn pointer_down(&mut self, x: i32, y: i32) -> bool;

    /// A typed character.
    fn text_input(&mut self, c: char) -> bool;

    /// Enter / confirm (e.g. submit the URL box).
    fn submit(&mut self) -> bool;

    /// Backspace.
    fn backspace(&mut self) -> bool;

    /// Scroll the page content vertically by `dy` logical pixels (positive =
    /// down / toward the end of the document). Wheel notches and arrow keys map
    /// here. Returns true if the offset changed and a redraw is needed. Default:
    /// no scrolling.
    fn scroll_by(&mut self, dy: i32) -> bool {
        let _ = dy;
        false
    }

    /// Scroll by whole viewport pages (`down` = toward the end), the Page
    /// Down/Up and Space keys. Returns true if a redraw is needed. Default:
    /// no scrolling.
    fn scroll_page(&mut self, down: bool) -> bool {
        let _ = down;
        false
    }

    /// Jump to the top (`end == false`) or bottom (`end == true`) of the page —
    /// the Home/End keys. Returns true if a redraw is needed. Default: no-op.
    fn scroll_to_end(&mut self, end: bool) -> bool {
        let _ = end;
        false
    }
}

/// An application that drives **multiple** surfaces (windows) at once — the
/// shape `cerberus-shell-winit::run_multi` renders. Window `0` is the master the
/// user drives; the rest mirror it and catch up when focused. Each input method
/// returns the window indices needing a redraw — driving the master leaves
/// followers to catch up lazily on [`focus`](MultiSurfaceApp::focus).
pub trait MultiSurfaceApp {
    /// How many windows to open.
    fn window_count(&self) -> usize;

    /// Title for window `idx`.
    fn title(&self, idx: usize) -> String;

    /// Render window `idx` at `size`.
    fn render(&mut self, idx: usize, size: Size) -> Framebuffer;

    /// Pointer press at device coordinates in window `idx`. Returns the windows
    /// that need redrawing.
    fn pointer_down(&mut self, idx: usize, x: i32, y: i32) -> Vec<usize>;

    /// A typed character into window `idx`. Returns the windows to redraw.
    fn text_input(&mut self, idx: usize, c: char) -> Vec<usize>;

    /// Window `idx` was raised/focused — a chance to catch it up. Returns the
    /// windows to redraw.
    fn focus(&mut self, idx: usize) -> Vec<usize>;

    /// Window `idx` was hidden/occluded/minimized — a chance to release its
    /// resident memory (it re-materializes when shown again). Default: no-op.
    fn surface_hidden(&mut self, idx: usize) {
        let _ = idx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cerberus_types::Color;

    #[test]
    fn headless_surface_captures_frames() {
        let size = Size::new(8, 8);
        let mut surface = HeadlessSurface::new(size);
        assert!(surface.last_frame().is_none());

        let mut fb = Framebuffer::new(size);
        fb.clear(Color::WHITE);
        surface.present(&fb).unwrap();

        assert_eq!(
            surface.last_frame().unwrap().pixel(0, 0),
            Some(Color::WHITE)
        );
    }
}
