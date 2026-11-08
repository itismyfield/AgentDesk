use super::*;
use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};
use crate::services::discord::queue_io::input_owner::{admission, routine, start};
use crate::services::turn_orchestrator::QueuePersistenceContext;
use poise::serenity_prelude::{MessageId, UserId};

async fn busy_rig(channel: u64, pool: sqlx::PgPool) -> IntakeRig {
    let rig = IntakeRig::with_storage(channel, true, ProviderKind::Claude, Some(pool)).await;
    let generation = discord::runtime_store::generation_path().unwrap();
    fs::create_dir_all(generation.parent().unwrap()).unwrap();
    fs::write(generation, "1").unwrap();
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Claude, &rig.shared.token_hash, None);
    rig.mailbox.clear(persistence).await;
    let native = rig.renderer.join("transcript");
    fs::write(&native, BUSY).unwrap();
    fs::remove_file(&rig.transcript).unwrap();
    fs::hard_link(&native, &rig.transcript).unwrap();
    let session = rig.shared.core.lock().await.sessions[&rig.channel]
        .session_id
        .clone();
    let name = ProviderKind::Claude.build_tmux_session_name(&format!("a1-rollback-{channel}"));
    let token = Arc::new(CancelToken::new());
    rig.mailbox
        .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(42))
        .await;
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        42,
        43,
        "이미 진행 중인 턴".into(),
        session,
        Some(name),
        Some(rig.transcript.display().to_string()),
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_string);
    save_inflight_state(&row).unwrap();
    rig
}

