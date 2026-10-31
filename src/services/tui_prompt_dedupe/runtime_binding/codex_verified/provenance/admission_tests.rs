use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::CodexEpisodeSpan;

/// Each test owns a channel so the process registry is never shared between tests.
fn origin(channel: u64, nonce: &str) -> OwedOrigin {
    let source = SourceId {
        session_id: "native".into(),
        path: "/native/rollout.jsonl".into(),
        dev: 1,
        ino: 2,
    };
    let execution = ExecutionProofRef {
        owner_runtime_root: "/runtime".into(),
        tmux_session: "pane".into(),
        execution_nonce: "execution".into(),
        proof_seq: 3,
        source,
    };
    let episode = TurnEpisodeRef {
        channel_id: channel,
        user_message_id: 10,
        request_owner_id: 8,
        turn_nonce: nonce.into(),
        native_turn_id: "n1".into(),
    };
    let span = CodexEpisodeSpan {
        execution,
        episode,
        delivery_channel_id: channel,
        offset_authority_channel_id: channel,
        generation_mtime_ns: 1,
        start: 0,
        end: Some(10),
    };
    OwedOrigin { span, range: 0..5 }
}

fn token(nonce: &str) -> Arc<CancelToken> {
    Arc::new(CancelToken::from_persisted_turn_nonce(Some(nonce.into())))
}

fn issue(
    origin: &OwedOrigin,
    token: &Arc<CancelToken>,
) -> Result<Arc<EpisodePermissionRecord>, &'static str> {
    let (execution, episode) = (origin.span.execution.clone(), origin.span.episode.clone());
    issue_for_tests(execution, episode, token)
}

struct Ready(u64);

impl Ready {
    fn on(channel: u64) -> Self {
        set_ready_for_tests(channel, true);
        Self(channel)
    }
}

impl Drop for Ready {
    fn drop(&mut self) {
        set_ready_for_tests(self.0, false);
        forget_channel_for_tests(self.0);
    }
}

#[test]
fn one_arc_per_exact_episode_even_when_issued_concurrently_and_a_conflict_never_replaces_it() {
    let _ready = Ready::on(584_601);
    let origin = origin(584_601, "turn");
    let owner = token("turn");
    let issued: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| issue(&origin, &owner).unwrap()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(issued.iter().all(|record| Arc::ptr_eq(record, &issued[0])));
    let restored = token("turn");
    assert_eq!(
        issue(&origin, &restored).err(),
        Some("episode_permit_conflict")
    );
    let found = lookup(&origin).unwrap();
    assert!(
        Arc::ptr_eq(&found, &issued[0]),
        "a conflicting token replaced the record"
    );
    assert_eq!(found.admit(&origin, || "posted"), Ok("posted"));
}

#[test]
fn a_same_message_successor_or_foreign_identity_finds_no_permit() {
    let _ready = Ready::on(584_602);
    let original = origin(584_602, "turn");
    issue(&original, &token("turn")).unwrap();
    let successor = origin(584_602, "successor-turn");
    assert_eq!(lookup(&successor).err(), Some("episode_permit_unavailable"));
    let mut owner = original.clone();
    owner.span.episode.request_owner_id += 1;
    assert_eq!(lookup(&owner).err(), Some("episode_permit_mismatch"));
    let mut execution = original.clone();
    execution.span.execution.execution_nonce = "other".into();
    assert_eq!(lookup(&execution).err(), Some("episode_permit_mismatch"));
    let mut native = original.clone();
    native.span.episode.native_turn_id = "n2".into();
    assert_eq!(lookup(&native).err(), Some("episode_permit_mismatch"));
}

#[test]
fn cancellation_before_or_after_attach_withholds_every_later_admission() {
    let _ready = Ready::on(584_603);
    let before = origin(584_603, "before");
    let cancelled = token("before");
    cancelled.publish_cancel("user stop");
    assert!(matches!(
        issue(&before, &cancelled),
        Err("cancelled_before_attach")
    ));
    let after = origin(584_603, "after");
    let live = token("after");
    let record = issue(&after, &live).unwrap();
    assert_eq!(record.admit(&after, || ()), Ok(()));
    live.publish_cancel_if_source_absent("voice barge-in");
    assert_eq!(record.admit(&after, || ()), Err("episode_cancelled"));
}

