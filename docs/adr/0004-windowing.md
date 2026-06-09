# ADR-0004: Windowing & presentation (winit + softbuffer)

- Status: Accepted
- Date: 2026-06-09
- Deciders: bbarker@barkbite.org (approved), engineering

## Context

We need to present rendered frames in a real OS window with **windowed** and
**fullscreen** modes, while keeping the existing **headless** path. The
`PlatformSurface` trait (`cerberus-shell`) already abstracts "present a
framebuffer"; we need a concrete adapter. Memory is priority #1 and our
rasterizer is CPU-side, so we do not want to drag in a GPU stack.

## Decision

Approve **winit** (cross-platform window creation, fullscreen, input + event
loop) and **softbuffer** (CPU framebuffer presentation — blit our RGBA buffer to
the window) behind `PlatformSurface`, in a new adapter crate
**`cerberus-shell-winit`**.

- winit drives the event loop and yields input that we translate into our own
  event/`ToolbarAction` types; **no winit type crosses** into the UI/browser
  layers.
- softbuffer presents our `Framebuffer` directly — no GPU device/queue, minimal
  memory.
- Fullscreen via winit borderless fullscreen (toggle, e.g. F11); windowed is the
  default; resizing re-lays out the toolbar + page.
- `HeadlessSurface` stays the surface for tests, CI, and the headless render mode
  — the *same* render→present path, no display required.

## Consequences

- **Easier:** real windowed/fullscreen with a small, idiomatic, permissively
  licensed (Apache-2.0/MIT) stack; CPU-only keeps RSS down.
- **Costs:** winit pulls platform/windowing transitive crates; a running display
  is required to actually open a window, so in headless CI the windowed path is
  **compile-checked** while the headless path is exercised. winit/softbuffer use
  `unsafe` internally (platform FFI); our adapter contains any `unsafe` to the
  `raw-window-handle` boundary and documents it (the workspace `unsafe_code =
  "deny"` is relaxed only in that crate).
- **Memory:** windowed RSS adds winit/softbuffer overhead; fold into the
  windowing milestone's budget recalibration.

## Alternatives considered

- **winit + wgpu (GPU):** heavier RAM/binary; unnecessary for a CPU rasterizer.
- **SDL2:** a single C dependency for window+input+present; less idiomatic, a
  foreign build dependency.
- **egui / iced (full GUI toolkits):** we paint our own minimal toolbar, so a
  toolkit is redundant weight and would constrain the look.
