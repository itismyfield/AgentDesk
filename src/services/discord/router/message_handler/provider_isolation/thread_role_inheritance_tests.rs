use super::*;
#[cfg(unix)]
const RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT: u32 = 3;

#[cfg(unix)]
fn clear_test_defer(channel_id: ChannelId) {
    clear_runtime_mismatch_defer(&ProviderKind::Claude, channel_id);
}

#[test]
fn matrix_preserves_live_turns_and_only_recreates_strong_idle_mismatches() {
    let cases = [
        (
            RuntimeKindEvidenceStrength::Weak,
            RuntimeKindEvidenceStrength::Strong,
            true,
            RuntimeMismatchVerdict::Defer,
            "weak expected plus strong observed live turn",
        ),
        (
            RuntimeKindEvidenceStrength::Weak,
            RuntimeKindEvidenceStrength::Weak,
            false,
            RuntimeMismatchVerdict::Defer,
            "weak expected plus weak observed evidence",
        ),
        (
            RuntimeKindEvidenceStrength::Strong,
            RuntimeKindEvidenceStrength::Strong,
            false,
            RuntimeMismatchVerdict::Recreate,
            "strong config change with no live turn",
        ),
        (
            RuntimeKindEvidenceStrength::Strong,
            RuntimeKindEvidenceStrength::Strong,
            true,
            RuntimeMismatchVerdict::Defer,
            "strong config change with live turn",
        ),
        (
            RuntimeKindEvidenceStrength::Weak,
            RuntimeKindEvidenceStrength::Strong,
            false,
            RuntimeMismatchVerdict::Defer,
            "strong observed evidence cannot authorize dispatch under a weak expectation",
        ),
        (
            RuntimeKindEvidenceStrength::Strong,
            RuntimeKindEvidenceStrength::Moderate,
            false,
            RuntimeMismatchVerdict::Defer,
            "moderate observed evidence cannot authorize destructive cleanup",
        ),
        (
            RuntimeKindEvidenceStrength::Strong,
            RuntimeKindEvidenceStrength::Weak,
            false,
            RuntimeMismatchVerdict::Defer,
            "provider-only observed fallback cannot authorize destructive cleanup",
        ),
    ];
    for (expected_strength, observed_strength, live_turn, want, label) in cases {
        let expected = ManagedRuntimeExpectation {
            runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
            evidence_strength: expected_strength,
        };
        let observed = ObservedManagedRuntimeKind {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            evidence_strength: observed_strength,
        };
        assert_eq!(
            runtime_mismatch_verdict(expected, observed, live_turn),
            want,
            "{label}"
        );
    }
}

#[test]
fn matching_runtime_never_recreates_regardless_of_liveness() {
    for live_turn in [false, true] {
        assert_eq!(
            runtime_mismatch_verdict(
                ManagedRuntimeExpectation {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                },
                ObservedManagedRuntimeKind {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                },
                live_turn,
            ),
            RuntimeMismatchVerdict::Match
        );
    }
}

#[cfg(unix)]
fn reconcile_with_observation(
    expected: ManagedRuntimeExpectation,
    observed: ObservedManagedRuntimeKind,
    live_turn: bool,
    sink: &mut Vec<String>,
) -> RuntimeMismatchVerdict {
    reconcile_managed_tmux_runtime_kind_for_config(
        &ProviderKind::Claude,
        ChannelId::new(50_150_001),
        Some("AgentDesk-5015-runtime"),
        Some(expected),
        |_| true,
        |_, _| Some(observed),
        || RuntimeInflightEvidence {
            open: live_turn,
            stale: false,
        },
        || crate::services::tui_turn_state::TuiTurnState::Idle,
        |_| true,
        |_, _, _, _, _| {},
        |name, _, _| sink.push(name.to_string()),
    )
}

#[cfg(unix)]
#[test]
fn reconcile_match_does_not_call_recreate_sink() {
    let runtime = ManagedRuntimeExpectation {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        evidence_strength: RuntimeKindEvidenceStrength::Strong,
    };
    let mut calls = Vec::new();
    assert_eq!(
        reconcile_with_observation(
            runtime,
            ObservedManagedRuntimeKind {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            },
            false,
            &mut calls,
        ),
        RuntimeMismatchVerdict::Match
    );
    assert!(calls.is_empty());
}

