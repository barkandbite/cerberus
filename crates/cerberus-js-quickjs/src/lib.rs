//! QuickJS adapter (ADR-0002): implements the [`JsEngine`] seam over `rquickjs`
//! (bundled QuickJS).
//!
//! # Realms over one runtime
//!
//! A QuickJS *runtime* owns a GC heap; a QuickJS *context* is a realm with its
//! own global object that nonetheless shares the runtime heap (browser frames of
//! the same origin work the same way). [`QuickJsEngine`] therefore holds exactly
//! one [`rquickjs::Runtime`] and a `HashMap<RealmId, rquickjs::Context>`: one
//! engine instance per active head (the memory-first design — see `cerberus-js`),
//! many realms (one per tab) sharing its heap. Dropping a context frees its
//! realm; dropping the engine frees the runtime.
//!
//! # Speed-first delay neutralization & the bounded event loop
//!
//! Product directive: "pure speed, ignore programmed delays." Every realm gets
//! the [`SPEED_FIRST_PRELUDE`] evaluated into it at creation, *before* any
//! per-head farbling prologue or page script. The prelude reinstalls the timer,
//! animation-frame, idle-callback and observer host APIs so a page's programmed
//! delays cost nothing. Implementing these in JavaScript (rather than Rust
//! bindings) is the simplest no-`unsafe` path and keeps the whole surface in one
//! auditable string.
//!
//! Timers do not fire at call time: they **enqueue** a task on a per-realm
//! **virtual clock**, which the host drains with a **bounded loop** via
//! `__cerberusStepTimer` (one task per call) under hard caps — so ordering is
//! correct (sync → microtasks → macrotask) and every page terminates (ADR-0013;
//! the driver is `cerberus-js-dom::run_event_loop`). Because delays are virtual,
//! the loop still resolves "reveal on a timer" content at once, just in order.
//!
//! Notable semantics (intentional):
//! * Virtual time means `setTimeout`/`setInterval` delays never wall-block, and
//!   `setInterval` ticks until the virtual-clock cap rather than looping forever.
//! * `queueMicrotask` is a real microtask (`Promise.resolve().then`), ordered
//!   against Promise reactions.
//! * `IntersectionObserver.observe` synchronously reports the target as fully
//!   intersecting, which is what makes lazy/scroll-in content load at once.

