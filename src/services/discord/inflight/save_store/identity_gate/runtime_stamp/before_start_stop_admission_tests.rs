//! The policy receipt's lock-held transitions: which fresh rows admit, ack and clear, what a stale
//! write keeps, and that an unknown or old-format receipt is never read as a known one.
use super::*;
use crate::services::discord::InflightRestartMode;
use crate::services::discord::inflight::{self, RelayOwnerKind};
use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;

const CHANNEL: u64 = 1_479_671_301_387_190_000;
const NONCE: &str = "nonce-before-start";
const ADMITTED: u64 = 5;

/// A change applied to a row.
type Edit = fn(&mut InflightTurnState);

/// A bridge turn's row saved under a fresh test root.
struct Fixture {
    _root: tempfile::TempDir,
    _env: crate::config::test_env::TestEnvVarGuard,
    row: InflightTurnState,
}

impl Fixture {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(shape: impl FnOnce(&mut InflightTurnState)) -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            CHANNEL,
            None,
            1,
            2,
            3,
            "cold input".into(),
            None,
            Some("AgentDesk-claude-before-start".into()),
            None,
            None,
            0,
        );
        row.turn_nonce = Some(NONCE.into());
        row.born_generation = 4;
        shape(&mut row);
        if row.herdr_before_start_stop.is_some() {
            let root = inflight::inflight_runtime_root().unwrap();
            let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, CHANNEL);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, serde_json::to_vec_pretty(&row).unwrap()).unwrap();
        } else {
            inflight::save_inflight_state(&row).unwrap();
        }
        let row = inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL).unwrap();
        Self {
            _root: root,
            _env: env,
            row,
        }
    }

    fn path(&self) -> std::path::PathBuf {
        inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            CHANNEL,
        )
    }

    fn bytes(&self) -> Option<Vec<u8>> {
        std::fs::read(self.path()).ok()
    }

    fn durable(&self) -> Option<InflightTurnState> {
        inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL)
    }

    /// Rewrites the durable row in place, keeping its save generation.
    fn overwrite(&self, row: &InflightTurnState) {
        std::fs::write(self.path(), serde_json::to_vec(row).unwrap()).unwrap();
    }

    fn receipt(&self) -> Option<BeforeStartReceipt> {
        self.durable()?.herdr_before_start_stop
    }
}

async fn admit(row: &InflightTurnState, basis: CloseBasis) -> Option<InflightTurnState> {
    row.admit_before_start_receipt(NONCE, basis, ADMITTED).await
}

fn edited() -> Result<ReplaceLongMessageOutcome, String> {
    Ok(ReplaceLongMessageOutcome::EditedOriginal)
}

/// The fresh row decides, under the lock: every row that is not this bridge turn, or that another
/// authority already owns, refuses and stays byte-for-byte.
#[tokio::test(flavor = "current_thread")]
async fn admission_refuses_every_fresh_row_that_is_not_this_bridge_turn() {
    let changes: [(&str, Edit); 12] = [
        ("successor nonce", |row| {
            row.turn_nonce = Some("next".into())
        }),
        ("started at", |row| row.started_at.push('9')),
        ("born generation", |row| row.born_generation += 1),
        ("user message", |row| row.user_msg_id += 1),
        ("start offset", |row| row.turn_start_offset = Some(7)),
        ("pane", |row| row.tmux_session_name = Some("other".into())),
        ("rebind origin", |row| row.rebind_origin = true),
        ("planned restart", |row| {
            row.restart_mode = Some(InflightRestartMode::DrainRestart)
        }),
        ("native terminal", |row| {
            row.tui_terminal_kind =
                Some(crate::services::agent_protocol::NativeTerminalKind::Aborted)
        }),
        ("delivered", |row| row.terminal_delivery_committed = true),
        ("watcher relay", |row| {
            row.set_relay_owner_kind(RelayOwnerKind::Watcher)
        }),
        ("other receipt", |row| {
            row.herdr_before_start_stop = Some(changed_receipt(
                "admitted_generation",
                (ADMITTED + 1).into(),
            ))
        }),
    ];
    for (case, change) in changes {
        let fixture = Fixture::new();
        let mut fresh = fixture.row.clone();
        change(&mut fresh);
        fixture.overwrite(&fresh);
        let before = fixture.bytes();
        assert!(
            admit(&fixture.row, CloseBasis::NoAttempt).await.is_none(),
            "{case}"
        );
        assert_eq!(fixture.bytes(), before, "{case}");
    }
    // A nameless turn's finalizer id is the only axis that tells two of them apart.
    let fixture = Fixture::with(|row| {
        row.user_msg_id = 0;
        row.finalizer_turn_id = 41;
    });
    let mut fresh = fixture.row.clone();
    fresh.finalizer_turn_id = 42;
    fixture.overwrite(&fresh);
    let before = fixture.bytes();
    assert!(admit(&fixture.row, CloseBasis::NoAttempt).await.is_none());
    assert_eq!(fixture.bytes(), before);
    std::fs::remove_file(fixture.path()).unwrap();
    assert!(admit(&fixture.row, CloseBasis::NoAttempt).await.is_none());
    assert!(
        fixture.bytes().is_none(),
        "a refused admission creates no row"
    );
}

