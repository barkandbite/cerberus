//! The DOM model presents ONE coherent identity per window when a
//! [`cerberus_profile::Profile`] is injected ahead of it (the head manager wires
//! this for every head). These tests pin the two defects an adversarial review
//! confirmed on the pre-profile surface:
//!
//!   * a *split-brain identity* — `navigator.userAgent`/`platform` tracked the
//!     honest env UA while `userAgentData`/WebGL were hardcoded Chrome-on-Windows,
//!     so the OS axes disagreed; and
//!   * *impossible geometry* — `outerHeight > screen.height` (a window taller than
//!     its monitor).
//!
//! With a profile, every axis is read from the one persona, so they cannot
//! disagree, and the screen is a real monitor larger than the viewport so the
//! `screen >= avail >= outer >= inner` invariant holds with real browser chrome.

use cerberus_dom::parse_html;
use cerberus_js::{JsEngine, JsEngineFactory, JsValue};
use cerberus_js_dom::{install_page, PageEnv};
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_profile::{derive_profile, Profile, ProfileOverrides};
use cerberus_types::RealmId;

/// A spread of seeds. Archetypes are market-share weighted, so this set covers
/// several device classes (Windows/Chrome, macOS, a Firefox class with no
/// UA-CH); each test asserts against whatever persona a seed actually derives.
const SEEDS: [u64; 8] = [
    0x1,
    0x2,
    0x3,
    0x9E37_79B9,
    0xABCD_1234_5678,
    0xFFFF_0000_FFFF_0001,
    0xDEAD_BEEF_CAFE_F00D,
    0x0123_4567_89AB_CDEF,
];

fn eval_str(engine: &mut dyn JsEngine, realm: RealmId, src: &str) -> String {
    match engine.eval(realm, src).expect("eval") {
        JsValue::Str(s) => s,
        other => panic!("expected string from {src:?}, got {other:?}"),
    }
}

/// Install a realm whose persona comes from `profile` (its prologue injected
/// before the DOM model, exactly as the head manager wires a real head). Passing
/// `None` exercises the no-profile fallback surface.
fn install_with_profile(profile: Option<&Profile>) -> (Box<dyn JsEngine>, RealmId) {
    let mut engine = QuickJsEngineFactory.instantiate().expect("instantiate");
    let realm = RealmId::from_u64_pair(0, 1);
    engine.create_realm(realm).expect("create realm");
    if let Some(p) = profile {
        engine
            .inject_prologue(realm, &p.profile_prologue())
            .expect("profile prologue");
    }
    let doc = parse_html("<html><body><div id=x></div></body></html>");
    let env = PageEnv {
        url: "https://example.test/".into(),
        viewport: (1280, 800),
        user_agent: "Cerberus/0.0".into(),
        cookie: String::new(),
    };
    install_page(engine.as_mut(), realm, &doc, &env).expect("install page");
    (engine, realm)
}

#[test]
fn navigator_surface_is_driven_by_the_injected_profile() {
    for &seed in &SEEDS {
        let profile = derive_profile(seed, &ProfileOverrides::default());
        let (mut engine, realm) = install_with_profile(Some(&profile));
        let e = engine.as_mut();

        assert_eq!(
            eval_str(e, realm, "navigator.userAgent"),
            profile.user_agent,
            "seed {seed:#x}: userAgent must be the profile's"
        );
        assert_eq!(
            eval_str(e, realm, "navigator.platform").as_str(),
            profile.platform,
            "seed {seed:#x}: platform must be the profile's"
        );
        assert_eq!(
            eval_str(e, realm, "String(navigator.hardwareConcurrency)"),
            profile.hardware_concurrency.to_string(),
            "seed {seed:#x}: hardwareConcurrency must be the profile's"
        );

        match &profile.ua_data {
            Some(ua) => {
                assert_eq!(
                    eval_str(e, realm, "typeof navigator.userAgentData"),
                    "object",
                    "seed {seed:#x}: a Chromium persona must expose UA-CH"
                );
                assert_eq!(
                    eval_str(e, realm, "navigator.userAgentData.platform").as_str(),
                    ua.platform,
                    "seed {seed:#x}: UA-CH platform must be the profile's"
                );
            }
            None => {
                // A Firefox-class persona exposes no UA-CH at all — exposing it
                // (asserting Chromium) over a Firefox UA is itself a tell.
                assert_eq!(
                    eval_str(e, realm, "typeof navigator.userAgentData"),
                    "undefined",
                    "seed {seed:#x}: a non-Chromium persona must NOT expose UA-CH"
                );
            }
        }
    }
}

