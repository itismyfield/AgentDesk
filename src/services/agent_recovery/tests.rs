use crate::services::provider::ProviderKind;
use crate::utils::redact::register_known_secret;

use super::checkpoint::{
    CheckpointError, CheckpointEventKind, CheckpointPayload, DEFAULT_READ_EVENT_LIMIT,
    READ_BYTE_CAP, last_n_events, prepare_event,
};
use super::detector::{DetectorSignal, MailboxStallKind};
use super::handoff::{RecoveryIntake, dual_processing};
use super::policy::{
    OrgAgentInput, OrgChannelInput, RecoveryConfigWire, build_recovery_catalog,
    load_org_recovery_catalog_from_yaml, validate_distinct_fallback_agent,
};
use super::restore::{RestoreSessionMode, format_restore_packet, session_mode_for_provider};
use super::{ObserveInput, RecoveryRuntime};

pub(super) const CHANNEL: &str = "1486723324259340408";
const SECRET: &str = "live-recovery-secret-token";

fn enabled_recovery(fallback: &str) -> RecoveryConfigWire {
    RecoveryConfigWire {
        enabled: Some(true),
        fallback_agent_id: Some(fallback.to_string()),
        stall_secs: Some(180),
        workspace_mode: Some("inherit".to_string()),
        triggers: None,
    }
}

fn agent(
    id: &str,
    provider: &str,
    workspace: Option<&str>,
    recovery: Option<RecoveryConfigWire>,
) -> OrgAgentInput {
    OrgAgentInput {
        id: id.to_string(),
        provider: Some(provider.to_string()),
        model: Some("grok-4.6".to_string()),
        workspace: workspace.map(ToOwned::to_owned),
        recovery,
    }
}

fn channel(owner: &str, recovery: Option<RecoveryConfigWire>) -> OrgChannelInput {
    OrgChannelInput {
        channel_id: CHANNEL.to_string(),
        agent: owner.to_string(),
        provider: None,
        workspace: None,
        recovery,
    }
}

pub(super) fn enabled_runtime() -> RecoveryRuntime {
    let catalog = build_recovery_catalog(
        &[
            agent(
                "claude",
                "grok",
                Some("/primary-workspace"),
                Some(enabled_recovery("monitoring")),
            ),
            agent("monitoring", "codex", Some("/fallback-workspace"), None),
        ],
        &[channel("claude", Some(enabled_recovery("monitoring")))],
    )
    .expect("valid recovery catalog");
    let mut runtime = RecoveryRuntime::new();
    runtime.install_catalog(catalog);
    runtime
}

#[test]
fn reloading_the_same_catalog_preserves_active_recovery_state() {
    let mut runtime = enabled_runtime();
    runtime.claim_turn(CHANNEL, "turn-preserve");
    runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-preserve".to_string(),
            signal: DetectorSignal::StreamIdleTimeout,
        })
        .spawn
        .expect("fallback spawn");
    let catalog = runtime.catalog().clone();

    runtime.install_catalog(catalog);
    acknowledge(&mut runtime);

    assert_eq!(runtime.spawned().len(), 1);
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Codex, CHANNEL),
        Some(RecoveryIntake::Allow)
    );
    assert!(!runtime.allows_cli_turn(CHANNEL, "claude"));
}

