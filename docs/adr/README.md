# Architecture Decision Records

ADRs capture significant, hard-to-reverse decisions and the context behind them.
They are short, numbered, append-only (supersede rather than rewrite), and live
with the code so the reasoning travels with the repo.

## Index

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-architecture-and-trait-boundaries.md) | Architecture & module/trait boundaries | Accepted |
| [0002](0002-js-engine.md) | JavaScript engine choice (QuickJS; V8 documented swap-in) | Accepted |
| [0003](0003-dependency-policy.md) | Dependency policy & approved list (incl. vault crates) | Accepted |
| [0004](0004-windowing.md) | Windowing & presentation (winit + softbuffer) | Accepted |
| [0005](0005-rendering-stack.md) | Rendering stack (shaping, raster, image decode) | Accepted |
| [0006](0006-networking.md) | M1 networking — HTTP/1.1, TLS (rustls), DoH (Quad9) | Accepted |
| [0007](0007-css-engine.md) | CSS engine + speed-first "raw render" (ignore delays) | Accepted |
| [0008](0008-page-scripts-dom-bridge.md) | Page-script execution via a snapshot/replay DOM bridge | Accepted |
| [0009](0009-svg-rasterization.md) | SVG image rasterization (resvg / usvg / tiny-skia) | Accepted |
| [0010](0010-vault-format-and-profile-layout.md) | Vault on-disk format & persistent-profile layout | Accepted |
| [0011](0011-cookie-dispositions-and-timing-hud.md) | Per-cookie dispositions & the Rust-side timing HUD | Accepted |
| [0012](0012-persistent-realm-and-incremental-sync.md) | Persistent JS realm & incremental DOM sync (evolves 0008) | Accepted |
| [0013](0013-bounded-event-loop.md) | Bounded virtual-clock event loop (evolves 0002 speed-first) | Accepted |
| [0014](0014-fetch-over-eval-seam.md) | `fetch` over the eval-only seam (bounded host-drained I/O) | Accepted |
| [0015](0015-windows-port.md) | Windows port (baseline) + `cerberus-sysmem` RSS adapter | Accepted |
| [0016](0016-content-addressed-cache-interning.md) | Content-addressed cache body interning (shared memory) | Accepted |
| [0017](0017-concurrent-multi-window-mirror-groups.md) | Concurrent multi-window mirror groups | Accepted |
| [0018](0018-mirror-group-app-integration.md) | Mirror-group app integration | Accepted |
| [0019](0019-css-selectors-media-visibility-opacity.md) | CSS selectors, `@media`, visibility/opacity | Accepted |
| [0020](0020-gzip-deflate-decompression.md) | gzip/deflate response decompression | Accepted |
| [0021](0021-layout-measurement-bridge.md) | Layout-measurement JS bridge (getBoundingClientRect, …) | Accepted |
| [0022](0022-autofill.md) | Autofill engine (field detection + fill plan) | Accepted |
| [0023](0023-flexbox-grid.md) | Flexbox + Grid v1 | Accepted |
| [0024](0024-autofill-app-integration.md) | Autofill app integration (per-identity fill across a mirror) | Accepted |
| [0025](0025-multi-identity-ux.md) | Multi-identity UX — drivable mirror typing + driven badge | Accepted |
| [0026](0026-mirror-layout-efficiency.md) | Mirror & layout efficiency — scratch reuse, refocus skip, large-N gate | Accepted |

## When to write one

- Adding (or swapping) a third-party dependency — **required** before the crate
  enters the tree (see ADR-0003).
- Changing a trait boundary or the crate topology.
- Any decision a future maintainer would otherwise have to reverse-engineer.

## Status values

`Proposed` → `Accepted` → (later) `Superseded by ADR-XXXX` / `Deprecated`.

## Template

```markdown
# ADR-XXXX: <title>

- Status: Proposed
- Date: YYYY-MM-DD
- Deciders: <names>

## Context
What's the situation and the forces at play?

## Decision
What we will do.

## Consequences
What becomes easier/harder. Trade-offs accepted.

## Alternatives considered
What else, and why not.
```
