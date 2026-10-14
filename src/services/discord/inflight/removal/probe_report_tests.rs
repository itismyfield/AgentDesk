use super::super::nondestructive_loader_tests::{CLAUDE, Env, STALE, row};
use super::*;
use std::cell::{Cell, RefCell};

thread_local! {
    static ROOT_UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
    static ENTRY_ERROR: Cell<bool> = const { Cell::new(false) };
    static BEFORE_SYNC: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}
pub(super) fn root(root: Option<PathBuf>) -> Option<PathBuf> {
    if ROOT_UNAVAILABLE.get() { None } else { root }
}
pub(super) fn entry(entry: std::io::Result<fs::DirEntry>) -> std::io::Result<fs::DirEntry> {
    if ENTRY_ERROR.replace(false) {
        Err(std::io::Error::other("injected entry failure"))
    } else {
        entry
    }
}
pub(in crate::services::discord::inflight::removal) fn before_sync(path: &Path) {
    BEFORE_SYNC.with(|hook| {
        if let Some(destination) = hook.borrow_mut().take() {
            fs::rename(path.parent().unwrap(), destination).unwrap();
        }
    });
}

#[test]
fn r0l_inflight_checked_root_scan_errors_and_hidden_rows_preserve_payload_and_bytes() {
    let env = Env::new();
    ROOT_UNAVAILABLE.set(true);
    let unavailable =
        super::super::super::load_inflight_states_excluding_checked(&CLAUDE, |_| false);
    ROOT_UNAVAILABLE.set(false);
    assert!(!unavailable.complete);
    assert!(matches!(
        unavailable.observations[0].cause,
        ProbeCause::RootUnavailable
    ));
    let empty = load_inflight_probe_with_scope(&env.dir(), &CLAUDE, |_| false, None, true);
    assert!(empty.complete && empty.states.is_empty() && empty.observations.is_empty());
    let valid = env.seed(&row(632531, None), 0);
    let stale = env.seed(&row(632532, None), STALE);
    let mut foreign = row(632533, None);
    foreign.provider = "codex".into();
    let foreign = env.seed(&foreign, 0);
    let malformed = valid.with_file_name("632534.json");
    fs::write(&malformed, b"bad").unwrap();
    let read_error = valid.with_file_name("632535.json");
    fs::create_dir(&read_error).unwrap();
    let locked = valid.with_file_name("632536.json");
    fs::write(&locked, b"bad").unwrap();
    fs::create_dir(locked.with_extension("json.lock")).unwrap();
    let snapshots: Vec<_> = [&valid, &stale, &foreign, &malformed, &locked]
        .iter()
        .map(|path| ((*path).clone(), fs::read(path).unwrap()))
        .collect();
    let report = super::super::super::load_inflight_states_excluding_checked(&CLAUDE, |_| false);
    assert!(
        !report.complete,
        "read and lock failures keep legacy completeness false"
    );
    assert_eq!(
        report
            .states
            .iter()
            .map(|state| state.channel_id)
            .collect::<Vec<_>>(),
        [632531]
    );
    assert!(report.observations.iter().any(
        |entry| matches!(&entry.cause, ProbeCause::HiddenStale(reason) if !reason.is_empty())
    ));
    assert!(report.observations.iter().any(|entry| matches!(
        entry.cause,
        ProbeCause::HiddenForeign("load_inflight_states_from_root_provider_mismatch")
    )));
    assert!(
        report
            .observations
            .iter()
            .any(|entry| entry.cause == ProbeCause::HiddenMalformed)
    );
    assert!(
        report
            .observations
            .iter()
            .any(|entry| matches!(entry.cause, ProbeCause::Read(_)))
    );
    assert!(
        report
            .observations
            .iter()
            .any(|entry| matches!(entry.cause, ProbeCause::Parse(_)))
    );
    assert!(
        report
            .observations
            .iter()
            .any(|entry| matches!(entry.cause, ProbeCause::Lock(store::LockCause::Io(_))))
    );
    for (path, bytes) in snapshots {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    let report = load_inflight_probe_with_scope(
        &env.dir(),
        &CLAUDE,
        |channel| channel >= 632535,
        None,
        true,
    );
    assert!(
        report.complete,
        "hidden rows do not change the legacy All bool"
    );
    let scoped = load_inflight_probe_with_scope(&env.dir(), &CLAUDE, |_| false, Some(632534), true);
    assert!(!scoped.complete, "the scoped hidden-row bool stays false");
    ENTRY_ERROR.set(true);
    let report = load_inflight_probe_with_scope(&env.dir(), &CLAUDE, |_| false, None, true);
    assert!(!report.complete);
    assert!(
        report
            .observations
            .iter()
            .any(|entry| matches!(entry.cause, ProbeCause::Entry(_)))
    );
    let blocked = env.dir().join("blocked");
    fs::write(&blocked, b"file").unwrap();
    let report = load_inflight_probe_with_scope(&blocked, &CLAUDE, |_| false, None, true);
    assert!(!report.complete);
    assert!(matches!(
        report.observations[0].cause,
        ProbeCause::ReadDir(_)
    ));
}

#[test]
fn r0l_inflight_excluded_and_receipt_rows_never_lock_or_backfill() {
    let env = Env::new();
    let mut legacy = row(632537, None);
    legacy.finalizer_turn_id = 0;
    let excluded = env.seed(&legacy, 0);
    let before = fs::read(&excluded).unwrap();
    let gate =
        crate::services::discord::input_runtime::fence::Gate::protect(CLAUDE, 632537).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let report =
        super::super::super::load_inflight_states_excluding_checked(&CLAUDE, |id| id == 632537);
    assert!(report.states.is_empty() && report.complete);
    assert!(
        report
            .observations
            .iter()
            .any(|event| event.cause == ProbeCause::InputProtected)
    );
    assert!(!excluded.with_extension("json.lock").exists());
    assert_eq!(fs::read(&excluded).unwrap(), before);
    let receipt = env.seed(&row(632538, None), STALE);
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    raw["finalizer_turn_id"] = 0.into();
    raw["herdr_before_start_stop"] = serde_json::json!({"protocol":1,"kind":"unsubmitted_before_start_stop","turn_nonce":"r0l","admitted_generation":1,"basis":"no_attempt","notice":"pending"});
    let bytes = serde_json::to_vec(&raw).unwrap();
    fs::write(&receipt, &bytes).unwrap();
    let report =
        super::super::super::load_inflight_states_excluding_checked(&CLAUDE, |id| id == 632537);
    assert!(report.complete);
    assert_eq!(report.states.len(), 1);
    assert!(
        report
            .observations
            .iter()
            .any(|event| event.cause == ProbeCause::ReceiptHeld)
    );
    assert_eq!(fs::read(&receipt).unwrap(), bytes);
    assert!(!receipt.with_extension("json.lock").exists());
}

#[test]
fn r0l_scoped_backfill_distinguishes_contention_fence_and_sync_failure() {
    let env = Env::new();
    let mut legacy = row(632539, None);
    legacy.finalizer_turn_id = 0;
    let path = env.seed(&legacy, 0);
    let guard = lock_inflight_state_path(&path).unwrap();
    let checked = load_inflight_probe_scoped_checked(&CLAUDE, 632539);
    assert!(!checked.complete);
    assert!(checked.observations.iter().any(|event| event.cause
        == ProbeCause::Backfill(BackfillOutcome::Lock(store::LockCause::Contended))));
    let legacy = load_inflight_probe_scoped(&CLAUDE, 632539);
    assert!(!legacy.complete && matches!(legacy.failure, Some(RestoreFailure::Retryable(_))));
    assert_eq!(
        checked.failure, legacy.failure,
        "legacy retry detail remains exact"
    );
    assert_eq!(
        serde_json::to_value(&checked.states).unwrap(),
        serde_json::to_value(&legacy.states).unwrap()
    );
    let invalid = load_inflight_probe_scoped_checked(&CLAUDE, 0);
    assert!(!invalid.complete && invalid.observations[0].cause == ProbeCause::ScopeMismatch);
    let (result, observation) =
        scoped_restore::backfill_scoped_restore_observed(&env.dir(), &path, &CLAUDE);
    assert!(matches!(result, Err(RestoreFailure::Retryable(_))));
    assert_eq!(
        observation,
        BackfillOutcome::Lock(store::LockCause::Contended)
    );
    drop(guard);
    let gate =
        crate::services::discord::input_runtime::fence::Gate::protect(CLAUDE, 632539).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let closing = gate.close().unwrap();
    let (result, observation) =
        scoped_restore::backfill_scoped_restore_observed(&env.dir(), &path, &CLAUDE);
    assert!(matches!(result, Err(RestoreFailure::Retryable(_))));
    assert!(matches!(
        observation,
        BackfillOutcome::Lock(store::LockCause::InputFence(_))
    ));
    drop(closing);
    let mut sibling = row(632540, None);
    sibling.finalizer_turn_id = 0;
    let path = env.seed(&sibling, 0);
    if cfg!(not(windows)) {
        BEFORE_SYNC.with(|hook| *hook.borrow_mut() = Some(env.dir().join("renamed")));
        let (result, observation) =
            scoped_restore::backfill_scoped_restore_observed(&env.dir(), &path, &CLAUDE);
        assert!(matches!(result, Err(RestoreFailure::Retryable(_))));
        assert!(matches!(observation, BackfillOutcome::Sync(_)));
        let saved: InflightTurnState =
            serde_json::from_slice(&fs::read(env.dir().join("renamed/632540.json")).unwrap())
                .unwrap();
        assert_ne!(
            saved.finalizer_turn_id, 0,
            "write succeeded before the existing fsync failed"
        );
    }
}

pub(super) fn mutant(name: &str) -> bool {
    std::env::var("ADK_R0L_MUTANT").ok().as_deref() == Some(name)
}