#[test]
fn replacing_or_clearing_catalog_preserves_the_active_lease_until_restore_ack() {
    let mut runtime = enabled_runtime();
    runtime.observe(ObserveInput {
        channel_id: CHANNEL.into(),
        primary_turn_id: "turn-clear".into(),
        signal: DetectorSignal::StreamIdleTimeout,
    });
    let before = runtime.states[CHANNEL].clone();
    for clear in [false, true] {
        if clear {
            runtime.clear_catalog();
        } else {
            runtime.install_catalog(super::policy::RecoveryCatalog::default());
        }
        assert_eq!(runtime.states[CHANNEL], before);
        assert!(!runtime.events(CHANNEL).is_empty());
        assert!(!runtime.allows_cli_turn(CHANNEL, "claude"));
        assert_eq!(
            runtime.inherit_workspace(CHANNEL).as_deref(),
            Some("/primary-workspace")
        );
        assert_eq!(
            runtime.channel_recovery_intake(&ProviderKind::Codex, CHANNEL),
            Some(RecoveryIntake::Skip)
        );
    }
    acknowledge(&mut runtime);
    runtime
        .note_fallback_progress(CHANNEL, CheckpointEventKind::Complete, compact("done"))
        .unwrap();
    runtime
        .try_restore_owner(CHANNEL, &ProviderKind::Grok, true, false)
        .unwrap();
    assert!(!runtime.allows_cli_turn(CHANNEL, "claude"));
    assert!(runtime.fallback_prompt_prefix(CHANNEL).is_none());
    acknowledge(&mut runtime);
    assert!(runtime.allows_cli_turn(CHANNEL, "claude"));
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Codex, CHANNEL),
        None
    );
}

fn acknowledge(runtime: &mut RecoveryRuntime) {
    let lease = super::RecoveryLease::from_state(&runtime.states[CHANNEL]);
    runtime.acknowledge_start(&lease).unwrap();
}

#[test]
fn recovery_catalog_rejects_fallback_on_the_owner_provider() {
    let error = build_recovery_catalog(
        &[
            agent(
                "claude",
                "grok",
                Some("/primary-workspace"),
                Some(enabled_recovery("backup")),
            ),
            agent("backup", "grok", Some("/fallback-workspace"), None),
        ],
        &[channel("claude", Some(enabled_recovery("backup")))],
    )
    .expect_err("provider-level dispatch cannot fence two same-provider agents");
    assert!(matches!(
        error,
        super::policy::PolicyError::SameProviderFallback { .. }
    ));
}

fn compact(progress: &str) -> CheckpointPayload {
    CheckpointPayload::compact(
        "claude",
        "ship recovery",
        progress,
        "stay on original channel",
        vec!["src/services/agent_recovery/mod.rs".to_string()],
        "continue from Next",
        "please keep going",
    )
}

#[test]
fn rejected_checkpoint_does_not_advance_the_wal_frontier() {
    let mut runtime = enabled_runtime().with_max_checkpoint_bytes(1);
    assert!(
        runtime
            .note_owner_progress(CHANNEL, compact("oversized"))
            .is_err()
    );
    assert!(runtime.events(CHANNEL).is_empty());
    assert_eq!(runtime.states.get(CHANNEL).unwrap().next_seq, 0);
}

#[test]
fn test_001_recovery_off_timeout_writes_zero_wal_and_does_not_spawn_fallback() {
    let catalog = build_recovery_catalog(
        &[
            agent("claude", "grok", Some("/primary-workspace"), None),
            agent("monitoring", "codex", None, None),
        ],
        &[channel("claude", None)],
    )
    .expect("unset recovery must load");
    let mut runtime = RecoveryRuntime::new();
    runtime.install_catalog(catalog);
    runtime.claim_turn(CHANNEL, "turn-1");
    let outcome = runtime.observe(ObserveInput {
        channel_id: CHANNEL.to_string(),
        primary_turn_id: "turn-1".to_string(),
        signal: DetectorSignal::StreamIdleTimeout,
    });
    assert!(outcome.trigger.is_none());
    assert!(outcome.spawn.is_none());
    assert!(runtime.events(CHANNEL).is_empty());
    assert_eq!(runtime.mailbox_handoff_calls(), 0);
    assert!(
        runtime
            .note_owner_progress(CHANNEL, compact("done"))
            .expect("disabled WAL")
            .is_none()
    );
    assert!(runtime.events(CHANNEL).is_empty());
}

