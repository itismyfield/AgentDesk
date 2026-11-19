use super::*;
use crate::services::claude_tui::readiness;
use crate::services::discord::queue_io::input_owner::drive;

#[test]
fn tmux_metadata_only_readiness_hold_refuses_once_without_another_wake_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    let mut config = crate::config::Config::default();
    config.runtime.followup_prompt_ready_timeout_secs = Some(1);
    crate::config_live_reload::install(config);
    let tail = concat!(
        "{\"type\":\"last-prompt\"}\n",
        "{\"type\":\"ai-title\"}\n",
        "{\"type\":\"mode\"}\n",
        "{\"type\":\"permission-mode\"}\n",
        "{\"type\":\"session-owner\"}\n",
        "{\"type\":\"atis-latch\"}\n",
    );
    let rig = rig(
        "oi-tmux-metadata-held",
        7_272_714,
        "normal",
        view("", false),
        tail,
    );
    let native = uuid::Uuid::new_v4().to_string();
    rehydrate(&rig, &native);
    let transcript = rig.transcript();
    assert_eq!(
        crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&transcript),
        crate::services::tui_turn_state::TuiTurnState::Unknown
    );
    assert!(readiness::observe(&rig.name, Some(&transcript)).is_none());
    assert!(
        crate::services::claude_tui::host_input::observe_draft(&rig.name)
            .as_deref()
            .is_some_and(readiness::empty_draft)
    );
    let elsewhere = tempfile::tempdir().unwrap();
    let (db, pool) = rig.runtime.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        (db, pool)
    });
    let (fronts, inbox) = std::sync::mpsc::channel::<(Front, Settled)>();
    let idle: Handoff = Arc::new(move |front| {
        let (settled, answer) = tokio::sync::oneshot::channel();
        let sent = fronts.send((front, settled));
        Box::pin(async move {
            sent.map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let mut pump = owner_drive(
        (rig.mailbox.clone(), Some(pool.clone())),
        (ProviderKind::Claude, rig.channel),
        idle,
        None,
    );
    let driver = Arc::get_mut(&mut pump).unwrap();
    let drain = driver
        .settled
        .take()
        .expect("owner attaches its actual notice drain");
    let (passed, passes) = std::sync::mpsc::channel();
    driver.settled = Some(Arc::new(move || {
        let pending = drain();
        let passed = passed.clone();
        Box::pin(async move {
            pending.await;
            let _ = passed.send(());
        })
    }));

    let mut first_due = None;
    for attempt in 0..2 {
        if attempt == 1 {
            // The original was already held when volatile readiness was lost.
            readiness::tests::forget_session(&rig.name);
        }
        {
            let _entered = rig.runtime.enter();
            drive::wake(pump.clone());
        }
        let front = inbox
            .recv_timeout(WAIT_CAP)
            .expect("the real drive handed over the front");
        assert_eq!(front.0.1.message_id, rig.original.message_id);
        assert_eq!(front.0.1.queued_generation, rig.original.queued_generation);
        let (result, _) = idle_turn(&rig, (&native, elsewhere.path()), fresh(), front, |_| None);
        let error = result.expect_err("metadata and an empty composer never authorize a key");
        assert!(
            error.starts_with("NotSent(")
                && error.contains("idle_evidence_pending")
                && error.contains("BeforePayload")
                && error.contains("readiness=follow-up"),
            "{error}"
        );
        passes.recv_timeout(WAIT_CAP).unwrap();
        let snap = rig.snapshot();
        assert_eq!(snap["items"].as_array().map(Vec::len), Some(1), "{snap}");
        assert!(snap["head_injection"].is_null(), "{snap}");
        let due = snap["readiness_holds"][0]["due_at_ms"].as_u64();
        if attempt == 0 {
            first_due = due;
        } else {
            assert_eq!(due, first_due, "the same original keeps its first due");
        }
    }
    let observed = passes.recv_timeout(Duration::from_secs(15));
    let final_state = rig.snapshot();
    let notices: i64 = rig.runtime.block_on(sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox WHERE reason_code = 'refused.readiness_unproven'",
    ).fetch_one(&pool)).unwrap();
    let events = rig.events();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.as_str(), "load" | "paste" | "Enter" | "C-s")),
        "the unproven pane got no key: {events:?}"
    );
    assert!(
        observed.is_ok()
            && final_state["items"].as_array().is_none_or(Vec::is_empty)
            && notices == 1,
        "typed tmux hold did not settle: notice_count={notices}, state={final_state}"
    );
    assert_eq!(
        final_state["refused_fronts"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(final_state["refused_fronts"][0]["notice"]["enqueued"], true);
    assert!(
        !final_state.to_string().contains(PROMPT),
        "refusal persisted only body-free evidence"
    );
    assert!(
        inbox.try_recv().is_err(),
        "expiry did not grant another write"
    );
    rig.runtime.block_on(async {
        pool.close().await;
        db.drop().await;
    });
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}
