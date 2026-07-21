//! Committed, always-on coverage for the Web IDL / Web API additions surfaced by
//! the WPT worker run: DOMException + QuotaExceededError, Event/EventTarget,
//! performance-as-EventTarget, TextDecoder utf-16, crypto.getRandomValues
//! validation, atob InvalidCharacterError, and the URL/URLSearchParams parser.
//! These mirror the assertions the real WPT tests make, so they guard the
//! behaviour even without network access to testharness.

use cerberus_dom::{Document, DocumentBuilder};
use cerberus_js::{JsEngine, JsEngineFactory, JsValue};
use cerberus_js_dom::{install_page, PageEnv};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_types::RealmId;

fn engine() -> (Box<dyn JsEngine>, RealmId) {
    let mut e = QuickJsEngineFactory.instantiate().expect("engine");
    let realm = RealmId::from_u64_pair(0, 1);
    e.create_realm(realm).expect("realm");
    let mut b = DocumentBuilder::new();
    let body = b.element("body", []);
    let html = b.element("html", [body]);
    let doc: Document = b.finish(html);
    let env = PageEnv {
        url: "https://example.test/a/b/page?q=1".into(),
        viewport: (1280, 800),
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36"
            .into(),
        cookie: String::new(),
    };
    install_page(e.as_mut(), realm, &doc, &env).expect("install");
    (e, realm)
}

/// Eval a JS expression expected to yield the string "true".
fn ok(e: &mut dyn JsEngine, realm: RealmId, expr: &str) {
    let js = format!("(function () {{ try {{ return ({expr}) ? 'true' : 'false'; }} catch (e) {{ return 'threw: ' + e; }} }})()");
    let got = e.eval(realm, &js).expect("eval");
    assert_eq!(
        got,
        JsValue::Str("true".into()),
        "expected true for: {expr}"
    );
}

#[test]
fn dom_exception_and_quota_exceeded() {
    let (mut e, r) = engine();
    ok(
        e.as_mut(),
        r,
        "new DOMException('m', 'TypeMismatchError') instanceof Error",
    );
    ok(
        e.as_mut(),
        r,
        "new DOMException('m', 'TypeMismatchError').name === 'TypeMismatchError'",
    );
    ok(
        e.as_mut(),
        r,
        "new DOMException('m', 'TypeMismatchError').code === 17",
    );
    ok(
        e.as_mut(),
        r,
        "new DOMException('m', 'InvalidCharacterError').code === 5",
    );
    ok(
        e.as_mut(),
        r,
        "new QuotaExceededError('m') instanceof DOMException",
    );
    ok(
        e.as_mut(),
        r,
        "new QuotaExceededError('m').name === 'QuotaExceededError'",
    );
    ok(e.as_mut(), r, "new QuotaExceededError('m').code === 22");
}

#[test]
fn get_random_values_validates_like_the_spec() {
    let (mut e, r) = engine();
    // Fills and returns integer views.
    ok(
        e.as_mut(),
        r,
        "crypto.getRandomValues(new Uint8Array(8)).constructor === Uint8Array",
    );
    ok(
        e.as_mut(),
        r,
        "crypto.getRandomValues(new BigInt64Array(4)).constructor === BigInt64Array",
    );
    ok(
        e.as_mut(),
        r,
        "crypto.getRandomValues(new Uint8Array(0)).length === 0",
    );
    // Subclasses of an integer view are accepted (tag-based, not name-based).
    ok(e.as_mut(), r, "(function () { class B extends Uint8Array {} crypto.getRandomValues(new B(4)); return true; })()");
    // Float / DataView reject with TypeMismatchError.
    ok(e.as_mut(), r, "(function () { try { crypto.getRandomValues(new Float32Array(4)); return false; } catch (x) { return x.name === 'TypeMismatchError'; } })()");
    ok(e.as_mut(), r, "(function () { try { crypto.getRandomValues(new DataView(new ArrayBuffer(4))); return false; } catch (x) { return x.name === 'TypeMismatchError'; } })()");
    // > 65536 bytes rejects with QuotaExceededError (name + code, null quota).
    ok(e.as_mut(), r, "(function () { try { crypto.getRandomValues(new Uint8Array(65537)); return false; } catch (x) { return x.name === 'QuotaExceededError' && x.code === 22 && x.quota === null && x.requested === null; } })()");
}