#[test]
fn test_002_active_foreground_stream_does_not_start_fallback_tmux_alive_relay_dead_after_stall_secs_does()
 {
    let mut runtime = enabled_runtime();
    runtime.claim_turn(CHANNEL, "turn-2");
    let ignored = runtime.observe(ObserveInput {
        channel_id: CHANNEL.to_string(),
        primary_turn_id: "turn-2".to_string(),
        signal: DetectorSignal::Mailbox {
            kind: MailboxStallKind::ActiveForegroundStream,
            elapsed_secs: 900,
            claimed_turn: true,
        },
    });
    assert!(ignored.trigger.is_none());
    assert!(ignored.spawn.is_none());
    assert!(runtime.events(CHANNEL).is_empty());

    let too_early = runtime.observe(ObserveInput {
        channel_id: CHANNEL.to_string(),
        primary_turn_id: "turn-2".to_string(),
        signal: DetectorSignal::Mailbox {
            kind: MailboxStallKind::TmuxAliveRelayDead,
            elapsed_secs: 30,
            claimed_turn: true,
        },
    });
    assert!(too_early.spawn.is_none());

    let started = runtime.observe(ObserveInput {
        channel_id: CHANNEL.to_string(),
        primary_turn_id: "turn-2".to_string(),
        signal: DetectorSignal::Mailbox {
            kind: MailboxStallKind::TmuxAliveRelayDead,
            elapsed_secs: 180,
            claimed_turn: true,
        },
    });
    assert_eq!(started.trigger, Some(super::TriggerKind::MailboxStall));
    assert!(started.spawn.is_some());
    let again = runtime.observe(ObserveInput {
        channel_id: CHANNEL.to_string(),
        primary_turn_id: "turn-2".to_string(),
        signal: DetectorSignal::Mailbox {
            kind: MailboxStallKind::TmuxAliveRelayDead,
            elapsed_secs: 180,
            claimed_turn: true,
        },
    });
    assert!(again.spawn.is_none());
    assert_eq!(runtime.spawned().len(), 1);
}

#[test]
fn recovery_takeover_issues_a_monotonic_generation_fence() {
    let mut runtime = enabled_runtime();
    runtime.claim_turn(CHANNEL, "turn-generation");
    let first = runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-generation".to_string(),
            signal: DetectorSignal::StreamIdleTimeout,
        })
        .spawn
        .expect("first fallback");
    assert_eq!(first.generation, 1);

    runtime.abort(CHANNEL);
    runtime.claim_turn(CHANNEL, "turn-generation-2");
    let second = runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-generation-2".to_string(),
            signal: DetectorSignal::StreamIdleTimeout,
        })
        .spawn
        .expect("second fallback");
    assert!(second.generation > first.generation);
}

#[test]
fn test_003_wal_append_last_n_size_cap_and_redact() {
    register_known_secret(SECRET);
    let mut runtime = enabled_runtime().with_max_checkpoint_bytes(2048);
    runtime
        .note_owner_progress(
            CHANNEL,
            CheckpointPayload::compact(
                "claude",
                "goal",
                format!("token={SECRET}"),
                "decisions",
                vec!["a.rs".to_string()],
                "next",
                "user",
            ),
        )
        .expect("append")
        .expect("enabled");
    let stored = &runtime.events(CHANNEL)[0];
    assert!(!stored.payload.progress.contains(SECRET));
    assert!(stored.payload.progress.contains("***"));

    let oversized = prepare_event(
        CHANNEL,
        2,
        "claude",
        CheckpointEventKind::OwnerProgress,
        CheckpointPayload::compact(
            "claude",
            "goal",
            "x".repeat(4096),
            "decisions",
            Vec::new(),
            "next",
            "user",
        ),
        512,
    );
    assert!(matches!(oversized, Err(CheckpointError::TooLarge { .. })));

    for index in 0..10 {
        runtime
            .note_owner_progress(CHANNEL, compact(&format!("step {index}")))
            .unwrap();
    }
    let last = runtime.last_n(CHANNEL);
    assert_eq!(last.len(), DEFAULT_READ_EVENT_LIMIT);
    let seqs: Vec<i64> = last.iter().map(|event| event.seq).collect();
    let mut ordered = seqs.clone();
    ordered.sort_unstable();
    assert_eq!(seqs, ordered);
    assert_eq!(
        last.last().map(|event| event.seq),
        runtime.events(CHANNEL).last().map(|event| event.seq)
    );

    let bulky = (0..10)
        .map(|index| {
            prepare_event(
                CHANNEL,
                i64::from(index + 1),
                "claude",
                CheckpointEventKind::OwnerProgress,
                CheckpointPayload::compact(
                    "claude",
                    "goal",
                    "b".repeat(5000),
                    "decisions",
                    Vec::new(),
                    "next",
                    "user",
                ),
                32 * 1024,
            )
            .expect("bulky event")
        })
        .collect::<Vec<_>>();
    let capped = last_n_events(&bulky, 8, READ_BYTE_CAP);
    assert!(capped.len() < 8);
    assert!(
        capped
            .iter()
            .map(|event| event.payload_bytes)
            .sum::<usize>()
            <= READ_BYTE_CAP
    );
}

