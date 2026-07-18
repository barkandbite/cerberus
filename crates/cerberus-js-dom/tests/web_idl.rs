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
}