#[test]
fn text_decoder_utf16() {
    let (mut e, r) = engine();
    // 'z' U+00A2 U+6C34 in utf-16le / utf-16be.
    ok(e.as_mut(), r, "new TextDecoder('utf-16le').decode(new Uint8Array([0x7A,0x00,0xA2,0x00,0x34,0x6C])) === 'z\\u00A2\\u6C34'");
    ok(e.as_mut(), r, "new TextDecoder('utf-16be').decode(new Uint8Array([0x00,0x7A,0x00,0xA2,0x6C,0x34])) === 'z\\u00A2\\u6C34'");
    // Accepts a bare ArrayBuffer too.
    ok(
        e.as_mut(),
        r,
        "new TextDecoder('utf-16le').decode(new Uint8Array([0x7A,0x00]).buffer) === 'z'",
    );
    // A leading BOM is stripped.
    ok(
        e.as_mut(),
        r,
        "new TextDecoder('utf-16le').decode(new Uint8Array([0xFF,0xFE,0x7A,0x00])) === 'z'",
    );
    // {ignoreBOM:true} preserves the BOM.
    ok(e.as_mut(), r, "new TextDecoder('utf-16le', { ignoreBOM: true }).decode(new Uint8Array([0xFF,0xFE,0x41,0x00])) === '\\uFEFFA'");
    // utf-8 leading BOM (EF BB BF) is stripped.
    ok(
        e.as_mut(),
        r,
        "new TextDecoder().decode(new Uint8Array([0xEF,0xBB,0xBF,0x41])) === 'A'",
    );
    // A dangling odd byte becomes U+FFFD.
    ok(
        e.as_mut(),
        r,
        "new TextDecoder('utf-16le').decode(new Uint8Array([0x41,0x00,0x42])) === 'A\\uFFFD'",
    );
    // A lone (unpaired) lead surrogate becomes U+FFFD.
    ok(
        e.as_mut(),
        r,
        "new TextDecoder('utf-16le').decode(new Uint8Array([0x3D,0xD8,0x41,0x00])) === '\\uFFFDA'",
    );
    // A valid surrogate pair round-trips.
    ok(e.as_mut(), r, "new TextDecoder('utf-16le').decode(new Uint8Array([0x3D,0xD8,0x1E,0xDD])) === '\\uD83D\\uDD1E'");
}

#[test]
fn performance_is_an_event_target() {
    let (mut e, r) = engine();
    ok(
        e.as_mut(),
        r,
        "(function () { var hit = false; performance.addEventListener('t', function () { hit = true; }, { once: true }); performance.dispatchEvent(new Event('t')); return hit; })()",
    );
}

#[test]
fn atob_throws_invalid_character_error() {
    let (mut e, r) = engine();
    ok(e.as_mut(), r, "atob('aGVsbG8=') === 'hello'");
    ok(e.as_mut(), r, "(function () { try { atob('a===='); return false; } catch (x) { return x.name === 'InvalidCharacterError'; } })()");
    ok(e.as_mut(), r, "(function () { try { btoa('\\u{1F600}'); return false; } catch (x) { return x.name === 'InvalidCharacterError'; } })()");
    // Forgiving-base64: padded strings that aren't length%4==0 (or have surplus
    // '=') must FAIL, not silently decode.
    ok(e.as_mut(), r, "(function () { try { atob('ab='); return false; } catch (x) { return x.name === 'InvalidCharacterError'; } })()");
    ok(e.as_mut(), r, "(function () { try { atob('===='); return false; } catch (x) { return x.name === 'InvalidCharacterError'; } })()");
    ok(e.as_mut(), r, "(function () { try { atob('YQ======'); return false; } catch (x) { return x.name === 'InvalidCharacterError'; } })()");
    ok(e.as_mut(), r, "atob('YQ==') === 'a'");
}