#[test]
fn test_004_lock_gives_fallback_allow_owner_skip_fallback_http_no_mailbox_handoff() {
    let mut runtime = enabled_runtime();
    runtime.claim_turn(CHANNEL, "turn-4");
    let spawn = runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-4".to_string(),
            signal: DetectorSignal::StreamIdleTimeout,
        })
        .spawn
        .expect("fallback spawn");
    acknowledge(&mut runtime);
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Grok, CHANNEL),
        Some(RecoveryIntake::Skip)
    );
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Codex, CHANNEL),
        Some(RecoveryIntake::Allow)
    );
    assert_eq!(spawn.watcher_http_bot, "codex");
    assert!(!spawn.mailbox_handoff_called);
    assert_eq!(runtime.mailbox_handoff_calls(), 0);
    assert!(!include_str!("handoff.rs").contains("start_agent_handoff_turn"));
    assert!(!include_str!("mod.rs").contains("start_agent_handoff_turn"));
}

#[test]
fn test_005_owner_cli_is_not_given_a_new_turn_while_fallback_holds_lock() {
    let mut runtime = enabled_runtime();
    runtime.claim_turn(CHANNEL, "turn-5");
    assert!(runtime.allows_cli_turn(CHANNEL, "claude"));
    runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-5".to_string(),
            signal: DetectorSignal::WatchdogUnresponsive,
        })
        .spawn
        .expect("spawn");
    acknowledge(&mut runtime);
    assert!(!runtime.allows_cli_turn(CHANNEL, "claude"));
    assert!(runtime.allows_cli_turn(CHANNEL, "monitoring"));
}

#[test]
fn test_006_fallback_cwd_is_primary_workspace() {
    let mut runtime = enabled_runtime();
    runtime.claim_turn(CHANNEL, "turn-6");
    let spawn = runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-6".to_string(),
            signal: DetectorSignal::TmuxSessionDead,
        })
        .spawn
        .expect("spawn");
    assert_eq!(spawn.cwd, "/primary-workspace");
    assert_ne!(spawn.cwd, "/fallback-workspace");
    assert_eq!(
        runtime.inherit_workspace(CHANNEL).as_deref(),
        Some("/primary-workspace")
    );
}

