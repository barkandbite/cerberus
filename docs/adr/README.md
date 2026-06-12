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
