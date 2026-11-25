//! AGY delivery custody opens without pane state and refuses a destination a pane owns.

use std::num::NonZeroU64;

use super::*;
use crate::services::tui_o::store::{INIT_FILE, Initialized, OStore, STORE_DIR_NAME, StoreConfig};

const fn id(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn store(root: &std::path::Path) -> OStore {
    OStore::open_if_enabled(&StoreConfig { enabled: true }, root)
        .unwrap()
        .unwrap()
}

// D4
#[test]
fn journal_root_opens_without_pane_init_and_refuses_a_pane_destination() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let now = chrono::Utc::now();
    store
        .begin_fixture_era(&[8], now, |channel| {
            Ok(Initialized {
                channel,
                at: now,
                sources: vec![],
                initial_anchor: 0,
                build_digest: "test".into(),
                policy_parent: None,
            })
        })
        .unwrap();

    let agy = AgyJournalRoot::open(root.path(), id(7)).unwrap();
    assert_eq!(agy.destination(), 7);
    let reopened = OStore::existing(root.path()).unwrap();
    assert!(!reopened.channels_with_init().unwrap().contains(&7));
    assert!(reopened.read_init(7).unwrap().is_none());

    assert!(AgyJournalRoot::open(root.path(), id(8)).is_err());

    let damaged = root.path().join(STORE_DIR_NAME).join("9");
    std::fs::create_dir_all(&damaged).unwrap();
    std::fs::write(damaged.join(INIT_FILE), b"{not json").unwrap();
    assert!(AgyJournalRoot::open(root.path(), id(9)).is_err());
}

// D5 (store side): the binding a journal carries is read back with its exact turn key
#[test]
fn journal_binding_is_read_with_its_turn_key() {
    let root = tempfile::tempdir().unwrap();
    store(root.path());
    let nonce = uuid::Uuid::new_v4();
    let dir = root
        .path()
        .join(STORE_DIR_NAME)
        .join("7")
        .join("agy-delivery");
    std::fs::create_dir_all(&dir).unwrap();
    let fingerprint = [0_u8; 32];
    let journal = serde_json::json!({
        "version": 1,
        "key": {"destination": 7, "id": 100, "nonce": nonce},
        "revision": 1,
        "fingerprint": fingerprint,
        "last_seq": 3,
        "receipts": [],
        "footer": null,
        "attempt": null,
        "ended": null,
        "binding": {"logical_channel": 41, "original": {"imessage": {"digest": "ab"}}},
    });
    std::fs::write(dir.join("100.json"), serde_json::to_vec(&journal).unwrap()).unwrap();

    let (key, binding) = read_binding(root.path(), 7, 100).unwrap().unwrap();
    assert_eq!(key.nonce, nonce);
    assert_eq!(key.id, id(100));
    assert_eq!(binding.logical_channel, 41);
    assert_eq!(
        binding.original,
        AgyOriginal::Imessage {
            digest: "ab".into()
        }
    );
    assert!(read_binding(root.path(), 7, 101).unwrap().is_none());

    let mut unbound = journal.clone();
    unbound.as_object_mut().unwrap().remove("binding");
    std::fs::write(dir.join("102.json"), serde_json::to_vec(&unbound).unwrap()).unwrap();
    assert!(read_binding(root.path(), 7, 102).unwrap().is_none());
}