#[test]
fn test_007_fallback_prompt_last_n_seq_order_restore_uses_latest_owner_retakes() {
    let mut runtime = enabled_runtime();
    runtime
        .note_owner_progress(CHANNEL, compact("first"))
        .unwrap();
    runtime
        .note_owner_progress(CHANNEL, compact("second"))
        .unwrap();
    runtime.claim_turn(CHANNEL, "turn-7");
    let spawn = runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-7".to_string(),
            signal: DetectorSignal::TurnRateLimit,
        })
        .spawn
        .expect("spawn");
    let first_idx = spawn
        .prompt
        .find("step first")
        .or_else(|| spawn.prompt.find("first"));
    let second_idx = spawn.prompt.find("second");
    assert!(first_idx.is_some());
    assert!(second_idx.is_some());
    assert!(first_idx < second_idx);
    assert!(spawn.prompt.contains("seq=1"));
    assert!(spawn.prompt.contains("seq=2"));

    runtime
        .note_fallback_progress(
            CHANNEL,
            CheckpointEventKind::Complete,
            CheckpointPayload::compact(
                "monitoring",
                "ship recovery",
                "fallback finished files",
                "keep channel",
                vec!["src/a.rs".to_string()],
                "owner continues here",
                "please keep going",
            ),
        )
        .unwrap();
    let plan = runtime
        .restore_owner(CHANNEL, true, "fallback parked")
        .expect("restore");
    assert!(plan.packet.starts_with("[recovery restore checkpoint_id="));
    assert!(plan.packet.contains("from=monitoring"));
    assert!(plan.packet.contains("to=claude"));
    assert!(plan.packet.contains("Goal: ship recovery"));
    assert!(plan.packet.contains("Progress: fallback finished files"));
    assert!(plan.packet.contains("Decisions: keep channel"));
    assert!(plan.packet.contains("Files: src/a.rs"));
    assert!(plan.packet.contains("Next: owner continues here"));
    assert!(
        plan.packet
            .contains("Fallback outcome: succeeded — fallback parked")
    );
    assert!(plan.packet.contains(
        "You are the primary agent restored as a checkpoint. Do not redo completed Files. Continue from Next."
    ));
    assert_eq!(plan.owner_intake, Some(RecoveryIntake::Skip));
    assert!(!runtime.allows_cli_turn(CHANNEL, "claude"));
    acknowledge(&mut runtime);
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Grok, CHANNEL),
        None
    );
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Codex, CHANNEL),
        None
    );
    assert!(!super::effective_handles(false, plan.fallback_intake));
    assert!(runtime.allows_cli_turn(CHANNEL, "claude"));
}

#[test]
fn test_008_fallback_equals_primary_or_missing_agent_fails_org_load() {
    let same = build_recovery_catalog(
        &[agent(
            "claude",
            "grok",
            None,
            Some(enabled_recovery("claude")),
        )],
        &[channel("claude", Some(enabled_recovery("claude")))],
    );
    assert!(same.is_err(), "self fallback must fail org load");

    let missing = build_recovery_catalog(
        &[agent(
            "claude",
            "grok",
            None,
            Some(enabled_recovery("monitoring")),
        )],
        &[channel("claude", Some(enabled_recovery("monitoring")))],
    );
    assert!(missing.is_err(), "unknown fallback must fail org load");

    let yaml = r#"
agents:
  claude:
    provider: grok
    recovery:
      enabled: true
      fallback_agent_id: claude
channels:
  by_id:
    "1":
      agent: claude
"#;
    assert!(load_org_recovery_catalog_from_yaml(yaml).is_err());
    assert!(validate_distinct_fallback_agent(Some("codex"), Some("codex")).is_err());
}

