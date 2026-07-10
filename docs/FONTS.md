# Fonts — rendering faces and the enumeration surface

Cerberus treats fonts as two separate concerns, deliberately kept apart:

1. **Rendering faces** — the *bytes* we actually rasterize with. A small,
   fixed, bundled set (never system fonts), chosen to be *metric-compatible*
   with the fonts a real browser renders, so pages get the right shapes and
   line-wrap points. This is a fidelity concern.
2. **The enumeration surface** — the *names* a page can discover as "installed."
   A large catalog we **never download or render**, from which each browser head
   reports a random subset. This is a privacy (anti-fingerprinting) concern.

The two never mix: a name in the enumeration catalog does not imply a bundled
face, and the bundled faces are reported under their generic class, not their
real names.

## 1. Rendering faces (bundled, metric-compatible)

`cerberus-text` maps the five CSS generic families to bundled faces. The named
`font-family` a page requests is resolved to its generic class in the cascade
(`cerberus-css::classify_font_family`) — e.g. `Georgia, serif` → serif,
`Consolas, monospace` → monospace — and that generic selects the face. The
literal named font is never shipped or read (a privacy property, ADR-0005), but
because the bundled face is metric-compatible, widths and shapes match closely.

| `font-family`                | Bundled face          | Metric-compatible with | License      |
|------------------------------|-----------------------|------------------------|--------------|
| `sans-serif` (generic, default) | Roboto             | (reference default)    | Apache-2.0   |
| `Arial` / `Helvetica`        | Liberation Sans       | Arial                  | SIL OFL 1.1  |
| `serif`                      | Liberation Serif      | Times New Roman        | SIL OFL 1.1  |
| `monospace`                  | Liberation Mono       | Courier New            | SIL OFL 1.1  |
| `cursive`                    | → Liberation Serif    | (no script face yet)   | —            |
| `fantasy`                    | → Roboto              | (no display face yet)  | —            |

Notes:

- **Liberation** (Red Hat, SIL OFL 1.1) is glyph-width-identical to Arial /
  Times New Roman / Courier New, so a page laid out for those metrics wraps at
  the same points. Chrome OS uses the equivalent Croscore family
  (Arimo/Tinos/Cousine) for exactly this reason.
- **Two sans faces, selected by what the page names.** The generic `sans-serif`
  (and non-Arial sans names) render in **Roboto** — measured to match the
  reference browser's default sans best (flipping the generic default to
  Liberation Sans regressed *every* sans page on the corpus). A page that names
  **Arial/Helvetica** specifically renders in **Liberation Sans** (Arial metrics)
  — what a real Chrome-on-Windows box shows for those, distinct from the generic
  default. Roboto also remains the browser's own UI/chrome face.
- **Monospace-size quirk:** Chrome resolves an unspecified (`medium`) font-size
  to **13px** for the monospace generic and 16px otherwise, so `<pre>`/`<code>`
  render smaller than surrounding text. The cascade reproduces this
  (`font_size_medium`), which is what makes an RFC (`rfc1`) match Chrome's line
  count and rhythm rather than rendering ~23% too large.
- **Cursive/fantasy** are unmapped for now (no bundled script/display face);
  they fall back to serif/sans. Adding `cursive` (e.g. a libre "Comic Neue" or a
  handwriting face) and `fantasy` later is data + one match arm.

Candidate faces for the unmapped generics and future swaps, if we bundle more
(all libre, redistributable):

- **cursive/script:** Comic Neue (OFL), Dancing Script (OFL), Caveat (OFL).
- **fantasy/display:** a heavy display face — e.g. a libre Impact-alike, or
  Bungee / Lobster (OFL).
- **sans (Arial-metric):** Liberation Sans / Arimo (metric-compatible), if we
  choose Chrome-Linux parity over Roboto.

## 2. Enumeration surface (the farbling font catalog)

`cerberus-profile::fonts` holds a categorized catalog of ~350 font **names** and
`derive_fonts(seed, os)` builds each head's presented set: the OS core (always,
for platform coherence) plus a per-head-random sample of the optional pools
(Office, Adobe, common webfonts) coherent with the OS. A Windows head reports
~90–100 families; the three heads report different sets, so no stable cross-site
font-fingerprint forms, while any one head answers consistently within itself.

Pools:

- `WINDOWS_CORE`, `MACOS_CORE`, `LINUX_CORE` — the stock OS fonts, always
  present on a matching persona.
- `OFFICE_OPTIONAL` — the classic Microsoft Office font set (Agency FB, Algerian,
  Bell MT, … the ~80 faces Word/Office installs).
- `ADOBE_OPTIONAL` — Creative Cloud faces (Source/Kozuka families, Minion/Myriad
  Pro, the Adobe *Std* faces) — a "creative professional" tell, sampled sparsely.
- `WEB_OPTIONAL` — popular Google-Fonts/webfonts users install locally (Roboto,
  Open Sans, Fira Code, JetBrains Mono, …).

These names are **never downloaded**. Two probe vectors answer from the same
per-head list, consistently (`cerberus-farbling` + the `cerberus-js-dom`
prelude):

- `document.fonts.check("12px 'Name'")` → true only for a generic or a listed
  name.
- `measureText` width comparison → an installed family gets a stable per-head
  advance; a non-installed family measures identically to the generic fallback,
  so it reads as absent.
