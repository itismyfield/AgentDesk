use super::*;

fn budget(seconds: u64) {
    let mut config = crate::config::Config::default();
    config.runtime.followup_prompt_ready_timeout_secs = Some(seconds);
    crate::config_live_reload::install(config);
}

fn wall_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

fn no_writer_keys(rig: &Rig) {
    let events = rig.events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.as_str(), "load" | "paste" | "Enter" | "C-s")),
        "{events:?}"
    );
}

fn notices(rig: &Rig, pool: &sqlx::PgPool) -> i64 {
    rig.runtime
        .block_on(sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_outbox WHERE reason_code = 'refused.readiness_unproven'",
        ).fetch_one(pool))
        .unwrap()
}

#[test]
fn readiness_review_retry_inherits_the_exact_first_due_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    budget(1);
    let rig = rig(
        "oi-review-due",
        7_272_740,
        "normal",
        view("", false),
        METADATA,
    );
    let native = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    rehydrate(&rig, &native);
    let (db, pool) = rig.runtime.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        (db, pool)
    });
    let (pump, inbox, passes) = notice_drive(&rig, pool.clone());
    let run = |front| idle_turn(&rig, (&native, elsewhere.path()), fresh(), front, |_| None).0;
    rig.wake(&pump);
    let first = run(inbox.recv_timeout(WAIT_CAP).unwrap());
    assert!(first.is_err_and(|e| e.contains("idle_evidence_pending")));
    passes.recv_timeout(WAIT_CAP).unwrap();
    let snapshot = rig.snapshot();
    let holds = snapshot["readiness_holds"].as_array().unwrap();
    assert_eq!(holds.len(), 1);
    let start = holds[0]["started_at_ms"].as_u64().unwrap();
    let due = holds[0]["due_at_ms"].as_u64().unwrap();
    assert_eq!(
        due.checked_sub(start),
        Some(10_000),
        "the first durable due"
    );

    budget(45);
    let delay = Duration::from_millis(due.saturating_sub(wall_ms() + 1_800));
    rig.runtime
        .block_on(async { tokio::time::sleep(delay).await });
    rig.wake(&pump);
    let front = inbox.recv_timeout(WAIT_CAP).unwrap();
    assert_eq!(front.0.1.message_id, rig.original.message_id);
    assert!(
        (wall_ms()..=wall_ms() + 2_500).contains(&due),
        "reserve just before the first due"
    );
    let result = run(front);
    let ended = wall_ms();
    assert!(result.is_err_and(|e| e.starts_with("NotSent(")));
    assert!(
        (due..=due + 4_000).contains(&ended),
        "warm retry exceeded the inherited deadline: due={due}, ended={ended}"
    );
    passes.recv_timeout(WAIT_CAP).unwrap();
    no_writer_keys(&rig);
    let snapshot = rig.snapshot();
    assert_eq!(snapshot["items"], json!([]), "{snapshot}");
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(
        snapshot["refused_inputs"].as_array().map(Vec::len),
        Some(1),
        "{snapshot}"
    );
    assert_eq!(snapshot["refused_inputs"][0]["notice"]["enqueued"], true);
    assert_eq!(notices(&rig, &pool), 1);
    rig.runtime.block_on(async {
        pool.close().await;
        db.drop().await;
    });
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

#[test]
fn readiness_review_modal_preserves_the_original_pg() {
    let modal = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n";
    screen_hold("oi-review-modal", 7_272_741, modal.into());
}

#[test]
fn readiness_review_manual_draft_preserves_the_original_pg() {
    screen_hold(
        "oi-review-draft",
        7_272_742,
        view("person's unsent draft", false),
    );
}

fn screen_hold(name: &str, channel: u64, screen: String) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    budget(1);
    let rig = rig(name, channel, "normal", screen, METADATA);
    let native = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    rehydrate(&rig, &native);
    let (db, pool) = rig.runtime.block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        (db, pool)
    });
    let (pump, inbox, passes) = notice_drive(&rig, pool.clone());
    rig.wake(&pump);
    let first = idle_turn(
        &rig,
        (&native, elsewhere.path()),
        fresh(),
        inbox.recv_timeout(WAIT_CAP).unwrap(),
        |_| None,
    )
    .0;
    assert!(first.is_err_and(|e| e.starts_with("NotSent(")));
    passes.recv_timeout(WAIT_CAP).unwrap();
    let elapsed_pass = passes.recv_timeout(Duration::from_millis(10_500));
    let held = rig.snapshot();
    no_writer_keys(&rig);
    assert!(
        elapsed_pass.is_err()
            && held["items"][0]["message_id"] == rig.original.message_id
            && held["readiness_holds"].as_array().is_none_or(Vec::is_empty)
            && notices(&rig, &pool) == 0,
        "screen hold was converted to a deadline: {held}"
    );
    let pane = format!("={}:", rig.name);
    rig.f.tmux(&["send-keys", "-t", &pane, "C-u"]);
    wait("person releases composer", || {
        events(&rig.renderer) == ["clear"]
    });
    rig.f.tmux(&["clear-history", "-t", &pane]);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(rig.transcript())
        .unwrap();
    writeln!(file, "{}", IDLE.trim_end()).unwrap();
    rehydrate(&rig, &native);
    let mut answering = answers(&rig);
    let next = inbox
        .recv_timeout(WAIT_CAP)
        .expect("the existing watch wakes on proof and empty composer");
    assert_eq!(next.0.1.message_id, rig.original.message_id);
    let result = idle_turn(
        &rig,
        (&native, elsewhere.path()),
        answering.token(),
        next,
        |_| None,
    )
    .0;
    answering.finish();
    assert_eq!(result, Ok(()));
    passes.recv_timeout(WAIT_CAP).unwrap();
    assert_eq!(rig.events(), ["clear", "paste", "Enter"]);
    assert_eq!(rig.snapshot()["items"], json!([]));
    assert_eq!(notices(&rig, &pool), 0);
    rig.runtime.block_on(async {
        pool.close().await;
        db.drop().await;
    });
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}