fn pending() -> BeforeStartReceipt {
    BeforeStartReceipt(serde_json::json!({
        "protocol": PROTOCOL, "kind": KIND, "turn_nonce": NONCE,
        "admitted_generation": ADMITTED, "basis": "no_attempt", "notice": PENDING,
    }))
}
fn changed_receipt(key: &str, value: serde_json::Value) -> BeforeStartReceipt {
    let mut receipt = pending();
    receipt.0[key] = value;
    receipt
}

/// A row the live turn advanced since the bridge's snapshot gains only the receipt; the same
/// admission again is a no-op and a different basis is a conflict.
#[tokio::test(flavor = "current_thread")]
async fn admission_adds_only_the_receipt_to_the_advanced_row() {
    let fixture = Fixture::new();
    let mut advanced = fixture.row.clone();
    advanced.session_id = Some("session-learned".into());
    advanced.current_msg_id = 99;
    advanced.full_response = "partial".into();
    inflight::save_inflight_state(&advanced).unwrap();
    let advanced = fixture.durable().unwrap();
    let admitted = admit(&fixture.row, CloseBasis::NoAttempt).await.unwrap();
    let durable = fixture.durable().unwrap();
    assert_eq!(admitted.herdr_before_start_stop, Some(pending()));
    assert_eq!(durable.herdr_before_start_stop, Some(pending()));
    assert_eq!(admitted.save_generation, durable.save_generation);
    let mut unchanged = durable.clone();
    unchanged.herdr_before_start_stop = None;
    unchanged.save_generation = advanced.save_generation;
    unchanged.updated_at = advanced.updated_at.clone();
    assert_eq!(
        serde_json::to_value(&unchanged).unwrap(),
        serde_json::to_value(&advanced).unwrap()
    );
    let again = admit(&fixture.row, CloseBasis::NoAttempt).await.unwrap();
    assert_eq!(again.save_generation, durable.save_generation, "no rewrite");
    let before = fixture.bytes();
    assert!(admit(&fixture.row, CloseBasis::Untouched).await.is_none());
    assert_eq!(fixture.bytes(), before);
    drop(fixture);
    let fixture = Fixture::new();
    let admitted = admit(&fixture.row, CloseBasis::Untouched).await.unwrap();
    assert_eq!(
        admitted.herdr_before_start_stop.unwrap().0["basis"],
        "untouched".to_string()
    );
}

/// Production's hard-false settlement admits nothing, also on the blocking thread that runs the
/// lock-held write; a zero generation or an empty nonce never admits either.
#[tokio::test(flavor = "current_thread")]
async fn admission_stays_off_without_settlement_and_without_identity() {
    let fixture = Fixture::new();
    let before = fixture.bytes();
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let off = admit(&fixture.row, CloseBasis::NoAttempt).await;
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    assert!(off.is_none());
    assert_eq!(fixture.bytes(), before);
    let zero = fixture
        .row
        .admit_before_start_receipt(NONCE, CloseBasis::NoAttempt, 0)
        .await;
    assert!(zero.is_none());
    let mut nameless = fixture.row.clone();
    nameless.turn_nonce = Some(String::new());
    fixture.overwrite(&nameless);
    let before = fixture.bytes();
    let empty = nameless
        .admit_before_start_receipt("", CloseBasis::NoAttempt, ADMITTED)
        .await;
    assert!(empty.is_none());
    assert_eq!(fixture.bytes(), before);
}

/// A receipt write that fails before or at publication leaves the row as it was.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_receipt_write_publishes_nothing() {
    for rename in [false, true] {
        let fixture = Fixture::new();
        let before = fixture.bytes();
        InflightTurnState::arm_before_start_receipt_fault(CHANNEL, rename);
        assert!(
            admit(&fixture.row, CloseBasis::NoAttempt).await.is_none(),
            "rename={rename}"
        );
        assert!(
            InflightTurnState::before_start_receipt_faults_consumed(CHANNEL),
            "rename={rename} ran on the write path"
        );
        assert_eq!(fixture.bytes(), before, "rename={rename}");
    }
}

