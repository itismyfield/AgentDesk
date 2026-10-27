use super::*;
use crate::config::TestRuntimeRootGuard;

#[cfg(unix)]
#[test]
fn directory_sync_failure_issues_no_permit_and_recovery_requires_sync() {
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(727280);
    owner.enqueue(&item(100)).unwrap();
    let key = owner.reserve(attempt(100, "sync-pane"), payload()).unwrap();
    let watch = Watch::new(
        &path(727280),
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    let result = owner.arm(&key, 1000);
    assert_eq!(
        usize::from(result.is_ok()),
        0,
        "actual directory sync failure must issue zero permits"
    );
    assert!(
        owner.recover().is_err(),
        "persistent parent sync failure cannot be adopted"
    );
    assert!(
        watch
            .events()
            .iter()
            .any(|event| event.step == Step::ParentSync && !event.ok)
    );
    drop(watch);
    owner.recover().unwrap();
    assert_eq!(
        owner.snapshot.head_injection.as_ref().unwrap().phase,
        HeadPhase::MayHaveSent
    );
    assert!(
        owner.arm(&key, 1001).is_err(),
        "recovery never recreates the lost permit"
    );
}

fn duplicate_head(channel: u64) -> (Owner, AttemptRef, PendingQueueItem) {
    let mut owner = owner(channel);
    let mut a = item(8_000_000_000_000_001_000);
    a.text = "first synthetic original".into();
    let mut b = a.clone();
    b.text = "second synthetic original".into();
    owner.enqueue(&a).unwrap();
    owner.enqueue(&b.clone()).unwrap();
    let key = owner
        .reserve(attempt(100, &format!("pane-{channel}")), payload())
        .unwrap();
    drop(owner.arm(&key, 1000).unwrap());
    (owner, key, b)
}

#[test]
fn expiry_removes_only_the_selected_head_and_preserves_duplicate_identity_raw() {
    let _root = TestRuntimeRootGuard::new();
    let (mut owner, _, b) = duplicate_head(727281);
    owner.tick(31_000).unwrap();
    assert_eq!(
        owner.snapshot.items.len(),
        1,
        "expiry preserves the second same-identity original"
    );
    assert_eq!(
        serde_json::to_value(&owner.snapshot.items[0]).unwrap(),
        serde_json::to_value(b).unwrap()
    );
}

#[test]
fn settlement_removes_only_the_selected_head_and_preserves_duplicate_identity_raw() {
    let _root = TestRuntimeRootGuard::new();
    let (mut owner, key, b) = duplicate_head(727282);
    owner.settle(&key, true, 2000).unwrap();
    assert_eq!(
        owner.snapshot.items.len(),
        1,
        "settlement preserves the second same-identity original"
    );
    assert_eq!(
        serde_json::to_value(&owner.snapshot.items[0]).unwrap(),
        serde_json::to_value(b).unwrap()
    );
}

#[test]
fn head_mismatch_refuses_expiry_and_settlement_without_publishing() {
    let _root = TestRuntimeRootGuard::new();
    for (channel, expiry) in [(727283, true), (727284, false)] {
        let (mut owner, key, _) = duplicate_head(channel);
        owner.snapshot.items[0] = item(300);
        let before = std::fs::read(path(channel)).unwrap();
        let result = if expiry {
            owner.tick(31_000)
        } else {
            owner.settle(&key, true, 2000)
        };
        assert!(result.is_err(), "a mismatched first item must fail closed");
        assert_eq!(std::fs::read(path(channel)).unwrap(), before);
    }
}

#[test]
fn failed_enqueue_and_parked_migration_keep_the_callers_original_for_retry() {
    let _root = TestRuntimeRootGuard::new();
    for (channel, parked) in [(727285, None), (727286, Some(true)), (727287, Some(false))] {
        let mut owner = owner(channel);
        let original = item(900);
        let before = serde_json::to_value(&original).unwrap();
        owner.fault = Fault::Before;
        let result = match parked {
            None => owner.enqueue(&original).map(|_| ()),
            Some(proven) => sources::migrate_parked(&mut owner, &original, proven),
        };
        assert!(
            result.is_err(),
            "failed durable admission cannot acknowledge a new original"
        );
        assert_eq!(serde_json::to_value(&original).unwrap(), before);
        assert!(owner.snapshot.items.is_empty() && owner.snapshot.retained_inputs.is_empty());
        owner.fault = Fault::None;
        match parked {
            None => {
                owner.enqueue(&original).unwrap();
            }
            Some(proven) => sources::migrate_parked(&mut owner, &original, proven).unwrap(),
        }
        assert_eq!(
            owner.snapshot.items.len() + owner.snapshot.retained_inputs.len(),
            1
        );
    }
}

#[cfg(unix)]
#[test]
fn resolved_source_commit_is_repaired_after_history_projection_failure_without_changing_key() {
    use crate::services::tui_input::human_frame::history::v2::{Registry, Source};
    use std::os::unix::fs::MetadataExt;
    let _root = TestRuntimeRootGuard::new();
    let native = path(727288).with_extension("jsonl");
    std::fs::create_dir_all(native.parent().unwrap()).unwrap();
    std::fs::write(&native, "native\n").unwrap();
    let mut owner = owner(727288);
    owner.enqueue(&item(800)).unwrap();
    let mut a = attempt(800, "bind-pane");
    a.source = AttemptSource::PreSource {
        expected_session_id: Some("native-session".into()),
        expected_transcript: Some(native.display().to_string()),
        binding_watermark: Some("watermark".into()),
    };
    let key = owner.reserve(a, payload()).unwrap();
    let history = Registry::runtime().unwrap();
    owner.prepare_history(&history).unwrap();
    let source = Source {
        target: serde_json::to_value(&key.attempt.target).unwrap(),
        provider: "codex".into(),
        session_id: "native-session".into(),
        transcript: native.clone(),
        file: {
            let m = std::fs::metadata(&native).unwrap();
            [m.dev(), m.ino()]
        },
        offset: 0,
        binding_watermark: Some("watermark".into()),
    };
    let history_path = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    let watch = crate::services::discord::runtime_store::durable_probe::Watch::new(
        &history_path,
        crate::services::discord::runtime_store::durable_probe::Plan {
            fail: vec![crate::services::discord::runtime_store::durable_probe::Step::ParentSync],
            ..Default::default()
        },
    );
    assert!(owner.bind_source(&key, &source, &history).is_err());
    assert!(
        owner
            .snapshot
            .head_injection
            .as_ref()
            .unwrap()
            .resolved_source
            .is_some()
    );
    drop(watch);
    let saved = serde_json::to_value(&key).unwrap();
    assert_eq!(
        owner
            .active_attempt()
            .map(|k| serde_json::to_value(k).unwrap())
            .unwrap(),
        saved
    );
    owner.sync_history(&history).unwrap();
    assert_eq!(
        history.json().unwrap()["live"][0]["registration"]["key"],
        saved
    );
    let mut wrong = source;
    wrong.target["execution_nonce"] = json!("other");
    assert!(owner.bind_source(&key, &wrong, &history).is_err());
    let mut restored = Owner::adopt(path(727288), "scope-727288".into()).unwrap();
    restored.tick(99_000).unwrap();
    restored.sync_history(&history).unwrap();
    assert_eq!(
        history.json().unwrap()["passive"][0]["registration"]["key"],
        saved
    );
}
