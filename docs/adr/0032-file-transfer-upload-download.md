# ADR-0032: File transfer — multipart form upload + downloads

- Status: Accepted
- Date: 2026-06-19
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

With GET/POST forms working (ADR-0031), the remaining form-encoding gap was
**`multipart/form-data`** — the only encoding that carries **file uploads**. The
mirror image of upload is **download**: a response that should be saved to disk
rather than parsed as a page (today a PDF/zip navigation is mis-parsed as HTML).
Both are core "use the web" capabilities and neither needs a new dependency.

## Decision

### Multipart form upload
- `submit_from` recognizes `enctype="multipart/form-data"` on a **POST** form
  (HTML ignores enctype for GET) and builds a multipart body via `build_multipart`
  instead of urlencoding. It POSTs through the same proven path as ADR-0031
  (`PostBody` → `fetch_in`), only the content type + body differ.
- `build_multipart` emits a text part per successful control and a **file part**
  per `<input type=file>`. A random boundary (`----CerberusFormBoundary<hex>`)
  avoids collision; part headers escape `"`/CR/LF; the part `Content-Type` is
  guessed from the file extension.
- **File selection (v1): the file input's value is a filesystem path** — typed
  into the field (a file input already renders as a text box) or, crucially, set
  **programmatically by the mirror driver** so each identity uploads its own
  file. The bytes are read at submit time; an empty/unreadable path still sends
  the part (blank), so the server sees the field. A **native file-picker dialog**
  is a deliberate follow-up (it needs platform code or a heavy dep + its own ADR);
  the path approach is also exactly what the automation use case wants.

### Downloads
- In `handle_page`, `download_target(headers, url)` decides if a response is a
  download: **`Content-Disposition: attachment`**, or a **non-renderable content
  type** (anything but HTML/XHTML/XML/plain text; an absent type stays
  renderable so a misconfigured page isn't force-saved).
- A download is written to the **downloads directory** (`<data-dir>/downloads`,
  else `~/Downloads`, else a temp dir) under a **unique, sanitized** name
  (path components stripped → no escaping the dir; `name (1)` on collision), and a
  "Download complete" page is shown. Downloads are never cached or parsed.

## Consequences

- **Easier:** file-upload forms submit correctly (verified: `build_multipart`
  emits well-formed text+file parts with the file bytes inlined), and binary
  navigations save instead of rendering garbage (verified end-to-end via the
  load path to a temp downloads dir). Both reuse existing seams (POST transport,
  the GET page path), so the surface added is small.
- **Costs / follow-ups:**
  - No native file-picker yet — GUI users type a path; automation sets it.
  - Content-type-based download is a heuristic; a server mislabeling HTML as
    `octet-stream` would be saved (matches other browsers, which then sniff).
  - A cached GET that is actually a download isn't re-detected from cache (only
    fresh loads), and Back to a download URL re-downloads (our model navigates;
    real browsers download out-of-band) — both minor, bounded.
  - Large uploads/downloads run on the existing bounded-body path; streaming to/
    from disk is a later memory refinement.

## Alternatives considered

- **Native OS file dialog now:** rejected for v1 — needs `rfd` (heavy GTK/
  platform tree) or hand-rolled unsafe per-OS code; the path approach is
  dependency-free and fits automation. Revisit behind its own ADR.
- **Download by content type only / by extension only:** rejected — the
  `Content-Disposition` header is the server's explicit intent and must win;
  content-type is the fallback.
- **A multipart crate:** rejected by the dependency policy; the encoder is ~40
  lines and shares the form-control collection already used for urlencoding.