use cerberus_js::{JsEngine, JsEngineFactory, JsError, JsValue};
use cerberus_types::RealmId;
use rquickjs::context::EvalOptions;
use rquickjs::{CatchResultExt, Coerced, Context, Ctx, Runtime, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Delay-free host environment installed into every realm at creation, before
/// any page script (see module docs for the speed-first rationale).
///
/// The whole body is wrapped in a `try { … } catch {}` so that even on an
/// unusually minimal context (e.g. a missing intrinsic) installing the shims can
/// never itself throw and abort realm creation.
const SPEED_FIRST_PRELUDE: &str = r#"
(function () {
  try {
    var g = globalThis;

    // Monotonic, non-zero handle source shared by every "schedule" shim. Real
    // browsers hand back opaque positive integers; pages compare them or pass
    // them to the matching clear*/cancel*.
    var nextId = 1;
    function newId() { return nextId++; }

    // ---- bounded virtual-clock event loop (ADR-0013) -------------------
    // Timers ENQUEUE a task due at a virtual time rather than firing at call
    // time. The host drains the queue via __cerberusStepTimer (one task per
    // call), draining microtasks between tasks, under hard caps — so ordering is
    // correct (sync -> microtasks -> macrotask) and every page terminates. The
    // driver is cerberus-js-dom::run_event_loop.
    var clock = 0;  // virtual "now", in ms
    var tasks = []; // pending macrotasks: {id, kind, due, cb, args, interval}

    function schedule(kind, fn, due, args, interval) {
      var id = newId();
      tasks.push({
        id: id,
        kind: kind,
        due: due,
        cb: (typeof fn === "function") ? fn : null,
        args: args,
        interval: interval, // null = one-shot; else a >=1 ms period
      });
      return id;
    }
    function cancel(id) {
      for (var i = 0; i < tasks.length; i++) {
        if (tasks[i].id === id) { tasks.splice(i, 1); return; }
      }
    }

    g.setTimeout = function (fn, delay) {
      var d = +delay; if (!(d >= 0)) d = 0;
      return schedule("timeout", fn, clock + d, Array.prototype.slice.call(arguments, 2), null);
    };
    g.setInterval = function (fn, delay) {
      var d = +delay; if (!(d >= 1)) d = 1; // clamp period so the clock advances each tick
      return schedule("interval", fn, clock + d, Array.prototype.slice.call(arguments, 2), d);
    };
    g.clearTimeout = function (id) { cancel(id); };
    g.clearInterval = function (id) { cancel(id); };

    g.requestAnimationFrame = function (fn) {
      // ~60fps virtual frame; the callback receives the (virtual) timestamp.
      return schedule("raf", fn, clock + 16, null, null);
    };
    g.cancelAnimationFrame = function (id) { cancel(id); };

    g.requestIdleCallback = function (fn) {
      return schedule("idle", fn, clock, null, null);
    };
    g.cancelIdleCallback = function (id) { cancel(id); };

    // queueMicrotask(fn): a real microtask, so it orders correctly against
    // Promise reactions (both drain through the job queue between macrotasks).
    g.queueMicrotask = function (fn) {
      if (typeof fn === "function") {
        Promise.resolve().then(function () { try { fn(); } catch (e) {} });
      }
    };

    // Run ONE due macrotask within the virtual-clock budget. Returns 1 if a task
    // ran, 0 if none is due (<= maxClock). The host calls this repeatedly,
    // draining microtasks between calls, under a task-count cap (see ADR-0013).
    g.__cerberusStepTimer = function (maxClock) {
      try {
        var best = -1;
        for (var i = 0; i < tasks.length; i++) {
          if (tasks[i].due <= maxClock && (best === -1 || tasks[i].due < tasks[best].due)) {
            best = i;
          }
        }
        if (best === -1) return 0;
        var task = tasks[best];
        if (clock < task.due) clock = task.due; // advance virtual time to the task
        if (task.interval != null) {
          task.due = clock + task.interval;     // re-arm in place
        } else {
          tasks.splice(best, 1);                // one-shot: remove before running
        }
        if (task.cb) {
          try {
            if (task.kind === "raf") task.cb(clock);
            else if (task.kind === "idle") task.cb({ didTimeout: false, timeRemaining: function () { return 0; } });
            else task.cb.apply(undefined, task.args || []);
          } catch (e) {}
        }
        return 1;
      } catch (e) {
        return 0;
      }
    };

    // IntersectionObserver: the key lazy-load defeat. observe() synchronously
    // reports the target as fully visible, so scroll-in / "load when seen"
    // content is delivered at once.
    g.IntersectionObserver = class IntersectionObserver {
      constructor(callback, options) {
        this._callback = callback;
        this._options = options;
      }
      observe(target) {
        if (typeof this._callback === "function") {
          var entry = {
            isIntersecting: true,
            intersectionRatio: 1,
            target: target,
            time: 0,
            boundingClientRect: {},
            intersectionRect: {},
            rootBounds: null,
          };
          try { this._callback([entry], this); } catch (e) {}
        }
      }
      unobserve() {}
      disconnect() {}
      takeRecords() { return []; }
    };

    // ResizeObserver / MutationObserver: must exist so feature-detecting scripts
    // don't throw, but they never fire (there is no real layout or DOM mutation
    // stream behind this engine). Safe no-ops.
    g.ResizeObserver = class ResizeObserver {
      constructor(callback) { this._callback = callback; }
      observe() {}
      unobserve() {}
      disconnect() {}
      takeRecords() { return []; }
    };

    g.MutationObserver = class MutationObserver {
      constructor(callback) { this._callback = callback; }
      observe() {}
      disconnect() {}
      takeRecords() { return []; }
    };
  } catch (e) {
    // Never let prelude installation abort realm creation.
  }
})();
"#;

/// Per-engine JS heap cap (issue #68): a single page's `<script>` must not be
/// able to OOM the whole browser process via an unbounded allocation loop
/// (e.g. `let a = []; while (true) a.push(a.slice());`). This is a *per-engine*
/// limit, not the whole-process mem-gate budget tracked elsewhere (64 MiB) —
/// 192 MiB is generous enough that no realistic page or existing test comes
/// close, while still turning "grow forever" into a prompt, catchable
/// [`JsError::Eval`] instead of a process OOM kill.
const MAX_JS_HEAP_BYTES: usize = 192 * 1024 * 1024;

/// Default max QuickJS interpreter stack size — the ceiling QuickJS enforces on
/// its own C-stack usage before raising a catchable stack-overflow, well below
/// the OS thread stack. This matches rquickjs's own built-in default; we set it
/// explicitly so the cap is auditable here rather than only inside the vendored
/// crate. It must stay a safe fraction of the smallest thread this engine runs
/// on (the default worker-thread stack is 2 MiB), so a runaway script surfaces
/// as a catchable [`JsError::Eval`] rather than overflowing the native stack and
/// crashing the process. Sized against release builds, where interpreter frames
/// are compact — a *debug* build's frames are several times larger, so tests
/// that need deep recursion build an engine with an explicit larger cap on an
/// explicitly large-stacked thread (see the recursion tests).
const MAX_JS_STACK_BYTES: usize = 256 * 1024;

/// Default wall-clock budget for a single top-level [`QuickJsEngine::eval`]
/// call (issue #68): stops a page-controlled `while (true) {}` from hanging
/// the browser process forever. Generous for real pages/tests, short enough
/// that a hang is a blip rather than an outage.
const DEFAULT_EVAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared deadline cell read by the QuickJS interrupt handler and written by
/// [`QuickJsEngine::eval`] just before each top-level `ctx.eval`. QuickJS
/// calls the interrupt handler periodically and automatically while running
/// script; returning `true` makes it raise an uncatchable exception and
/// return control to the caller — exactly what stops a runaway loop. One
/// `Runtime` (and thus one handler closure) is reused across every realm and
/// call over the engine's lifetime, so the deadline must be refreshed per
/// call rather than fixed at construction; a `Mutex<Option<Instant>>` is the
/// simplest shared cell that lets `eval` set a fresh deadline and the
/// interrupt closure read it, without pulling in a generic timer type.
type DeadlineCell = Arc<Mutex<Option<Instant>>>;

/// Install the interrupt handler that enforces `deadline` on `runtime`.
///
/// `None` in the cell means "no deadline armed" (never interrupt) — used
/// between top-level calls so background bookkeeping (if any) isn't cut off
/// mid-flight by a stale deadline from a previous `eval`.
fn install_deadline_watchdog(runtime: &Runtime, deadline: DeadlineCell) {
    runtime.set_interrupt_handler(Some(Box::new(move || {
        match deadline.lock() {
            Ok(guard) => guard.is_some_and(|by| Instant::now() >= by),
            // A poisoned lock means a prior holder panicked mid-access; fail
            // safe by interrupting rather than risking an unbounded script.
            Err(_) => true,
        }
    })));
}

/// A live QuickJS engine: one runtime (one GC heap) hosting many realms.
///
/// Not `Send` (QuickJS is single-threaded): it lives on the UI thread with the
/// active head, matching the [`JsEngine`] contract.
pub struct QuickJsEngine {
    runtime: Runtime,
    realms: HashMap<RealmId, Context>,
    /// Per-call eval budget (issue #68); see [`QuickJsEngine::with_eval_timeout`].
    eval_timeout: Duration,
    /// Shared with the runtime's interrupt handler; `eval` refreshes this
    /// right before each top-level `ctx.eval`.
    deadline: DeadlineCell,
}

impl QuickJsEngine {
    /// Build an engine with the production-default eval timeout
    /// ([`DEFAULT_EVAL_TIMEOUT`]). Delegates to [`Self::with_eval_timeout`].
    ///
    /// Returns [`JsError::Instantiate`] if the runtime cannot be created (only
    /// happens on allocation failure).
    pub fn new() -> Result<Self, JsError> {
        Self::with_eval_timeout(DEFAULT_EVAL_TIMEOUT)
    }

    /// Build an engine whose top-level `eval` calls are interrupted after
    /// `timeout` (issue #68). A separate constructor (rather than a `new()`
    /// parameter) so production call sites keep the zero-argument default
    /// while tests can arm a millisecond-scale timeout without a real sleep.
    ///
    /// Returns [`JsError::Instantiate`] if the runtime cannot be created (only
    /// happens on allocation failure).
    pub fn with_eval_timeout(timeout: Duration) -> Result<Self, JsError> {
        Self::with_limits(timeout, MAX_JS_STACK_BYTES)
    }

    /// Build an engine with an explicit interpreter stack cap. Production uses
    /// the [`MAX_JS_STACK_BYTES`] default via [`Self::with_eval_timeout`]; the
    /// recursion tests use this to raise the cap (on an explicitly large-stacked
    /// thread) so a *debug* build's oversized interpreter frames don't clip
    /// legitimate recursion the shipped release build handles comfortably.
    ///
    /// `stack_bytes` must stay a safe fraction of the running thread's stack, or
    /// a runaway script overflows the native stack instead of raising a
    /// catchable error — the caller owns that invariant.
    ///
    /// Returns [`JsError::Instantiate`] if the runtime cannot be created (only
    /// happens on allocation failure).
    fn with_limits(timeout: Duration, stack_bytes: usize) -> Result<Self, JsError> {
        let runtime = Runtime::new().map_err(|e| JsError::Instantiate(e.to_string()))?;
        runtime.set_memory_limit(MAX_JS_HEAP_BYTES);
        runtime.set_max_stack_size(stack_bytes);
        let deadline: DeadlineCell = Arc::new(Mutex::new(None));
        install_deadline_watchdog(&runtime, Arc::clone(&deadline));
        Ok(Self {
            runtime,
            realms: HashMap::new(),
            eval_timeout: timeout,
            deadline,
        })
    }

    /// Arm the shared deadline for one top-level entry point, run `f`, then
    /// disarm it. Disarming afterwards means a slow-but-legitimate host
    /// operation between calls (e.g. the pending-job drain in `eval`, which
    /// runs script and so must stay covered) is never cut short by a stale
    /// deadline from a previous call — each entry point gets its own fresh
    /// budget and nothing outside one is bounded by it.
    fn with_deadline<T>(&self, f: impl FnOnce() -> T) -> T {
        let by = Instant::now() + self.eval_timeout;
        {
            let mut guard = self.deadline.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(by);
        }
        let result = f();
        {
            let mut guard = self.deadline.lock().unwrap_or_else(|e| e.into_inner());
            *guard = None;
        }
        result
    }
}

impl JsEngine for QuickJsEngine {
    fn name(&self) -> &'static str {
        "quickjs"
    }

    fn create_realm(&mut self, id: RealmId) -> Result<(), JsError> {
        let context =
            Context::full(&self.runtime).map_err(|e| JsError::Instantiate(e.to_string()))?;
        // Install the speed-first host environment before any page script. The
        // prelude is self-guarding (its body is wrapped in try/catch), but a
        // genuine engine error (e.g. compile failure) still surfaces here.
        // Armed with the same deadline as page `eval` calls: the prelude is
        // fixed, trusted source, but arming it uniformly means one code path
        // (`with_deadline`) governs every script the runtime ever executes.
        self.with_deadline(|| {
            context.with(|ctx| {
                ctx.eval::<(), _>(SPEED_FIRST_PRELUDE)
                    .catch(&ctx)
                    .map_err(|e| JsError::Instantiate(e.to_string()))
            })
        })?;
        // Inserting over an existing id refreshes the realm: the old context is
        // dropped (freeing it) and replaced. Simple and non-panicking.
        self.realms.insert(id, context);
        Ok(())
    }

    fn inject_prologue(&mut self, id: RealmId, script: &str) -> Result<(), JsError> {
        let context = self.realms.get(&id).ok_or(JsError::NoSuchRealm(id))?;
        self.with_deadline(|| {
            context.with(|ctx| {
                ctx.eval_with_options::<(), _>(script, sloppy_eval_options())
                    .catch(&ctx)
                    .map_err(|e| JsError::Eval(e.to_string()))
            })
        })
    }

    fn eval(&mut self, id: RealmId, source: &str) -> Result<JsValue, JsError> {
        let context = self.realms.get(&id).ok_or(JsError::NoSuchRealm(id))?;
        self.with_deadline(|| {
            context.with(|ctx| {
                let value = ctx
                    .eval_with_options::<Value<'_>, _>(source, sloppy_eval_options())
                    .catch(&ctx)
                    .map_err(|e| JsError::Eval(e.to_string()))?;
                // Drain the job queue so Promise reactions and queueMicrotask
                // callbacks scheduled by `source` actually run before we return.
                // `execute_pending_job` operates on this context's runtime, and
                // pending jobs are page-derived callbacks too, so they stay
                // covered by the same deadline armed above.
                while ctx.execute_pending_job() {}
                Ok(js_value_from(&ctx, value))
            })
        })
    }

    fn destroy_realm(&mut self, id: RealmId) -> Result<(), JsError> {
        // Dropping the removed context frees the realm. Absent id is a no-op.
        self.realms.remove(&id);
        Ok(())
    }

    fn realm_count(&self) -> usize {
        self.realms.len()
    }
}

