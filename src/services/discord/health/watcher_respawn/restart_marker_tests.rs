use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::agent_recovery::{self, policy::*};
use crate::services::discord::{InflightRestartMode, inflight, recovery_engine};
use std::os::unix::fs::PermissionsExt;

const CHANNEL: u64 = 940_487_400_000_001;

struct CatalogGuard;
impl Drop for CatalogGuard {
    fn drop(&mut self) {
        agent_recovery::clear_catalog();
    }
}

fn skip_catalog() -> CatalogGuard {
    let binding = ChannelRecoveryBinding {
        channel_id: CHANNEL.to_string(),
        owner_agent_id: "marker-owner".into(),
        owner_provider: ProviderKind::Claude,
        owner_model: None,
        owner_auth_profile: "default".into(),
        workspace: String::new(),
        policy: Some(RecoveryPolicy {
            enabled: true,
            fallback_agent_id: "marker-fallback".into(),
            stall_secs: 180,
            workspace_mode: WorkspaceMode::Inherit,
            triggers: Default::default(),
        }),
    };
    agent_recovery::install_catalog(RecoveryCatalog {
        channels: [(CHANNEL.to_string(), binding)].into(),
        ..Default::default()
    });
    CatalogGuard
}

fn boot_leave_then_watchdog(clear_io_error: bool) {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let tmux = root.path().join("tmux");
    std::fs::write(&tmux, "#!/bin/sh\nwhile [ \"${1#-}\" != \"$1\" ]; do shift; done\ncase \"$1\" in list-panes) echo 0 ;; esac\nexit 0\n").unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(std::iter::once(root.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let _path = TestEnvVarGuard::set_value_after_shared_test_env_lock("PATH", &path);
    let output = root.path().join("restart-watchdog.jsonl");
    std::fs::write(&output, concat!(
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"tool\",\"name\":\"Bash\",\"input\":{}}]}}\n",
        "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}\n",
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"next\"}}\n"
    )).unwrap();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(CHANNEL);
    let mut row = inflight::InflightTurnState::new(
        provider.clone(),
        CHANNEL,
        Some("marker-watchdog".into()),
        0,
        0,
        940_487_400_000_021,
        String::new(),
        Some("marker-session".into()),
        Some("AgentDesk-claude-marker-watchdog".into()),
        Some(output.display().to_string()),
        None,
        0,
    );
    row.finalizer_turn_id = 940_487_400_000_020;
    row.turn_nonce = Some("predecessor-watchdog-episode".into());
    row.turn_start_offset = Some(0);
    row.last_offset = std::fs::metadata(&output).unwrap().len();
    row.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    row.restart_mode = Some(InflightRestartMode::DrainRestart);
    row.restart_generation = Some(6);
    row.terminal_delivery_committed = clear_io_error;
    inflight::save_inflight_state(&row).unwrap();
    let row_path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &provider,
        CHANNEL,
    );
    let before = std::fs::read(&row_path).unwrap();
    std::fs::create_dir_all(root.path().join("runtime")).unwrap();
    std::fs::write(root.path().join("runtime/generation"), "7").unwrap();
    let mut shared = discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 7;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        shared.settings.write().await.allowed_channel_ids = vec![CHANNEL];
        let (ctx, server) = discord::tui_prompt_relay::relay_e2e::mock_discord_context().await;
        shared.http.cached_serenity_ctx.set(ctx).unwrap();
        let _catalog = (!clear_io_error).then(skip_catalog);
        if !clear_io_error {
            assert_eq!(
                agent_recovery::channel_recovery_intake(&provider, &CHANNEL.to_string()).await,
                Some(agent_recovery::RecoveryIntake::Skip)
            );
        }
        if clear_io_error {
            inflight::FAIL_NEXT_IDENTITY_REMOVE.with(|fail| fail.set(true));
        }
        assert_eq!(
            recovery_engine::boot_row_decision(&provider, &shared, &row).await,
            recovery_engine::BootRow::Leave
        );
        if clear_io_error {
            assert!(
                !inflight::FAIL_NEXT_IDENTITY_REMOVE.with(|fail| fail.replace(false)),
                "boot must have attempted the failing clear"
            );
        }
        assert_eq!(std::fs::read(&row_path).unwrap(), before);
        let registry = HealthRegistry::new();
        registry
            .register(provider.as_str().into(), shared.clone())
            .await;
        clear_watcher_absence(&provider, channel);
        let snapshot = registry
            .snapshot_watcher_state_for_shared(&provider, shared.clone(), CHANNEL)
            .await
            .unwrap();
        assert_eq!(snapshot.tmux_session_alive, Some(true));
        super::super::recovery::run_stall_watchdog_pass(&registry, &provider).await;
        let spawned = shared.tmux_watchers.contains_key(&channel);
        let after = std::fs::read(&row_path).unwrap();
        let mailbox = discord::mailbox_snapshot(&shared, channel).await;
        let attempted = WATCHER_ABSENCE
            .get(&WatcherAbsenceKey::new(&provider, channel))
            .map(|state| state.failed_attempts);
        clear_watcher_absence(&provider, channel);
        server.abort();
        assert!(
            !spawned,
            "watchdog must leave the boot-rejected marked episode unadopted"
        );
        assert_eq!(
            attempted,
            Some(1),
            "watchdog must reach and reject one respawn"
        );
        assert_eq!(
            after, before,
            "rebind must preserve the exact durable bytes"
        );
        assert!(mailbox.active_user_message_id.is_none());
        assert!(mailbox.cancel_token.is_none());
    });
}

#[test]
fn intake_skip_marked_boot_leave_survives_watchdog_respawn() {
    boot_leave_then_watchdog(false);
}

#[test]
fn clear_io_error_marked_boot_leave_survives_watchdog_respawn() {
    boot_leave_then_watchdog(true);
}