#[cfg(unix)]
#[test]
fn reconcile_defer_does_not_call_recreate_sink() {
    let mut calls = Vec::new();
    assert_eq!(
        reconcile_with_observation(
            ManagedRuntimeExpectation {
                runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            },
            ObservedManagedRuntimeKind {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            },
            true,
            &mut calls,
        ),
        RuntimeMismatchVerdict::Defer
    );
    assert!(
        calls.is_empty(),
        "Defer must return before destructive wiring"
    );
}

#[cfg(unix)]
#[test]
fn escalation_notice_contains_identity_and_runtime_facts() {
    let message = super::runtime_mismatch::build_runtime_mismatch_escalation_notice(
        ChannelId::new(50_150_009),
        "AgentDesk-5015-escalation",
        ManagedRuntimeExpectation {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            evidence_strength: RuntimeKindEvidenceStrength::Strong,
        },
        ObservedManagedRuntimeKind {
            runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
            evidence_strength: RuntimeKindEvidenceStrength::Moderate,
        },
    );
    assert!(message.contains("channel_id: 50150009"));
    assert!(message.contains("session: AgentDesk-5015-escalation"));
    assert!(message.contains("observed runtime: legacy_tmux_wrapper"));
    assert!(message.contains("expected runtime: claude_tui"));
    assert!(message.contains("continues without kill/recreate"));
}

#[cfg(unix)]
#[test]
fn repeated_weak_idle_mismatch_escalates_once_without_cleanup() {
    let channel_id = ChannelId::new(50_150_010);
    clear_test_defer(channel_id);
    let mut calls = Vec::new();
    let escalation_events = std::cell::Cell::new(0_u32);
    for _ in 0..(RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT * 2 + 1) {
        let verdict = reconcile_managed_tmux_runtime_kind_for_config(
            &ProviderKind::Claude,
            channel_id,
            Some("AgentDesk-5015-weak-idle"),
            Some(ManagedRuntimeExpectation {
                runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                evidence_strength: RuntimeKindEvidenceStrength::Weak,
            }),
            |_| true,
            |_, _| {
                Some(ObservedManagedRuntimeKind {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                })
            },
            || RuntimeInflightEvidence {
                open: false,
                stale: false,
            },
            || crate::services::tui_turn_state::TuiTurnState::Idle,
            |_| true,
            |_, _, _, _, _| escalation_events.set(escalation_events.get() + 1),
            |name, _, _| calls.push(name.to_string()),
        );
        assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
    }
    assert!(calls.is_empty());
    assert_eq!(escalation_events.get(), 1);
    clear_test_defer(channel_id);
}

#[cfg(unix)]
#[test]
fn stale_inflight_with_busy_transcript_never_cleans_up() {
    let channel_id = ChannelId::new(50_150_011);
    clear_test_defer(channel_id);
    let mut calls = Vec::new();
    let probe_calls = std::cell::Cell::new(0_u32);
    let escalation_events = std::cell::Cell::new(0_u32);
    for _ in 0..(RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT + 3) {
        let verdict = reconcile_managed_tmux_runtime_kind_for_config(
            &ProviderKind::Claude,
            channel_id,
            Some("AgentDesk-5015-stale-busy"),
            Some(ManagedRuntimeExpectation {
                runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            }),
            |_| true,
            |_, _| {
                Some(ObservedManagedRuntimeKind {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    evidence_strength: RuntimeKindEvidenceStrength::Strong,
                })
            },
            || RuntimeInflightEvidence {
                open: true,
                stale: true,
            },
            || {
                probe_calls.set(probe_calls.get() + 1);
                crate::services::tui_turn_state::TuiTurnState::Streaming
            },
            |_| true,
            |_, _, _, _, _| escalation_events.set(escalation_events.get() + 1),
            |name, _, _| calls.push(name.to_string()),
        );
        assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
    }
    assert_eq!(
        probe_calls.get(),
        RUNTIME_MISMATCH_DEFER_ESCALATION_COUNT + 3,
        "every stale inflight decision must query the transcript"
    );
    assert_eq!(escalation_events.get(), 1);
    assert!(
        calls.is_empty(),
        "busy transcript must veto destructive cleanup"
    );
    clear_test_defer(channel_id);
}

