//! Non-scoped writers change only keyless slots of a V2 history, under the owner's lock.
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::tui_input::human_frame::{history as h, idle};

struct Fx {
    _root: TestRuntimeRootGuard,
    _dir: tempfile::TempDir,
    t: PathBuf,
    history: Registry,
}
fn fx() -> Fx {
    let (_root, _dir) = (TestRuntimeRootGuard::new(), tempfile::tempdir().unwrap());
    // The runtime directory exists; the ledger file itself stays missing.
    std::fs::create_dir_all(h::path().unwrap().parent().unwrap()).unwrap();
    let t = _dir.path().join("native.jsonl");
    std::fs::write(&t, "").unwrap();
    Fx {
        _root,
        _dir,
        t,
        history: Registry::runtime().unwrap(),
    }
}
fn frame(body: &str) -> String {
    idle("ann", "7", body)
}
fn keyed_on(history: &Registry, t: &Path, n: u64, frame: &str) -> Value {
    let file = file_id(&std::fs::metadata(t).unwrap());
    let key = json!({"identity":{"primary":n,"sources":[{"message_id":n}]},"attempt":{"nonce":format!("nonce{n}"),
        "provider":"claude","target":{"owner":"pane","execution_nonce":"spawn"},
        "source":{"kind":"existing","session_id":"native","transcript":t,"file":file,"offset":0}}});
    let input = json!({"identity":key["identity"],"attempt":key["attempt"],"original":{"author_id":7},"payload":{"frame":frame}});
    history.prepare("ns", &input).unwrap();
    key
}
fn keyless_on(t: &Path, id: &str, frame: &str) -> Result<(), String> {
    h::register(("claude", "pane", id, "7"), t, 0, frame)
}
impl Fx {
    fn keyed(&self, n: u64, frame: &str) -> Value {
        keyed_on(&self.history, &self.t, n, frame)
    }
    fn keyless(&self, id: &str, frame: &str) -> Result<(), String> {
        keyless_on(&self.t, id, frame)
    }
    fn receipt(&self, id: &str, frame: &str) -> Option<bool> {
        h::receipt(id, &self.t, 0, "user", Some("row"), frame)
    }
    fn keyed_receipt(&self, key: &Value, frame: &str) -> Option<bool> {
        (self.history).receipt("ns", key, (&self.t, 0, "user", Some("row"), frame))
    }
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(h::path().unwrap()).unwrap()
    }
    fn disk(&self) -> Value {
        serde_json::from_slice(&self.bytes()).unwrap()
    }
}

#[test]
fn keyless_reconcile_claude_register_then_receipt_changes_only_its_slot() {
    let f = fx();
    f.keyed(1, &frame("keyed"));
    f.keyless("k1", &frame("one")).unwrap();
    f.keyless("k2", &frame("two")).unwrap();
    let mut before = f.disk();
    assert_eq!(f.receipt("k2", &frame("two")), Some(true));
    let after = f.disk();
    before["passive"][1]["entry"]["events"] =
        json!([{"key": "user:row", "row": "row", "kind": "user"}]);
    before["passive"][1]["state"] = after["passive"][1]["state"].clone();
    assert_eq!(after, before);
}

/// A keyless enqueue holds only its row: a same-frame keyed enqueue of another row lands, while
/// the held row and a later command or user receipt stay refused.
#[test]
fn keyless_enqueue_leaves_another_row_to_a_same_frame_keyed_enqueue() {
    let f = fx();
    f.keyless("k1", &frame("same")).unwrap();
    let enqueue = h::receipt("k1", &f.t, 0, "enqueue", Some("q1"), &frame("same"));
    assert_eq!(enqueue, Some(true));
    let key = f.keyed(1, &frame("same"));
    let keyed =
        |kind, row| (f.history).receipt("ns", &key, (&f.t, 0, kind, Some(row), &frame("same")));
    assert_eq!(
        keyed("enqueue", "q2"),
        Some(true),
        "another row's keyed enqueue"
    );
    assert_eq!(keyed("enqueue", "q1"), None, "the keyless row");
    assert_eq!((keyed("command", "c"), keyed("user", "u")), (None, None));
}

/// An unscoped withdraw or receipt of a keyed attempt writes nothing; a recorded row still reads.
#[test]
fn keyless_reconcile_refuses_an_apply_that_edits_a_keyed_event_and_keeps_bytes() {
    let f = fx();
    let key = f.keyed(1, &frame("keyed"));
    let before = f.bytes();
    h::withdraw("nonce1", &f.t);
    assert_eq!(f.receipt("nonce1", &frame("keyed")), None);
    assert_eq!(f.bytes(), before);
    assert_eq!(f.keyed_receipt(&key, &frame("keyed")), Some(true));
    let recorded = f.bytes();
    assert_eq!(f.receipt("nonce1", &frame("keyed")), Some(true));
    assert_eq!(f.bytes(), recorded);
}