#[test]
fn os_axes_never_disagree_the_split_brain_is_closed() {
    // The exact CRITICAL cross-check an anti-bot sensor runs: the OS implied by
    // navigator.userAgent, by navigator.platform, and by
    // navigator.userAgentData.platform must all name the same OS.
    const PROBE: &str = r#"(function(){
        function osUA(u){
            if(u.indexOf("Windows")>=0)return "Windows";
            if(u.indexOf("Mac OS X")>=0||u.indexOf("Macintosh")>=0)return "macOS";
            if(u.indexOf("Linux")>=0||u.indexOf("X11")>=0)return "Linux";
            return "?ua:"+u;
        }
        function osPlat(p){
            if(p.indexOf("Win")>=0)return "Windows";
            if(p==="MacIntel")return "macOS";
            if(p.indexOf("Linux")>=0)return "Linux";
            return "?plat:"+p;
        }
        var a=osUA(navigator.userAgent), b=osPlat(navigator.platform);
        // Firefox exposes no UA-CH; treat its absence as consistent.
        var c=navigator.userAgentData ? navigator.userAgentData.platform : b;
        return (a===b && b===c) ? "coherent" : (a+"|"+b+"|"+c);
    })()"#;

    for &seed in &SEEDS {
        let profile = derive_profile(seed, &ProfileOverrides::default());
        let (mut engine, realm) = install_with_profile(Some(&profile));
        assert_eq!(
            eval_str(engine.as_mut(), realm, PROBE),
            "coherent",
            "seed {seed:#x}: the OS axes disagree (split-brain)"
        );
    }
}

#[test]
fn high_entropy_client_hints_agree_with_the_static_surface() {
    // getHighEntropyValues() resolves a promise; it drains in the engine's
    // post-eval job pump, so trigger first and read the captured result next.
    for &seed in &SEEDS {
        let profile = derive_profile(seed, &ProfileOverrides::default());
        let Some(ua) = profile.ua_data.clone() else {
            continue; // Firefox: no UA-CH to probe.
        };
        let (mut engine, realm) = install_with_profile(Some(&profile));
        let e = engine.as_mut();
        e.eval(
            realm,
            "globalThis.__hep=''; \
             navigator.userAgentData.getHighEntropyValues(['platform','architecture','bitness']) \
               .then(function(v){ globalThis.__hep = v.platform+'|'+v.architecture+'|'+v.bitness; });",
        )
        .expect("trigger");
        let hep = eval_str(e, realm, "globalThis.__hep");
        assert_eq!(
            hep,
            format!("{}|{}|{}", ua.platform, ua.architecture, ua.bitness),
            "seed {seed:#x}: high-entropy hints must match the static UA-CH"
        );
    }
}

#[test]
fn window_geometry_is_coherent_and_has_real_chrome_under_a_profile() {
    // screen >= avail >= outer >= inner on both axes, the window fits under the
    // monitor, real vertical chrome is present, and the screen is a real monitor
    // at least as large as the viewport (never screen == viewport on both axes).
    const PROBE: &str = r#"(function(){
        var s=screen, w=window;
        var inv = (w.innerHeight<=w.outerHeight) && (w.outerHeight<=s.availHeight) && (s.availHeight<=s.height)
               && (w.innerWidth<=w.outerWidth)  && (w.outerWidth<=s.availWidth)   && (s.availWidth<=s.width)
               && ((w.screenY|0)+w.outerHeight<=s.height);
        return inv
            ? ("ok:"+(w.outerHeight-w.innerHeight)+":"+(s.height>w.innerHeight)+":"+(s.width>=w.innerWidth))
            : ("bad:inner="+w.innerHeight+" outer="+w.outerHeight+" avail="+s.availHeight+" screen="+s.height);
    })()"#;

    for &seed in &SEEDS {
        let profile = derive_profile(seed, &ProfileOverrides::default());
        let (mut engine, realm) = install_with_profile(Some(&profile));
        let r = eval_str(engine.as_mut(), realm, PROBE);
        assert!(r.starts_with("ok:"), "seed {seed:#x}: {r}");
        let parts: Vec<&str> = r.split(':').collect();
        assert!(
            parts[1].parse::<i64>().expect("chrome delta") > 0,
            "seed {seed:#x}: expected real vertical chrome, got {r}"
        );
        assert_eq!(
            parts[2], "true",
            "seed {seed:#x}: screen.height must exceed innerHeight ({r})"
        );
        assert_eq!(
            parts[3], "true",
            "seed {seed:#x}: screen.width must be >= innerWidth ({r})"
        );
    }
}

#[test]
fn no_profile_fallback_geometry_is_self_consistent() {
    // With no profile the surface is an inert maximized viewport: screen == avail
    // == outer == inner (no chrome). Self-consistent — never a window taller than
    // its screen.
    const PROBE: &str = r#"(function(){
        var s=screen, w=window;
        var inv = (w.innerHeight===w.outerHeight) && (w.outerHeight===s.availHeight) && (s.availHeight===s.height)
               && (w.innerWidth===w.outerWidth)  && (w.outerWidth===s.availWidth)   && (s.availWidth===s.width);
        return inv ? "ok" : ("bad:inner="+w.innerHeight+" outer="+w.outerHeight+" avail="+s.availHeight+" screen="+s.height);
    })()"#;
    let (mut engine, realm) = install_with_profile(None);
    assert_eq!(eval_str(engine.as_mut(), realm, PROBE), "ok");
}