#[cfg(unix)]
#[test]
fn stale_inflight_with_unknown_transcript_fails_closed() {
    let channel_id = ChannelId::new(50_150_012);
    clear_test_defer(channel_id);
    let mut calls = Vec::new();
    let verdict = reconcile_managed_tmux_runtime_kind_for_config(
        &ProviderKind::Claude,
        channel_id,
        Some("AgentDesk-5015-stale-unknown"),
        Some(ManagedRuntimeExpectation {
            runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
            evidence_strength: RuntimeKindEvidenceStrength::Strong,
        }),
        |_| true,
        |_, _| {
            Some(ObservedManagedRuntimeKind {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            })
        },
        || RuntimeInflightEvidence {
            open: true,
            stale: true,
        },
        || crate::services::tui_turn_state::TuiTurnState::Unknown,
        |_| true,
        |_, _, _, _, _| {},
        |name, _, _| calls.push(name.to_string()),
    );
    assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
    assert!(
        calls.is_empty(),
        "unknown transcript evidence must fail closed"
    );
    clear_test_defer(channel_id);
}

#[cfg(unix)]
#[test]
fn cleanup_revalidation_rejects_owner_change() {
    let channel_id = ChannelId::new(50_150_013);
    clear_test_defer(channel_id);
    let mut calls = Vec::new();
    let verdict = reconcile_managed_tmux_runtime_kind_for_config(
        &ProviderKind::Claude,
        channel_id,
        Some("AgentDesk-5015-owner-change"),
        Some(ManagedRuntimeExpectation {
            runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
            evidence_strength: RuntimeKindEvidenceStrength::Strong,
        }),
        |_| true,
        |_, _| {
            Some(ObservedManagedRuntimeKind {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            })
        },
        || RuntimeInflightEvidence {
            open: false,
            stale: false,
        },
        || crate::services::tui_turn_state::TuiTurnState::Idle,
        |_| false,
        |_, _, _, _, _| {},
        |name, _, _| calls.push(name.to_string()),
    );
    assert_eq!(verdict, RuntimeMismatchVerdict::Defer);
    assert!(calls.is_empty());
    clear_test_defer(channel_id);
}

#[cfg(unix)]
#[test]
fn transcript_state_probe_preserves_provider_argument_order() {
    fn claude(
        current_path: Option<&str>,
        session_id: Option<&str>,
        tmux_session_name: Option<&str>,
    ) -> crate::services::tui_turn_state::TuiTurnState {
        assert_eq!(current_path, Some("/worktree/claude"));
        assert_eq!(session_id, Some("claude-session"));
        assert_eq!(tmux_session_name, Some("AgentDesk-claude"));
        crate::services::tui_turn_state::TuiTurnState::Streaming
    }
    fn codex(
        current_path: Option<&str>,
        tmux_session_name: Option<&str>,
        provider_session_id: Option<&str>,
    ) -> crate::services::tui_turn_state::TuiTurnState {
        assert_eq!(current_path, Some("/worktree/codex"));
        assert_eq!(tmux_session_name, Some("AgentDesk-codex"));
        assert_eq!(provider_session_id, Some("codex-session"));
        crate::services::tui_turn_state::TuiTurnState::Streaming
    }
    assert_eq!(
        managed_runtime_transcript_state_using(
            &ProviderKind::Claude,
            Some("/worktree/claude"),
            Some("claude-session"),
            Some("AgentDesk-claude"),
            claude,
            codex,
        ),
        crate::services::tui_turn_state::TuiTurnState::Streaming
    );
    assert_eq!(
        managed_runtime_transcript_state_using(
            &ProviderKind::Codex,
            Some("/worktree/codex"),
            Some("codex-session"),
            Some("AgentDesk-codex"),
            claude,
            codex,
        ),
        crate::services::tui_turn_state::TuiTurnState::Streaming
    );
}

#[cfg(unix)]
#[test]
fn reconcile_recreate_calls_sink_once_for_exact_tmux_session() {
    let mut calls = Vec::new();
    assert_eq!(
        reconcile_with_observation(
            ManagedRuntimeExpectation {
                runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            },
            ObservedManagedRuntimeKind {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                evidence_strength: RuntimeKindEvidenceStrength::Strong,
            },
            false,
            &mut calls,
        ),
        RuntimeMismatchVerdict::Recreate
    );
    assert_eq!(calls, ["AgentDesk-5015-runtime"]);
}

#[test]
fn prelaunch_claude_tui_seed_omits_wrapper_output_but_preserves_fifo() {
    let seed = prelaunch_inflight_runtime_seed_from_paths(
        "AgentDesk-claude-seed",
        "/runtime/wrapper-stream.log".to_string(),
        "/runtime/input.fifo".to_string(),
        true,
        Some(RuntimeHandoffKind::ClaudeTui),
    );
    assert_eq!(seed.0.as_deref(), Some("AgentDesk-claude-seed"));
    assert_eq!(
        seed.1, None,
        "ClaudeTui must wait for RuntimeReady transcript binding"
    );
    assert_eq!(seed.2.as_deref(), Some("/runtime/input.fifo"));
    assert_eq!(seed.3, 0);
}

