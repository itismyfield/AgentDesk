use super::*;
use crate::config::TestRuntimeRootGuard;
#[cfg(unix)]
use crate::services::discord::runtime_store::durable_publish::flush_probe::{Plan, Step, Watch};
use crate::services::provider::ProviderKind;
#[cfg(unix)]
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

#[cfg(unix)]
fn legacy_attempt() -> HeadAttempt {
    HeadAttempt {
        nonce: "legacy-nonce".into(),
        transcript: "/legacy/session.jsonl".into(),
        session_id: "legacy-session".into(),
        file: (3, 4),
        offset: 9,
    }
}

#[cfg(unix)]
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

// The pre-notice V1 writer's format: both the queue and reserved original lack notice metadata.
#[cfg(unix)]
const LEGACY_V1_RESERVED: &str = include_str!("../fixtures/legacy_v1_reserved.json");

#[cfg(unix)]
#[tokio::test]
async fn unmarked_v1_heads_recover_and_accept_new_input() {
    use crate::services::turn_orchestrator::{
        ChannelMailboxRegistry, QueuePersistenceContext, load_pending_queues,
        pending_queue_persistence::load_pending_queues_checked,
    };

    let _root = TestRuntimeRootGuard::new();
    assert!(!LEGACY_V1_RESERVED.contains("queue_notice_claimed"));
    let channel = ChannelId::new(7272);
    for phase in ["reserved", "may_have_sent", "unknown"] {
        let raw = LEGACY_V1_RESERVED.replace("\"reserved\"", &format!("\"{phase}\""));
        let file = seed(channel.get(), &raw);
        let loaded = load_pending_queues_checked(&ProviderKind::Claude, "v2-migration");
        assert!(
            loaded.quarantined.is_empty(),
            "{phase}: valid old V1 must recover: {:?}",
            loaded.quarantined
        );
        assert!(loaded.scan_errors.is_empty());
        let (queues, _) = load_pending_queues(&ProviderKind::Claude, "v2-migration");
        let ids: Vec<_> = queues[&channel]
            .iter()
            .map(|item| item.message_id.get())
            .collect();
        assert_eq!(ids, [100, 101], "{phase}: boot keeps order");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), raw);

        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "v2-migration", None);
        handle
            .v2_ensure_channel(&persistence, channel)
            .await
            .unwrap();
        let mut fresh = item(102);
        fresh.queue_notice_claimed = Some(false);
        assert_eq!(
            handle.v2_admit_ticket(fresh, "after-upgrade".into()).await,
            Ok(source_admission::Admitted::Queued),
            "{phase}: a new input is accepted after upgrade"
        );
        let owner = Owner::adopt(file, "claude/v2-migration".into()).unwrap();
        let ids: Vec<_> = owner
            .snapshot
            .items
            .iter()
            .map(|item| item.message_id)
            .collect();
        assert_eq!(ids, [100, 101, 102]);
        assert_eq!(owner.snapshot.items[0].queue_notice_claimed, None);
        assert_eq!(owner.snapshot.items[1].queue_notice_claimed, None);
        assert_eq!(owner.snapshot.items[2].queue_notice_claimed, Some(false));
        if phase == "reserved" {
            assert!(owner.snapshot.head_injection.is_none());
            assert_eq!(owner.front().unwrap().1.message_id, 100);
        } else {
            assert_eq!(
                owner.snapshot.head_injection.as_ref().unwrap().phase,
                HeadPhase::Unknown
            );
            assert!(owner.front().is_none(), "{phase}: possibly sent stays held");
        }
    }
}

#[cfg(unix)]
#[test]
fn v1_head_identity_ignores_only_notice_metadata() {
    for queued_flag in [None, Some(false), Some(true)] {
        for original_flag in [None, Some(false), Some(true)] {
            let mut snapshot: serde_json::Value = serde_json::from_str(LEGACY_V1_RESERVED).unwrap();
            if let Some(flag) = queued_flag {
                snapshot["items"][0]["queue_notice_claimed"] = json!(flag);
            }
            if let Some(flag) = original_flag {
                snapshot["head_injection"]["original"]["queue_notice_claimed"] = json!(flag);
            }
            let parse = |value: &serde_json::Value| parse_snapshot(&value.to_string());
            let (items, meta) =
                parse(&snapshot).expect("notice metadata does not change V1 identity");
            assert_eq!(items[0].queue_notice_claimed, queued_flag);
            assert_eq!(
                meta.unwrap()
                    .head_injection
                    .unwrap()
                    .original
                    .queue_notice_claimed,
                original_flag
            );

            let mut edited = snapshot.clone();
            edited["items"][0]["text"] = json!("changed body");
            assert!(
                matches!(parse(&edited), Err(error) if error == "reserved snapshot payload differs from its original")
            );
            snapshot["items"][0]["queued_generation"] = json!(6);
            assert!(
                matches!(parse(&snapshot), Err(error) if error == "reserved snapshot source identity differs from its original")
            );
        }
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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
            owed_drop_receipts: Vec::new(),
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
