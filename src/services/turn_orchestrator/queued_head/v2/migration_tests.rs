use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::durable_publish::flush_probe::{Plan, Step, Watch};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::queued_head::{
    PersistedHeadInjection, canonical_item, compat, mend::HeadAttempt, parse_snapshot,
    snapshot_json,
};
use poise::serenity_prelude::ChannelId;
use serde_json::json;
use std::path::{Path, PathBuf};

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":7272,
        "source_message_ids":[id],"queued_generation":5,"text":"legacy original"}))
    .unwrap()
}

fn path(channel: u64) -> PathBuf {
    crate::services::turn_orchestrator::queued_head::pending_queue_file_path(
        &ProviderKind::Claude,
        "v2-migration",
        ChannelId::new(channel),
    )
    .unwrap()
}

fn seed(channel: u64, raw: &str) -> PathBuf {
    let file = path(channel);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, raw).unwrap();
    file
}

fn legacy_attempt() -> HeadAttempt {
    HeadAttempt {
        nonce: "legacy-nonce".into(),
        transcript: "/legacy/session.jsonl".into(),
        session_id: "legacy-session".into(),
        file: (3, 4),
        offset: 9,
    }
}

fn envelope(items: Vec<PendingQueueItem>, attempted: bool) -> String {
    let head = attempted.then(|| {
        let original = items[0].clone();
        let intervention =
            crate::services::turn_orchestrator::pending_queue_persistence::pending_queue_item_to_intervention(
                original.clone(),
                std::time::SystemTime::now(),
                std::time::Instant::now(),
            );
        PersistedHeadInjection {
            identity: sources::identity(&original),
            original: canonical_item(&intervention, ChannelId::new(7272)),
            phase: HeadPhase::MayHaveSent,
            attempt: Some(legacy_attempt()),
        }
    });
    let meta = SnapshotMeta {
        head_injection: head,
        terminal_identities: vec![sources::identity(&item(99))],
    };
    snapshot_json(items, Some(&meta)).unwrap()
}

fn adopt(file: &Path, channel: u64) -> Result<Owner, String> {
    Owner::adopt(file.to_path_buf(), format!("migration-{channel}"))
}

#[test]
fn legacy_shapes_migrate_once_without_inventing_provenance() {
    let _root = TestRuntimeRootGuard::new();
    let shapes = [
        (
            "array",
            serde_json::to_string(&vec![item(100)]).unwrap(),
            true,
        ),
        ("envelope", envelope(vec![item(100)], false), true),
        ("metadata-only", envelope(vec![], false), true),
        ("attempted", envelope(vec![item(100)], true), true),
        ("unknown", "{\"version\":3,\"items\":[]}".into(), false),
        ("corrupt", "{\"version\":1,\"items\":[".into(), false),
    ];
    for (n, (shape, raw, migrates)) in shapes.into_iter().enumerate() {
        let channel = 727_500 + n as u64;
        let file = seed(channel, &raw);
        let Ok(mut owner) = adopt(&file, channel) else {
            assert!(!migrates, "{shape} must migrate");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), raw, "{shape} kept");
            continue;
        };
        assert!(migrates, "{shape} must be refused");
        let migrated = std::fs::read_to_string(&file).unwrap();
        assert!(matches!(
            compat::inspect(&migrated),
            Ok(compat::SnapshotRead::ReadOnlyV2(_))
        ));
        assert!(
            parse_snapshot(&migrated).is_err(),
            "{shape}: old reader refuses V2"
        );
        assert!(compat::require_legacy_write(&file).is_err());
        assert!(
            adopt(&file, channel + 1).is_err(),
            "{shape}: namespace is explicit"
        );
        drop(adopt(&file, channel).unwrap());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            migrated,
            "{shape}: once"
        );
        if shape == "attempted" {
            let head = owner.snapshot.head_injection.as_ref().unwrap();
            assert!(head.attempt.is_none(), "no provider or target is invented");
            assert_eq!(head.legacy_attempt, Some(legacy_attempt()));
            owner.tick(0).unwrap();
            let retained = &owner.snapshot.retained_inputs[0].input;
            assert_eq!(retained.legacy_attempt, Some(legacy_attempt()));
            assert!(
                owner.snapshot.items.is_empty(),
                "possibly sent is never re-queued"
            );
        }
        if shape != "array" {
            assert_eq!(owner.snapshot.terminal_identities.len(), 1, "{shape}");
        }
    }
}

