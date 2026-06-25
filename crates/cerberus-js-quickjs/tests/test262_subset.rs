//! Test262-style engine conformance subset (Phase 0, the correctness oracle).
//!
//! Runs a curated set of ECMAScript conformance assertions — written in the
//! upstream Test262 idiom (`assert`, `assert.sameValue`, `assert.throws`, with
//! Test262's SameValue semantics) — against the real QuickJS engine, one fresh
//! realm per test, and reports the pass rate. It proves the language surface that
//! obfuscated production bundles and anti-bot sensors rely on (Proxy/Reflect,
//! classes + private fields, generators/iterators, destructuring/spread, BigInt,
//! typed arrays, Symbols, modern Array/String/Object/RegExp methods) actually
//! works, and locks it in as a regression gate.
//!
//! This is a curated subset in the Test262 *style*, run offline/CI-friendly with
//! no network — NOT the upstream tc39/test262 corpus. Vendoring a slice of the
//! real corpus (with its frontmatter + $262 host harness) is a follow-up (#41).

use cerberus_js::{JsEngineFactory, JsError};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

/// The Test262 assertion harness (a faithful subset of upstream `sta.js` +
/// `assert.js`): `Test262Error`, `assert`, `assert.sameValue`/`notSameValue`
/// (with the spec SameValue, so `NaN` matches and `+0`/`-0` differ), and
/// `assert.throws`.
const HARNESS: &str = r#"
function Test262Error(message) { this.message = message || ""; }
Test262Error.prototype.toString = function () { return "Test262Error: " + this.message; };
function assert(mustBeTrue, message) {
  if (mustBeTrue === true) return;
  throw new Test262Error("assert failed: " + (message === undefined ? "" : message));
}
assert._isSameValue = function (a, b) {
  if (a === b) return a !== 0 || 1 / a === 1 / b; // distinguish +0 / -0
  return a !== a && b !== b;                       // both NaN
};
assert.sameValue = function (actual, expected, message) {
  if (assert._isSameValue(actual, expected)) return;
  throw new Test262Error("sameValue: expected " + String(expected) +
    ", got " + String(actual) + (message ? " (" + message + ")" : ""));
};
assert.notSameValue = function (actual, unexpected, message) {
  if (!assert._isSameValue(actual, unexpected)) return;
  throw new Test262Error("notSameValue: unexpectedly " + String(actual) +
    (message ? " (" + message + ")" : ""));
};
assert.throws = function (ctor, func, message) {
  if (typeof func !== "function") throw new Test262Error("assert.throws: not a function");
  try { func(); } catch (e) {
    if (e instanceof ctor) return;
    throw new Test262Error("assert.throws: wrong error type" + (message ? " (" + message + ")" : ""));
  }
  throw new Test262Error("assert.throws: no exception" + (message ? " (" + message + ")" : ""));
};
"#;