#[test]
fn prelaunch_seed_is_identical_for_intake_and_headless_callers() {
    let intake = prelaunch_inflight_runtime_seed_from_paths(
        "AgentDesk-claude-symmetric",
        "/runtime/wrapper-stream.log".to_string(),
        "/runtime/input.fifo".to_string(),
        true,
        Some(RuntimeHandoffKind::ClaudeTui),
    );
    let headless = prelaunch_inflight_runtime_seed_from_paths(
        "AgentDesk-claude-symmetric",
        "/runtime/wrapper-stream.log".to_string(),
        "/runtime/input.fifo".to_string(),
        true,
        Some(RuntimeHandoffKind::ClaudeTui),
    );
    assert_eq!(intake, headless);
}

#[test]
fn managed_linked_worktree_skips_provider_reisolation_without_session_state() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    let git = |args: &[&str]| {
        crate::services::git::GitCommand::new()
            .repo(&repo)
            .args(args)
            .run_output()
            .unwrap();
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "provider-isolation@test.invalid"]);
    git(&["config", "user.name", "Provider Isolation Test"]);
    std::fs::write(repo.join("README"), "test").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-m", "initial"]);

    let (worktree_path, _) = create_git_worktree(
        repo.to_str().unwrap(),
        "restart-reisolation",
        "test-provider",
    )
    .unwrap();

    let mut session = DiscordSession {
        session_id: Some("preserved-session".to_string()),
        memento_context_loaded: true,
        memento_reflected: false,
        current_path: Some(worktree_path.clone()),
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: Some(43_170_001),
        channel_name: Some("restart-reisolation".to_string()),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
    };
    reconstruct_managed_worktree_metadata(
        &mut session,
        &ProviderKind::Claude,
        ChannelId::new(43_170_001),
        &worktree_path,
    );

    assert!(session.worktree.is_some());
    assert_eq!(session.session_id.as_deref(), Some("preserved-session"));

    let mut conflicted_session = DiscordSession {
        worktree: None,
        ..session.clone()
    };
    assert!(!reconstruct_unowned_managed_worktree(
        &mut conflicted_session,
        Some("owner-channel"),
        &ProviderKind::Claude,
        ChannelId::new(43_170_003),
        &worktree_path,
    ));
    assert!(conflicted_session.worktree.is_none());

    git(&["-C", &worktree_path, "checkout", "--detach"]);
    let mut detached_session = DiscordSession {
        worktree: None,
        ..session.clone()
    };
    reconstruct_managed_worktree_metadata(
        &mut detached_session,
        &ProviderKind::Claude,
        ChannelId::new(43_170_002),
        &worktree_path,
    );
    assert!(detached_session.worktree.is_none());
}