/// Eval options that match a browser's classic inline `<script>`: global scope,
/// **sloppy** (non-strict) mode. rquickjs defaults `strict` to `true`, which
/// makes an implicit global assignment (`foo = 1` with no prior declaration)
/// throw `ReferenceError`. Real pages rely on sloppy-mode semantics — e.g.
/// Wikipedia's portal reveal script assigns `portalSearchDomain = '…'` without
/// declaring it — so we evaluate page scripts in sloppy mode to keep parity.
fn sloppy_eval_options() -> EvalOptions {
    let mut opts = EvalOptions::default();
    opts.strict = false;
    opts
}

/// Convert a QuickJS [`Value`] into the engine-neutral [`JsValue`].
///
/// Primitives map directly; `null` collapses to [`JsValue::Undefined`] (the seam
/// has no null). Anything else (objects, arrays, functions, symbols, BigInt) is
/// coerced to its string form via JS `String(x)`; if even that throws or yields a
/// non-string, we fall back to [`JsValue::Undefined`] rather than surface an
/// error from a successful eval.
fn js_value_from<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> JsValue {
    if value.is_undefined() || value.is_null() {
        return JsValue::Undefined;
    }
    if let Some(b) = value.as_bool() {
        return JsValue::Bool(b);
    }
    if let Some(n) = value.as_number() {
        return JsValue::Number(n);
    }
    if value.is_string() {
        if let Some(s) = value.as_string() {
            if let Ok(rust) = s.to_string() {
                return JsValue::Str(rust);
            }
        }
        return JsValue::Undefined;
    }
    // Non-primitive: stringify via coercion (e.g. objects → "[object Object]",
    // arrays → "1,2,3"). Re-borrow through `get` so coercion runs in-context.
    match value.get::<Coerced<String>>() {
        Ok(coerced) => JsValue::Str(coerced.0),
        Err(_) => {
            // Coercion itself threw (e.g. a Symbol, or a toString that throws).
            // Clear any pending exception so the realm stays usable, then yield
            // Undefined for the otherwise-successful eval.
            let _ = ctx.catch();
            JsValue::Undefined
        }
    }
}