#[test]
fn legacy_terminal_window_starts_at_the_first_tick() {
    let _root = TestRuntimeRootGuard::new();
    let file = seed(727_520, &envelope(vec![], false));
    let mut owner = adopt(&file, 727_520).unwrap();
    owner.tick(5_000).unwrap();
    assert_eq!(owner.snapshot.terminal_identities[0].settled_at_ms, 5_000);
    owner.tick(5_000 + 86_399_999).unwrap();
    assert_eq!(owner.snapshot.terminal_identities.len(), 1);
    owner.tick(5_000 + 86_400_000).unwrap();
    assert!(
        owner.snapshot.terminal_identities.is_empty(),
        "a legacy terminal is fenced for 24h, not forever"
    );
}

#[test]
fn an_unchanged_tick_publishes_nothing() {
    let _root = TestRuntimeRootGuard::new();
    let file = seed(727_530, &envelope(vec![item(100)], false));
    let mut owner = adopt(&file, 727_530).unwrap();
    owner.tick(1_000).unwrap();
    let before = std::fs::read_to_string(&file).unwrap();
    let probe = Watch::new(file.parent().unwrap(), Plan::default());
    owner.tick(2_000).unwrap();
    assert!(
        probe.events().is_empty(),
        "no-op tick writes or syncs nothing"
    );
    assert_eq!(probe.count(Step::Write), 0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
}

#[test]
fn adoption_forgets_the_ledger_entry_of_a_proven_unsent_paste() {
    let _root = TestRuntimeRootGuard::new();
    for (n, proven) in [false, true].into_iter().enumerate() {
        let channel = 727_540 + n as u64;
        let pane = format!("pane-unsent-{n}");
        let original = item(100);
        let attempt = AttemptIdentity {
            nonce: format!("unsent-{n}"),
            provider: "codex".into(),
            target: TargetIdentity {
                host: "tmux".into(),
                owner: pane.clone(),
                root: "/test".into(),
                execution_nonce: "execution".into(),
            },
            source: AttemptSource::Existing {
                session_id: "native".into(),
                transcript: "/test/rollout".into(),
                file: [1, 2],
                offset: 0,
            },
        };
        let snapshot = SnapshotV2 {
            version: 2,
            items: vec![],
            head_injection: None,
            retained_inputs: vec![RetainedInput {
                input: InputAttempt {
                    identity: sources::identity(&original),
                    original,
                    phase: HeadPhase::Reserved,
                    attempt: Some(attempt.clone()),
                    payload: None,
                    confirmation_deadline_ms: None,
                    resolved_source: None,
                    payload_window: None,
                    resolved_target: None,
                    preparation_hold: None,
                    legacy_attempt: None,
                },
                reason: RetainedReason::Unconfirmed,
                disposition: proven.then_some(RetainedDisposition::KnownNotSent),
                notice: NoticeIntent {
                    kind: "retained_retry_required".into(),
                    enqueued: false,
                },
            }],
            terminal_identities: vec![],
            namespace: Some(format!("migration-{channel}")),
            admission_receipts: vec![],
            admission_sequence: 0,
            admission_binds: Vec::new(),
        };
        let file = seed(channel, &serde_json::to_string_pretty(&snapshot).unwrap());
        let register = || {
            crate::services::tui_prompt_dedupe::register_injected_steer(
                "codex",
                &pane,
                &attempt.nonce,
                "turn",
                None,
            )
        };
        assert!(register());
        drop(adopt(&file, channel).unwrap());
        assert_eq!(
            register(),
            proven,
            "only a proven-unsent paste leaves the observation ledger"
        );
    }
}
