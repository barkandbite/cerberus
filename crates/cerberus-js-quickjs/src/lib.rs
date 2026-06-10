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
//! # Speed-first delay neutralization
//!
//! Product directive: "pure speed, ignore programmed delays." Every realm gets
//! the [`SPEED_FIRST_PRELUDE`] evaluated into it at creation, *before* any
//! per-head farbling prologue or page script. The prelude reinstalls the timer,
//! animation-frame, idle-callback and observer host APIs as delay-free shims so
//! content that a page would normally reveal on a timer, an animation frame, or a
//! scroll-into-view appears immediately. Implementing these in JavaScript (rather
//! than Rust bindings) is the simplest no-`unsafe` path and keeps the whole
//! neutralization surface in one auditable string.
//!
//! Notable semantics (intentional, and load-bearing for speed):
//! * `setInterval` fires its callback **once, immediately**, then never again —
//!   a real repeating interval would simply hang this single-threaded engine.
//! * `IntersectionObserver.observe` synchronously reports the target as fully
//!   intersecting, which is what makes lazy/scroll-in content load at once.

use cerberus_js::{JsEngine, JsEngineFactory, JsError, JsValue};
use cerberus_types::RealmId;
use rquickjs::{CatchResultExt, Coerced, Context, Ctx, Runtime, Value};
use std::collections::HashMap;

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
    // browsers hand back opaque positive integers; pages only ever compare them
    // or pass them to the matching clear*, which are no-ops here.
    var nextId = 1;
    function newId() { return nextId++; }

    // setTimeout(fn, delay, ...args): run `fn` synchronously *now*, ignoring the
    // delay. Non-function first args (the legacy "eval a string" form) are
    // ignored. Returns a handle.
    g.setTimeout = function (fn) {
      var args = Array.prototype.slice.call(arguments, 2);
      if (typeof fn === "function") {
        try { fn.apply(undefined, args); } catch (e) {}
      }
      return newId();
    };

    // setInterval(fn, delay, ...args): fire ONCE, immediately, then stop. We must
    // not actually repeat — a real interval loop would hang this single-threaded
    // engine forever. Firing once is the speed-first neutralization: code that
    // polls "until ready" on an interval gets one tick, which is usually enough
    // to advance state, and never blocks.
    g.setInterval = function (fn) {
      var args = Array.prototype.slice.call(arguments, 2);
      if (typeof fn === "function") {
        try { fn.apply(undefined, args); } catch (e) {}
      }
      return newId();
    };

    // Cancellation APIs are no-ops: nothing is ever actually pending.
    g.clearTimeout = function () {};
    g.clearInterval = function () {};
    g.cancelAnimationFrame = function () {};
    g.cancelIdleCallback = function () {};

    // requestAnimationFrame(fn): invoke immediately with a timestamp of 0 instead
    // of waiting for a frame.
    g.requestAnimationFrame = function (fn) {
      if (typeof fn === "function") {
        try { fn(0); } catch (e) {}
      }
      return newId();
    };

    // requestIdleCallback(fn): invoke immediately with a deadline that reports no
    // time remaining and no timeout, instead of waiting for idle time.
    g.requestIdleCallback = function (fn) {
      if (typeof fn === "function") {
        try { fn({ didTimeout: false, timeRemaining: function () { return 0; } }); } catch (e) {}
      }
      return newId();
    };

    // queueMicrotask(fn): run immediately. (Equivalent to enqueuing a resolved
    // promise, but synchronous is simpler and indistinguishable for our purposes.)
    g.queueMicrotask = function (fn) {
      if (typeof fn === "function") {
        try { fn(); } catch (e) {}
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

/// A live QuickJS engine: one runtime (one GC heap) hosting many realms.
///
/// Not `Send` (QuickJS is single-threaded): it lives on the UI thread with the
/// active head, matching the [`JsEngine`] contract.
pub struct QuickJsEngine {
    runtime: Runtime,
    realms: HashMap<RealmId, Context>,
}

impl QuickJsEngine {
    /// Build an engine over a fresh runtime with no realms yet.
    ///
    /// Returns [`JsError::Instantiate`] if the runtime cannot be created (only
    /// happens on allocation failure).
    pub fn new() -> Result<Self, JsError> {
        let runtime = Runtime::new().map_err(|e| JsError::Instantiate(e.to_string()))?;
        Ok(Self {
            runtime,
            realms: HashMap::new(),
        })
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
        context.with(|ctx| {
            ctx.eval::<(), _>(SPEED_FIRST_PRELUDE)
                .catch(&ctx)
                .map_err(|e| JsError::Instantiate(e.to_string()))
        })?;
        // Inserting over an existing id refreshes the realm: the old context is
        // dropped (freeing it) and replaced. Simple and non-panicking.
        self.realms.insert(id, context);
        Ok(())
    }

    fn inject_prologue(&mut self, id: RealmId, script: &str) -> Result<(), JsError> {
        let context = self.realms.get(&id).ok_or(JsError::NoSuchRealm(id))?;
        context.with(|ctx| {
            ctx.eval::<(), _>(script)
                .catch(&ctx)
                .map_err(|e| JsError::Eval(e.to_string()))
        })
    }

    fn eval(&mut self, id: RealmId, source: &str) -> Result<JsValue, JsError> {
        let context = self.realms.get(&id).ok_or(JsError::NoSuchRealm(id))?;
        context.with(|ctx| {
            let value = ctx
                .eval::<Value<'_>, _>(source)
                .catch(&ctx)
                .map_err(|e| JsError::Eval(e.to_string()))?;
            // Drain the job queue so Promise reactions and queueMicrotask
            // callbacks scheduled by `source` actually run before we return.
            // `execute_pending_job` operates on this context's runtime.
            while ctx.execute_pending_job() {}
            Ok(js_value_from(&ctx, value))
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
    fn speed_first_settimeout_fires_immediately() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        // A 5-second timer must have already fired by the time eval returns.
        assert_eq!(
            e.eval(r, "let x = 0; setTimeout(() => { x = 9; }, 5000); x")
                .unwrap(),
            JsValue::Number(9.0)
        );
    }

    #[test]
    fn speed_first_setinterval_fires_once() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        // setInterval fires exactly once (it must not loop / hang); the counter
        // lands at 1.
        assert_eq!(
            e.eval(r, "let n = 0; setInterval(() => { n++; }, 1000); n")
                .unwrap(),
            JsValue::Number(1.0)
        );
    }

    #[test]
    fn speed_first_raf_and_idle_fire_immediately() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert_eq!(
            e.eval(r, "let t = -1; requestAnimationFrame(ts => { t = ts; }); t")
                .unwrap(),
            JsValue::Number(0.0)
        );
        assert_eq!(
            e.eval(
                r,
                "let r = -1; requestIdleCallback(d => { r = d.timeRemaining(); }); r"
            )
            .unwrap(),
            JsValue::Number(0.0)
        );
    }

    #[test]
    fn speed_first_queue_microtask_runs() {
        let r = realm(1);
        let mut e = engine_with_realm(r);
        assert_eq!(
            e.eval(r, "let m = false; queueMicrotask(() => { m = true; }); m")
                .unwrap(),
            JsValue::Bool(true)
        );
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
}
