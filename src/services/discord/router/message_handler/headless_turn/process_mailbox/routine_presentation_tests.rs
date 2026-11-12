use super::*;
use crate::test_support::wait::{WAIT_CAP, await_within};
use serde_json::json;

#[tokio::test]
async fn i8117_actual_bridge_uses_exact_terminal_not_progress_or_preview() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async {
        const USER_MESSAGE: u64 = 9_100_000_000_008_117_001;
        const PLACEHOLDER: u64 = 9_100_000_000_008_117_002;
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let mut hashes = Vec::new();
        let store = crate::services::routines::RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "Asia/Seoul", 256*1024);
        let executor = crate::services::routines::RoutineAgentExecutor::new(Arc::new(pool.clone()), None, 60);
        for (terminal, quiet, legacy) in [(" NO_reply ", true, false), (" NO_reply ", false, true), ("완료: 인용 NO_REPLY 포함", false, false)] {
            let (fixture, _lease) =
                Fixture::with_storage(ProviderKind::Claude, USER_MESSAGE, Some(pool.clone())).await;
            fixture.shared.core.lock().await.sessions.insert(fixture.channel, crate::services::discord::DiscordSession {
                session_id: None, memento_context_loaded:false, memento_reflected:false,
                current_path:None, history:Vec::new(), pending_uploads:Vec::new(), cleared:false,
                channel_id:Some(fixture.channel.get()), channel_name:None, category_name:None,
                last_active:tokio::time::Instant::now(), worktree:None, born_generation:0,
            });
            let routine = store.attach_routine(crate::services::routines::NewRoutine {
                agent_id:None, fallback_agent_id:None, max_retries:Some(0),
                script_ref:format!("fixture-{}.js", fixture.channel), name:"C1 fixture".into(),
                status:None, execution_strategy:"fresh".into(), schedule:None,
                next_due_at:None, checkpoint:None, discord_thread_id:None, timeout_secs:None,
            }).await.unwrap();
            let claimed = store.claim_run_now(&routine.id).await.unwrap().unwrap();
            let turn = format!("discord:{}:{}", fixture.channel, USER_MESSAGE);
            assert!(store.mark_agent_turn_started(&claimed.run_id, &turn,
                Some(json!({"channel_id":fixture.channel.to_string(),"provider":"claude","execution_mode":"process","agent_id":"fixture","fresh_context_guaranteed":true})),
                "fixture", "primary").await.unwrap());
            let mut row = inflight::InflightTurnState::new(
                ProviderKind::Claude,
                fixture.channel.get(),
                None,
                7,
                USER_MESSAGE,
                PLACEHOLDER,
                "routine".into(),
                None,
                None,
                None,
                None,
                0,
            );
            row.turn_nonce = fixture.token.turn_nonce().map(str::to_owned);
            row.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
            let mut encoded = serde_json::to_value(&row).unwrap();
            encoded["routine_presentation"] =
                json!({"policy":"claude_process_c1", "terminal":"missing"});
            if legacy { encoded.as_object_mut().unwrap().remove("routine_presentation"); }
            row = serde_json::from_value(encoded).unwrap();
            inflight::save_inflight_state(&row).unwrap();
            assert!(fixture.shared.ui.placeholder_live_events.set_session_panel_lifecycle_event(
                fixture.channel, Some("fixture-routine-nonce"), "session_fresh",
                &json!({"provider_session_id":"fixture-routine-session","tmux_reused":false}),
            ));
            crate::services::discord::increment_global_active(&fixture.shared, "i8117_bridge");
            let (completion, completed) = tokio::sync::oneshot::channel();
            let shown = Arc::new(RecordingDirectGateway::default());
            let gateway: Arc<dyn TurnGateway> = shown.clone();
            let context = TurnBridgeContext {
                provider: ProviderKind::Claude,
                gateway,
                channel_id: fixture.channel,
                user_msg_id: Some(MessageId::new(USER_MESSAGE)),
                user_text_owned: "quiet routine".into(),
                request_owner_name: "routine fixture".into(),
                role_binding: None,
                adk_session_key: None,
                adk_session_name: None,
                adk_session_info: None,
                adk_cwd: None,
                dispatch_id: None,
                dispatch_kind: None,
                memory_recall_usage: TokenUsage::default(),
                context_window_tokens: 0,
                context_compact_percent: 0,
                current_msg_id: Some(MessageId::new(PLACEHOLDER)),
                response_sent_offset: 0,
                full_response: String::new(),
                tmux_last_offset: None,
                new_session_id: None,
                defer_watcher_resume: false,
                reuse_status_panel_message: false,
                completion_tx: Some(completion),
                is_external_input_tui_direct: false,
                inflight_state: row,
            };
            let (tx, rx) = std::sync::mpsc::channel();
            spawn_turn_bridge(fixture.shared.clone(), fixture.token.clone(), rx, context);
            for text in ["보고 메시지1\n", "진행2\n", "진행3\n", "진행4\n"] {
                tx.send(StreamMessage::Text {
                    content: text.into(),
                })
                .unwrap();
            }
            tx.send(StreamMessage::Done {
                result: terminal.into(),
                session_id: None,
            })
            .unwrap();
            drop(tx);
            let signal = await_within(completed, WAIT_CAP).await.unwrap();
            assert!(matches!(
                signal,
                crate::services::discord::turn_bridge::BridgeCompletionSignal::Finalized
            ));
            let (raw, events): (String, serde_json::Value) = sqlx::query_as("SELECT assistant_message, events_json FROM session_transcripts WHERE channel_id=$1")
                .bind(fixture.channel.to_string()).fetch_one(&pool).await.unwrap();
            assert_eq!(raw, "보고 메시지1\n진행2\n진행3\n진행4");
            let terminals: Vec<_> = events.as_array().unwrap().iter().filter(|event| event["kind"] == "result").collect();
            assert_eq!(terminals.len(), 1, "{events}");
            assert_eq!(terminals[0]["content"], terminal.trim());
            assert_eq!(terminals[0]["status"], "success");
            let outcomes = crate::services::routines::poll_agent_turns(&store, &executor, 10, false).await.unwrap();
            assert_eq!(outcomes.len(), 1);
            assert_eq!(outcomes[0].status, "succeeded");
            let runs = store.list_runs(&routine.id, 10).await.unwrap();
            assert_eq!(runs[0].status, "succeeded");
            assert!(runs[0].discord_log_sections["outcome"].is_string());
            use sha2::Digest;
            hashes.push(format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(&(&raw, &events)).unwrap())));
            let wire = shown.0.lock().unwrap();
            if quiet {
                assert!(wire.is_empty(), "quiet C1 leaked wire body: {wire:?}");
            } else {
                let bodies: Vec<_> = wire
                    .iter()
                    .filter(|(op, content)| matches!(*op, "replace" | "send") && content.contains("보고 메시지1"))
                    .collect();
                assert_eq!(bodies.len(), 1, "{wire:?}");
                if legacy {
                    assert!(bodies[0].1.contains("🆕 새 세션 시작"), "legacy banner contract");
                    assert!(bodies[0].1.ends_with("보고 메시지1\n진행2\n진행3\n진행4"));
                } else {
                    assert_eq!(bodies[0].1, "보고 메시지1\n진행2\n진행3\n진행4");
                }
            }
        }
        assert_eq!(hashes[0], hashes[1], "wire suppression preserves the raw transcript bytes");
        println!("RAW_TRANSCRIPT_PAYLOAD_SHA256 {}", hashes[0]);
        pool.close().await;
        db.drop().await;
    })
    .await;
}