/// Factory for [`QuickJsEngine`]. A unit struct, hence `Send + Sync` for free —
/// the identity manager holds one and instantiates the engine for the active
/// head only.
pub struct QuickJsEngineFactory;

impl JsEngineFactory for QuickJsEngineFactory {
    fn instantiate(&self) -> Result<Box<dyn JsEngine>, JsError> {
        Ok(Box::new(QuickJsEngine::new()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn realm(n: u64) -> RealmId {
        RealmId::from_u64_pair(0, n)
    }

    /// A fresh engine with one realm already created. Most tests want this.
    fn engine_with_realm(id: RealmId) -> QuickJsEngine {
        let mut e = QuickJsEngine::new().expect("runtime");
        e.create_realm(id).expect("create realm");
        e
    }

    #[test]
    fn name_is_quickjs() {
        let e = QuickJsEngine::new().unwrap();
        assert_eq!(e.name(), "quickjs");
    }

    #[test]
    fn eval_arithmetic_returns_number() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert_eq!(e.eval(r, "1 + 2").unwrap(), JsValue::Number(3.0));
    }

    #[test]
    fn eval_string_expression_returns_str() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert_eq!(
            e.eval(r, "'foo' + 'bar'").unwrap(),
            JsValue::Str("foobar".to_string())
        );
    }

    #[test]
    fn eval_boolean_returns_bool() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert_eq!(e.eval(r, "1 < 2").unwrap(), JsValue::Bool(true));
        assert_eq!(e.eval(r, "1 > 2").unwrap(), JsValue::Bool(false));
    }