fn bind_parent(
    root: &std::path::Path,
    id: ChannelId,
    prompt: &std::path::Path,
    workspace: &std::path::Path,
    thread_inherit: Option<bool>,
) {
    let path = crate::runtime_layout::role_map_path(root);
    std::fs::create_dir_all(path.parent().expect("role-map parent")).unwrap();
    let mut entry = serde_json::json!({
        "roleId": "parent-role",
        "promptFile": prompt,
        "workspace": workspace,
    });
    if let Some(enabled) = thread_inherit {
        entry["threadInherit"] = serde_json::Value::Bool(enabled);
    }
    let json = serde_json::json!({ "byChannelId": { (id.get().to_string()): entry } });
    std::fs::write(path, json.to_string()).unwrap();
}
#[test]
fn thread_inherits_parent_role_workspace_and_memory_scope_by_default() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let prompt = root.path().join("parent-role.md");
    let workspace = root.path().join("parent-memory-workspace");
    std::fs::write(&prompt, "PARENT ROLE PROMPT").unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let child = ChannelId::new(43_170_101);
    let parent = (ChannelId::new(43_170_102), Some("parent".to_string()));
    bind_parent(root.path(), parent.0, &prompt, &workspace, None);
    let resolved = resolve_thread_role_binding(child, Some("thread"), Some(&parent));
    let binding = resolved.role_binding.as_ref().expect("parent role");
    assert_eq!(binding.role_id, "parent-role");
    assert_eq!(
        resolve_thread_workspace(child, Some("thread"), Some(&parent)).as_deref(),
        workspace.to_str()
    );
    assert_eq!(resolved.memory_channel_id(child), parent.0);
    assert_eq!(resolved.memory_channel_name(None), parent.1);
    let memory = settings::ResolvedMemorySettings {
        backend: settings::MemoryBackendKind::Memento,
        ..Default::default()
    };
    let built = super::super::super::super::prompt_builder::build_system_prompt_with_manifest(
        "discord",
        &[],
        workspace.to_str().unwrap(),
        child,
        parent.0,
        "token",
        Some(binding),
        false,
        super::super::super::super::prompt_builder::PromptProfiles::foreground(
            DispatchProfile::Full,
        ),
        None,
        None,
        None,
        None,
        Some(&memory),
        true,
        true,
        None,
        None,
        None,
        None,
    );
    assert!(built.system_prompt.contains("PARENT ROLE PROMPT"));
    assert!(
        built
            .system_prompt
            .contains("workspace=agentdesk-parent-memory-workspace")
    );
    let unbound_parent = (ChannelId::new(43_170_103), Some("unbound".to_string()));
    let unbound = resolve_thread_role_binding(child, Some("thread"), Some(&unbound_parent));
    assert!(unbound.role_binding.is_none());
    assert_eq!(unbound.memory_channel_id(child), child);
}
#[test]
fn thread_inherit_false_opts_out() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let prompt = std::path::Path::new("/tmp/parent-role.md");
    let workspace = std::path::Path::new("/tmp/parent-workspace");
    let child = ChannelId::new(43_170_201);
    let parent = (ChannelId::new(43_170_202), Some("parent".to_string()));
    bind_parent(root.path(), parent.0, prompt, workspace, Some(false));

    let resolved = resolve_thread_role_binding(child, Some("thread"), Some(&parent));
    assert!(resolved.role_binding.is_none());
    assert!(resolve_thread_workspace(child, Some("thread"), Some(&parent)).is_none());
    assert_eq!(resolved.memory_channel_id(child), child);
    assert_eq!(
        resolved.memory_channel_name(Some("t")).as_deref(),
        Some("t")
    );
}

#[test]
fn non_thread_resolution_is_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let prompt = std::path::Path::new("/tmp/child-role.md");
    let workspace = std::path::Path::new("/tmp/child-workspace");
    let child = ChannelId::new(43_170_301);
    bind_parent(root.path(), child, prompt, workspace, Some(false));

    let resolved = resolve_thread_role_binding(child, Some("channel"), None);
    let binding = resolved.role_binding.as_ref().expect("direct child role");
    assert_eq!(binding.role_id, "parent-role");
    assert_eq!(
        resolve_thread_workspace(child, Some("channel"), None).as_deref(),
        workspace.to_str()
    );
    assert_eq!(resolved.memory_channel_id(child), child);
}

#[test]
fn redirect_uses_final_parent_for_inheritance_and_fast_mode_key() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let prompt = std::path::Path::new("/tmp/final-parent-role.md");
    let workspace = std::path::Path::new("/tmp/final-parent-workspace");
    let incoming_channel = ChannelId::new(43_170_401);
    let final_thread = ChannelId::new(43_170_402);
    let final_parent = (ChannelId::new(43_170_403), Some("final-parent".to_string()));
    bind_parent(root.path(), final_parent.0, prompt, workspace, None);

    let resolved =
        resolve_thread_role_binding(final_thread, Some("dispatch-thread"), Some(&final_parent));
    assert_eq!(
        resolved
            .role_binding
            .as_ref()
            .map(|binding| binding.role_id.as_str()),
        Some("parent-role")
    );
    assert_eq!(resolved.memory_channel_id(final_thread), final_parent.0);
    assert_eq!(
        resolve_thread_workspace(final_thread, Some("dispatch-thread"), Some(&final_parent))
            .as_deref(),
        workspace.to_str()
    );
    let fast_mode_key = effective_fast_mode_channel_id(final_thread, Some(final_parent.clone()));
    assert_eq!(fast_mode_key, final_parent.0);
    assert_ne!(fast_mode_key, incoming_channel);
    let workspace = workspace.to_str();
    let inherited = select_final_path("/default", workspace, false);
    assert_eq!(inherited, workspace.unwrap());
    let should_update = dispatch_session_path_should_update;
    assert!(should_update(true, None, false, false, "/in", inherited));
    assert_eq!(select_final_path("/explicit", workspace, true), "/explicit");
}