#[test]
fn test_010_owner_healthy_without_fallback_inflight_restores_from_wal() {
    let mut runtime = enabled_runtime();
    runtime.claim_turn(CHANNEL, "turn-10");
    runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-10".to_string(),
            signal: DetectorSignal::TmuxSessionDead,
        })
        .spawn
        .expect("spawn");
    acknowledge(&mut runtime);
    assert!(
        runtime
            .try_restore_owner(CHANNEL, &ProviderKind::Grok, true, true)
            .is_none(),
        "must not steal the channel while fallback inflight is live"
    );
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Codex, CHANNEL),
        Some(RecoveryIntake::Allow)
    );
    assert!(
        runtime
            .try_restore_owner(CHANNEL, &ProviderKind::Codex, true, false)
            .is_none(),
        "fallback bot health must not restore owner"
    );
    assert!(
        runtime
            .try_restore_owner(CHANNEL, &ProviderKind::Grok, false, false)
            .is_none()
    );
    assert!(
        runtime
            .try_restore_owner(CHANNEL, &ProviderKind::Grok, true, false)
            .is_none()
    );
    runtime
        .note_fallback_progress(CHANNEL, CheckpointEventKind::Complete, compact("done"))
        .unwrap();
    let plan = runtime
        .try_restore_owner(CHANNEL, &ProviderKind::Grok, true, false)
        .expect("owner healthy restore");
    assert!(plan.packet.contains("to=claude"));
    assert!(plan.packet.contains("Goal:"));
    acknowledge(&mut runtime);
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Grok, CHANNEL),
        None
    );
    assert_eq!(
        runtime.channel_recovery_intake(&ProviderKind::Codex, CHANNEL),
        None
    );
    assert!(runtime.allows_cli_turn(CHANNEL, "claude"));

    let mut rate_limited = enabled_runtime();
    rate_limited.claim_turn(CHANNEL, "turn-10b");
    rate_limited
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-10b".to_string(),
            signal: DetectorSignal::TurnRateLimit,
        })
        .spawn
        .expect("rate-limit spawn");
    assert!(
        rate_limited
            .try_restore_owner(CHANNEL, &ProviderKind::Grok, true, false)
            .is_none(),
        "rate-limit fallback must keep exclusive intake until fallback completes"
    );
    rate_limited
        .note_fallback_progress(
            CHANNEL,
            CheckpointEventKind::Complete,
            CheckpointPayload::compact(
                "monitoring",
                "",
                "fallback finished",
                "",
                Vec::new(),
                "owner continues",
                "",
            ),
        )
        .unwrap();
    assert!(
        rate_limited
            .try_restore_owner(CHANNEL, &ProviderKind::Grok, true, false)
            .is_some()
    );
}

#[test]
fn test_009_dual_processing_fails() {
    let mut runtime = enabled_runtime();
    let owner = ProviderKind::Grok;
    let fallback = ProviderKind::Codex;
    assert!(!dual_processing(
        true,
        runtime.channel_recovery_intake(&owner, CHANNEL),
        false,
        runtime.channel_recovery_intake(&fallback, CHANNEL),
    ));
    runtime.claim_turn(CHANNEL, "turn-9");
    runtime
        .observe(ObserveInput {
            channel_id: CHANNEL.to_string(),
            primary_turn_id: "turn-9".to_string(),
            signal: DetectorSignal::StreamIdleTimeout,
        })
        .spawn
        .expect("spawn");
    acknowledge(&mut runtime);
    let owner_overlay = runtime.channel_recovery_intake(&owner, CHANNEL);
    let fallback_overlay = runtime.channel_recovery_intake(&fallback, CHANNEL);
    assert_eq!(owner_overlay, Some(RecoveryIntake::Skip));
    assert_eq!(fallback_overlay, Some(RecoveryIntake::Allow));
    assert!(!dual_processing(
        true,
        owner_overlay,
        false,
        fallback_overlay
    ));
    assert!(dual_processing(
        true,
        Some(RecoveryIntake::Allow),
        false,
        Some(RecoveryIntake::Allow)
    ));
}

#[test]
fn opencode_restore_is_always_fresh_packet() {
    assert_eq!(
        session_mode_for_provider(&ProviderKind::OpenCode),
        RestoreSessionMode::Fresh
    );
    let packet = format_restore_packet(
        "arc_test",
        "monitoring",
        "claude",
        &CheckpointPayload::compact("claude", "g", "p", "d", vec!["f.rs".into()], "n", "u"),
        false,
        "failed summary",
    );
    assert!(packet.contains("Fallback outcome: failed — failed summary"));
    assert!(packet.contains("Files: f.rs"));
}
