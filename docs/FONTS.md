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

| `font-family`                | Bundled face          | Why (measured)                          | License      |
|------------------------------|-----------------------|-----------------------------------------|--------------|
| `serif` (generic, **standard/default**) | Liberation Serif | Chrome's serif pref is Times New Roman → fontconfig metric alias | SIL OFL 1.1 |
| `sans-serif` (generic)       | Liberation Sans       | Chrome's sans pref is Arial → alias      | SIL OFL 1.1  |
| `Arial` / `Helvetica` (named)| Liberation Sans       | fontconfig metric alias                  | SIL OFL 1.1  |
| `Times New Roman` (named)    | Liberation Serif      | fontconfig metric alias                  | SIL OFL 1.1  |
| `monospace` (generic)        | DejaVu Sans Mono      | Chrome's fixed pref resolves here        | Bitstream Vera |
| `Courier New` (named)        | Liberation Mono       | fontconfig metric alias                  | SIL OFL 1.1  |
| `system-ui`                  | DejaVu Sans           | the reference's system font              | Bitstream Vera |
| `cursive` / `fantasy`        | → Liberation Serif    | prefs uninstalled → standard-font fallback | —          |
| *any other named face*       | *(falls through)*     | uninstalled names skip to the stack's next entry; a wholly unresolvable stack → standard serif | — |

Notes:

- **Every mapping above is measured, not assumed**: a 100px `H`-run calibration
  page rendered in the reference Chrome and in Cerberus produces identical ink
  extents per family. Two traps the measurements caught: `fc-match` on a bare
  generic reports DejaVu, but Chrome asks for its *preference font* (Times/
  Arial) and gets the Liberation alias instead; and uninstalled named faces
  (Verdana, Georgia, Menlo, Roboto, Segoe UI…) do **not** take fontconfig's
  weak best-match — Chrome skips them, so the *stack* decides
  (`Verdana, Geneva, sans-serif` renders as the generic sans).
- **Liberation** (Red Hat, SIL OFL 1.1) is glyph-width-identical to Arial /
  Times New Roman / Courier New, so a page laid out for those metrics wraps at
  the same points. Chrome OS uses the equivalent Croscore family
  (Arimo/Tinos/Cousine) for exactly this reason.
- **Scaling is `px / units_per_em`** (the CSS convention) for both advances and
  rasterization, and `line-height: normal` derives from each face's real
  vertical metrics — both verified against the calibration page.
- Roboto remains the browser's own UI/chrome face (and serves a page that
  names Roboto only if it ends up the resolved stack entry — on this persona it
  is uninstalled, so it falls through like any other name).
- **Monospace-size quirk:** Chrome resolves an unspecified (`medium`) font-size
  to **13px** for the monospace generic and 16px otherwise, so `<pre>`/`<code>`
  render smaller than surrounding text. The cascade reproduces this
  (`font_size_medium`), which is what makes an RFC (`rfc1`) match Chrome's line
  count and rhythm rather than rendering ~23% too large.
- **Cursive/fantasy** fall back to the standard serif — matching the reference,
  whose Comic Sans/Impact preferences are uninstalled there. Bundling a libre
  script/display face later is data + one match arm.

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
