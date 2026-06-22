# ADR-0031: Form submission — POST bodies (and the "evergreen forms" verdict)

- Status: Accepted
- Date: 2026-06-19
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

Forms are how the web is *used* — search, login, signup, comments, and the
owner's repetitive "bump-through" workflows. A question came up: is form support
"evergreen" (works with any form), or do we need per-language modules (PHP / ASPX
/ …) to submit them?

**The framing is the key finding: a browser is language-agnostic.** A form
submits over **HTTP**; the server-side language is invisible to the client. What
actually matters is the HTTP *method* (GET vs POST) and the *encoding*
(`application/x-www-form-urlencoded`, `multipart/form-data`, …). No "module per
language" exists or is needed.

Reviewing the code, GET forms worked (`build_query` → `action?query` → navigate),
but **POST was downgraded to a GET of the action** (`submit_from` had a long-
standing `TODO POST`). That breaks the majority of real forms (login/signup/
checkout almost always POST) and would leak credentials into the URL. The network
layer already spoke POST — JS `fetch` uses `Router::fetch_in(url, method, headers,
body, ctx)` — so the gap was only the navigation/form path on the app side.

## Decision

Wire **POST form submission** through the existing page-load path:

- `submit_from` branches on the form's `method`. For POST, the successful
  controls (already serialized by `build_query` as `x-www-form-urlencoded`) become
  the **request body**; the URL stays query-free (the action's own query is
  preserved). GET is unchanged.
- A page navigation now carries an optional `PostBody { content_type, body }`
  end-to-end: `begin_load` → `dispatch` → `Job::Page` → `fetch_page`, which calls
  `fetch_in("POST", …)` when a body is present and `get_in` otherwise.
- **POST is never cached** (not idempotent): `dispatch` skips the cache read and
  `commit_response` is told not to store it.
- **https-upgrade safety:** a POST to `http://` upgrades to `https://` like any
  load; if that fails and the user accepts the plaintext risk, the **POST is
  replayed** (the body is held in `insecure_post`), not silently downgraded to a
  GET.

`BrowserApp` also gains small automation hooks used by the end-to-end example:
`page_text()`, `is_loading()`, `text_field_centers()`.

### Verdict: evergreen for the common case
- **Works now:** GET and POST forms with `application/x-www-form-urlencoded` —
  search, login, signup, comments, and the owner's claim "bump-through" forms,
  against any backend (verified end-to-end against httpbin: the server echoed the
  POSTed field with `Content-Type: application/x-www-form-urlencoded`).
- **Not yet (follow-ups, all encoding/feature gaps — never language gaps):**
  - `multipart/form-data` + `<input type=file>` (file uploads).
  - Per-button `formaction`/`formmethod`, and including the clicked submit
    button's `name=value` (today submit buttons contribute no value).
  - `method=dialog`, and `text/plain` enctype.
  - Re-POST warning on Back/refresh (POST results are not added to history, so
    Back returns to the pre-form page — acceptable, avoids silent re-submits).

## Consequences

- **Easier:** real login/signup/search forms submit correctly; the body path
  reuses `build_query` and `fetch_in`, so there's one serializer and one network
  seam. Unlocks using the browser for "almost any of its purposes."
- **Costs:** file-upload forms still don't submit (multipart is the next form
  module); a couple of niche form behaviors remain. These are bounded encoding
  features, not open-ended.

## Alternatives considered

- **Keep downgrading POST→GET:** rejected — wrong semantics, server errors, and
  credential leakage into URLs/history.
- **A separate POST job type returning a non-page result:** rejected — a form
  POST's response *is* the next page, so it must flow through the page path
  (render, history, images) — only the request construction differs.
- **Per-language adapters:** a category error — the browser speaks HTTP, not PHP/
  ASPX. No such modules are needed.