#[test]
fn keyless_reconcile_refuses_a_duplicate_id_path_instead_of_folding() {
    let f = fx();
    f.keyless("k1", &frame("one")).unwrap();
    let before = f.bytes();
    let twin = h::update(|store| {
        store.entries.push(store.entries[0].clone());
        Ok(())
    });
    assert_eq!(twin, Err(AMBIGUOUS.to_string()));
    assert_eq!(f.bytes(), before);
    let mut doubled = f.disk();
    let first = doubled["passive"][0].clone();
    doubled["passive"].as_array_mut().unwrap().push(first);
    std::fs::write(h::path().unwrap(), doubled.to_string()).unwrap();
    assert_eq!(f.keyless("k2", &frame("two")), Err(AMBIGUOUS.to_string()));
    assert_eq!(f.disk(), doubled);
}

#[test]
fn keyless_reconcile_two_removals_keep_later_slot_indices() {
    let f = fx();
    for id in ["k1", "k2", "k3", "k4"] {
        f.keyless(id, &frame(id)).unwrap()
    }
    let before = f.disk();
    let kept = |e: &Entry| !["k1", "k3"].contains(&e.id.as_str());
    h::update(|store| {
        store.entries.retain(kept);
        store.entries[1].author = "changed".into();
        Ok(())
    })
    .unwrap();
    let after = f.disk();
    assert_eq!(after["passive"][0], before["passive"][1]);
    assert_eq!(after["passive"][1]["entry"]["author"], "changed");
    assert_eq!(after["passive"].as_array().unwrap().len(), 2);
}

/// From a V1 file, an owner handle that migrates it and a keyless register race; both survive.
#[test]
fn concurrent_owner_prepare_and_keyless_register_both_survive() {
    use crate::test_support::wait::{WAIT_CAP, join_within};
    let f = fx();
    let owner = Registry::at(h::path().unwrap());
    for n in 0..24u64 {
        std::fs::write(h::path().unwrap(), "{\"entries\":[]}").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (t, b, owner) = (f.t.clone(), barrier.clone(), owner.clone());
        let keyed = std::thread::spawn(move || {
            b.wait();
            keyed_on(&owner, &t, n, &frame("keyed"))
        });
        let t = f.t.clone();
        let keyless = std::thread::spawn(move || {
            barrier.wait();
            keyless_on(&t, &format!("k{n}"), &frame("keyless"))
        });
        join_within(keyed, WAIT_CAP);
        let refused = join_within(keyless, WAIT_CAP);
        assert_eq!(refused, Ok(()), "{n}: the keyless register was refused");
        let disk = f.disk();
        assert_eq!(
            disk["live"][0]["id"],
            format!("nonce{n}"),
            "{n}: keyed lost"
        );
        let kept = &disk["passive"][0]["entry"]["id"];
        assert_eq!(*kept, format!("k{n}"), "{n}: keyless lost");
    }
}

/// Another handle's commit leaves the shared handle's cache stale; the withdraw keeps it and k2.
#[test]
fn keyless_withdraw_after_owner_commit_leaves_keyed_bytes() {
    let f = fx();
    f.keyless("k1", &frame("one")).unwrap();
    f.keyless("k2", &frame("two")).unwrap();
    keyed_on(&Registry::at(h::path().unwrap()), &f.t, 1, &frame("keyed"));
    let mut before = f.disk();
    let ids = |p: &Value| p["entry"]["id"].clone();
    let keyless: Vec<_> = before["passive"]
        .as_array()
        .unwrap()
        .iter()
        .map(ids)
        .collect();
    assert_eq!(
        (&before["live"][0]["id"], keyless),
        (&json!("nonce1"), vec![json!("k1"), json!("k2")])
    );
    h::withdraw("k1", &f.t);
    before["passive"].as_array_mut().unwrap().remove(0);
    assert_eq!(f.disk(), before);
}

#[test]
fn shared_row_keyless_receipt_after_keyed_commit_is_refused() {
    let f = fx();
    let key = f.keyed(1, &frame("same"));
    assert_eq!(f.keyed_receipt(&key, &frame("same")), Some(true));
    f.keyless("k1", &frame("same")).unwrap();
    assert_eq!(f.receipt("k1", &frame("same")), None);
}

#[test]
fn shared_row_keyed_receipt_after_keyless_commit_is_refused() {
    let f = fx();
    f.keyless("k1", &frame("same")).unwrap();
    assert_eq!(f.receipt("k1", &frame("same")), Some(true));
    let key = f.keyed(1, &frame("same"));
    assert_eq!(f.keyed_receipt(&key, &frame("same")), None);
}

#[test]
fn shared_row_first_keyless_event_contested_by_same_frame_keyed() {
    let f = fx();
    f.keyless("k1", &frame("same")).unwrap();
    f.keyed(1, &frame("same"));
    let before = f.bytes();
    assert_eq!(f.receipt("k1", &frame("same")), None);
    assert_eq!(f.bytes(), before);
}

#[test]
fn a_v1_ledger_keeps_the_v1_writer_and_a_missing_or_v2_one_takes_keyless_slots() {
    let f = fx();
    std::fs::write(h::path().unwrap(), "{\"entries\":[]}").unwrap();
    f.keyless("k1", &frame("one")).unwrap();
    assert_eq!(f.disk()["entries"][0]["id"], "k1");
    std::fs::remove_file(h::path().unwrap()).unwrap();
    f.keyless("k2", &frame("two")).unwrap();
    f.keyless("k3", &frame("three")).unwrap();
    assert!(f.bytes().starts_with(b"{\"version\":2,"));
    assert_eq!(f.disk()["passive"][1]["entry"]["id"], "k3");
}
