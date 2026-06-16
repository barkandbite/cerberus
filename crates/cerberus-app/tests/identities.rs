//! Arbitrary-N identity admin (the `identities` CLI / `identities_admin`).
//! A profile is no longer limited to the 3 hardcoded heads.

use cerberus_app::identities_admin;

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