/// `(name, source)` conformance cases. Each asserts via the harness and throws a
/// `Test262Error` on failure.
const TESTS: &[(&str, &str)] = &[
    (
        "proxy-get-set-has-traps",
        r#"
        var log = [];
        var p = new Proxy({}, {
          get: function (t, k) { log.push("get:" + k); return k === "x" ? 42 : t[k]; },
          set: function (t, k, v) { log.push("set:" + k); t[k] = v; return true; },
          has: function (t, k) { return k === "x"; },
        });
        assert.sameValue(p.x, 42, "get trap");
        p.y = 9;
        assert.sameValue("x" in p, true, "has trap");
        assert.sameValue("z" in p, false, "has trap negative");
        assert.sameValue(log.indexOf("set:y") >= 0, true, "set trap ran");
        "#,
    ),
    (
        "reflect-operations",
        r#"
        var o = { a: 1 };
        assert.sameValue(Reflect.get(o, "a"), 1);
        assert.sameValue(Reflect.set(o, "b", 2), true);
        assert.sameValue(o.b, 2);
        assert.sameValue(Reflect.has(o, "a"), true);
        assert.sameValue(Reflect.ownKeys(o).join(","), "a,b");
        assert.sameValue(Reflect.deleteProperty(o, "a"), true);
        assert.sameValue("a" in o, false);
        "#,
    ),
    (
        "accessor-defineProperty",
        r#"
        var o = {}; var store = 0;
        Object.defineProperty(o, "v", {
          get: function () { return store; },
          set: function (x) { store = x * 2; },
          configurable: true, enumerable: true,
        });
        o.v = 21;
        assert.sameValue(o.v, 42, "getter/setter");
        var d = Object.getOwnPropertyDescriptor(o, "v");
        assert.sameValue(typeof d.get, "function");
        "#,
    ),
    (
        "class-inheritance-and-static",
        r#"
        class A { constructor(x) { this.x = x; } hi() { return "A" + this.x; } static make() { return new A(7); } }
        class B extends A { constructor(x) { super(x); } hi() { return "B" + super.hi(); } }
        var b = new B(3);
        assert.sameValue(b.hi(), "BA3", "super dispatch");
        assert.sameValue(b instanceof A, true, "instanceof base");
        assert.sameValue(A.make().x, 7, "static method");
        "#,
    ),
    (
        "class-private-fields-and-methods",
        r#"
        class Counter {
          #n = 0;
          #step() { this.#n += 1; }
          inc() { this.#step(); return this.#n; }
          static has(o) { return #n in o; }
        }
        var c = new Counter();
        assert.sameValue(c.inc(), 1);
        assert.sameValue(c.inc(), 2);
        assert.sameValue(Counter.has(c), true, "ergonomic #n in o");
        assert.sameValue(Counter.has({}), false);
        "#,
    ),
    (
        "generators-and-iterator-return",
        r#"
        function* g() { yield 1; yield 2; return 3; yield 4; }
        var it = g();
        assert.sameValue(it.next().value, 1);
        assert.sameValue(it.next().value, 2);
        var r = it.next();
        assert.sameValue(r.value, 3); assert.sameValue(r.done, true);
        assert.sameValue(it.next().done, true);
        assert.sameValue([...g()].join(","), "1,2", "spread consumes yields only");
        "#,
    ),
    (
        "custom-iterator-protocol",
        r#"
        var range = {
          from: 1, to: 3,
          [Symbol.iterator]() {
            var cur = this.from, last = this.to;
            return { next() { return cur <= last ? { value: cur++, done: false } : { value: undefined, done: true }; } };
          }
        };
        assert.sameValue([...range].join(","), "1,2,3");
        var sum = 0; for (var v of range) sum += v;
        assert.sameValue(sum, 6);
        "#,
    ),
    (
        "destructuring-defaults-and-rest",
        r#"
        var [a, b = 10, ...rest] = [1, undefined, 3, 4];
        assert.sameValue(a, 1); assert.sameValue(b, 10);
        assert.sameValue(rest.join(","), "3,4");
        var { p, q: { r } = { r: 5 }, ...others } = { p: 1, x: 2, y: 3 };
        assert.sameValue(p, 1); assert.sameValue(r, 5);
        assert.sameValue(Object.keys(others).join(","), "x,y");
        var t = [1, 2]; [t[0], t[1]] = [t[1], t[0]];
        assert.sameValue(t.join(","), "2,1", "swap");
        "#,
    ),
    (
        "spread-rest-and-object-spread",
        r#"
        function sum(...xs) { return xs.reduce(function (a, b) { return a + b; }, 0); }
        assert.sameValue(sum(...[1, 2, 3], 4), 10);
        var merged = { a: 1, ...{ b: 2, c: 3 }, c: 9 };
        assert.sameValue(merged.a + merged.b + merged.c, 12, "later key wins");
        assert.sameValue([0, ...[1, 2], 3].join(","), "0,1,2,3");
        "#,
    ),
    (
        "template-and-tagged-literals",
        r#"
        var x = 2;
        assert.sameValue(`v=${x + 1}`, "v=3");
        function tag(strings, ...vals) { return strings.raw.join("|") + "~" + vals.join(","); }
        assert.sameValue(tag`a\n${1}b${2}`, "a\\n|b|~1,2", "raw strings + substitutions");
        "#,
    ),
    (
        "optional-chaining-and-nullish",
        r#"
        var o = { a: { b: null } };
        assert.sameValue(o?.a?.b ?? "fallback", "fallback");
        assert.sameValue(o?.a?.c?.d, undefined);
        assert.sameValue(o?.missing?.(), undefined, "optional call");
        assert.sameValue((0 ?? 1), 0, "nullish keeps 0");
        assert.sameValue((null ?? 5), 5);
        "#,
    ),
    (
        "bigint-arithmetic",
        r#"
        var big = 9007199254740993n; // 2^53 + 1, inexact as a double
        assert.sameValue(big + 1n, 9007199254740994n);
        assert.sameValue(typeof big, "bigint");
        assert.sameValue(2n ** 64n, 18446744073709551616n);
        assert.sameValue(10n / 3n, 3n, "bigint division truncates");
        "#,
    ),
    (
        "typed-arrays-and-dataview",
        r#"
        var buf = new ArrayBuffer(8);
        var u8 = new Uint8Array(buf);
        u8[0] = 255; u8[1] = 1;
        assert.sameValue(u8[0], 255);
        var dv = new DataView(buf);
        dv.setUint16(2, 0x0102, false); // big-endian
        assert.sameValue(dv.getUint8(2), 0x01);
        assert.sameValue(dv.getUint8(3), 0x02);
        var i32 = Int32Array.from([1, 2, 3]);
        assert.sameValue(i32.reduce(function (a, b) { return a + b; }, 0), 6);
        "#,
    ),
    (
        "symbols-and-well-known",
        r#"
        var s = Symbol("k");
        assert.sameValue(typeof s, "symbol");
        var o = {}; o[s] = 1;
        assert.sameValue(o[s], 1);
        assert.sameValue(Object.getOwnPropertySymbols(o).length, 1);
        var obj = { [Symbol.toPrimitive](hint) { return hint === "number" ? 42 : "str"; } };
        assert.sameValue(+obj, 42, "Symbol.toPrimitive number");
        assert.sameValue(`${obj}`, "str", "Symbol.toPrimitive string");
        "#,
    ),
    (
        "map-set-weakmap",
        r#"
        var m = new Map([["a", 1]]); m.set("b", 2);
        assert.sameValue(m.get("a"), 1); assert.sameValue(m.size, 2);
        assert.sameValue([...m.keys()].join(","), "a,b", "insertion order");
        var st = new Set([1, 1, 2, 3]);
        assert.sameValue(st.size, 3);
        var wm = new WeakMap(); var key = {}; wm.set(key, "v");
        assert.sameValue(wm.get(key), "v"); assert.sameValue(wm.has({}), false);
        "#,
    ),
    (
        "json-roundtrip-with-reviver-replacer",
        r#"
        var src = { a: 1, b: [2, 3], c: "x" };
        var s = JSON.stringify(src);
        var back = JSON.parse(s, function (k, v) { return typeof v === "number" ? v * 10 : v; });
        assert.sameValue(back.a, 10); assert.sameValue(back.b[1], 30);
        assert.sameValue(JSON.stringify(src, ["a", "c"]), '{"a":1,"c":"x"}', "array replacer");
        "#,
    ),
    (
        "array-methods-modern",
        r#"
        assert.sameValue([1, [2, [3]]].flat(Infinity).join(","), "1,2,3");
        assert.sameValue([1, 2].flatMap(function (x) { return [x, x * 2]; }).join(","), "1,2,2,4");
        assert.sameValue([5, 6, 7].find(function (x) { return x > 5; }), 6);
        assert.sameValue([1, 2, 3].includes(2), true);
        assert.sameValue(Array.from("ab").join(","), "a,b");
        assert.sameValue(Array.of(1, 2, 3).length, 3);
        assert.sameValue([3, 1, 2].sort(function (a, b) { return a - b; }).join(","), "1,2,3");
        assert.sameValue([1, 2, 3, 4].at(-1), 4, "Array.prototype.at");
        "#,
    ),
    (
        "string-methods-modern",
        r#"
        assert.sameValue("5".padStart(3, "0"), "005");
        assert.sameValue("a-b-c".replaceAll("-", "+"), "a+b+c");
        assert.sameValue("café".normalize("NFC").length, 4);
        assert.sameValue("abc".at(-1), "c");
        assert.sameValue([..."a😀b"].length, 3, "code-point iteration");
        assert.sameValue(Array.from("x1x2".matchAll(/x(\d)/g), function (m) { return m[1]; }).join(","), "1,2");
        "#,
    ),
    (
        "number-and-math",
        r#"
        assert.sameValue(Number.isInteger(3.0), true);
        assert.sameValue(Number.isNaN(NaN), true);
        assert.sameValue(Number.parseFloat("3.14abc"), 3.14);
        assert.sameValue(Math.trunc(-4.7), -4);
        assert.sameValue(Math.sign(-2), -1);
        assert.sameValue(Math.hypot(3, 4), 5);
        assert.sameValue((255).toString(16), "ff");
        "#,
    ),
    (
        "regexp-named-groups-and-lookbehind",
        r#"
        var m = /(?<year>\d{4})-(?<mon>\d{2})/.exec("2026-06");
        assert.sameValue(m.groups.year, "2026");
        assert.sameValue(m.groups.mon, "06");
        assert.sameValue("$5.00".match(/(?<=\$)\d+/)[0], "5", "lookbehind");
        var re = /a/g; re.lastIndex = 0;
        assert.sameValue("aaa".replace(/a/g, "b"), "bbb");
        "#,
    ),
    (
        "object-methods-modern",
        r#"
        var o = { a: 1, b: 2 };
        assert.sameValue(Object.entries(o).map(function (e) { return e.join(":"); }).join(","), "a:1,b:2");
        assert.sameValue(Object.values(o).join(","), "1,2");
        var fe = Object.fromEntries([["x", 1], ["y", 2]]);
        assert.sameValue(fe.x + fe.y, 3);
        var t = Object.assign({}, o, { c: 3 });
        assert.sameValue(Object.keys(t).length, 3);
        "#,
    ),
    (
        "let-const-block-scope-and-tdz",
        r#"
        var outer = 1; { let outer = 2; assert.sameValue(outer, 2); } assert.sameValue(outer, 1);
        assert.throws(TypeError, function () { const k = 1; k = 2; }, "const reassign");
        assert.throws(ReferenceError, function () { x; let x = 1; return x; }, "TDZ");
        "#,
    ),
    (
        "default-and-computed-properties",
        r#"
        function f(a, b = a * 2) { return a + b; }
        assert.sameValue(f(3), 9);
        var k = "dyn";
        var o = { [k + "1"]: 1, [`${k}2`]: 2 };
        assert.sameValue(o.dyn1 + o.dyn2, 3);
        var o2 = { get half() { return 5; }, set half(_) {} };
        assert.sameValue(o2.half, 5);
        "#,
    ),
    (
        "promise-and-async-shape",
        r#"
        assert.sameValue(typeof Promise, "function");
        assert.sameValue(Promise.resolve(1) instanceof Promise, true);
        var af = async function () { return 1; };
        assert.sameValue(af() instanceof Promise, true, "async returns a promise");
        assert.sameValue(typeof Promise.all, "function");
        assert.sameValue(typeof Promise.allSettled, "function");
        "#,
    ),
    (
        "error-types-and-custom-errors",
        r#"
        assert.sameValue(new TypeError("x") instanceof Error, true);
        assert.sameValue(new RangeError() instanceof RangeError, true);
        class MyError extends Error { constructor(m) { super(m); this.name = "MyError"; } }
        var e = new MyError("boom");
        assert.sameValue(e instanceof Error, true);
        assert.sameValue(e.name, "MyError");
        assert.sameValue(e.message, "boom");
        assert.throws(TypeError, function () { null.x; });
        "#,
    ),
    (
        "labeled-loops-break-continue",
        r#"
        var hits = [];
        outer: for (var i = 0; i < 3; i++) {
          for (var j = 0; j < 3; j++) {
            if (j === 1) continue outer;
            if (i === 2) break outer;
            hits.push(i + "" + j);
          }
        }
        assert.sameValue(hits.join(","), "00,10");
        "#,
    ),
    (
        "getters-in-classes-and-tostring-tag",
        r#"
        class Temp {
          constructor(c) { this._c = c; }
          get fahrenheit() { return this._c * 9 / 5 + 32; }
          set fahrenheit(f) { this._c = (f - 32) * 5 / 9; }
          get [Symbol.toStringTag]() { return "Temp"; }
        }
        var t = new Temp(100);
        assert.sameValue(t.fahrenheit, 212);
        t.fahrenheit = 32; assert.sameValue(t._c, 0);
        assert.sameValue(Object.prototype.toString.call(t), "[object Temp]");
        "#,
    ),
];

#[test]
fn test262_subset_conformance() {
    let mut engine = QuickJsEngineFactory
        .instantiate()
        .expect("instantiate engine");

    let mut passed = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (i, (name, code)) in TESTS.iter().enumerate() {
        let realm = RealmId::from_u64_pair(0, i as u64 + 1);
        engine.create_realm(realm).expect("create realm");
        let program = format!("{HARNESS}\n{code}\n");
        match engine.eval(realm, &program) {
            Ok(_) => passed += 1,
            Err(JsError::Eval(msg)) => failures.push(format!("  [{name}] {msg}")),
            Err(other) => failures.push(format!("  [{name}] infra error: {other:?}")),
        }
        let _ = engine.destroy_realm(realm);
    }

    let total = TESTS.len();
    eprintln!("Test262 subset: {passed}/{total} passed");
    assert!(
        failures.is_empty(),
        "{}/{} Test262-subset conformance tests failed:\n{}",
        failures.len(),
        total,
        failures.join("\n")
    );
}
