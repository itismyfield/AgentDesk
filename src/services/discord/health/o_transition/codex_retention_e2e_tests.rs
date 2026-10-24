//! Native source/P5/context -> periodic sweep -> targeted Inspect -> scoped Apply -> durable audit.
use super::*;
use crate::services::discord::health::o_transition::automatic::test_support;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::writer::tests::with_ready_codex_backlog;
use crate::services::turn_orchestrator::{ChannelMailboxSnapshot, registry_purge::MailboxRefusal};

fn backlog(fixture: &Fixture, count: usize) -> (Vec<SourceId>, chrono::DateTime<chrono::Utc>) {
    let now = chrono::Utc::now();
    let older = now - chrono::TimeDelta::days(8);
    let root = crate::config::runtime_root().unwrap();
    let mut sources = Vec::new();
    let mut events = Vec::new();
    for index in 0..count {
        let id = format!("019e660d-4859-7522-9cee-{:012x}", index + 100);
        let path = fixture
            .context
            .provider_root
            .as_ref()
            .unwrap()
            .join(format!("rollout-old-{id}.jsonl"));
        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/codex_retention/activity_rollout_016x.jsonl"
        ));
        let rows: Vec<serde_json::Value> = golden
            .lines()
            .enumerate()
            .map(|(row_index, line)| {
                let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
                let timestamp = (older + chrono::TimeDelta::seconds(row_index as i64)).to_rfc3339();
                row["timestamp"] = json!(timestamp);
                if row["type"] == "session_meta" {
                    row["payload"]["id"] = json!(id);
                    row["payload"]["session_id"] = json!(id);
                    row["payload"]["timestamp"] = json!(timestamp);
                } else {
                    // Distinct source executions have distinct native units; mirrors keep that id.
                    for pointer in ["/payload/id", "/payload/item/id"] {
                        if let Some(value) = row.pointer_mut(pointer)
                            && let Some(native) = value.as_str()
                        {
                            *value = json!(format!("{native}-source-{index}"));
                        }
                    }
                }
                if row["payload"].get("thread_id").is_some() {
                    row["payload"]["thread_id"] = json!(id);
                }
                row
            })
            .collect();
        let bytes = rows
            .iter()
            .map(|row| format!("{}\n", serde_json::to_string(row).unwrap()))
            .collect::<String>();
        std::fs::write(&path, bytes).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let source = SourceId {
            session_id: id,
            path,
            dev: meta.dev(),
            ino: meta.ino(),
        };
        events.push(BindingEvent {
            seq: index as u64 + 1,
            channel_id: CHANNEL,
            provider: "codex".into(),
            tmux_session: fixture.context.tmux_session.clone(),
            execution_nonce: Some(format!("{:032x}", index + 100)),
            old: sources.last().cloned(),
            new: BindingTarget::Source(source.clone()),
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: Some("session_start".into()),
                received_at: older,
            },
            committed_at: older,
        });
        sources.push(source);
    }
    let mut current = std::fs::OpenOptions::new()
        .append(true)
        .open(&fixture.source.path)
        .unwrap();
    use std::io::Write;
    writeln!(current, "{}", json!({"timestamp":now.to_rfc3339(),"type":"response_item","payload":{"type":"message","role":"assistant","id":"current-native","content":[{"type":"output_text","text":"current native dialogue"}]}})).unwrap();
    events.push(BindingEvent {
        seq: count as u64 + 1,
        channel_id: CHANNEL,
        provider: "codex".into(),
        tmux_session: fixture.context.tmux_session.clone(),
        execution_nonce: Some(fixture.context.execution_nonce.clone()),
        old: sources.last().cloned(),
        new: BindingTarget::Source(fixture.source.clone()),
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: now,
        },
        committed_at: now,
    });
    sources.push(fixture.source.clone());
    std::fs::write(
        root.join(format!("binding_events/{CHANNEL}.log")),
        events
            .iter()
            .map(|event| format!("{}\n", serde_json::to_string(event).unwrap()))
            .collect::<String>(),
    )
    .unwrap();
    (sources, older + chrono::TimeDelta::seconds(11))
}

