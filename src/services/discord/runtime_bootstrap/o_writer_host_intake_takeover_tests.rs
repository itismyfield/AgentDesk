//! Late takeovers through the gateway against another turn's open intake: only the rows of the
//! turn Legacy's mailbox runs pass, and O posts that turn once the other input ends.

use super::*;

/// Signals each intake fence `channel` passes from now on, until `passed` is dropped.
fn count_fences(channel: u64, passed: tokio::sync::mpsc::UnboundedSender<()>) {
    test_hook::set(channel, Step::AfterFence, move || {
        if passed.send(()).is_ok() {
            count_fences(channel, passed);
        }
        Ok(())
    });
}

/// Through the real gateway writer, a Claude late takeover of the turn Legacy's mailbox runs waits
/// while another turn's input is open; once only that input ends, its own row passes and O posts.
#[test]
fn claude_takeover_waits_for_another_turns_open_intake_and_passes_its_own_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(intake_takeover(Claude, 64_700_521));
}

/// As the Claude case, for a Codex late takeover of a fresh rollout.
#[test]
fn codex_takeover_waits_for_another_turns_open_intake_and_passes_its_own_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(intake_takeover(Codex, 64_700_522));
}

async fn intake_takeover(provider: ShadowProvider, channel: u64) {
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::tui_o::writer::host;
    use crate::test_support::wait::{WAIT_CAP, await_within};
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let rest = Rest::start(channel).await;
    let (name, kind) = match provider {
        Claude => ("claude", ProviderKind::Claude),
        Codex => ("codex", ProviderKind::Codex),
    };
    let _reserved = reserve_as(name, &[channel]);
    // Another turn's message waits as an open intake row before the writer starts.
    let (own, other) = (channel * 10 + 1, channel * 10 + 2);
    let other_row = open_intake_as(&pool, channel, name, &other.to_string()).await;

    let session = "c1a0de00-0000-4000-8000-000000700521";
    let record = |value: serde_json::Value| format!("{value}\n");
    let transcripts = tempfile::tempdir().unwrap();
    let log = Log::of(channel, provider);
    let (stub, taken, reply): (Option<Arc<dyn LegacyView>>, SourceId, String) = match provider {
        Claude => {
            let prompted = record(json!({"type": "mode", "mode": "normal", "sessionId": session}))
                + &record(json!({"type": "user", "sessionId": session, "uuid": "u1",
                    "message": {"role": "user", "content": "hi"}}));
            let old = prompted.clone()
                + &record(
                    json!({"type": "assistant", "sessionId": session, "uuid": "a-msg_0",
                    "apiBlockIndex": 0, "message": {"id": "msg_0", "role": "assistant",
                    "content": [{"type": "text", "text": "the delivered old answer"}]}}),
                )
                + &record(json!({"type": "system", "subtype": "turn_duration",
                    "durationMs": 5, "sessionId": session}));
            let old_path = transcripts.path().join("claude-old.jsonl");
            std::fs::write(&old_path, &old).unwrap();
            log.bind_now(&source_id_for(session, &old_path).unwrap());
            let delivered = Arc::new(Delivered {
                path: old_path,
                frontier: (old.len() as u64).into(),
            });
            let new_path = transcripts.path().join("claude-new.jsonl");
            std::fs::write(&new_path, &prompted).unwrap();
            let taken = source_id_for(session, &new_path).unwrap();
            (Some(delivered), taken, "the Claude turn's answer".into())
        }
        Codex => {
            let taken = rollout(transcripts.path(), "rollout-taken.jsonl");
            (None, taken, "the Codex turn's answer".into())
        }
    };
    let (passed, mut fences) = tokio::sync::mpsc::unbounded_channel();
    count_fences(channel, passed);
    let (wrote, written) = tokio::sync::oneshot::channel();
    test_hook::set(channel, Step::AfterWrite, move || {
        let _ = wrote.send(());
        Ok(())
    });

    let local = crate::services::cluster::node_registry::SELF_INSTANCE_ID.get();
    let local = local.map_or("takeover-local", String::as_str);
    let prepared = test_host::rest_parts(&shared, &kind, Arc::clone(&rest.http), local);
    let Ok(gateway) = Arc::try_unwrap(prepared.io) else {
        panic!("the gateway host is shared")
    };
    let io = Arc::new(Late {
        host: gateway,
        log: Arc::clone(&log),
        stub,
    });
    prepared.gate.acquired();
    let parts = host::HostParts {
        io: Arc::clone(&io),
        runtime_root: prepared.runtime_root,
        gate: prepared.gate,
        readiness: prepared.readiness,
    };
    let owned = crate::services::tui_o::cutover::boot_ownership();
    let mut handles = host::start_managed(provider, true, owned, || parts);
    assert_eq!(handles.len(), 1);
    if provider == Claude {
        let first = await_within(fences.recv(), WAIT_CAP).await;
        assert!(
            first.is_some(),
            "the old transcript's handoff reached the fence"
        );
    }
    // The mailbox runs this channel's own turn as its source binds.
    let token = Arc::new(CancelToken::new());
    let claim = crate::services::discord::mailbox_try_start_turn;
    let (owner, message) = (UserId::new(1), MessageId::new(own));
    let claimed = claim(&shared, ChannelId::new(channel), token, owner, message);
    assert!(claimed.await, "the turn claims the mailbox");
    log.bind_now(&taken);
    // A fence that begins after another ended saw the takeover of the bound source refused.
    for _ in 0..3 {
        assert!(await_within(fences.recv(), WAIT_CAP).await.is_some());
    }
    let root = crate::config::runtime_root().unwrap();
    let read_init = || OStore::existing(&root).and_then(|s| s.read_init(channel).ok().flatten());
    assert_eq!(candidate(channel).peek(), Adoption::Deferred);
    assert!(
        read_init().is_none(),
        "another turn's open intake holds the takeover"
    );
    assert!(
        rest.posts.lock().unwrap().is_empty(),
        "nothing is posted while held"
    );

    // In one transaction the other turn's input ends and this turn's own row opens.
    let mut swap = pool.begin().await.unwrap();
    sqlx::query("UPDATE intake_outbox SET status='done', completed_at=NOW() WHERE id=$1")
        .bind(other_row)
        .execute(&mut *swap)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id, channel_id,
            user_msg_id, request_owner_id, user_text, turn_kind, agent_id, provider, status,
            claim_owner)
         SELECT target_instance_id, forwarded_by_instance_id, channel_id, $2, request_owner_id,
            user_text, turn_kind, agent_id, provider, 'spawned', 'worker'
           FROM intake_outbox WHERE id=$1",
    )
    .bind(other_row)
    .bind(own.to_string())
    .execute(&mut *swap)
    .await
    .unwrap();
    swap.commit().await.unwrap();
    drop(fences);
    await_within(written, WAIT_CAP).await.unwrap();
    assert_eq!(candidate(channel).peek(), Adoption::Committed);
    let sources = init_sources(&OStore::existing(&root).unwrap(), channel);
    let sources: Vec<_> = sources.into_iter().map(|(source, _)| source).collect();
    assert_eq!(sources, [taken.clone()]);

    let answer = match provider {
        Claude => {
            record(
                json!({"type": "assistant", "sessionId": session, "uuid": "a-msg_1",
                "apiBlockIndex": 0, "message": {"id": "msg_1", "role": "assistant",
                "content": [{"type": "text", "text": reply}]}}),
            ) + &record(json!({"type": "system", "subtype": "turn_duration",
                    "durationMs": 5, "sessionId": session}))
        }
        Codex => {
            record(json!({"type":"response_item","payload":{"type":"message",
                "role":"assistant","id":"m1","content":[{"type":"output_text","text":reply}]}}))
                + &record(json!({"type":"event_msg","payload":{"type":"task_complete",
                    "turn_id":"t1"}}))
        }
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&taken.path)
        .unwrap();
    std::io::Write::write_all(&mut file, answer.as_bytes()).unwrap();
    let mut posted = rest.posted.subscribe();
    let shown = await_within(posted.wait_for(|_| rest.posts_with(&reply) > 0), WAIT_CAP);
    assert!(
        shown.await.is_ok(),
        "O posts the answer once the takeover commits"
    );
    for handle in handles.drain(..) {
        await_within(handle.stop_and_join(), WAIT_CAP)
            .await
            .unwrap();
    }
    assert_eq!(rest.posts_with(&reply), 1, "O posts the answer once");
    rest.server.abort();
    pool.close().await;
    fixture.drop().await;
}
