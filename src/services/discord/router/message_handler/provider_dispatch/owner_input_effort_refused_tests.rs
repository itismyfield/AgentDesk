use super::*;
use crate::services::discord::queue_io::input_owner::{admission, notice, recovery, start};
use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, register_tmux_runtime_binding};
use std::io::Write;

const CONTINUATION: &str =
    "effort 변경 처리를 위해 세션을 다시 시작했으니 중단된 작업을 이어서 진행하라";

fn assert_refused(snapshot: &Value) {
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["items"], json!([]), "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    assert_eq!(snapshot["refused_inputs"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["effort_restart"]["cancelled"], true);
    assert!(snapshot["effort_restart"]["continuation"].is_null());
    assert!(!snapshot.to_string().contains(CONTINUATION), "{snapshot}");
}

#[test]
fn a_refused_effort_continuation_loses_every_body_copy_across_restart_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    fs::create_dir_all(generation.parent().unwrap()).unwrap();
    fs::write(generation, "5").unwrap();
    let channel = 8_217_202;
    let mut rig = rig("effort-refused", channel, "normal", view("", false), IDLE);
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-dispatch")
        .join(format!("{channel}.json"));
    let mut context = json!({"origin":"headless","source":"agentdesk_effort_continue",
        "metadata":{"effort_restart":{"operation":"operation-refused",
        "native_session":"session-refused","interrupted_episode":"episode-refused",
        "desired_revision":1,"ready":true}}});
    let request = serde_json::from_value(context["metadata"]["effort_restart"].clone()).unwrap();
    let generation = crate::services::discord::runtime_store::process_generation();
    let (original, ticket) = rig.runtime.block_on(async {
        rig.mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                owner.clear_waiting()?;
                owner
                    .prepare_effort(Some(request), channel, generation)
                    .map(drop)
            })
            .await
            .unwrap();
        let (original, ticket) = start::original(
            &rig.mailbox,
            ("effort-refused", channel, CONTINUATION),
            &mut context,
        )
        .await
        .unwrap();
        let admitted =
            admission::admit_with_context(&rig.mailbox, &original, ticket.clone(), context.clone())
                .await;
        assert!(
            matches!(admitted, admission::Admission::Accepted(_)),
            "{admitted:?}"
        );
        (original, ticket)
    });
    rig.original = original.clone();
    rig.ticket = ticket.clone();
    assert!(rig.snapshot().to_string().contains(CONTINUATION));
    assert!(fs::read_to_string(&path).unwrap().contains(CONTINUATION));
    let front = rig.granted_front();
    let (reached, idle) = std::sync::mpsc::sync_channel(1);
    let reached = std::sync::Mutex::new(reached);
    let handoff: Handoff = Arc::new(move |front| {
        reached.lock().unwrap().send(front).unwrap();
        Box::pin(async { Ok(()) })
    });
    let drive = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, channel),
        handoff,
        None,
    );
    assert_eq!(rig.runtime.block_on((drive.handoff)(front)), Ok(()));
    let front = idle.recv_timeout(Duration::from_secs(5)).unwrap();
    register_tmux_runtime_binding(
        &rig.name,
        TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: rig.transcript().display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("transcript".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    assert!(matches!(
        native_wait(
            &ProviderKind::Claude,
            NativeTarget {
                tmux: rig.name.clone(),
                dir: None,
                session_id: None,
                own_pane: true,
                host_evidence_only: false,
            }
        ),
        Wait::Go
    ));
    let transcript = rig.transcript();
    let mut owner = rig.registration(
        &ProviderKind::Claude,
        target::Phase::Idle,
        &transcript,
        Execution::LocalTui,
    );
    owner.origin = InputOrigin::EffortContinue;
    crate::services::claude_tui::readiness::test_support::daemon_restart(&rig.name);
    let moved = crate::services::claude_tui::readiness::test_support::after_first_look(
        &rig.name,
        move || {
            writeln!(
                fs::OpenOptions::new()
                    .append(true)
                    .open(transcript)
                    .unwrap(),
                "{}",
                json!({"type":"ai-title"})
            )
            .unwrap();
        },
    );
    let claim = rig.claim(&front).unwrap();
    let result = rig.with_turn(&ProviderKind::Claude, Some(&rig.name), |turn, sender| {
        let (settled, _) = tokio::sync::oneshot::channel();
        execute_front(
            StreamingTurn {
                prompt: CONTINUATION,
                ..turn
            },
            sender,
            claim,
            owner,
            settled,
        )
    });
    crate::test_support::wait::join_within(moved, crate::test_support::wait::WAIT_CAP);
    assert!(result.is_err_and(|error| error.contains("unproven")));
    assert!(rig.events().is_empty(), "no key reaches the pane");
    let snapshot = rig.snapshot();
    assert_refused(&snapshot);
    assert_eq!(
        snapshot["refused_inputs"][0]["notice"],
        json!({"kind":"readiness_unproven","enqueued":false})
    );
    assert_refused(&serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap());

    let _entered = rig.runtime.enter();
    let registry = ChannelMailboxRegistry::default();
    let restored = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let history = crate::services::tui_input::human_frame::history::v2::Registry::at(
        rig.f.root.path().join("history.json"),
    );
    rig.runtime.block_on(async {
        assert_eq!(
            recovery::recover(
                &restored,
                (path.clone(), "input-owner".into()),
                history,
                None,
                ("claude", channel, 20_000)
            )
            .await,
            Ok(0)
        );
        let mut late = context["metadata"]["effort_restart"].clone();
        late["ready"] = json!(true);
        late["desired_revision"] = json!(2);
        let late = serde_json::from_value(late).unwrap();
        assert!(
            restored
                .v2_request(move |slot| slot.as_mut().ok_or("no owner")?.prepare_effort(
                    Some(late),
                    channel,
                    generation
                ))
                .await
                .is_err()
        );
        assert!(
            start::original(
                &restored,
                ("effort-refused", channel, CONTINUATION),
                &mut context
            )
            .await
            .is_err()
        );
        assert!(matches!(
            admission::admit_with_context(&restored, &original, ticket, context).await,
            admission::Admission::Rejected(_)
        ));
        let snapshot = restored
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert_refused(&snapshot);
        assert_refused(&serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap());
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        assert_eq!(
            notice::drain(&pool, &restored, "claude", channel).await,
            Ok(1)
        );
        assert_eq!(
            notice::drain(&pool, &restored, "claude", channel).await,
            Ok(0)
        );
        let count: (i64,) = sqlx::query_as("SELECT count(*) FROM message_outbox WHERE target = $1")
            .bind(format!("channel:{channel}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1, "one refusal owes one actual notice");
    });
    // A fresh readiness fixture supplies the next person's evidence independently of the refusal.
    let transcript = rig.transcript();
    fs::write(&transcript, IDLE).unwrap();
    crate::services::claude_tui::readiness::test_support::daemon_restart(&rig.name);
    crate::services::claude_tui::readiness::test_support::pre_source(&rig.name, &transcript);
    assert!(
        crate::services::claude_tui::readiness::observe(&rig.name, Some(&transcript)).is_some()
    );
    rig.mailbox = restored;
    rig.original = serde_json::from_value(json!({"author_id":7,
        "message_id":1_490_000_000_008_217_202u64,"channel_id":channel,
        "source_message_ids":[1_490_000_000_008_217_202u64],
        "queued_generation":1,"text":PROMPT}))
    .unwrap();
    rig.ticket = AdmissionTicket::new();
    assert!(matches!(
        rig.runtime.block_on(admission::admit(
            &rig.mailbox,
            &rig.original,
            rig.ticket.clone()
        )),
        admission::Admission::Accepted(_)
    ));
    assert_eq!(
        rig.handoff(
            rig.granted_front(),
            ProviderKind::Claude,
            target::Phase::Idle,
            &rig.transcript()
        ),
        Ok(())
    );
    assert_eq!(rig.events(), ["paste", "Enter"]);
    rig.delivered();
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

#[test]
fn an_effort_continuation_waits_on_the_channel_execution_like_a_person() {
    use super::super::start_wait::keyed_to_channel;
    assert_eq!(keyed_to_channel(InputOrigin::EffortContinue), Some(true));
    assert_eq!(keyed_to_channel(InputOrigin::HttpStart), Some(true));
    assert_eq!(keyed_to_channel(InputOrigin::Routine), None);
}