/// Every row write keeps the durable receipt of its birth episode: a stale snapshot neither drops
/// nor lowers it, an unknown one is never lowered, and a successor episode cannot erase it.
#[tokio::test(flavor = "current_thread")]
async fn the_durable_receipt_survives_stale_writes() {
    let fixture = Fixture::new();
    let admitted = admit(&fixture.row, CloseBasis::NoAttempt).await.unwrap();
    let mut stale = fixture.row.clone();
    stale.full_response = "stale body".into();
    inflight::save_inflight_state(&stale).unwrap();
    let durable = fixture.durable().unwrap();
    assert_eq!(durable.full_response, "stale body");
    assert_eq!(durable.herdr_before_start_stop, Some(pending()));
    // Boot's stale-generation sweep keeps it: only a planned-restart row carries that generation.
    assert_eq!(
        inflight::invalidate_stale_generation(&ProviderKind::Claude, ADMITTED + 10),
        0
    );
    assert_eq!(fixture.receipt(), Some(pending()));
    let acked = admitted.ack_before_start_notice(&edited()).await.unwrap();
    inflight::save_inflight_state(&admitted).unwrap();
    assert_eq!(fixture.receipt(), acked.herdr_before_start_stop);
    assert_eq!(fixture.receipt().unwrap().0["notice"], DELIVERED);
    let mut unknown = fixture.durable().unwrap();
    unknown.herdr_before_start_stop.as_mut().unwrap().0["notice"] = "shown_v2".into();
    fixture.overwrite(&unknown);
    inflight::save_inflight_state(&admitted).unwrap();
    assert_eq!(fixture.receipt().unwrap().0["notice"], "shown_v2");
    // The restart marker rewrite reads the durable row, so it keeps the receipt too.
    inflight::mark_restart_mode_blocking(ProviderKind::Claude, InflightRestartMode::DrainRestart)
        .await;
    let marked = fixture.durable().unwrap();
    assert!(marked.restart_mode.is_some());
    assert_eq!(
        marked.herdr_before_start_stop.unwrap().0["notice"],
        "shown_v2"
    );
    // A create-only write never replaces an existing row.
    assert!(inflight::save_inflight_state_create_new(&fixture.row).is_err());
    let mut successor = fixture.row.clone();
    successor.turn_nonce = Some("successor".into());
    successor.started_at.push('9');
    let before = fixture.bytes();
    assert!(inflight::save_inflight_state(&successor).is_err());
    assert_eq!(
        fixture.bytes(),
        before,
        "a successor cannot erase the protected episode"
    );
}

/// The ack needs a shown notice and the same admission on the fresh row; it is idempotent.
#[tokio::test(flavor = "current_thread")]
async fn the_ack_needs_a_shown_notice_and_the_same_admission() {
    let fixture = Fixture::new();
    let admitted = admit(&fixture.row, CloseBasis::NoAttempt).await.unwrap();
    let partial = Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
        sent_chunks: 1,
        total_chunks: 2,
        failed_chunk_index: 1,
        sent_continuation_message_ids: Vec::new(),
        cleanup_errors: Vec::new(),
        error: "second chunk".into(),
    });
    for sent in [Err("send failed".to_string()), partial] {
        assert!(admitted.ack_before_start_notice(&sent).await.is_none());
        assert_eq!(fixture.receipt(), Some(pending()));
    }
    let fallback = Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
        edit_error: "gone".into(),
        replacement_anchor: None,
    });
    let acked = admitted.ack_before_start_notice(&fallback).await.unwrap();
    let durable = fixture.durable().unwrap();
    assert_eq!(
        durable.herdr_before_start_stop.as_ref().unwrap().0["notice"],
        DELIVERED
    );
    assert_eq!(acked.save_generation, durable.save_generation);
    let again = admitted.ack_before_start_notice(&edited()).await.unwrap();
    assert_eq!(again.save_generation, durable.save_generation, "no rewrite");
    let conflicts: [(&str, Edit); 3] = [
        ("other admission", |row| {
            row.herdr_before_start_stop = Some(changed_receipt(
                "admitted_generation",
                (ADMITTED + 1).into(),
            ))
        }),
        ("no receipt", |row| row.herdr_before_start_stop = None),
        ("native terminal", |row| {
            row.tui_terminal_kind =
                Some(crate::services::agent_protocol::NativeTerminalKind::Aborted)
        }),
    ];
    for (case, change) in conflicts {
        let mut fresh = fixture.row.clone();
        fresh.herdr_before_start_stop = Some(pending());
        change(&mut fresh);
        fixture.overwrite(&fresh);
        let before = fixture.bytes();
        assert!(
            admitted.ack_before_start_notice(&edited()).await.is_none(),
            "{case}"
        );
        assert_eq!(fixture.bytes(), before, "{case}");
    }
}

