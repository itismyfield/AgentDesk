use super::*;

#[path = "footer_retirement_raw_tests.rs"]
mod footer_retirement_raw_tests;

#[test]
fn retired_tick_with_legacy_ownership_preserves_long_body_and_row() {
    const CHILD: &str = "AGENTDESK_N4D1B_RETIRED_TICK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let name = format!(
            "{}::retired_tick_with_legacy_ownership_preserves_long_body_and_row",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("AGENTDESK_SINGLE_MESSAGE_PANEL", "0")
            .env(
                crate::services::tui_o::cutover::test_override::CHILD_ENV,
                "1",
            )
            .env(
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                "[]",
            )
            .output()
            .unwrap();
        assert!(output.status.success(), "retired tick child: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        return;
    }
    let (_lock, guard) = isolate_root();
    let fx = seed_row(guard.root.path(), 63_251, false, false);
    let before = fx.row_bytes();
    let mtime = std::fs::metadata(fx.path()).unwrap().modified().unwrap();
    capture_warns(async {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        let ui = &mut Arc::get_mut(&mut shared).unwrap().ui;
        ui.status_panel_v2_enabled = true;
        ui.two_message_panel_enabled = true;
        assert_eq!(
            crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel_tmux(
                fx.channel.get(),
                Some(&fx.tmux),
            ),
            Ok(false),
            "the real ownership decision permits Legacy before the mode fence"
        );
        let rec = recorder(fx.channel, true).await;
        let mut locals = tick_locals(&fx, Some(PLACEHOLDER_MSG));
        locals.panel_id = msg(PANEL_MSG);
        let body = "native observed body ".repeat(300);
        let observation = crate::services::discord::inflight::o_seed_observation::Guard::new(
            &fx.provider,
            fx.channel.get(),
        );
        assert_eq!(
            run_tick_mode(
                &mut locals,
                &rec,
                &shared,
                &fx,
                false,
                &body,
                WatcherLegacyMode::RetiredO,
            )
            .await,
            FALLTHROUGH,
        );
        let raw = observation.raw_process_snapshot();
        assert_eq!(
            raw,
            observation.snapshot(),
            "no keyed observation exclusions"
        );
        assert_eq!(raw.event_count("streaming_tick"), 1, "{raw:?}");
        assert_eq!(raw.writable_inflight_load_calls, 0, "{raw:?}");
        assert_eq!(raw.readonly_inflight_load_calls, 0, "{raw:?}");
        assert_eq!(raw.compatibility_backfill_attempts, 0, "{raw:?}");
        assert_eq!(
            rec.total(),
            0,
            "RetiredO cannot emit or roll over a Legacy body"
        );
        assert_eq!(locals.sent, 0);
        assert!(locals.frozen.is_empty());
        assert_eq!(fx.row_bytes(), before);
        assert_eq!(
            std::fs::metadata(fx.path()).unwrap().modified().unwrap(),
            mtime
        );
        drop(observation);
        let observation = crate::services::discord::inflight::o_seed_observation::Guard::new(
            &fx.provider,
            fx.channel.get(),
        );
        run_tick_mode(
            &mut locals,
            &rec,
            &shared,
            &fx,
            false,
            &body,
            WatcherLegacyMode::Legacy,
        )
        .await;
        assert!(
            rec.total() > 0,
            "the same Legacy transport fixture emits real HTTP"
        );
        assert!(
            observation
                .raw_process_snapshot()
                .writable_inflight_load_calls
                > 0,
            "the Legacy control executes its actual loader"
        );
        rec.server.abort();
        while !rec.server.is_finished() {
            tokio::task::yield_now().await;
        }
    });
}