#[test]
fn event_target_dom_semantics() {
    let (mut e, r) = engine();
    // Duplicate identical listener is a no-op.
    ok(e.as_mut(), r, "(function () { var t = new EventTarget(); var n = 0; function h() { n++; } t.addEventListener('x', h); t.addEventListener('x', h); t.dispatchEvent(new Event('x')); return n === 1; })()");
    // handleEvent objects are called with `this` = the listener object.
    ok(e.as_mut(), r, "(function () { var t = new EventTarget(); var got; var o = { id: 42, handleEvent: function () { got = this && this.id; } }; t.addEventListener('x', o); t.dispatchEvent(new Event('x')); return got === 42; })()");
    // A listener removed during dispatch by an earlier one is not invoked.
    ok(e.as_mut(), r, "(function () { var t = new EventTarget(); var order = []; function a() { order.push('a'); t.removeEventListener('x', b); } function b() { order.push('b'); } t.addEventListener('x', a); t.addEventListener('x', b); t.dispatchEvent(new Event('x')); return order.join(',') === 'a'; })()");
    // currentTarget is cleared after dispatch.
    ok(e.as_mut(), r, "(function () { var t = new EventTarget(); var ev = new Event('x'); t.addEventListener('x', function () {}); t.dispatchEvent(ev); return ev.currentTarget === null; })()");
}

#[test]
fn url_parses_and_resolves() {
    let (mut e, r) = engine();
    ok(
        e.as_mut(),
        r,
        "new URL('https://User@Example.COM:443/a/b?x=1&y=2#f').origin === 'https://example.com'",
    );
    ok(
        e.as_mut(),
        r,
        "new URL('https://example.com:8080/p').host === 'example.com:8080'",
    );
    ok(
        e.as_mut(),
        r,
        "new URL('https://example.com/a/b?x=1&y=2').searchParams.get('y') === '2'",
    );
    ok(
        e.as_mut(),
        r,
        "new URL('https://h/a/b/page').pathname === '/a/b/page'",
    );
    // Relative resolution against a base.
    ok(e.as_mut(), r, "new URL('../c?z=9', 'https://example.com/a/b/page').href === 'https://example.com/a/c?z=9'");
    ok(
        e.as_mut(),
        r,
        "new URL('/x', 'https://example.com/a/b').href === 'https://example.com/x'",
    );
    ok(
        e.as_mut(),
        r,
        "new URL('//other.com/p', 'https://example.com/a').host === 'other.com'",
    );
    // Default ports are dropped; searchParams round-trips.
    ok(e.as_mut(), r, "new URL('http://h:80/').host === 'h'");
    ok(
        e.as_mut(),
        r,
        "new URLSearchParams('a=1&b=2').toString() === 'a=1&b=2'",
    );
    // Backslashes normalize to slashes for special schemes.
    ok(
        e.as_mut(),
        r,
        "new URL('http://example.com\\\\foo\\\\bar').pathname === '/foo/bar'",
    );
    // Relative against an opaque base doesn't fabricate an authority.
    ok(
        e.as_mut(),
        r,
        "new URL('#x', 'mailto:a@b.com').href === 'mailto:a@b.com#x'",
    );
    // Empty-string relative drops the base fragment.
    ok(
        e.as_mut(),
        r,
        "new URL('', 'http://example.com/a?b#c').href === 'http://example.com/a?b'",
    );
    // blob: origin comes from the inner URL.
    ok(
        e.as_mut(),
        r,
        "new URL('blob:https://example.com/uuid').origin === 'https://example.com'",
    );
    // A special same-scheme relative reference resolves against the base.
    ok(
        e.as_mut(),
        r,
        "new URL('http:foo', 'http://example.com/bar').href === 'http://example.com/foo'",
    );
    // Leading-zero ports are numerically normalized (and dropped when default).
    ok(
        e.as_mut(),
        r,
        "new URL('https://example.com:00443/').host === 'example.com'",
    );
    // Space is percent-encoded in the path.
    ok(
        e.as_mut(),
        r,
        "new URL('http://example.com/foo bar').href === 'http://example.com/foo%20bar'",
    );
}
