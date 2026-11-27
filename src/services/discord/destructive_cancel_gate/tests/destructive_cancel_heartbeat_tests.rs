use super::*;
use std::sync::atomic::Ordering::Release;
use std::time::SystemTime;

#[test]
fn finished_caught_up_capture_releases_fresh_heartbeat_gate() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::TempDir::new().expect("runtime root");
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
    current_thread_rt().block_on(async {
        let shared = super::super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(8_448_001);
        let tmux = "tmux-heartbeat-caught-up";
        let output_path = root.path().join("heartbeat-caught-up.jsonl");
        write_generation_marker(tmux);
        let generation =
            crate::services::tmux_common::spawn_markers::read_generation_file_mtime_ns(tmux);
        let coord = shared.tmux_relay_coord(channel);
        coord
            .confirmed_end_generation_mtime_ns
            .store(generation, Release);
        shared
            .tmux_watchers
            .insert(channel, fresh_watcher_handle(tmux, &output_path));

        let done = r#"{"type":"result","result":"done","session_id":"s"}"#;
        let started = r#"{"type":"user","message":{"role":"user","content":"next turn"}}"#;
        for (case, tail, grows, advances, lag, age, allowed) in [
            ("capture grew", done, true, false, 0, 178, false),
            ("frontier advanced", done, false, true, 0, 178, false),
            ("new turn started", started, false, false, 0, 178, false),
            ("frontier lags", done, false, false, 1, 178, false),
            ("finished recently", done, false, false, 0, 110, false),
            ("finished and caught up", done, false, false, 0, 178, true),
        ] {
            let len = write_jsonl(&output_path, &[done, tail]);
            let frontier = len + u64::from(grows) - lag;
            coord
                .confirmed_end_offset
                .store(frontier - u64::from(advances), Release);
            let state = save_gate_state(
                provider.clone(),
                channel.get(),
                8_448_002,
                tmux,
                &output_path,
                len,
            );
            let snapshot =
                DestructiveCancelProbeSnapshot::from_state(&shared, &state, None, channel);
            assert_eq!(
                snapshot.relay_frontier,
                Some(frontier - u64::from(advances)),
                "{case}"
            );
            if grows {
                write_jsonl(&output_path, &[done, tail, ""]);
            }
            coord.confirmed_end_offset.store(frontier, Release);
            filetime::set_file_mtime(
                &output_path,
                filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(age)),
            )
            .expect("set capture age");
            assert_eq!(
                terminal_envelope_present(&provider, &snapshot),
                tail == done,
                "{case}"
            );

            let gate = evaluate(&shared, &provider, channel, channel, &snapshot).await;
            let expected = if allowed {
                DestructiveCancelGate::Allowed("terminal_envelope_present")
            } else {
                DestructiveCancelGate::Denied("fresh_watcher_heartbeat")
            };
            assert_eq!(gate, expected, "{case}");
        }
    });
}