/// Restart's clear re-reads the row under its lock: only the snapshot's exact delivered receipt
/// row, admitted and born before this process, is removed.
#[test]
fn restart_clear_rechecks_the_delivered_receipt_under_its_lock() {
    const CURRENT: u64 = 9;
    let fixture = Fixture::with(|row| {
        row.herdr_before_start_stop = Some(changed_receipt("notice", DELIVERED.into()))
    });
    let snapshot = fixture.row.clone();
    let changes: [(&str, Edit); 12] = [
        ("receipt removed", |row| row.herdr_before_start_stop = None),
        ("notice pending", |row| {
            row.herdr_before_start_stop.as_mut().unwrap().0["notice"] = PENDING.into()
        }),
        ("receipt replaced", |row| {
            row.herdr_before_start_stop.as_mut().unwrap().0["admitted_generation"] =
                (ADMITTED - 1).into()
        }),
        ("unknown notice", |row| {
            row.herdr_before_start_stop.as_mut().unwrap().0["notice"] = "shown_v2".into()
        }),
        ("save generation", |row| row.save_generation += 1),
        ("nonce", |row| row.turn_nonce = Some("other".into())),
        ("anchor", |row| row.current_msg_id += 1),
        ("planned restart", |row| {
            row.restart_mode = Some(InflightRestartMode::DrainRestart)
        }),
        ("rebind origin", |row| row.rebind_origin = true),
        ("native terminal", |row| {
            row.tui_terminal_kind =
                Some(crate::services::agent_protocol::NativeTerminalKind::Aborted)
        }),
        ("delivered body", |row| {
            row.terminal_delivery_committed = true
        }),
        ("born now", |row| row.born_generation = CURRENT),
    ];
    for (case, change) in changes {
        let mut fresh = snapshot.clone();
        change(&mut fresh);
        fixture.overwrite(&fresh);
        let before = fixture.bytes();
        let outcome = snapshot.clear_before_start_receipt(CURRENT);
        assert_ne!(outcome, inflight::GuardedClearOutcome::Cleared, "{case}");
        assert_eq!(fixture.bytes(), before, "{case}");
    }
    fixture.overwrite(&snapshot);
    for current in [ADMITTED, ADMITTED - 1] {
        let outcome = snapshot.clear_before_start_receipt(current);
        assert_ne!(outcome, inflight::GuardedClearOutcome::Cleared, "{current}");
    }
    let outcome = snapshot.clear_before_start_receipt(CURRENT);
    assert_eq!(outcome, inflight::GuardedClearOutcome::Cleared);
    assert!(fixture.durable().is_none());
}

/// A row without the field keeps its bytes; a receipt this binary does not know, even one with
/// missing fields, keeps its values through a save and is never read as supported.
#[test]
fn old_rows_and_unknown_receipts_keep_their_shape() {
    let fixture = Fixture::new();
    let raw: serde_json::Value = serde_json::from_slice(&fixture.bytes().unwrap()).unwrap();
    assert!(raw.get("herdr_before_start_stop").is_none());
    let mut future = raw.clone();
    future["herdr_before_start_stop"] = serde_json::json!({
        "protocol": 2, "kind": "future_kind", "turn_nonce": NONCE,
        "admitted_generation": ADMITTED, "basis": "future_basis", "notice": "future_notice"
    });
    std::fs::write(fixture.path(), serde_json::to_vec(&future).unwrap()).unwrap();
    let loaded = fixture.durable().unwrap();
    let receipt = loaded.herdr_before_start_stop.clone().unwrap();
    assert!(!receipt.supported() && !receipt.notice_pending());
    inflight::save_inflight_state(&fixture.row).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fixture.bytes().unwrap()).unwrap();
    assert_eq!(
        saved["herdr_before_start_stop"],
        future["herdr_before_start_stop"]
    );
    let mut partial = raw;
    partial["herdr_before_start_stop"] = serde_json::json!({"protocol": 1});
    std::fs::write(fixture.path(), serde_json::to_vec(&partial).unwrap()).unwrap();
    let loaded = fixture
        .durable()
        .expect("a malformed receipt keeps the row readable");
    assert!(!loaded.herdr_before_start_stop.unwrap().supported());
}
