//! Arbitrary-N identity admin (the `identities` CLI / `identities_admin`).
//! A profile is no longer limited to the 3 hardcoded heads.

use cerberus_app::{identities_admin, identities_admin_full};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("cerberus-it-{}-{tag}-{nanos}", std::process::id()))
}

#[test]
fn identities_admin_lists_adds_persists_and_removes() {
    let dir = temp_dir("ids");
    let d = dir.to_str().unwrap();

    // First call initializes + persists the default three.
    let list = identities_admin(d, None, None).expect("list");
    assert_eq!(list.len(), 3, "a fresh profile has 3 identities");

    // Add a fourth — arbitrary N — and confirm it persists across calls.
    let list = identities_admin(d, Some("burner"), None).expect("add");
    assert_eq!(list.len(), 4);
    assert!(list.iter().any(|l| l.contains("burner")));
    let list = identities_admin(d, None, None).expect("relist");
    assert_eq!(list.len(), 4, "the addition persisted to heads.txt");

    // Remove it.
    let list = identities_admin(d, None, Some(3)).expect("remove");
    assert_eq!(list.len(), 3);
    assert!(!list.iter().any(|l| l.contains("burner")));

    // Out-of-range removal is an error, not a panic.
    assert!(identities_admin(d, None, Some(99)).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_identity_proxy_sets_persists_lists_and_clears() {
    let dir = temp_dir("proxy");
    let d = dir.to_str().unwrap();

    // Initialize the default identities.
    identities_admin(d, None, None).expect("init");

    // Assign identity 1 its own egress proxy (per-window proxy).
    let list = identities_admin_full(d, None, None, Some("1=127.0.0.1:3128"), None).expect("set");
    assert!(
        list[1].contains("proxy=127.0.0.1:3128"),
        "identity 1 shows its proxy: {:?}",
        list[1]
    );
    assert!(
        !list[0].contains("proxy="),
        "identity 0 has no proxy: {:?}",
        list[0]
    );

    // It survives a reload (persisted to heads.txt as a `proxy` line).
    let list = identities_admin(d, None, None).expect("relist");
    assert!(list[1].contains("proxy=127.0.0.1:3128"), "proxy persisted");

    // A malformed proxy is rejected (fail-closed) and never persisted.
    assert!(
        identities_admin_full(d, None, None, Some("1=not-a-proxy"), None).is_err(),
        "a proxy with no port is rejected"
    );
    assert!(
        identities_admin_full(d, None, None, Some("9=127.0.0.1:1"), None).is_err(),
        "an out-of-range identity index is rejected"
    );
    // The bad attempts left the good value intact.
    let list = identities_admin(d, None, None).expect("relist after bad");
    assert!(
        list[1].contains("proxy=127.0.0.1:3128"),
        "unchanged by errors"
    );

    // Clearing removes it (falls back to global/direct).
    let list = identities_admin_full(d, None, None, None, Some(1)).expect("clear");
    assert!(!list[1].contains("proxy="), "proxy cleared: {:?}", list[1]);
    let list = identities_admin(d, None, None).expect("relist after clear");
    assert!(!list[1].contains("proxy="), "clear persisted");

    let _ = std::fs::remove_dir_all(&dir);
}