/// A cancellation published while the admitted closure runs waits for the record lock.
#[test]
fn a_cancellation_during_admission_waits_until_the_closure_returns() {
    let _ready = Ready::on(584_604);
    let origin = origin(584_604, "turn");
    let owner = token("turn");
    let record = issue(&origin, &owner).unwrap();
    let (trying, tried) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let admitted = record.admit(&origin, || {
            let cancel = scope.spawn(|| {
                trying.send(()).unwrap();
                owner.publish_cancel("user stop");
            });
            tried.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            let raced = owner.cancelled.load(Ordering::Acquire);
            (cancel, raced)
        });
        let (cancel, raced) = admitted.unwrap();
        assert!(
            !raced,
            "the cancellation landed inside the admitted closure"
        );
        cancel.join().unwrap();
    });
    assert!(owner.cancelled.load(Ordering::Acquire));
    assert_eq!(record.admit(&origin, || ()), Err("episode_cancelled"));
}

#[test]
fn not_ready_or_revoked_records_admit_nothing() {
    let origin = origin(584_605, "turn");
    let record = issue(&origin, &token("turn")).unwrap();
    assert_eq!(lookup(&origin).err(), Some("verified_owed_not_ready"));
    assert_eq!(record.admit(&origin, || ()), Err("verified_owed_not_ready"));
    let _ready = Ready::on(584_605);
    assert_eq!(record.admit(&origin, || ()), Ok(()));
    revoke_channel(584_605, "o_unavailable");
    assert_eq!(record.admit(&origin, || ()), Err("o_unavailable"));
}

/// C17-A3: even a valid exact permit cannot start Prepared or POST at the production fallback.
/// No READY override is installed: the real writer must reach the default-off admission check.
#[cfg(unix)]
#[tokio::test]
async fn production_readiness_fallback_withholds_real_writer_effects() {
    use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
    use crate::services::tui_o::store::{
        Initialized, LEDGER_FILE, OStore, STORE_DIR_NAME, StoreConfig,
    };
    use crate::services::tui_o::writer::deliver::Step;
    use crate::services::tui_o::writer::owed::tests::owed_writer;
    use crate::services::tui_o::writer::pieces::{Derived, PieceWork};

    struct RegistryCleanup(u64);
    impl Drop for RegistryCleanup {
        fn drop(&mut self) {
            forget_channel_for_tests(self.0);
        }
    }

    let channel = 584_606;
    let _cleanup = RegistryCleanup(channel);
    let origin = origin(channel, "fallback-turn");
    let owner = token("fallback-turn");
    let record = issue(&origin, &owner).unwrap();
    let lookup_result = lookup(&origin);

    let runtime = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
        .unwrap()
        .unwrap();
    let era = store
        .begin_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![],
                initial_anchor: 100,
                build_digest: "c17-a3".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let channel_store = store.open_channel(&era, channel).unwrap().unwrap();
    let (mut writer, port) = owed_writer(channel_store);
    let ledger_path = runtime
        .path()
        .join(STORE_DIR_NAME)
        .join(channel.to_string())
        .join(LEDGER_FILE);
    let before_bytes = std::fs::read(&ledger_path).unwrap();
    let before_serial = writer.store().ledger().next_serial();
    let piece = Derived::Piece(PieceWork {
        output_origin: None,
        unit_key: UnitKey {
            channel_id: channel,
            provider: ShadowProvider::Codex,
            native_key: "c17-a3-body".into(),
            kind: UnitKind::Body,
        },
        index: 0,
        payload: "출처 활성화 전에는 보내지 않는다".into(),
        notice_origin: None,
    });
    let step = writer
        .deliver_admitted(&piece, Some((&record, &origin)))
        .await;

    // Check actual effects before helper results so an opened fallback fails on a real POST.
    assert!(port.posts().is_empty(), "fallback permitted a real POST");
    assert_eq!(std::fs::read(ledger_path).unwrap(), before_bytes);
    assert_eq!(writer.store().ledger().next_serial(), before_serial);
    assert_eq!(step, Step::Withheld);
    assert!(!writer.is_stopped());
    assert_eq!(lookup_result.err(), Some("verified_owed_not_ready"));
    assert_eq!(record.admit(&origin, || ()), Err("verified_owed_not_ready"));
}