async fn assert_sweep(fixture: &Fixture, shared: Arc<SharedData>, count: usize, configured: bool) {
    let (sources, activity_at) = backlog(fixture, count);
    shared.settings.write().await.allowed_channel_ids = vec![CHANNEL];
    shared.mailboxes.insert_idle_snapshot_for_test(
        ChannelId::new(CHANNEL),
        ChannelMailboxSnapshot::default(),
        MailboxRefusal::Closed,
    );
    let registry = HealthRegistry::new();
    registry.register("codex".into(), shared.clone()).await;
    let (reply, cursors, reading) = with_ready_codex_backlog(
        sources,
        fixture.context.tmux_session.clone(),
        configured,
        |ready| async move {
            let first = test_support::codex_sweep_for_test(&registry, &ready, CHANNEL)
                .await
                .unwrap();
            assert!(first.refused.is_none(), "{first:?}");
            let owed = first
                .summary
                .as_ref()
                .unwrap()
                .obligations
                .as_ref()
                .unwrap();
            assert_eq!(
                (
                    owed.owed,
                    owed.prepared,
                    owed.unsealed,
                    owed.posting,
                    owed.verified_work
                ),
                (0, 0, 0, 0, 0),
                "the actual discovery confirms every output obligation drained before retention"
            );
            assert!(owed.protected.is_empty());
            first
        },
    )
    .await;
    for cursor in &cursors[..count] {
        assert!(
            cursor.retired,
            "[ba:e2e] old native source retired after actual sweep: {:?}; reply={reply:?}",
            cursor.source
        );
        let audit = cursor
            .transition_cleanup
            .as_ref()
            .expect("durable retention audit");
        assert_eq!(
            audit.retention.as_ref().unwrap().last_dialogue,
            activity_at,
            "durable age is the latest activity, including terminal metadata"
        );
        let retention = serde_json::to_value(audit).unwrap();
        assert!(
            retention["retention"].is_object(),
            "new Codex audit contains typed age/source evidence: {retention}"
        );
    }
    assert!(!cursors[count].retired, "current execution source survives");
    assert_eq!(
        reading,
        Some(1),
        "actual tend reports remaining live reader count"
    );
    shared
        .mailboxes
        .remove_fixture_for_test(ChannelId::new(CHANNEL));
}

#[test]
fn codex_native_legacy_sweep_retires_only_aged_backlog_and_records_atomic_audit() {
    let fixture = Fixture::new(false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(assert_sweep(&fixture, fixture.shared(None), 2, false));
}

#[test]
fn pg_codex_native_configured_herdr_sweep_retires_aged_backlog() {
    let fixture = Fixture::new(true);
    let _node =
        crate::config::session_hosts::force_for_test(Some("test-node"), &[(CHANNEL, "test-node")]);
    let rig = crate::services::session_host::herdr_socket_rig_tests::HerdrRig::start();
    rig.show_panes(&["w1-1"]);
    let _registry = rig.registry_as_on_this_thread(
        "test-node",
        "test-node-endpoint",
        rig.socket(),
        "agentdesk",
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let db=crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool=db.connect_and_migrate().await;
        let shared=fixture.shared(Some(pool.clone()));
        let mut owner=crate::db::dispatched_sessions::hosted_execution::tests::owner(&CHANNEL.to_string());
        owner.provider="codex".into();owner.discord_token_hash=shared.token_hash.clone();owner.logical_key=fixture.context.tmux_session.clone();owner.runtime_root=fixture.context.owner_runtime_root.clone();
        let mut record=crate::db::dispatched_sessions::hosted_execution::tests::record(&owner,&fixture.context.execution_nonce,crate::db::dispatched_sessions::hosted_execution::HostedState::Bound);
        let location=record.location.as_mut().unwrap();location.pane_id="w1-1".into();location.endpoint_config_key="test-node-endpoint".into();location.socket_addr=rig.socket().display().to_string();
        record.expected.as_mut().unwrap().binding_provider="codex".into();
        let key=format!("codex/{}/test-node:{}",shared.token_hash,fixture.context.tmux_session);
        sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3, $4)").bind(key).bind(&shared.token_hash).bind(CHANNEL.to_string()).bind(serde_json::to_value(record).unwrap()).execute(&pool).await.unwrap();
        assert_sweep(&fixture,shared,2,true).await;
        pool.close().await;drop(db);
    });
    assert!(
        rig.sends().is_empty(),
        "retention never sends terminal input"
    );
}
