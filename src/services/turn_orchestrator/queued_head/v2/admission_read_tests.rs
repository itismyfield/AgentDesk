use super::super::v2_types::marker_control::MarkerControl;
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, queued_head::compat};
use poise::serenity_prelude::ChannelId;
use serde_json::json;

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":8057101,
        "source_message_ids":[id],"queued_generation":5,"text":"원문 그대로\n두 번째 줄"}))
    .unwrap()
}

fn control() -> MarkerControl {
    serde_json::from_value(json!({"cleanup_id":"cleanup-a",
        "identity":{"primary":42,"queued_generation":5,
            "sources":[{"message_id":42,"queued_generation":5}]},
        "front_receipt":"ticket-a","epoch":"epoch-a",
        "witness":{"nonce":null,"source":null},
        "expected":{"kind":"legacy","row_id":17,"exact_session_key":"logical-a",
            "owner":{"provider":"codex","discord_token_hash":"fixture-token",
                "channel_id":"8057101","logical_key":"logical-a","owner_node":"fixture-node",
                "runtime_root":"/fixture"}},
        "paths":[{"path":"/fixture/canonical.kind","parent":[1,2],
            "entry":{"kind":"codex_tui","file":[1,3]}}],
        "stage":{"kind":"outbox","event_id":"scope/cleanup-a/ticket-a/epoch-a"},
        "attempt":{"nonce":"attempt-a","provider":"codex",
            "target":{"host":"herdr","owner":"logical-a","root":"/fixture",
                "execution_nonce":"execution-a"},
            "source":{"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null}}}))
    .unwrap()
}

#[tokio::test]
async fn v3_boot_recovery_refuses_effectful_adoption_and_preserves_original_bytes() {
    let directory = TestRuntimeRootGuard::new();
    let path = directory.path().join("runtime").join("8057101.json");
    let mut seed = Owner::adopt(path.clone(), "scope".into()).unwrap();
    assert!(seed.enqueue(&item(42)).unwrap());
    let mut snapshot = serde_json::to_value(&seed.snapshot).unwrap();
    snapshot["version"] = json!(3);
    snapshot["a_control"] = json!([control()]);
    let raw = serde_json::to_string_pretty(&snapshot).unwrap();
    std::fs::write(&path, &raw).unwrap();
    assert!(matches!(
        compat::inspect(&raw),
        Ok(compat::SnapshotRead::ReadOnlyV2(_))
    ));
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(8057101));
    let history =
        crate::services::tui_input::human_frame::history::v2::Registry::runtime().unwrap();
    let result = crate::services::discord::recover_input_owner_for_test(
        &handle,
        (path.clone(), "scope".into()),
        history,
        None,
        ("codex", 8057101, 1),
    )
    .await;
    let error = result.expect_err("readable V3 must not reach the owner's tick or requeue");
    assert!(error.contains("read-only"), "{error}");
    assert!(handle.v2_request(|slot| Ok(slot.is_none())).await.unwrap());
    assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
}

#[test]
fn an_existing_v2_owner_keeps_production_bytes_without_marker_records() {
    let directory = TestRuntimeRootGuard::new();
    let path = directory.path().join("runtime").join("queue.json");
    let mut owner = Owner::adopt(path.clone(), "scope".into()).unwrap();
    assert!(owner.enqueue(&item(42)).unwrap());
    let before = std::fs::read(&path).unwrap();
    drop(owner);
    let mut restored = Owner::adopt(path.clone(), "scope".into()).unwrap();
    restored.tick(0).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let value: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(value["version"], 2);
    assert!(value.get("a_control").is_none());
    assert_eq!(value["items"][0]["text"], "원문 그대로\n두 번째 줄");
    assert_eq!(restored.front().unwrap().1.message_id, 42);
}

#[test]
fn control_debt_pins_real_tick_and_prune_history_until_acknowledged() {
    let directory = TestRuntimeRootGuard::new();
    let mut owner = Owner::adopt(
        directory.path().join("runtime").join("queue.json"),
        "scope".into(),
    )
    .unwrap();
    let debt = control();
    let identity = debt.identity.clone();
    owner.snapshot.version = 3;
    owner.snapshot.a_control.push(debt);
    owner.snapshot.terminal_identities = vec![
        TerminalIdentity {
            identity: identity.clone(),
            settled_at_ms: 1,
        },
        TerminalIdentity {
            identity: sources::identity(&item(43)),
            settled_at_ms: 1,
        },
    ];
    owner.snapshot.admission_binds = vec![
        AdmissionBind {
            receipt: "ticket-a".into(),
            at: BindAt::Queued {
                index: 9,
                item: "old".into(),
            },
            context: None,
        },
        AdmissionBind {
            receipt: "unreferenced-queued".into(),
            at: BindAt::Queued {
                index: 9,
                item: "old".into(),
            },
            context: None,
        },
        AdmissionBind {
            receipt: "settled-a".into(),
            at: BindAt::Settled {
                attempt: "attempt-a".into(),
                source: None,
                settled_at_ms: 1,
            },
            context: None,
        },
        AdmissionBind {
            receipt: "unreferenced-settled".into(),
            at: BindAt::Settled {
                attempt: "attempt-b".into(),
                source: None,
                settled_at_ms: 1,
            },
            context: None,
        },
    ];
    owner.snapshot.prune_binds();
    owner.tick(86_400_002).unwrap();
    assert_eq!(owner.snapshot.terminal_identities.len(), 1);
    assert_eq!(owner.snapshot.terminal_identities[0].identity, identity);
    let kept: Vec<_> = owner
        .snapshot
        .admission_binds
        .iter()
        .map(|bind| bind.receipt.as_str())
        .collect();
    assert_eq!(kept, ["ticket-a", "settled-a"]);
    owner.snapshot.a_control.clear();
    owner.snapshot.prune_binds();
    owner.tick(86_400_003).unwrap();
    assert!(owner.snapshot.terminal_identities.is_empty());
    assert!(owner.snapshot.admission_binds.is_empty());
    let raw = std::fs::read_to_string(&owner.path).unwrap();
    let compat::SnapshotRead::ReadOnlyV2(restored) = compat::inspect(&raw).unwrap() else {
        panic!("acknowledged V3 was downgraded to legacy");
    };
    assert_eq!(restored.version, 3);
    assert!(!restored.has_control_debt());
}

#[test]
fn marker_debt_sources_fence_replayed_ids_without_recreating_queue_items() {
    let directory = TestRuntimeRootGuard::new();
    let mut owner = Owner::adopt(
        directory.path().join("runtime").join("queue.json"),
        "scope".into(),
    )
    .unwrap();
    owner.snapshot.a_control.push(control());
    assert!(owner.fenced(&item(42)));
    assert!(!owner.fenced(&item(43)));
    assert!(owner.snapshot.items.is_empty());
    assert!(owner.snapshot.head_injection.is_none());
    owner.snapshot.a_control.clear();
    assert!(!owner.fenced(&item(42)));
}

#[test]
fn control_only_origin_stays_visible_until_its_exact_debt_is_acknowledged() {
    let directory = TestRuntimeRootGuard::new();
    let folder = directory
        .path()
        .join("runtime/discord_pending_queue/codex/origin-read");
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join("8057101.json");
    let mut owner = Owner::adopt(path.clone(), "scope".into()).unwrap();
    owner.snapshot.version = 3;
    owner.snapshot.a_control.push(control());
    let context = |origin: &str| {
        Some(AdmissionContext {
            original: None,
            execution: Some(
                json!({"origin":"headless","metadata":{"human_input":{"origin_id":origin}}}),
            ),
        })
    };
    owner.snapshot.admission_binds = vec![
        AdmissionBind {
            receipt: "ticket-a".into(),
            at: BindAt::Queued {
                index: 9,
                item: "old".into(),
            },
            context: context("kept-origin"),
        },
        AdmissionBind {
            receipt: "unreferenced".into(),
            at: BindAt::Queued {
                index: 9,
                item: "old".into(),
            },
            context: context("unrelated-origin"),
        },
    ];
    for at in [
        BindAt::Queued {
            index: 9,
            item: "old".into(),
        },
        BindAt::Settled {
            attempt: "attempt-a".into(),
            source: None,
            settled_at_ms: 1,
        },
    ] {
        owner.snapshot.admission_binds[0].at = at;
        let raw = serde_json::to_string_pretty(&owner.snapshot).unwrap();
        std::fs::write(&path, &raw).unwrap();
        assert_eq!(
            origin::waiting_origin("codex", 8057101, "kept-origin"),
            Ok(Some(origin::Waiting::Accepted))
        );
        assert_eq!(
            origin::waiting_origin("codex", 8057101, "unrelated-origin"),
            Ok(None)
        );
        assert!(owner.snapshot.items.is_empty());
        assert!(owner.snapshot.head_injection.is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    }
    owner.snapshot.a_control.clear();
    let raw = serde_json::to_string_pretty(&owner.snapshot).unwrap();
    std::fs::write(&path, &raw).unwrap();
    assert_eq!(
        origin::waiting_origin("codex", 8057101, "kept-origin"),
        Ok(None)
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
}