async fn start_input(rig: &IntakeRig, source: &str, origin: &str) {
    let turn = format!("discord:{}:9100095826424066539", rig.channel);
    let (original, ticket) =
        routine::synthetic_original(&turn, rig.channel.get(), PROMPT, 1).unwrap();
    let context = start::HeadlessProvenance {
        owner: (7, source),
        source: Some(source),
        metadata: Some(
            json!({"human_input":{"origin_id":origin,"source":source,"reply_contract":"v1"}}),
        ),
        channel_hint: None,
        label: None,
        dm: None,
    }
    .json(&turn);
    assert!(matches!(
        admission::admit_with_context(&rig.mailbox, &original, ticket, context).await,
        Admission::Accepted(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_imessage_joins_the_running_turn_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::person_origin::",
        "busy_imessage_joins_the_running_turn_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = busy_rig(7_896_411, pool.clone()).await;
    start_input(&rig, "imessage", "busy-person-origin").await;
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        next_settle(&mut settled, &rig.trace).await,
        Ok(()),
        "{}",
        rig.trace.text()
    );
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    let body = fs::read_to_string(rig.renderer.join("body")).unwrap();
    assert_eq!(body.matches("[iMessage reply contract v1]").count(), 1);
    assert!(body.contains("origin: busy-person-origin"));
    assert_eq!(rig.mailbox.has_active_turn().await, Ok(true));
    let state = rig.state().await;
    assert!(state["items"].as_array().unwrap().is_empty(), "{state}");
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_machine_front_preserves_people_order_while_the_turn_runs_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::person_origin::",
        "held_machine_front_preserves_people_order_while_the_turn_runs_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = busy_rig(7_896_412, pool.clone()).await;
    start_input(&rig, "http", "machine-origin").await;
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
    assert_eq!(
        next_settle(&mut settled, &rig.trace).await,
        Err("NotSent(\"synthetic_waits_idle\")".into())
    );
    for (message, text) in [(78964121, "첫째 사람 입력"), (78964122, "둘째 사람 입력")]
    {
        let original = serde_json::from_value(json!({"author_id":7,"message_id":message,
            "channel_id":rig.channel.get(),"queued_generation":1,"text":text}))
        .unwrap();
        assert!(matches!(
            admission::admit(&rig.mailbox, &original, AdmissionTicket::new()).await,
            Admission::Accepted(_)
        ));
    }
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        next_settle(&mut settled, &rig.trace).await,
        Err("NotSent(\"synthetic_waits_idle\")".into())
    );
    let state = rig.state().await;
    assert_eq!(state["items"].as_array().unwrap().len(), 1, "{state}");
    assert_eq!(state["items"][0]["text"], PROMPT);
    assert!(state["head_injection"].is_null(), "{state}");
    assert_eq!(state["retained_inputs"], json!([]));
    let keys = events(&rig.renderer);
    assert!(
        keys.iter()
            .all(|key| matches!(key.as_str(), "paste" | "Enter" | "C-s"))
    );
    let sent: Vec<_> = keys
        .iter()
        .filter(|key| key.as_str() != "C-s")
        .map(String::as_str)
        .collect();
    assert_eq!(sent, ["paste", "Enter", "paste", "Enter"]);
    let transcript = fs::read_to_string(&rig.transcript)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        transcript.find("첫째 사람 입력").unwrap() < transcript.find("둘째 사람 입력").unwrap(),
        "{transcript}"
    );
    assert_eq!(rig.mailbox.has_active_turn().await, Ok(true));
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imessage_owner_retry_reuses_one_ticket_and_stored_generation_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::person_origin::",
        "imessage_owner_retry_reuses_one_ticket_and_stored_generation_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = busy_rig(7_896_413, pool.clone()).await;
    rig.shared
        .restart
        .reconcile_done
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let baseline = rig.state().await["admission_receipts"]
        .as_array()
        .unwrap()
        .len();
    let mut first = None;
    for (message, generation) in [
        (9_100_000_000_000_001_111_u64, 1),
        (9_100_000_000_000_002_222, 2),
    ] {
        fs::write(
            discord::runtime_store::generation_path().unwrap(),
            generation.to_string(),
        )
        .unwrap();
        let turn = format!("discord:{}:{message}", rig.channel);
        let context = start::HeadlessProvenance {
            owner: (7, "imessage:7"),
            source: Some("imessage"),
            metadata: Some(
                json!({"human_input":{"origin_id":"retry-person-guid", "source":"imessage"}}),
            ),
            channel_hint: None,
            label: None,
            dm: None,
        }
        .json(&turn);
        assert_eq!(
            super::super::super::admit_start(&rig.shared, rig.channel, (&turn, PROMPT), context)
                .await,
            Some(crate::services::discord::queue_io::input_ingress::Intake::Accepted)
        );
        let state = rig.state().await;
        assert_eq!(
            state["admission_receipts"].as_array().unwrap().len(),
            baseline + 1,
            "{state}"
        );
        assert_eq!(state["items"].as_array().unwrap().len(), 1, "{state}");
        assert_eq!(state["items"][0]["queued_generation"], 1);
        if let Some(original) = &first {
            assert_eq!(&state["items"][0], original);
        } else {
            first = Some(state["items"][0].clone());
        }
    }
    assert!(events(&rig.renderer).is_empty());
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imessage_joins_an_external_busy_row_without_claiming_a_new_turn_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::person_origin::",
        "imessage_joins_an_external_busy_row_without_claiming_a_new_turn_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = busy_rig(7_896_414, pool.clone()).await;
    let mut row = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        rig.channel.get(),
    )
    .unwrap();
    row.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    save_inflight_state(&row).unwrap();
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Claude, &rig.shared.token_hash, None);
    rig.mailbox.finish_turn(persistence).await;
    assert_eq!(rig.mailbox.has_active_turn().await, Ok(false));
    start_input(&rig, "imessage", "external-person-origin").await;
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        next_settle(&mut settled, &rig.trace).await,
        Ok(()),
        "{}",
        rig.trace.text()
    );
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    assert_eq!(rig.mailbox.has_active_turn().await, Ok(false));
    assert_eq!(rig.state().await["items"], json!([]));
    let current = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        rig.channel.get(),
    )
    .unwrap();
    assert_eq!(current.turn_source, row.turn_source);
    assert_eq!(current.user_msg_id, row.user_msg_id);
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imessage_native_end_before_busy_paste_keeps_one_delivery_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::person_origin::",
        "imessage_native_end_before_busy_paste_keeps_one_delivery_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = busy_rig(7_896_415, pool.clone()).await;
    start_input(&rig, "imessage", "boundary-person-origin").await;
    fs::write(&rig.transcript, IDLE).unwrap();
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        next_settle(&mut settled, &rig.trace).await,
        Ok(()),
        "{}",
        rig.trace.text()
    );
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    assert_eq!(rig.state().await["items"], json!([]));
    drop(rig);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imessage_joins_a_native_busy_pane_without_a_mailbox_turn_pg() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::person_origin::",
        "imessage_joins_a_native_busy_pane_without_a_mailbox_turn_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig =
        IntakeRig::with_storage(7_896_417, true, ProviderKind::Claude, Some(pool.clone())).await;
    let generation = discord::runtime_store::generation_path().unwrap();
    fs::create_dir_all(generation.parent().unwrap()).unwrap();
    fs::write(generation, "1").unwrap();
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Claude, &rig.shared.token_hash, None);
    rig.mailbox.clear(persistence).await;
    let native = rig.renderer.join("transcript");
    fs::write(&native, BUSY).unwrap();
    fs::remove_file(&rig.transcript).unwrap();
    fs::hard_link(&native, &rig.transcript).unwrap();
    let key = discord::adk_session::build_adk_session_key(
        &rig.shared,
        rig.channel,
        &ProviderKind::Claude,
        None,
    )
    .await
    .unwrap();
    discord::inflight::seed_session_row_keyed(&pool, &key, rig.channel.get(), None).await;
    assert_eq!(rig.mailbox.has_active_turn().await, Ok(false));
    assert!(
        discord::inflight::load_inflight_state_read_only(&ProviderKind::Claude, rig.channel.get())
            .is_none()
    );
    start_input(&rig, "imessage", "native-person-origin").await;
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((rig.channel, completed));
    let (drive, mut settled) = rig.drive();
    crate::services::discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        next_settle(&mut settled, &rig.trace).await,
        Ok(()),
        "{}",
        rig.trace.text()
    );
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    let state = rig.state().await;
    assert_eq!(state["items"], json!([]), "{state}");
    let kept = state["admission_binds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|bind| {
            bind.pointer("/context/execution/metadata/human_input/origin_id")
                == Some(&json!("native-person-origin"))
        })
        .unwrap();
    assert_eq!(
        kept["context"]["original"]["message_id"],
        9_100_095_826_424_066_539_u64
    );
    let body = fs::read_to_string(rig.renderer.join("body")).unwrap();
    assert_eq!(body.matches("[iMessage reply contract v1]").count(), 1);
    assert!(body.contains("origin: native-person-origin"));
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&rig.transcript)
        .unwrap();
    for row in [
        json!({"type":"assistant","uuid":"native-person-answer","message":{"id":"native-person-answer","role":"assistant","content":[{"type":"text","text":"native turn answered"}],"stop_reason":"end_turn"}}),
        json!({"type":"system","subtype":"turn_duration"}),
    ] {
        std::io::Write::write_all(&mut file, format!("{row}\n").as_bytes()).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(60), bridge)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events(&rig.renderer), ["paste", "Enter"]);
    drop(rig);
    pool.close().await;
    db.drop().await;
}
