use super::*;
use std::cell::{Cell, RefCell};

type Hook = Box<dyn FnOnce(&Path)>;
thread_local! {
    static BEFORE_PERSIST: RefCell<Option<Hook>> = const { RefCell::new(None) };
    static PERSIST_CALLS: Cell<usize> = const { Cell::new(0) };
}
pub(super) fn before_persist(path: &Path) {
    PERSIST_CALLS.set(PERSIST_CALLS.get() + 1);
    BEFORE_PERSIST.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(path);
        }
    });
}

fn fixture(channel: u64) -> (tempfile::TempDir, std::path::PathBuf, InflightTurnState) {
    let root = tempfile::tempdir().unwrap();
    let path = super::super::inflight_state_path(root.path(), &ProviderKind::Claude, channel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        8,
        9,
        "queued".into(),
        None,
        None,
        Some("/tmp/r0l".into()),
        None,
        0,
    );
    state.finalizer_turn_id = 0;
    state.full_response = "body".into();
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    (root, path, state)
}

#[test]
fn r0l_backfill_preserves_legacy_state_and_reports_actual_persist_outcomes() {
    let (root, path, state) = fixture(632541);
    PERSIST_CALLS.set(0);
    let report = backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude);
    assert_eq!(report.outcome, BackfillOutcome::Persisted);
    assert_eq!(
        report.state.as_ref().unwrap().finalizer_turn_id,
        state.user_msg_id
    );
    assert_eq!(PERSIST_CALLS.get(), 1);
    let saved: InflightTurnState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved.finalizer_turn_id, state.user_msg_id);
    assert_eq!(saved.save_generation, state.save_generation + 1);
    assert_eq!(
        backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude).outcome,
        BackfillOutcome::Unneeded
    );
    assert_eq!(PERSIST_CALLS.get(), 1);

    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    BEFORE_PERSIST.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|path| {
            let mut raw: serde_json::Value =
                serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            raw["replay_hold_reasons"] = serde_json::json!(["p5925_coordinate:concurrent owner"]);
            fs::write(path, serde_json::to_vec(&raw).unwrap()).unwrap();
        }))
    });
    let report = backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude);
    assert_eq!(report.outcome, BackfillOutcome::Refused);
    assert!(report.state.is_some(), "legacy tolerance retained");
    let durable: InflightTurnState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(durable.finalizer_turn_id, 0);
    assert_eq!(
        durable.replay_hold_reasons,
        ["p5925_coordinate:concurrent owner"]
    );

    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    BEFORE_PERSIST.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|path| {
            fs::remove_file(path).unwrap();
            fs::create_dir(path).unwrap();
        }))
    });
    let report = backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude);
    assert!(matches!(report.outcome, BackfillOutcome::Persist(_)));
    assert!(
        report.state.is_some(),
        "write failure does not discard legacy state"
    );
}

#[test]
fn r0l_backfill_receipt_foreign_read_parse_and_lock_outcomes_are_distinct() {
    let (root, path, mut state) = fixture(632542);
    let mut raw = serde_json::to_value(&state).unwrap();
    raw["herdr_before_start_stop"] = serde_json::json!({"protocol":1,"kind":"unsubmitted_before_start_stop","turn_nonce":"r0l","admitted_generation":1,"basis":"no_attempt","notice":"pending"});
    let bytes = serde_json::to_vec(&raw).unwrap();
    fs::write(&path, &bytes).unwrap();
    PERSIST_CALLS.set(0);
    let report = backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude);
    assert_eq!(report.outcome, BackfillOutcome::ReceiptHeld);
    assert!(report.state.is_some());
    assert_eq!(PERSIST_CALLS.get(), 0);
    assert_eq!(fs::read(&path).unwrap(), bytes);
    state.provider = "codex".into();
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(
        backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude).outcome,
        BackfillOutcome::Foreign
    );
    fs::write(&path, b"bad").unwrap();
    assert!(matches!(
        backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude).outcome,
        BackfillOutcome::Parse(_)
    ));
    fs::remove_file(&path).unwrap();
    assert!(matches!(
        backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude).outcome,
        BackfillOutcome::Read(_)
    ));
    fs::remove_file(path.with_extension("json.lock")).unwrap();
    fs::create_dir(path.with_extension("json.lock")).unwrap();
    assert!(matches!(
        backfill_finalizer_turn_id_observed(root.path(), &path, &ProviderKind::Claude).outcome,
        BackfillOutcome::Lock(LockCause::Io(_))
    ));
}

pub(super) fn mutant(name: &str) -> bool {
    std::env::var("ADK_R0L_MUTANT").ok().as_deref() == Some(name)
}