    #[test]
    fn eval_undefined_and_statements_return_undefined() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert_eq!(e.eval(r, "undefined").unwrap(), JsValue::Undefined);
        // A bare statement (let-binding) has no completion value → undefined.
        assert_eq!(e.eval(r, "let q = 5;").unwrap(), JsValue::Undefined);
        // null collapses to Undefined (the seam has no null).
        assert_eq!(e.eval(r, "null").unwrap(), JsValue::Undefined);
    }

    #[test]
    fn implicit_global_assignment_is_sloppy_not_strict() {
        // Browsers run classic inline <script> in sloppy mode, where assigning to
        // an undeclared identifier creates a global rather than throwing. rquickjs
        // defaults to strict mode, so without an override this would be a
        // ReferenceError (regression: Wikipedia's portal reveal script does
        // `portalSearchDomain = '…'` and relied on this).
        let r = realm(1);
        let mut e = engine_with_realm(r);
        e.eval(r, "portalSearchDomain = 'wikipedia.org';").unwrap();
        assert_eq!(
            e.eval(r, "portalSearchDomain").unwrap(),
            JsValue::Str("wikipedia.org".to_string())
        );
    }

    #[test]
    fn eval_object_is_stringified() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert_eq!(
            e.eval(r, "({})").unwrap(),
            JsValue::Str("[object Object]".to_string())
        );
        assert_eq!(
            e.eval(r, "[1,2,3]").unwrap(),
            JsValue::Str("1,2,3".to_string())
        );
    }

    #[test]
    fn realm_lifecycle_create_eval_destroy() {
        let r = realm(7);
        let mut e = QuickJsEngine::new().unwrap();
        assert_eq!(e.realm_count(), 0);
        e.create_realm(r).unwrap();
        assert_eq!(e.eval(r, "40 + 2").unwrap(), JsValue::Number(42.0));
        assert_eq!(e.realm_count(), 1);
        e.destroy_realm(r).unwrap();
        assert_eq!(e.realm_count(), 0);
    }

    #[test]
    fn eval_on_absent_realm_is_no_such_realm() {
        let mut e = QuickJsEngine::new().unwrap();
        let r = realm(99);
        match e.eval(r, "1") {
            Err(JsError::NoSuchRealm(got)) => assert_eq!(got, r),
            other => panic!("expected NoSuchRealm, got {other:?}"),
        }
    }

    #[test]
    fn inject_prologue_on_absent_realm_is_no_such_realm() {
        let mut e = QuickJsEngine::new().unwrap();
        let r = realm(99);
        match e.inject_prologue(r, "1") {
            Err(JsError::NoSuchRealm(got)) => assert_eq!(got, r),
            other => panic!("expected NoSuchRealm, got {other:?}"),
        }
    }

    #[test]
    fn destroy_absent_realm_is_ok() {
        let mut e = QuickJsEngine::new().unwrap();
        // Destroying a realm that was never created is a no-op, not an error.
        assert!(e.destroy_realm(realm(123)).is_ok());
    }

    #[test]
    fn prologue_globals_visible_to_later_eval() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        e.inject_prologue(r, "globalThis.__farble = 42;").unwrap();
        assert_eq!(e.eval(r, "__farble").unwrap(), JsValue::Number(42.0));
    }

    #[test]
    fn realms_are_isolated() {
        let a = realm(1);
        let b = realm(2);
        let mut e = QuickJsEngine::new().unwrap();
        e.create_realm(a).unwrap();
        e.create_realm(b).unwrap();

        e.eval(a, "globalThis.secret = 'in_a';").unwrap();
        // The global set in realm A must NOT leak into realm B.
        assert_eq!(
            e.eval(a, "typeof secret === 'string' ? secret : 'missing'")
                .unwrap(),
            JsValue::Str("in_a".to_string())
        );
        assert_eq!(
            e.eval(b, "typeof secret").unwrap(),
            JsValue::Str("undefined".to_string())
        );
    }

    #[test]
    fn eval_error_is_reported_and_engine_stays_usable() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        match e.eval(r, "throw new Error('boom')") {
            Err(JsError::Eval(msg)) => assert!(
                msg.contains("boom"),
                "exception message should mention 'boom', got: {msg}"
            ),
            other => panic!("expected Eval error, got {other:?}"),
        }
        // The realm must remain usable after a thrown exception.
        assert_eq!(e.eval(r, "1 + 1").unwrap(), JsValue::Number(2.0));
    }

    #[test]
    fn syntax_error_is_eval_error() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert!(matches!(
            e.eval(r, "this is not js {{{"),
            Err(JsError::Eval(_))
        ));
    }

    #[test]
    fn settimeout_enqueues_then_fires_on_step() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        // A timer no longer fires at call time (ADR-0013): x is still 0 right
        // after scheduling.
        assert_eq!(
            e.eval(
                r,
                "globalThis.x = 0; setTimeout(() => { globalThis.x = 9; }, 5000); x"
            )
            .unwrap(),
            JsValue::Number(0.0)
        );
        // Stepping the loop runs the due task — the virtual clock ignores the 5s.
        assert_eq!(
            e.eval(r, "__cerberusStepTimer(1000000)").unwrap(),
            JsValue::Number(1.0)
        );
        assert_eq!(e.eval(r, "x").unwrap(), JsValue::Number(9.0));
        // Queue now empty: the next step reports nothing ran.
        assert_eq!(
            e.eval(r, "__cerberusStepTimer(1000000)").unwrap(),
            JsValue::Number(0.0)
        );
    }

    #[test]
    fn setinterval_ticks_each_step_and_is_clock_bounded() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        e.eval(
            r,
            "globalThis.n = 0; setInterval(() => { globalThis.n++; }, 1000);",
        )
        .unwrap();
        // Each step advances the virtual clock by the period and runs one tick.
        for _ in 0..3 {
            assert_eq!(
                e.eval(r, "__cerberusStepTimer(1000000)").unwrap(),
                JsValue::Number(1.0)
            );
        }
        assert_eq!(e.eval(r, "n").unwrap(), JsValue::Number(3.0));
        // Bounded by the virtual-clock budget: clock is 3000, next due 4000, so a
        // step capped at 3500 runs nothing (this is what stops an interval loop).
        assert_eq!(
            e.eval(r, "__cerberusStepTimer(3500)").unwrap(),
            JsValue::Number(0.0)
        );
    }

    #[test]
    fn raf_and_idle_enqueue_then_fire_on_step() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        e.eval(
            r,
            "globalThis.t = -1; requestAnimationFrame(ts => { globalThis.t = ts; });",
        )
        .unwrap();
        e.eval(
            r,
            "globalThis.rem = -1; requestIdleCallback(d => { globalThis.rem = d.timeRemaining(); });",
        )
        .unwrap();
        // Drain the loop: idle (due 0) reports no time remaining; rAF runs at the
        // virtual frame timestamp (16).
        while e.eval(r, "__cerberusStepTimer(1000000)").unwrap() == JsValue::Number(1.0) {}
        assert_eq!(e.eval(r, "t").unwrap(), JsValue::Number(16.0));
        assert_eq!(e.eval(r, "rem").unwrap(), JsValue::Number(0.0));
    }

    #[test]
    fn queue_microtask_runs_as_a_real_microtask() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        // Real microtask timing: NOT run within the scheduling eval (like a
        // Promise reaction)...
        assert_eq!(
            e.eval(
                r,
                "globalThis.m = false; queueMicrotask(() => { globalThis.m = true; }); m"
            )
            .unwrap(),
            JsValue::Bool(false)
        );
        // ...but the post-eval job pump drains it before the next eval.
        assert_eq!(e.eval(r, "m").unwrap(), JsValue::Bool(true));
    }

    #[test]
    fn speed_first_intersection_observer_fires_immediately() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert_eq!(
            e.eval(
                r,
                "let seen = false; \
                 new IntersectionObserver(es => { seen = es[0].isIntersecting; }).observe({}); \
                 seen"
            )
            .unwrap(),
            JsValue::Bool(true)
        );
    }

    #[test]
    fn speed_first_resize_and_mutation_observers_are_safe_noops() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        // Must exist and be constructible/usable without throwing, but never fire.
        assert_eq!(
            e.eval(
                r,
                "let fired = false; \
                 let ro = new ResizeObserver(() => { fired = true; }); ro.observe({}); ro.disconnect(); \
                 let mo = new MutationObserver(() => { fired = true; }); mo.observe({}, {}); mo.disconnect(); \
                 fired"
            )
            .unwrap(),
            JsValue::Bool(false)
        );
    }

    #[test]
    fn job_queue_is_pumped_for_promises() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        // Faithful microtask timing: the `.then` reaction is a microtask, so it
        // has NOT run yet when the trailing `p` in the same script is evaluated —
        // microtasks drain only after the synchronous script completes. So this
        // eval's completion value is still 0...
        assert_eq!(
            e.eval(
                r,
                "globalThis.p = 0; Promise.resolve(7).then(v => { p = v; }); p"
            )
            .unwrap(),
            JsValue::Number(0.0)
        );
        // ...but our post-eval job pump then drains that microtask, so the side
        // effect is visible on the next eval. (Without pumping, `p` would stay 0.)
        assert_eq!(e.eval(r, "p").unwrap(), JsValue::Number(7.0));
    }

    #[test]
    fn awaited_promise_completion_resolves_via_pump() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        // When the eval's own completion value is a settled promise chain, pumping
        // the queue lets the chained value land. Use an async IIFE writing a global
        // we then read back: the await continuation is a job the pump must run.
        e.eval(
            r,
            "globalThis.out = 0; (async () => { out = await Promise.resolve(13); })();",
        )
        .unwrap();
        assert_eq!(e.eval(r, "out").unwrap(), JsValue::Number(13.0));
    }

    #[test]
    fn factory_instantiates_independent_engines() {
        let factory = QuickJsEngineFactory;
        let mut e1 = factory.instantiate().unwrap();
        let mut e2 = factory.instantiate().unwrap();
        assert_eq!(e1.name(), "quickjs");

        let r = realm(1);
        e1.create_realm(r).unwrap();
        e2.create_realm(r).unwrap();

        // State set in one engine's realm must not be visible in the other's,
        // even though both use the same RealmId.
        e1.eval(r, "globalThis.tag = 'engine_one';").unwrap();
        assert_eq!(
            e1.eval(r, "tag").unwrap(),
            JsValue::Str("engine_one".to_string())
        );
        assert_eq!(
            e2.eval(r, "typeof tag").unwrap(),
            JsValue::Str("undefined".to_string())
        );

        // And tearing one down leaves the other intact.
        drop(e1);
        assert_eq!(e2.eval(r, "2 * 21").unwrap(), JsValue::Number(42.0));
    }

    #[test]
    fn create_realm_twice_refreshes_without_panicking() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        e.eval(r, "globalThis.keep = 1;").unwrap();
        // Re-creating the same realm id resets it (fresh globals).
        e.create_realm(r).unwrap();
        assert_eq!(e.realm_count(), 1);
        assert_eq!(
            e.eval(r, "typeof keep").unwrap(),
            JsValue::Str("undefined".to_string())
        );
    }

    // ---- issue #68: memory limit / interrupt watchdog / stack cap --------

    /// A tiny timeout so the infinite-loop test below is bounded and fast
    /// rather than a real multi-second sleep; production uses
    /// [`DEFAULT_EVAL_TIMEOUT`] via [`QuickJsEngine::new`].
    const TINY_TIMEOUT: Duration = Duration::from_millis(20);

    #[test]
    fn infinite_loop_is_interrupted_within_configured_timeout() {
        let r = realm(1);
        let mut e = QuickJsEngine::with_eval_timeout(TINY_TIMEOUT).expect("runtime");
        e.create_realm(r).unwrap();
        let start = Instant::now();
        match e.eval(r, "while (true) {}") {
            Err(JsError::Eval(_)) => {}
            other => panic!("expected Eval error from interrupted loop, got {other:?}"),
        }
        // Bounded: well under a real hang, and not dependent on wall-clock
        // flakiness on a loaded CI box.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "interrupt took too long: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn allocation_loop_is_interrupted_within_configured_timeout() {
        // A page that never busy-loops but keeps allocating (the OOM half of
        // #68) must also be bounded by the same watchdog, since QuickJS's
        // interrupt handler is polled during allocation-heavy execution too.
        let r = realm(1);
        let mut e = QuickJsEngine::with_eval_timeout(TINY_TIMEOUT).expect("runtime");
        e.create_realm(r).unwrap();
        match e.eval(r, "var a = []; while (true) { a.push(new Array(1024)); }") {
            Err(JsError::Eval(_)) => {}
            other => panic!("expected Eval error from runaway allocation, got {other:?}"),
        }
    }

    #[test]
    fn fast_script_still_succeeds_with_interrupt_handler_installed() {
        // No false positives: a normal, fast script must still complete
        // successfully even though the watchdog is armed on every eval.
        let r = realm(1);
        let mut e = QuickJsEngine::with_eval_timeout(Duration::from_secs(5)).expect("runtime");
        e.create_realm(r).unwrap();
        assert_eq!(e.eval(r, "1 + 1").unwrap(), JsValue::Number(2.0));
    }

    #[test]
    fn memory_limit_bounds_unbounded_allocation() {
        // rquickjs's `Runtime` exposes no getter for the configured limit, so
        // exercise it behaviorally: a runtime with a tiny injected limit must
        // fail an unbounded allocation loop with an error rather than growing
        // forever. Use a generous timeout so the interrupt watchdog above
        // isn't what trips first — this test is isolating the memory limit.
        let r = realm(1);
        let mut e = QuickJsEngine::with_eval_timeout(Duration::from_secs(5)).expect("runtime");
        // Create the realm (and its prelude eval, which needs some heap)
        // BEFORE injecting a tiny limit, then clamp down for the allocation
        // loop under test — isolates "does the limit stop growth" from
        // "is the limit big enough to bootstrap a realm at all".
        e.create_realm(r).unwrap();
        e.runtime.set_memory_limit(64 * 1024);
        match e.eval(
            r,
            "var a = []; for (var i = 0; i < 10_000_000; i++) { a.push(new Array(1024)); }",
        ) {
            Err(JsError::Eval(_)) => {}
            other => panic!("expected Eval error from memory-limited allocation, got {other:?}"),
        }
    }

    #[test]
    fn deep_but_bounded_recursion_still_works() {
        // Legitimate, bounded recursion must evaluate normally when the engine
        // has stack room. Run on a thread with an explicit, generous stack and
        // an engine cap to match: a *debug* build's interpreter frames are
        // several times larger than the shipped release build's, so depth 100
        // can need ~1 MiB of native stack here even though release handles far
        // deeper within the production default. The large-stacked thread keeps
        // this deterministic across platforms (a default worker thread is only
        // 2 MiB, and Windows debug frames are the heaviest).
        let handle = std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                let r = realm(1);
                let mut e = QuickJsEngine::with_limits(DEFAULT_EVAL_TIMEOUT, 16 * 1024 * 1024)
                    .expect("runtime");
                e.create_realm(r).expect("create realm");
                e.eval(
                    r,
                    "function sum(n) { return n <= 0 ? 0 : n + sum(n - 1); } sum(100)",
                )
                .unwrap()
            })
            .expect("spawn recursion test thread");
        assert_eq!(handle.join().unwrap(), JsValue::Number(5_050.0));
    }

    #[test]
    fn runaway_recursion_errors_instead_of_crashing() {
        // Unbounded recursion must surface as a catchable JsError::Eval (stack
        // overflow), not crash the process — this is rquickjs's own default
        // stack guard, set explicitly here for auditability (see
        // MAX_JS_STACK_BYTES).
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert!(matches!(
            e.eval(r, "function f() { return f(); } f()"),
            Err(JsError::Eval(_))
        ));
    }
}
