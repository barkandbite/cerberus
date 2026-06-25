# ADR-0068: JavaScript console + error observability bridge

- Status: Accepted
- Date: 2026-06-25
- Deciders: benz.benbarker@gmail.com (owner), engineering

## Context

The web-platform arc's **Phase 0** is observability — the correctness oracle and
the tool every later phase is debugged with. A five-way code re-audit (Epic #40,
2026-06-25) found the engine was **operationally blind to JavaScript failures**:

- `console.*` was captured into an in-VM array (`globalThis.__cerberusConsole`)
  but **never drained into Rust** — its only reader was one test via raw `eval`.
- A `<script>` that threw was swallowed by `run_scripts` (`Err(JsError::Eval(_))`
  dropped) and discarded — even though `rquickjs` already includes the JS **stack
  trace** in that error's message. So a sensor or SPA that failed left no trace at
  all; you could not tell *why* a page came out blank.

The directive calls for "a JavaScript console bridge into the Rust logging layer
and structured error reporting that includes JS stack traces throughout, so
failures surface precisely."

## Decision

Add a single observability channel and wire it into the Rust host.

- **Structured console.** The prelude's console sink now records `{level, text}`
  (level ∈ log/warn/error/info/debug/trace) instead of a bare string, and exposes
  `__cerberusTakeConsole()` which `JSON.stringify`s and clears the buffer.
- **`take_console(engine, realm) -> Vec<ConsoleMessage>`** (cerberus-js-dom) drains
  it. Non-fatal by construction: a realm without the DOM model, or any parse
  hiccup, yields an empty `Vec` — **observability must never break a render**.
- **Script errors flow through the same channel.** Rather than change
  `run_scripts`'s signature (and ripple through every caller), the swallow site
  now records the thrown error — message *and* its JS stack — as an `error`-level
  console entry via `__cerberusRecordError`. The browser-faithful behavior is
  unchanged (a throw does **not** abort the run; later scripts still execute); the
  error simply no longer vanishes. A small `js_escape` safely embeds the message
  in the recording `eval`.
- **Host wiring = stderr.** Both the headless `render` path and the interactive
  browser drain the console after the JS phase/event-loop and emit each line as
  `[js:<level>] <text>` to **stderr** — the diagnostic channel, so it never
  pollutes the rendered image or `--dump-text` stdout. This is the "console bridge
  into the Rust logging layer"; the project has no `log`/`tracing` dep (memory-
  first), so stderr is the logging layer.

## Consequences

- **Page JS is now debuggable.** `console.log/warn/error` and uncaught script
  exceptions (with stacks) appear on stderr for any `render` or interactive run —
  the foundation the rest of the web-platform phases are debugged against.
- **Single channel, minimal ripple.** Routing script errors through the console
  buffer means one drain (`take_console`) surfaces both, with **zero call-site
  changes** to `run_scripts` (signature preserved) — the four existing callers are
  untouched.
- **Browser-faithful semantics preserved.** `throwing_script_does_not_abort_run`
  still passes; top-level `let`/`const`/`function` scoping is unchanged (we do not
  wrap scripts in a `try`/block, which would have changed declaration scope).
- **No memory regression.** The console buffer is transient and drained each
  phase; `mem-gate` stays at 7.6 MB. No new third-party deps.
- All gates green: `fmt`, `clippy -D warnings`, `cargo test --workspace` (incl. the
  updated `console_log_is_captured` + new `console_levels_and_thrown_script_error_surface`),
  `mem-gate --budget-mb 64`, `bench` (48 ms).

## Limitations / follow-ups (tracked under #41)

- No `window.onerror` / `onunhandledrejection` hooks yet (an uncaught Promise
  rejection inside the event loop is not yet recorded — only synchronous script
  throws are).
- The console is not yet exposed on `RenderOutcome` for programmatic assertion in
  app-level integration tests (the library-level `take_console` is tested
  directly); a thin field can be added when an app test needs it.
- Test262 / WPT subset harnesses (the other half of Phase 0) remain to be wired.

## Alternatives considered

- **Change `run_scripts` to return `Vec<JsError>`:** the "purer" library shape, but
  it ripples through every caller and still needs a second channel for console;
  routing both through the console buffer is one channel and zero ripple.
- **Wrap each script in `try { … } catch`:** would capture throws in JS directly,
  but a block `{}` makes top-level `let`/`const` block-scoped, silently breaking
  multi-`<script>` pages that share declarations — rejected.
- **Add a `tracing`/`log` dependency:** a new third-party dep against the
  memory-first, no-foreign-deps policy for what stderr already does; rejected.
