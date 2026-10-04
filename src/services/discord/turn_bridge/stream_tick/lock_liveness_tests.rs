use super::*;
use crate::services::discord::inflight::{
    InflightTurnIdentity, inflight_state_path, load_inflight_state, lock_inflight_state_path,
    save_inflight_state,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn contended_stream_tick_keeps_scheduler_timer_and_loopback_live() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    for mode in 0..3 {
        let channel = ChannelId::new(6_603_100 + mode);
        let state = InflightTurnState::new(
            ProviderKind::Codex,
            channel.get(),
            None,
            1,
            77_010,
            0,
            "잠금 경합 시험".to_owned(),
            Some("session".to_owned()),
            Some("lock-liveness-fixture".to_owned()),
            None,
            None,
            512,
        );
        save_inflight_state(&state).expect("seed row");
        let root = crate::services::discord::inflight::inflight_runtime_root().unwrap();
        let path = inflight_state_path(&root, &ProviderKind::Codex, channel.get());
        let before = std::fs::read(&path).unwrap();
        let lock = lock_inflight_state_path(&path).expect("independent sidecar handle");
        let (progress_tx, progress_rx) = std::sync::mpsc::channel();
        // The OS deadline releases even a blocking mutant, so a stalled runtime cannot hang the test.
        let deadline = std::thread::spawn(move || {
            let live = progress_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            drop(lock);
            live
        });
        let expected = InflightTurnIdentity::from_state(&state);
        let (outcome, pending, gateway) = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let save = tokio::spawn(async move {
                let gateway =
                    super::super::provider_output_guard_tests::CapturingGateway::default();
                let mut baseline = state.clone();
                let mut state = state;
                state.full_response = "미저장 본문".to_owned();
                state.current_msg_id = 2;
                let mut expected_message = (0, 0);
                let mut current = MessageId::new(2);
                let mut pending = Some(current);
                let mut created = Some(current);
                entered_tx.send(()).unwrap();
                let context = StreamTickCandidateSaveContext {
                    gateway: &gateway,
                    provider: &ProviderKind::Codex,
                    token_hash: "lock-liveness",
                    channel_id: channel,
                    persisted_baseline: &mut baseline,
                    inflight_state: &mut state,
                    expected_identity: &expected,
                    expected_current_message: &mut expected_message,
                    current_msg_id: &mut current,
                    pending_current_message_candidate: &mut pending,
                    bridge_created_response_placeholder_msg_id: &mut created,
                };
                let outcome = match mode {
                    0 => {
                        fence_stream_tick_visible_mutation_with_candidate_cleanup(
                            context,
                            "lock-liveness",
                        )
                        .await
                    }
                    1 => {
                        persist_stream_tick_state_with_candidate_cleanup(context, "lock-liveness")
                            .await
                    }
                    _ => persist_stream_tick_heartbeat(&ProviderKind::Codex, channel, &expected),
                };
                (outcome, pending, gateway)
            });
            entered_rx.await.unwrap();
            let heartbeat = async { tokio::time::sleep(Duration::from_millis(20)).await };
            let server = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                stream.write_all(&byte).await.unwrap();
            };
            let client = async {
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                stream.write_all(b"x").await.unwrap();
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                assert_eq!(byte, *b"x");
            };
            tokio::join!(heartbeat, server, client);
            let _ = progress_tx.send(());
            save.await.unwrap()
        });
        assert!(
            deadline.join().unwrap(),
            "timer and loopback exceeded OS deadline while sidecar was held (mode {mode})"
        );
        assert_eq!(outcome, GuardedSaveOutcome::IoError);
        assert!(
            dirty_after_guarded_save(outcome),
            "unsaved tick stays dirty"
        );
        let durable = load_inflight_state(&ProviderKind::Codex, channel.get()).unwrap();
        assert_eq!(
            visible_mutation_authority_after_guarded_save(
                outcome,
                &durable,
                crate::services::discord::inflight::StreamRelayAuthority::from_state(&durable),
            )
            .mutation_permission(),
            Some(false),
            "contention cannot authorize a visible mutation"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "contention must not mutate the row"
        );
        assert_eq!(
            pending,
            Some(MessageId::new(2)),
            "candidate survives a retryable tick"
        );
        assert!(gateway.deletes.lock().unwrap().is_empty());
        let mut state = load_inflight_state(&ProviderKind::Codex, channel.get()).unwrap();
        let expected = InflightTurnIdentity::from_state(&state);
        let mut baseline = state.clone();
        state.full_response = "잠금 해제 후 저장".to_owned();
        let mut epoch = (0, 0);
        let mut current = detached_current_msg_id_from_durable(0);
        assert_eq!(
            persist_stream_tick_visible_mutation_fence(
                &mut baseline,
                &mut state,
                &expected,
                &mut epoch,
                &mut current,
                channel,
                "lock-liveness-retry"
            ),
            GuardedSaveOutcome::Saved
        );
        assert_eq!(
            load_inflight_state(&ProviderKind::Codex, channel.get())
                .unwrap()
                .full_response,
            "잠금 해제 후 저장"
        );
    }
}

#[test]
fn one_shot_candidate_bind_waits_out_brief_sidecar_contention() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    for exit_settle in [false, true] {
        let channel = ChannelId::new(6_603_200 + u64::from(exit_settle));
        let state = InflightTurnState::new(
            ProviderKind::Codex,
            channel.get(),
            None,
            1,
            77_010,
            0,
            "일회성 bind 경합 시험".to_owned(),
            Some("session".to_owned()),
            Some("lock-liveness-fixture".to_owned()),
            None,
            None,
            512,
        );
        save_inflight_state(&state).expect("seed row");
        let root = crate::services::discord::inflight::inflight_runtime_root().unwrap();
        let path = inflight_state_path(&root, &ProviderKind::Codex, channel.get());
        let lock = lock_inflight_state_path(&path).expect("independent sidecar handle");
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(lock);
        });
        let expected = InflightTurnIdentity::from_state(&state);
        let gateway = super::super::provider_output_guard_tests::CapturingGateway::default();
        let mut baseline = state.clone();
        let mut state = state;
        state.current_msg_id = 2;
        state.current_msg_len = 10;
        let mut expected_message = (0, 0);
        let mut current = MessageId::new(2);
        let mut pending = Some(current);
        let mut created = Some(current);
        let mut context = StreamTickCandidateSaveContext {
            gateway: &gateway,
            provider: &ProviderKind::Codex,
            token_hash: "lock-liveness",
            channel_id: channel,
            persisted_baseline: &mut baseline,
            inflight_state: &mut state,
            expected_identity: &expected,
            expected_current_message: &mut expected_message,
            current_msg_id: &mut current,
            pending_current_message_candidate: &mut pending,
            bridge_created_response_placeholder_msg_id: &mut created,
        };
        let bound = runtime.block_on(async {
            if exit_settle {
                settle_pending_current_message_candidate_on_loop_exit(context).await
            } else {
                bind_pending_current_message_candidate(&mut context, "lock-liveness").await
            }
        });
        holder.join().unwrap();
        assert!(
            bound,
            "brief contention must not discard the created message (exit_settle {exit_settle})"
        );
        assert!(gateway.deletes.lock().unwrap().is_empty());
        assert_eq!(pending, None);
        assert_eq!(
            load_inflight_state(&ProviderKind::Codex, channel.get())
                .unwrap()
                .current_msg_id,
            2
        );
    }
}
