//! End-to-end mirror-group tests over the **real** QuickJS engine and DOM
//! bridge (ADR-0017). They drive a master, catch followers up, and assert the
//! four properties that matter: convergence, session isolation, the
//! ≤1-live-engine invariant, and honest divergence.

use std::cell::RefCell;
use std::rc::Rc;

use cerberus_dom::{parse_html, Document};
use cerberus_js::JsEngineFactory;
use cerberus_js_quickjs::QuickJsEngineFactory;
use cerberus_mirror::{Action, MirrorGroup, PageSource, Target};
use cerberus_types::InstanceId;

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) Cerberus/0.0";
const URL: &str = "https://app.test/";

/// Build a page whose `#who` div bakes in the *session identity* (to prove
/// isolation) and whose button, when clicked, mutates `#out` (to prove
/// convergence). `with_button` is dropped for a session to simulate a divergent
/// DOM (logged-out / A-B variant).
fn page(instance: InstanceId, with_button: bool) -> String {
    let btn = if with_button {
        "<button id=\"btn\">Go</button>"
    } else {
        ""
    };
    format!(
        "<html><body>\
         <div id=\"who\">{instance}</div>\
         {btn}\
         <div id=\"out\">initial</div>\
         <script>\
         var b = document.getElementById('btn');\
         if (b) {{ b.addEventListener('click', function() {{ \
         document.getElementById('out').textContent = 'clicked'; }}); }}\
         </script>\
         </body></html>"
    )
}

/// A fake network: serves [`page`] per session and records which identities it
/// was asked for, so a test can assert sessions were loaded independently.
struct FakeSource {
    /// When false, only the master gets the button (followers diverge).
    follower_has_button: bool,
    master: InstanceId,
    seen: Rc<RefCell<Vec<(InstanceId, String)>>>,
}

impl PageSource for FakeSource {
    fn load(&self, instance: InstanceId, url: &str) -> Result<Document, String> {
        self.seen.borrow_mut().push((instance, url.to_string()));
        let with_button = self.follower_has_button || instance == self.master;
        Ok(parse_html(&page(instance, with_button)))
    }
}

fn group(
    follower_has_button: bool,
    seen: Rc<RefCell<Vec<(InstanceId, String)>>>,
) -> (MirrorGroup, InstanceId, InstanceId) {
    let master = InstanceId::from_u64_pair(0, 1);
    let follower = InstanceId::from_u64_pair(0, 2);
    let source = FakeSource {
        follower_has_button,
        master,
        seen,
    };
    let engine = QuickJsEngineFactory
        .instantiate()
        .expect("instantiate engine");
    let members = vec![
        (master, "master".to_string()),
        (follower, "follower".to_string()),
    ];
    let g = MirrorGroup::new(engine, Box::new(source), members, (1024, 768), UA).expect("group");
    (g, master, follower)
}

#[test]
fn follower_converges_to_master() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (mut g, _master, _follower) = group(true, seen);
    assert_eq!(g.live_realms(), 0, "engine starts with no realm");

    g.act(Action::Navigate(URL.into())).unwrap();
    assert_eq!(g.master().text_of_id("out").as_deref(), Some("initial"));
    assert!(g.live_realms() <= 1);

    g.act(Action::Click(Target::Id("btn".into()))).unwrap();
    assert_eq!(
        g.master().text_of_id("out").as_deref(),
        Some("clicked"),
        "the click handler ran in the master's realm"
    );
    assert!(g.live_realms() <= 1);

    // The follower is behind until focused; then it catches up to the master.
    assert_eq!(g.instance(1).unwrap().cursor(), 0);
    g.focus(1).unwrap();
    assert_eq!(
        g.instance(1).unwrap().text_of_id("out").as_deref(),
        Some("clicked"),
        "the follower replayed navigate + click in its own session"
    );
    assert_eq!(g.instance(1).unwrap().cursor(), g.log().len());
    assert!(g.instance(1).unwrap().diverged().is_none());
    assert_eq!(g.live_realms(), 1);
    assert_eq!(g.live_instances(), 1);
}

#[test]
fn sessions_stay_isolated() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (mut g, master_id, follower_id) = group(true, seen.clone());

    g.act(Action::Navigate(URL.into())).unwrap();
    g.act(Action::Click(Target::Id("btn".into()))).unwrap();
    g.focus(1).unwrap();

    // The shared action converged on both.
    assert_eq!(g.master().text_of_id("out").as_deref(), Some("clicked"));
    assert_eq!(
        g.instance(1).unwrap().text_of_id("out").as_deref(),
        Some("clicked")
    );

    // But each session shows its OWN identity — they never merged.
    let who_master = master_id.to_string();
    let who_follower = follower_id.to_string();
    assert_eq!(
        g.master().text_of_id("who").as_deref(),
        Some(who_master.as_str())
    );
    assert_eq!(
        g.instance(1).unwrap().text_of_id("who").as_deref(),
        Some(who_follower.as_str())
    );
    assert_ne!(who_master, who_follower);

    // The source was asked for both identities independently.
    let ids: Vec<InstanceId> = seen.borrow().iter().map(|(i, _)| *i).collect();
    assert!(ids.contains(&master_id), "master session loaded");
    assert!(ids.contains(&follower_id), "follower session loaded");
}

#[test]
fn at_most_one_live_realm_across_focus_switches() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (mut g, _master, _follower) = group(true, seen);

    assert!(g.live_realms() <= 1);
    g.act(Action::Navigate(URL.into())).unwrap();
    assert!(g.live_realms() <= 1);
    g.act(Action::Click(Target::Id("btn".into()))).unwrap();
    assert!(g.live_realms() <= 1);

    // Switch focus back and forth; the invariant must hold every time.
    for target in [1usize, 0, 1, 0] {
        g.focus(target).unwrap();
        assert!(g.live_realms() <= 1, "never more than one live realm");
        assert!(g.live_instances() <= 1, "never more than one live instance");
        assert_eq!(g.focused_index(), target);
        // The focused instance is always converged to the head of the log.
        assert_eq!(g.instance(target).unwrap().cursor(), g.log().len());
    }
}

#[test]
fn follower_without_target_flags_divergence() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (mut g, _master, _follower) = group(false, seen); // follower has no #btn

    g.act(Action::Navigate(URL.into())).unwrap();
    g.act(Action::Click(Target::Id("btn".into()))).unwrap();
    assert!(g.master().diverged().is_none(), "master has the button");

    g.focus(1).unwrap();
    let div = g
        .instance(1)
        .unwrap()
        .diverged()
        .expect("follower cannot click a button it does not have");
    assert_eq!(div.action, Action::Click(Target::Id("btn".into())));

    // The follower still navigated (page loaded) but did not apply the click.
    assert_eq!(
        g.instance(1).unwrap().text_of_id("out").as_deref(),
        Some("initial")
    );
    // The master, meanwhile, stayed converged.
    assert_eq!(g.master().text_of_id("out").as_deref(), Some("clicked"));
}

#[test]
fn dormant_instances_release_dom_and_rematerialize_on_focus() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (mut g, _master, _follower) = group(true, seen);

    g.act(Action::Navigate(URL.into())).unwrap();
    g.act(Action::Click(Target::Id("btn".into()))).unwrap();
    g.focus(1).unwrap(); // follower live; master now dormant

    // Drop every dormant instance's resident DOM (the N-can-be-2000 case).
    g.release_dormant();
    assert!(
        g.master().text_of_id("out").is_none(),
        "released dormant master holds no resident DOM"
    );
    assert_eq!(
        g.instance(1).unwrap().text_of_id("out").as_deref(),
        Some("clicked"),
        "the live, focused instance keeps its DOM"
    );

    // Re-focusing the released master rebuilds it from the log — converged.
    g.focus(0).unwrap();
    assert_eq!(g.master().text_of_id("out").as_deref(), Some("clicked"));
    assert!(g.live_realms() <= 1);
}
