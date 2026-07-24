use super::*;

fn reset_state() {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    *state = TuiPromptDedupeState::default();
}

// #tui-hook-ttl-buffer key-match: the reverse lookup must resolve the
// provider session UUID for a tmux session (the readiness layer only knows
// the tmux name, but the hooks buffer under the provider UUID), and must
// stay provider-isolated even when two providers share a tmux name.
#[test]
fn provider_session_for_tmux_resolves_reverse_mapping() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_provider_session("claude", "uuid-claude-1", "tmux-shared");
    register_provider_session("codex", "uuid-codex-1", "tmux-shared");

    // Resolves the right provider's UUID for the shared tmux name.
    assert_eq!(
        provider_session_for_tmux("claude", "tmux-shared"),
        Some("uuid-claude-1".to_string())
    );
    assert_eq!(
        provider_session_for_tmux("codex", "tmux-shared"),
        Some("uuid-codex-1".to_string())
    );
    // No mapping for an unknown tmux session => None (caller falls back to
    // the tmux name as the registry key).
    assert_eq!(provider_session_for_tmux("claude", "tmux-unknown"), None);
    // Empty inputs are rejected.
    assert_eq!(provider_session_for_tmux("claude", ""), None);
    assert_eq!(provider_session_for_tmux("", "tmux-shared"), None);
}

#[test]
fn provider_session_for_tmux_prefers_most_recent_mapping() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    // A relaunch of the same tmux session under a new provider UUID must
    // resolve to the newest UUID (the prior turn's hooks have expired/moved).
    register_provider_session("claude", "uuid-old", "tmux-relaunch");
    register_provider_session("claude", "uuid-new", "tmux-relaunch");
    assert_eq!(
        provider_session_for_tmux("claude", "tmux-relaunch"),
        Some("uuid-new".to_string())
    );
}

#[test]
fn claude_hook_payload_adopts_sibling_continuation_once_without_cursor_reset() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let tmp = tempfile::tempdir().unwrap();
    let old_session = uuid::Uuid::new_v4().to_string();
    let new_session = uuid::Uuid::new_v4().to_string();
    let old_path = tmp.path().join(format!("{old_session}.jsonl"));
    let new_path = tmp.path().join(format!("{new_session}.jsonl"));
    std::fs::write(&old_path, b"old\n").unwrap();
    std::fs::write(&new_path, b"new\n").unwrap();
    let tmux = format!("tmux-4423-continuation-{}", std::process::id());
    register_provider_session("claude", &old_session, &tmux);
    register_tmux_runtime_binding(
        &tmux,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: old_path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(old_session.clone()),
            last_offset: 99,
            relay_last_offset: Some(99),
        },
    );

    let adopted = adopt_claude_continuation_session(&old_session, &new_session)
        .expect("safe sibling continuation adoption");
    assert_eq!(adopted.0, tmux);
    assert_eq!(adopted.1, new_path.display().to_string());
    let binding = runtime_binding_for_tmux_session(&tmux).unwrap();
    assert_eq!(binding.session_id.as_deref(), Some(new_session.as_str()));
    assert_eq!(binding.output_path, new_path.display().to_string());
    assert_eq!(binding.last_offset, 0);
    assert_eq!(
        provider_session_for_tmux("claude", &tmux).as_deref(),
        Some(old_session.as_str()),
        "future waits must keep using the live process's cached hook command UUID"
    );

    assert!(adopt_claude_continuation_session(&old_session, &new_session).is_some());
    let mut progressed = runtime_binding_for_tmux_session(&tmux).unwrap();
    progressed.last_offset = 4;
    register_tmux_runtime_binding(&tmux, progressed);
    assert!(adopt_claude_continuation_session(&old_session, &new_session).is_some());
    assert_eq!(
        runtime_binding_for_tmux_session(&tmux).unwrap().last_offset,
        4,
        "subsequent old-query/new-payload hooks must not rewind the adopted cursor"
    );
}

#[test]
fn claude_hook_payload_can_advance_multiple_continuation_hops_but_not_rewind() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let tmp = tempfile::tempdir().unwrap();
    let command_session = uuid::Uuid::new_v4().to_string();
    let first_continuation = uuid::Uuid::new_v4().to_string();
    let second_continuation = uuid::Uuid::new_v4().to_string();
    let stale_continuation = uuid::Uuid::new_v4().to_string();
    let command_path = tmp.path().join(format!("{command_session}.jsonl"));
    let first_path = tmp.path().join(format!("{first_continuation}.jsonl"));
    let second_path = tmp.path().join(format!("{second_continuation}.jsonl"));
    let stale_path = tmp.path().join(format!("{stale_continuation}.jsonl"));
    for path in [&command_path, &first_path, &second_path, &stale_path] {
        std::fs::write(path, b"{}\n").unwrap();
    }
    filetime::set_file_mtime(&first_path, filetime::FileTime::from_unix_time(20, 0)).unwrap();
    filetime::set_file_mtime(&second_path, filetime::FileTime::from_unix_time(30, 0)).unwrap();
    filetime::set_file_mtime(&stale_path, filetime::FileTime::from_unix_time(10, 0)).unwrap();
    let tmux = format!("tmux-4423-multihop-{}", std::process::id());
    register_provider_session("claude", &command_session, &tmux);
    register_tmux_runtime_binding(
        &tmux,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: command_path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(command_session.clone()),
            last_offset: 7,
            relay_last_offset: None,
        },
    );

    adopt_claude_continuation_session(&command_session, &first_continuation)
        .expect("first continuation hop");
    adopt_claude_continuation_session(&command_session, &second_continuation)
        .expect("newer second continuation hop through cached command UUID");
    let binding = runtime_binding_for_tmux_session(&tmux).unwrap();
    assert_eq!(
        binding.session_id.as_deref(),
        Some(second_continuation.as_str())
    );
    assert!(
        adopt_claude_continuation_session(&command_session, &stale_continuation).is_none(),
        "a delayed historical payload must not rewind the current continuation"
    );
    assert_eq!(
        runtime_binding_for_tmux_session(&tmux)
            .unwrap()
            .session_id
            .as_deref(),
        Some(second_continuation.as_str())
    );
}

#[test]
fn provider_session_mapping_survives_prompt_purge_ttl() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_provider_session("claude", "uuid-long-lived", "tmux-long-lived");
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        let key = PromptKey::new("claude", "uuid-long-lived");
        state
            .tmux_by_provider_session
            .get_mut(&key)
            .expect("registered provider-session mapping")
            .recorded_at = Instant::now() - SESSION_MAPPING_TTL - Duration::from_secs(1);
    }

    // Any API that calls purge_expired should not delete the provider UUID
    // bridge while the TUI session can still be alive.
    register_tmux_channel("tmux-other", 42);

    assert_eq!(
        provider_session_for_tmux("claude", "tmux-long-lived"),
        Some("uuid-long-lived".to_string())
    );
}

#[test]
fn provider_session_mapping_is_removed_with_runtime_binding_clear() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_provider_session("claude", "uuid-stale", "tmux-stale");
    assert_eq!(
        provider_session_for_tmux("claude", "tmux-stale"),
        Some("uuid-stale".to_string())
    );

    assert!(clear_tmux_runtime_binding("tmux-stale"));
    assert_eq!(
        provider_session_for_tmux("claude", "tmux-stale"),
        None,
        "clearing a tmux runtime binding must also clear stale provider-session reverse mappings"
    );
}

#[test]
fn provider_session_mapping_is_removed_with_dead_tmux_mirror() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_provider_session("claude", "uuid-dead", "tmux-dead");
    assert!(evict_dead_tmux_mirror("tmux-dead"));
    assert_eq!(
        provider_session_for_tmux("claude", "tmux-dead"),
        None,
        "dead tmux mirror eviction must not leave provider-session reverse mappings behind"
    );
}

// U-14 Provider-keyed channel isolation: registering the same tmux name
// under both `claude` and `codex` providers must keep two independent
// mappings — the dedupe state must not collapse them, otherwise cc/cdx
// turns running side-by-side could cross-relay into each other's
// channels.
#[test]
fn provider_session_mapping_isolates_claude_and_codex_for_same_session_id() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_provider_session("claude", "session-shared", "tmux-claude");
    register_provider_session("codex", "session-shared", "tmux-codex");

    assert_eq!(
        resolve_tmux_session_name("claude", "session-shared"),
        Some("tmux-claude".to_string())
    );
    assert_eq!(
        resolve_tmux_session_name("codex", "session-shared"),
        Some("tmux-codex".to_string())
    );

    // Recording a Discord-originated prompt for one provider must not
    // suppress an SSH-direct prompt the other provider observes.
    record_discord_originated_prompt("claude", "tmux-claude", "shared-text");

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-claude", "shared-text"),
        PromptObservation::SuppressedDiscordDuplicate
    );
    // The codex pane has no pending entry, so the same text is a fresh
    // direct-input observation, not a duplicate.
    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-codex", "shared-text"),
        PromptObservation::PublishedSshDirect
    );
}

// U-12 `relay_output_path` falls back to `output_path` when no dedicated
// relay path is configured. A blank/whitespace-only override must not
// shadow the primary output_path — otherwise the relay would tail an
// empty path and silently drop frames.
#[test]
fn relay_output_path_falls_back_to_output_path_when_unset_or_blank() {
    let none_binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: "/tmp/transcript.jsonl".to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: None,
        last_offset: 0,
        relay_last_offset: None,
    };
    assert_eq!(none_binding.relay_output_path(), "/tmp/transcript.jsonl");

    let blank_binding = TuiRuntimeBinding {
        relay_output_path: Some("   ".to_string()),
        ..none_binding.clone()
    };
    assert_eq!(blank_binding.relay_output_path(), "/tmp/transcript.jsonl");

    let override_binding = TuiRuntimeBinding {
        relay_output_path: Some("/tmp/relay.jsonl".to_string()),
        ..none_binding.clone()
    };
    assert_eq!(override_binding.relay_output_path(), "/tmp/relay.jsonl");
}

// U-12 `relay_last_offset()` mirrors `last_offset` when the override is
// None — without this, the very first idle scan after a rehydrate
// would tail from byte 0 and replay the entire transcript.
#[test]
fn relay_last_offset_falls_back_to_last_offset_when_unset() {
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: "/tmp/transcript.jsonl".to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: None,
        last_offset: 4096,
        relay_last_offset: None,
    };
    assert_eq!(binding.relay_last_offset(), 4096);

    let with_override = TuiRuntimeBinding {
        relay_last_offset: Some(1024),
        ..binding
    };
    assert_eq!(with_override.relay_last_offset(), 1024);
}

// U-10 `advance_tmux_runtime_binding_offset` is the cold-start entry
// point used by relay readers to record where they left off. Calls with
// a mismatched output_path that is not the configured relay override
// must be rejected — otherwise a sibling reader writing the wrong path
// could fast-forward our offset past unread frames.
#[test]
fn advance_offset_rejects_mismatched_path_when_relay_override_differs() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_tmux_runtime_binding(
        "tmux-cold",
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/primary.jsonl".to_string(),
            relay_output_path: Some("/tmp/relay.jsonl".to_string()),
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );

    // Primary path advances `last_offset` and (because relay override
    // is set) leaves `relay_last_offset` alone.
    assert!(advance_tmux_runtime_binding_offset(
        "tmux-cold",
        "/tmp/primary.jsonl",
        500
    ));
    let after_primary = runtime_binding_for_tmux_session("tmux-cold").unwrap();
    assert_eq!(after_primary.last_offset, 500);
    assert!(after_primary.relay_last_offset.is_none());

    // Relay override path advances `relay_last_offset`.
    assert!(advance_tmux_runtime_binding_offset(
        "tmux-cold",
        "/tmp/relay.jsonl",
        900
    ));
    let after_relay = runtime_binding_for_tmux_session("tmux-cold").unwrap();
    assert_eq!(after_relay.relay_last_offset, Some(900));

    // An unrelated path is rejected and does not corrupt either offset.
    assert!(!advance_tmux_runtime_binding_offset(
        "tmux-cold",
        "/tmp/wrong.jsonl",
        9999
    ));
    let after_wrong = runtime_binding_for_tmux_session("tmux-cold").unwrap();
    assert_eq!(after_wrong.last_offset, 500);
    assert_eq!(after_wrong.relay_last_offset, Some(900));
}

#[test]
fn refresh_runtime_binding_activity_extends_mapping_ttl_without_offset_advance() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_tmux_runtime_binding(
        "tmux-runtime-activity",
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/live-transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("session-activity".to_string()),
            last_offset: 123,
            relay_last_offset: None,
        },
    );
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state
            .runtime_by_tmux
            .get_mut("tmux-runtime-activity")
            .expect("runtime binding")
            .recorded_at = Instant::now() - SESSION_MAPPING_TTL + Duration::from_secs(1);
    }

    assert!(refresh_tmux_runtime_binding_activity(
        "tmux-runtime-activity",
        "/tmp/live-transcript.jsonl",
    ));
    let refreshed_age = {
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state
            .runtime_by_tmux
            .get("tmux-runtime-activity")
            .expect("runtime binding")
            .recorded_at
            .elapsed()
    };
    assert!(
        refreshed_age < Duration::from_secs(1),
        "fresh transcript activity should refresh the purge timestamp"
    );
    let binding = runtime_binding_for_tmux_session("tmux-runtime-activity")
        .expect("binding survives purge after refresh");
    assert_eq!(binding.last_offset, 123);
}

#[test]
fn suppresses_exact_pending_prompt() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    record_discord_originated_prompt("claude", "tmux-a", "hello");

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-a", "hello"),
        PromptObservation::SuppressedDiscordDuplicate
    );
}

#[test]
fn stores_runtime_binding_by_tmux_session() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_tmux_runtime_binding(
        "tmux-runtime",
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/tmp/codex-rollout.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("thread-123".to_string()),
            last_offset: 77,
            relay_last_offset: None,
        },
    );

    assert_eq!(
        runtime_binding_for_tmux_session("tmux-runtime"),
        Some(TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/tmp/codex-rollout.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("thread-123".to_string()),
            last_offset: 77,
            relay_last_offset: None,
        })
    );
}

#[test]
fn clears_runtime_binding_by_tmux_session() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_tmux_runtime_binding(
        "tmux-runtime",
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/claude-transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("session-123".to_string()),
            last_offset: 77,
            relay_last_offset: None,
        },
    );

    assert!(runtime_binding_for_tmux_session("tmux-runtime").is_some());
    assert!(clear_tmux_runtime_binding("tmux-runtime"));
    assert!(runtime_binding_for_tmux_session("tmux-runtime").is_none());
    assert!(!clear_tmux_runtime_binding("tmux-runtime"));
    assert!(!clear_tmux_runtime_binding("   "));
}

// #3105 (codex P1 sub-case B): evicting a dead/orphaned mirror must drop BOTH
// the runtime binding (which the idle relay loop iterates) AND the channel
// mirror (which the drift-alert resolver reads), so a subsequent relay pass
// finds no mapping and stops re-emitting the per-poll drift/skip WARN. A
// later legitimate re-registration must still repopulate both maps.
#[test]
fn evict_dead_tmux_mirror_drops_runtime_and_channel_then_allows_reregister() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
    register_tmux_runtime_binding(
        tmux,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/claude-transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 12,
            relay_last_offset: None,
        },
    );
    register_tmux_channel(tmux, 1_504_468_805_772_902_471);
    assert!(runtime_binding_for_tmux_session(tmux).is_some());
    assert_eq!(
        owner_channel_for_tmux_session(tmux),
        Some(1_504_468_805_772_902_471)
    );

    // Eviction removes both mirror maps and reports the change once.
    assert!(evict_dead_tmux_mirror(tmux));
    assert!(
        runtime_binding_for_tmux_session(tmux).is_none(),
        "runtime binding gone → relay loop no longer iterates the dead session"
    );
    assert_eq!(
        owner_channel_for_tmux_session(tmux),
        None,
        "channel mirror gone → drift-alert resolver finds no mapping"
    );
    // Idempotent: a second eviction reports no change (single bounded incident).
    assert!(!evict_dead_tmux_mirror(tmux));
    assert!(!evict_dead_tmux_mirror("   "));

    // A later legitimate re-registration repopulates both maps (session came back).
    register_tmux_runtime_binding(
        tmux,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/claude-transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    register_tmux_channel(tmux, 1_504_468_805_772_902_471);
    assert!(runtime_binding_for_tmux_session(tmux).is_some());
    assert_eq!(
        owner_channel_for_tmux_session(tmux),
        Some(1_504_468_805_772_902_471)
    );
}

#[test]
fn lists_runtime_bindings_by_kind() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_tmux_runtime_binding(
        "tmux-codex",
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/tmp/codex-rollout.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("thread-123".to_string()),
            last_offset: 77,
            relay_last_offset: None,
        },
    );
    register_tmux_runtime_binding(
        "tmux-claude",
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/claude-transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 88,
            relay_last_offset: None,
        },
    );

    assert_eq!(
        runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui),
        vec![(
            "tmux-codex".to_string(),
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: None,
            },
        )]
    );
}

#[test]
fn prompt_anchor_is_consumed_for_matching_tmux_and_channel() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    record_prompt_anchor("Claude", "tmux-anchor", 42, 9001);

    assert_eq!(
        take_prompt_anchor_for_response("claude", "tmux-anchor", 43),
        None
    );
    assert_eq!(
        take_prompt_anchor_for_response("claude", "tmux-anchor", 42),
        Some(TuiPromptAnchor {
            channel_id: 42,
            message_id: 9001,
        })
    );
    assert_eq!(
        take_prompt_anchor_for_response("claude", "tmux-anchor", 42),
        None
    );
}

// #3174: the narrow ordering race — the watcher's lease-gated completion
// fires BEFORE this turn's `record_prompt_anchor` lands (the provider
// committed terminal output inside the `notify-post + ⏳-add` window). The
// anchor-less completion must NOT silently drop the ⏳; it records a deferred
// marker that the SAME turn's late anchor record drains.
//
// This reproduces the EXACT ordering: completion-before-anchor. Before the
// fix `take_deferred_anchor_completion` did not exist and the anchor-less
// completion had nowhere to defer to — the ⏳ was stranded (no later pass,
// because the lease that gated the completion is cleared after delivery).
#[test]
fn deferred_anchor_completion_reconciles_when_anchor_recorded_after_completion() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    // 1) Watcher's lease-gated completion runs; the anchor for THIS turn is
    //    not recorded yet (notify-post + ⏳-add still in flight), so the
    //    anchor lookup the completion does returns None.
    assert_eq!(
        prompt_anchor_for_response("claude", "tmux-anchor", 42),
        None,
        "anchor must not exist yet at completion time (the race window)"
    );
    // The anchor-less completion records a deferred marker (stamped with
    // THIS turn's lease generation) instead of dropping the ⏳.
    let turn_gen = 7_u64;
    record_deferred_anchor_completion("Claude", "tmux-anchor", 42, turn_gen);

    // 2) The late `record_prompt_anchor` lands for the SAME turn. Its site
    //    drains the deferred marker → the relay finishes the ⏳ → ✅ swap.
    record_prompt_anchor("Claude", "tmux-anchor", 42, 9001);
    assert!(
        take_deferred_anchor_completion("claude", "tmux-anchor", turn_gen),
        "late anchor record must drain the deferred completion marker"
    );
    // The anchor is present so the relay's completion can act on it.
    assert_eq!(
        prompt_anchor_for_response("claude", "tmux-anchor", 42),
        Some(TuiPromptAnchor {
            channel_id: 42,
            message_id: 9001,
        }),
    );
    // The marker is single-shot: a second drain is a no-op.
    assert!(
        !take_deferred_anchor_completion("claude", "tmux-anchor", turn_gen),
        "deferred marker must be consumed exactly once"
    );
}

// #3174: the common (non-racing) path records no deferred marker, so the
// late anchor record drains nothing — the relay's reconcile is a no-op and
// the normal watcher completion owns the ⏳ → ✅ swap. Guards against the
// fix double-completing on every turn.
#[test]
fn no_deferred_completion_when_completion_did_not_race_the_anchor() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    // No anchor-less completion happened (provider took the usual seconds),
    // so no marker was recorded.
    record_prompt_anchor("Claude", "tmux-anchor", 42, 9001);
    assert!(
        !take_deferred_anchor_completion("claude", "tmux-anchor", 7),
        "no deferred completion must be drained on the common non-racing path"
    );
}

// #3174 turn-identity safety: a deferred marker is keyed to
// `(provider, tmux)` and must not be drained by a DIFFERENT provider's or a
// different tmux session's anchor record.
#[test]
fn deferred_anchor_completion_is_isolated_by_provider_and_session() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let turn_gen = 11_u64;
    record_deferred_anchor_completion("claude", "tmux-a", 42, turn_gen);

    // Wrong provider: codex must not drain claude's marker.
    assert!(!take_deferred_anchor_completion(
        "codex", "tmux-a", turn_gen
    ));
    // Wrong session: a different tmux must not drain it.
    assert!(!take_deferred_anchor_completion(
        "claude", "tmux-b", turn_gen
    ));
    // The exact key still drains it.
    assert!(take_deferred_anchor_completion(
        "claude", "tmux-a", turn_gen
    ));
}

// #3174 codex P1 (turn-identity isolation): a deferred marker stamped with
// one turn's lease generation must NOT be drained by a DIFFERENT turn on the
// SAME provider/tmux. Without the generation stamp the `(provider, tmux)` key
// alone would let a newer turn within the marker TTL cross-consume the
// previous turn's marker and complete the wrong turn's ⏳ → ✅.
#[test]
fn deferred_anchor_completion_is_not_cross_consumed_by_a_different_turn_same_key() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    // Turn A's anchor-less completion records a marker stamped gen=100.
    let turn_a_gen = 100_u64;
    record_deferred_anchor_completion("claude", "tmux-shared", 42, turn_a_gen);

    // A NEWER turn B on the SAME provider/tmux records its own lease (a
    // different, higher generation) and lands its anchor first. Its drain
    // must NOT consume turn A's marker — generations differ.
    let turn_b_gen = 101_u64;
    assert!(
        !take_deferred_anchor_completion("claude", "tmux-shared", turn_b_gen),
        "a newer turn must not cross-consume the previous turn's deferred marker"
    );
    // peek also reports it as not-present for turn B's identity.
    assert!(
        !deferred_anchor_completion_present_for_turn("claude", "tmux-shared", turn_b_gen),
        "peek must not match a different turn's generation"
    );

    // Turn A's own late anchor record (its matching generation) DOES drain it.
    assert!(
        deferred_anchor_completion_present_for_turn("claude", "tmux-shared", turn_a_gen),
        "peek must match the owning turn's generation"
    );
    assert!(
        take_deferred_anchor_completion("claude", "tmux-shared", turn_a_gen),
        "the owning turn's anchor record must drain its own marker"
    );
}

// #3174 codex P2 (HTTP fail-open): the relay PEEKS before consuming, so it
// can leave the marker intact when command_http is unavailable. Prove peek is
// non-destructive: a peek leaves the marker drainable by a later attempt.
#[test]
fn deferred_anchor_completion_peek_is_non_destructive() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let turn_gen = 55_u64;
    record_deferred_anchor_completion("claude", "tmux-peek", 42, turn_gen);

    // Simulate the HTTP-unavailable relay path: it peeks (marker is owed) but
    // does NOT take, because there is no command_http to deliver the swap.
    assert!(
        deferred_anchor_completion_present_for_turn("claude", "tmux-peek", turn_gen),
        "peek must report the owed marker"
    );
    assert!(
        deferred_anchor_completion_present_for_turn("claude", "tmux-peek", turn_gen),
        "a second peek must still report it (peek does not consume)"
    );

    // A later attempt (HTTP now available) can still drain it — it was not
    // silently lost by the fail-open path.
    assert!(
        take_deferred_anchor_completion("claude", "tmux-peek", turn_gen),
        "the marker survives a peek and remains drainable"
    );
}

#[test]
fn prompt_anchor_can_be_peeked_until_delivery_commits() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let anchor = TuiPromptAnchor {
        channel_id: 42,
        message_id: 9001,
    };
    record_prompt_anchor(
        "Claude",
        "tmux-anchor",
        anchor.channel_id,
        anchor.message_id,
    );

    assert_eq!(
        prompt_anchor_for_response("claude", "tmux-anchor", 42),
        Some(anchor)
    );
    assert_eq!(
        prompt_anchor_for_response("claude", "tmux-anchor", 42),
        Some(anchor)
    );
    assert!(!clear_prompt_anchor_for_response(
        "claude",
        "tmux-anchor",
        TuiPromptAnchor {
            channel_id: 42,
            message_id: 9002,
        },
    ));
    assert!(clear_prompt_anchor_for_response(
        "claude",
        "tmux-anchor",
        anchor,
    ));
    assert_eq!(
        prompt_anchor_for_response("claude", "tmux-anchor", 42),
        None
    );
}

#[test]
fn ssh_direct_observation_marker_is_set_on_publish_and_cleared_with_anchor() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    // No observation yet → the bypass signal must stay false so the
    // post-terminal suppress guard keeps catching ghost output.
    assert!(!is_ssh_direct_observation_pending("claude", "tmux-direct"));

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-direct", "echo direct"),
        PromptObservation::PublishedSshDirect
    );
    // observe → marker is set immediately, before the relay subscriber
    // has even started its Discord notify await. This closes the race
    // window where a very fast TUI response would otherwise hit the
    // watcher with no anchor and get suppressed.
    assert!(is_ssh_direct_observation_pending("claude", "tmux-direct"));

    // Other (provider, tmux) pairs must not see the marker — cc/cdx
    // running side-by-side must not cross-bypass.
    assert!(!is_ssh_direct_observation_pending("codex", "tmux-direct"));
    assert!(!is_ssh_direct_observation_pending("claude", "tmux-other"));

    // Consuming the full anchor (i.e., response delivered to Discord)
    // also clears the pre-anchor marker so subsequent ghost output is
    // again subject to the suppress guard.
    let anchor = TuiPromptAnchor {
        channel_id: 77,
        message_id: 4242,
    };
    record_prompt_anchor(
        "claude",
        "tmux-direct",
        anchor.channel_id,
        anchor.message_id,
    );
    assert!(clear_prompt_anchor_for_response(
        "claude",
        "tmux-direct",
        anchor
    ));
    assert!(!is_ssh_direct_observation_pending("claude", "tmux-direct"));
}

#[test]
fn advances_runtime_binding_offset_for_same_output_path() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_tmux_runtime_binding(
        "tmux-runtime",
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/claude-transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 77,
            relay_last_offset: None,
        },
    );

    assert!(!advance_tmux_runtime_binding_offset(
        "tmux-runtime",
        "/tmp/other.jsonl",
        200
    ));
    assert_eq!(
        runtime_binding_for_tmux_session("tmux-runtime")
            .expect("binding")
            .last_offset,
        77
    );
    assert!(advance_tmux_runtime_binding_offset(
        "tmux-runtime",
        "/tmp/claude-transcript.jsonl",
        200
    ));
    assert_eq!(
        runtime_binding_for_tmux_session("tmux-runtime")
            .expect("binding")
            .last_offset,
        200
    );
}

#[test]
fn advances_runtime_binding_relay_offset_separately_from_runtime_path() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    register_tmux_runtime_binding(
        "tmux-runtime",
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/tmp/codex-rollout.jsonl".to_string(),
            relay_output_path: Some("/tmp/tmux-wrapper.jsonl".to_string()),
            input_fifo_path: None,
            session_id: Some("thread-123".to_string()),
            last_offset: 77,
            relay_last_offset: Some(33),
        },
    );

    assert!(advance_tmux_runtime_binding_offset(
        "tmux-runtime",
        "/tmp/tmux-wrapper.jsonl",
        88
    ));
    let binding = runtime_binding_for_tmux_session("tmux-runtime").expect("binding");
    assert_eq!(binding.last_offset, 77);
    assert_eq!(binding.relay_last_offset, Some(88));
    assert!(!advance_tmux_runtime_binding_offset(
        "tmux-runtime",
        "/tmp/other.jsonl",
        99
    ));
}

#[test]
fn suppresses_trailing_newline_pending_prompt() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    record_discord_originated_prompt("claude", "tmux-a", "hello\n");

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-a", "hello"),
        PromptObservation::SuppressedDiscordDuplicate
    );
}

#[test]
fn suppresses_fuzzy_whitespace_prompt() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    record_discord_originated_prompt("codex", "tmux-b", "Please   inspect\n\nthe failing test");

    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-b", "please inspect the failing test"),
        PromptObservation::SuppressedDiscordDuplicate
    );
}

#[test]
fn candidate_observation_checks_all_pending_forms_before_direct_publish() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    record_discord_originated_prompt("claude", "tmux-c", "hello wrapped prompt");

    assert_eq!(
        observe_prompt_candidates_by_tmux(
            "claude",
            "tmux-c",
            &[
                "hellowrappedprompt".to_string(),
                "hello wrapped prompt".to_string()
            ],
        ),
        PromptObservation::SuppressedDiscordDuplicate
    );
    assert!(
        !external_input_relay_lease_present("claude", "tmux-c", 42),
        "a candidate matching a Discord-origin prompt must not create an ExternalInput lease"
    );
}

#[test]
fn discord_relayed_user_prompt_format_is_recognized_3527() {
    // AgentDesk's own `[User: <author> (ID: <digits>)]` relay lines — author
    // may contain parens; prefix may be followed by a newline (multi-line).
    assert!(is_discord_relayed_user_prompt(
        "[User: 0hbujang (ID: 343742347365974026)] A부턱ㄱ"
    ));
    assert!(is_discord_relayed_user_prompt(
        "[User: Alice (ops) team (ID: 77)] deploy it"
    ));
    assert!(is_discord_relayed_user_prompt(
        "[User: Bob (ID: 5)]\nmultiline\nbody"
    ));
    // genuine external / cron / SSH injections carry no `[User: (ID:)]` prefix
    assert!(!is_discord_relayed_user_prompt(
        "/relay-scan — supervise relays"
    ));
    assert!(!is_discord_relayed_user_prompt(
        "just typed directly via ssh"
    ));
    assert!(!is_discord_relayed_user_prompt("[User: no id here] text"));
    assert!(!is_discord_relayed_user_prompt(
        "[User: x (ID: abc)] non-numeric"
    ));
    assert!(!is_discord_relayed_user_prompt(""));
    // codex #3527: the `[User:]` chunk may be PRECEDED by prepended context
    // ([External Recall], reply/upload context, Codex reuse wrappers) — the
    // marker is not necessarily on the first line, so every line is scanned.
    assert!(is_discord_relayed_user_prompt(
        "[External Recall]\n- prior context\n\n[User: Alice (ID: 77)] deploy it"
    ));
    assert!(is_discord_relayed_user_prompt(
        "[Reply context] ...\n[User: 0hbujang (ID: 343742347365974026)] hi"
    ));
    // codex #3527 r2: the legacy pane observer submits join("")/join(" ")
    // collapsed variants of one block, so the marker can be MID-LINE — the
    // whole-string scan must catch those too, not just the newline variant.
    assert!(is_discord_relayed_user_prompt(
        "[External Recall]- prior context[User: Alice (ID: 77)] deploy it"
    ));
    assert!(is_discord_relayed_user_prompt(
        "[External Recall] - prior context  [User: Alice (ID: 77)] deploy it"
    ));
    // author containing parens, collapsed mid-line
    assert!(is_discord_relayed_user_prompt(
        "ctx [User: Alice (ops) team (ID: 77)] deploy it"
    ));
}

#[test]
fn observe_skips_discord_relayed_user_line_without_ledger_3527() {
    // #3527: a re-observed `[User:]` relay line WITHOUT a discord-originated
    // ledger entry (simulating a quiescence-timeout re-observation after the
    // entry was consumed/expired) must NOT publish an SSH-direct turn and must
    // not record an ExternalInput lease — otherwise it posts a spurious 직접
    // 주입 notice + orphan placeholder panel.
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    assert_eq!(
        observe_prompt_by_tmux(
            "claude",
            "tmux-3527",
            "[User: 0hbujang (ID: 343742347365974026)] A부턱ㄱ"
        ),
        PromptObservation::Ignored
    );
    assert!(
        !external_input_relay_lease_present("claude", "tmux-3527", 42),
        "a [User:] relay re-observation must not create an ExternalInput lease (#3527)"
    );
}

#[test]
fn observe_publishes_user_prefixed_subagent_notification_machine_event_3818() {
    // #3818 regression: Codex subagent completions can be wrapped by
    // Provider Session Reuse and the Discord author prefix before the TUI
    // observer sees them. The #3527 self-relay filter must not swallow these
    // terminal machine events, or the card renderer never gets a chance to
    // hide the raw XML envelope from Discord.
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let prompt = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
[User: 0hbujang (ID: 343742347365974026)] No response requested.\n\
<subagent_notification>{\"agent_path\":\"/tmp/private\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-3818", prompt),
        PromptObservation::PublishedSshDirect,
        "start-anchored subagent_notification must bypass the [User:] duplicate filter"
    );
    assert!(clear_external_input_relay_lease("codex", "tmux-3818", 42));

    let chrome_before_user = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
No response requested.\n\
[User: 0hbujang (ID: 343742347365974026)] \
<subagent_notification>{\"agent_path\":\"/tmp/private\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";
    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-3818-chrome-first", chrome_before_user),
        PromptObservation::PublishedSshDirect,
        "TUI chrome before the Discord author prefix must not re-enable the [User:] duplicate filter"
    );
    assert!(clear_external_input_relay_lease(
        "codex",
        "tmux-3818-chrome-first",
        42
    ));
}

#[test]
fn relay_lease_only_observation_does_not_create_late_prompt_anchor_signal() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    assert_eq!(
        observe_prompt_candidates_by_tmux_for_relay_lease(
            "claude",
            "tmux-lease-only",
            &["typed over ssh".to_string()],
        ),
        PromptObservation::PublishedSshDirect
    );
    assert!(external_input_relay_lease_present(
        "claude",
        "tmux-lease-only",
        42
    ));
    assert!(
        !is_ssh_direct_observation_pending("claude", "tmux-lease-only"),
        "watcher emergency observation must not create a late prompt-anchor signal"
    );
}

#[test]
fn external_input_turn_lease_carries_owner_and_trace_fields() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    record_external_input_turn_lease(
        "codex",
        "tmux-trace",
        ExternalInputRelayLease {
            channel_id: Some(42),
            turn_id: Some("external:codex:42:tmux-trace:123".to_string()),
            session_key: Some("host:tmux-trace".to_string()),
            relay_owner: ExternalInputRelayOwner::SessionBoundRelay,
            runtime_kind: Some(RuntimeHandoffKind::CodexTui),
            generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        },
    );

    let lease = external_input_relay_lease("codex", "tmux-trace", 42).expect("lease");
    assert_eq!(
        lease.turn_id.as_deref(),
        Some("external:codex:42:tmux-trace:123")
    );
    assert_eq!(lease.session_key.as_deref(), Some("host:tmux-trace"));
    assert_eq!(
        lease.relay_owner,
        ExternalInputRelayOwner::SessionBoundRelay
    );
    assert_eq!(lease.relay_owner.as_str(), "session_bound_relay");
    assert_eq!(lease.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
    assert!(external_input_relay_lease("codex", "tmux-trace", 43).is_none());
}

#[test]
fn clear_external_input_relay_lease_if_matches_preserves_newer_turn() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let original = ExternalInputRelayLease {
        channel_id: Some(42),
        turn_id: Some("external:codex:42:tmux-trace:1".to_string()),
        session_key: Some("host:tmux-trace".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::CodexTui),
        generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let newer = ExternalInputRelayLease {
        turn_id: Some("external:codex:42:tmux-trace:2".to_string()),
        ..original.clone()
    };

    // Capture the RECORDED leases (each stamped with a distinct generation) —
    // those are the exact identities `_if_matches` compares against.
    let recorded_original =
        record_external_input_turn_lease("codex", "tmux-trace", original.clone());
    let recorded_newer = record_external_input_turn_lease("codex", "tmux-trace", newer.clone());
    assert_ne!(
        recorded_original.generation, recorded_newer.generation,
        "each recorded lease must get a distinct generation"
    );

    // The OLD recorded lease no longer matches the CURRENT (newer) one.
    assert!(!clear_external_input_relay_lease_if_matches(
        "codex",
        "tmux-trace",
        42,
        &recorded_original
    ));
    assert_eq!(
        external_input_relay_lease("codex", "tmux-trace", 42),
        Some(recorded_newer.clone())
    );
    assert!(clear_external_input_relay_lease_if_matches(
        "codex",
        "tmux-trace",
        42,
        &recorded_newer
    ));
    assert!(external_input_relay_lease("codex", "tmux-trace", 42).is_none());
}

#[test]
fn clear_external_input_relay_lease_if_generation_matches_preserves_newer_unassigned() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    // Two value-identical Unassigned leases (all trace fields None) for the same
    // key receive DISTINCT generations.
    record_external_input_relay_lease("codex", "tmux-gen", Some(99));
    let first = external_input_relay_lease("codex", "tmux-gen", 99).expect("first lease");
    record_external_input_relay_lease("codex", "tmux-gen", Some(99));
    let second = external_input_relay_lease("codex", "tmux-gen", 99).expect("second lease");

    assert_eq!(first.relay_owner, ExternalInputRelayOwner::Unassigned);
    assert_eq!(second.relay_owner, ExternalInputRelayOwner::Unassigned);
    assert_ne!(
        first.generation, second.generation,
        "two Unassigned leases for the same key must get distinct generations"
    );

    // Clearing by the OLD generation must NOT clear the newer lease.
    assert!(!clear_external_input_relay_lease_if_generation_matches(
        "codex",
        "tmux-gen",
        99,
        first.generation
    ));
    assert_eq!(
        external_input_relay_lease("codex", "tmux-gen", 99),
        Some(second.clone()),
        "the newer Unassigned lease must survive a clear by the old generation"
    );

    // The UNRECORDED sentinel generation clears nothing.
    assert!(!clear_external_input_relay_lease_if_generation_matches(
        "codex",
        "tmux-gen",
        99,
        EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED
    ));

    // Clearing by the CURRENT generation clears exactly it.
    assert!(clear_external_input_relay_lease_if_generation_matches(
        "codex",
        "tmux-gen",
        99,
        second.generation
    ));
    assert!(external_input_relay_lease("codex", "tmux-gen", 99).is_none());
}

/// Watcher-style no-clobber: turn-1 snapshots the lease generation G1 before its
/// awaited send; turn-2 records a NEWER same-key lease G2 during that send; turn-1
/// then clears BY G1 — which must NOT remove turn-2's G2 lease. The snapshot is taken
/// from a single `external_input_relay_lease` read (the watcher derives both the
/// presence bool and the generation from that one atomic read).
#[test]
fn watcher_snapshot_generation_clear_preserves_newer_same_key_lease() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    // turn-1 records & the watcher snapshots its generation BEFORE the awaited send.
    record_external_input_relay_lease("codex", "tmux-watch", Some(7));
    let g1 = external_input_relay_lease("codex", "tmux-watch", 7)
        .map(|lease| lease.generation)
        .expect("turn-1 generation snapshot");

    // turn-2 records a NEWER same-key lease while turn-1's send is in flight.
    record_external_input_relay_lease("codex", "tmux-watch", Some(7));
    let g2 = external_input_relay_lease("codex", "tmux-watch", 7)
        .map(|lease| lease.generation)
        .expect("turn-2 generation");
    assert_ne!(
        g1, g2,
        "the newer same-key lease must get a distinct generation"
    );

    // turn-1's post-send clear BY G1 must be a no-op (G1 != current G2).
    assert!(
        !clear_external_input_relay_lease_if_generation_matches("codex", "tmux-watch", 7, g1),
        "clear by the stale G1 snapshot must not match the current G2 lease"
    );
    assert_eq!(
        external_input_relay_lease("codex", "tmux-watch", 7).map(|lease| lease.generation),
        Some(g2),
        "turn-2's lease must survive turn-1's stale-snapshot clear (no clobber)"
    );
}

#[test]
fn legacy_external_input_relay_lease_defaults_to_unassigned_owner() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    record_external_input_relay_lease("claude", "tmux-legacy", Some(7));

    let lease = external_input_relay_lease("claude", "tmux-legacy", 7).expect("lease");
    assert_eq!(lease.channel_id, Some(7));
    assert_eq!(lease.turn_id, None);
    assert_eq!(lease.session_key, None);
    assert_eq!(lease.relay_owner, ExternalInputRelayOwner::Unassigned);
    assert_eq!(lease.runtime_kind, None);
}

#[test]
fn merged_draft_does_not_suppress_pending_discord_prompt() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    record_discord_originated_prompt("codex", "tmux-b", "[TUI-REL-OLD] respond with marker");

    assert_eq!(
        observe_prompt_by_tmux(
            "codex",
            "tmux-b",
            "[TUI-REL-OLD] respond with marker [TUI-REL-NEW] respond with marker",
        ),
        PromptObservation::PublishedSshDirect
    );
}

#[test]
fn expired_pending_prompt_publishes_as_direct_input() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    record_discord_originated_prompt("claude", "tmux-a", "hello");
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        let queue = state
            .pending_by_tmux
            .get_mut(&PromptKey::new("claude", "tmux-a"))
            .expect("pending prompt queue");
        queue.front_mut().expect("pending prompt").recorded_at =
            Instant::now() - PENDING_PROMPT_TTL - Duration::from_secs(1);
    }

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-a", "hello"),
        PromptObservation::PublishedSshDirect
    );
}

#[test]
fn removed_prompt_after_submit_failure_publishes_as_direct_input() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    record_discord_originated_prompt("codex", "tmux-b", "failed submit");
    remove_discord_originated_prompt("codex", "tmux-b", "failed submit");

    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-b", "failed submit"),
        PromptObservation::PublishedSshDirect
    );
}

#[test]
fn publishes_unmatched_prompt() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-a", "typed over ssh"),
        PromptObservation::PublishedSshDirect
    );
    assert!(
        external_input_relay_lease_present("claude", "tmux-a", 42),
        "prompt observation creates a relay lease before Discord notification/anchor succeeds"
    );
    assert!(clear_external_input_relay_lease("claude", "tmux-a", 42));
    assert!(!external_input_relay_lease_present("claude", "tmux-a", 42));
}

#[test]
fn local_only_control_creates_no_external_turn_effects_without_a_subscriber() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-effect-generation", "/compact"),
        PromptObservation::PublishedSshDirect
    );
    assert!(!external_input_relay_lease_present(
        "claude",
        "tmux-effect-generation",
        42
    ));
    assert!(!is_ssh_direct_observation_pending(
        "claude",
        "tmux-effect-generation"
    ));
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let key = PromptKey::new("claude", "tmux-effect-generation");
    assert!(
        !state.recent_observed_by_tmux.contains_key(&key),
        "local controls must bypass the 30-second direct-input tombstone"
    );
    assert!(
        !state.pending_by_tmux.contains_key(&key),
        "local controls must not create a Discord-originated pending entry"
    );
}

#[test]
fn local_compact_entry_id_is_recorded_only_after_a_successful_note_delivery() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let now = Utc::now();

    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-local-compact",
            "/compact",
            Some("compact-entry-1"),
            now,
        ),
        PromptObservation::PublishedSshDirect,
    );
    // This test deliberately has no subscriber. A broadcast miss, absent
    // owner/channel/http, or Discord send error never calls the delivery
    // acknowledgement helper, so an exact later replay must still publish.
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-local-compact",
            "/compact",
            Some("compact-entry-1"),
            now,
        ),
        PromptObservation::PublishedSshDirect,
        "without a confirmed note delivery, an exact local entry replay is not suppressed"
    );

    // The relay calls this only from the successful `channel.say` branch.
    record_local_only_entry_id_after_note_delivery(&ObservedTuiPrompt {
        provider: "claude".to_string(),
        tmux_session_name: "tmux-local-compact".to_string(),
        prompt: "/compact".to_string(),
        source_event_id: Some("compact-entry-1".to_string()),
        observed_at: now,
        external_input_lease_generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation: SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
    });
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-local-compact",
            "/compact",
            Some("compact-entry-1"),
            now,
        ),
        PromptObservation::SuppressedReplayedEntry,
        "only a successfully delivered note records the exact local entry identity"
    );
    assert!(!external_input_relay_lease_present(
        "claude",
        "tmux-local-compact",
        42
    ));
    assert!(!is_ssh_direct_observation_pending(
        "claude",
        "tmux-local-compact"
    ));
}

#[test]
fn local_compact_raw_and_envelope_each_publish_without_time_pairing() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let wrapper = "<command-message>compact</command-message>\n\
                   <command-name>/compact</command-name>\n\
                   <command-args></command-args>";

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-local-pair", "/compact"),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-local-pair", wrapper),
        PromptObservation::PublishedSshDirect,
        "the transcript envelope is allowed to duplicate the raw local note"
    );
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-local-pair", "/compact"),
        PromptObservation::PublishedSshDirect,
        "a later human /compact is never collapsed by a text/time pair window"
    );
}

#[test]
fn local_note_delivery_ack_does_not_record_nonlocal_entries() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let now = Utc::now();
    let nonlocal = ObservedTuiPrompt {
        provider: "claude".to_string(),
        tmux_session_name: "tmux-local-ack-scope".to_string(),
        prompt: "normal human prompt".to_string(),
        source_event_id: Some("nonlocal-entry".to_string()),
        observed_at: now,
        external_input_lease_generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation: SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
    };

    record_local_only_entry_id_after_note_delivery(&nonlocal);
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-local-ack-scope",
            "normal human prompt",
            Some("nonlocal-entry"),
            now,
        ),
        PromptObservation::PublishedSshDirect,
        "the local-delivery acknowledgement path cannot alter generic entry-id semantics"
    );
}

#[test]
fn task_notification_is_status_only_and_next_prompt_keeps_lease_free() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let task = "<task-notification><status>killed</status><task-id>stop-1</task-id></task-notification>";
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-stop-task", task),
        PromptObservation::PublishedTaskNotification
    );
    assert!(
        !external_input_relay_lease_present("claude", "tmux-stop-task", 42),
        "killed task status must not create an external-input lease"
    );
    assert!(!is_ssh_direct_observation_pending(
        "claude",
        "tmux-stop-task"
    ));
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-stop-task", "the next real prompt"),
        PromptObservation::PublishedSshDirect
    );
    assert!(external_input_relay_lease_present(
        "claude",
        "tmux-stop-task",
        42
    ));
}

#[test]
fn ignores_synthetic_context_prompt_without_relay_lease() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    assert_eq!(
        observe_prompt_by_tmux(
            "codex",
            "tmux-c",
            "<environment_context>\n  <cwd>/tmp/project</cwd>\n</environment_context>",
        ),
        PromptObservation::Ignored
    );
    assert!(
        !external_input_relay_lease_present("codex", "tmux-c", 42),
        "bootstrap context must not create an SSH-direct relay lease"
    );
}

#[test]
fn ignores_claude_interrupt_marker_without_relay_lease() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-stop", "[Request interrupted by user]"),
        PromptObservation::Ignored
    );
    assert!(
        !external_input_relay_lease_present("claude", "tmux-stop", 42),
        "a stop-control transcript marker must not create an SSH-direct relay lease"
    );
}

#[test]
fn interrupt_marker_filter_is_claude_scoped_for_direct_observation() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let marker = "[Request interrupted by user]";

    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-codex-stop-text", marker),
        PromptObservation::PublishedSshDirect,
        "Codex direct input with the same text remains a user prompt"
    );
    assert!(clear_external_input_relay_lease(
        "codex",
        "tmux-codex-stop-text",
        42
    ));

    assert_eq!(
        observe_prompt_by_tmux("qwen", "tmux-qwen-stop-text", marker),
        PromptObservation::PublishedSshDirect,
        "Qwen direct input with the same text remains a user prompt"
    );
    assert!(clear_external_input_relay_lease(
        "qwen",
        "tmux-qwen-stop-text",
        42
    ));
}

#[test]
fn external_input_relay_lease_can_be_bound_to_channel_after_observation() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    observe_prompt_by_tmux("claude", "tmux-a", "typed over ssh");
    record_external_input_relay_lease("claude", "tmux-a", Some(42));

    assert!(external_input_relay_lease_present("claude", "tmux-a", 42));
    assert!(!external_input_relay_lease_present("claude", "tmux-a", 43));
}

#[test]
fn suppresses_recent_direct_duplicate_prompt() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-c", "typed over ssh"),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-c", "typed over ssh\n"),
        PromptObservation::SuppressedRecentDuplicate
    );
}

#[test]
fn suppresses_recent_slash_command_xml_and_invocation_forms() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let wrapper = "<command-message>loop</command-message>\n\
                   <command-name>/loop</command-name>\n\
                   <command-args>check relay gaps every 30m</command-args>";

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", wrapper),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", "/loop  check relay gaps every 30m"),
        PromptObservation::SuppressedRecentDuplicate
    );
}

#[test]
fn slash_command_dedupe_does_not_collapse_raw_args_or_other_commands() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let wrapper = "<command-message>loop</command-message>\n\
                   <command-name>/loop</command-name>\n\
                   <command-args>check relay gaps every 30m</command-args>";

    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", wrapper),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", "check relay gaps every 30m"),
        PromptObservation::PublishedSshDirect,
        "a real raw prompt matching only command args must not be dropped"
    );
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", "/model check relay gaps every 30m"),
        PromptObservation::PublishedSshDirect,
        "a different slash command with the same args is a new submission"
    );
}

#[test]
fn pending_match_leaves_recent_tombstone_for_second_observer() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    record_discord_originated_prompt("codex", "tmux-c", "from discord");

    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-c", "from discord"),
        PromptObservation::SuppressedDiscordDuplicate
    );
    assert_eq!(
        observe_prompt_by_tmux("codex", "tmux-c", "from discord"),
        PromptObservation::SuppressedRecentDuplicate
    );
}

#[test]
fn extracts_codex_rollout_user_message_text() {
    let json = serde_json::json!({
        "type": "response_item",
        "payload": {
            "id": "codex-entry-1",
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": "hello" },
                { "type": "input_text", "text": "world" }
            ]
        }
    });

    assert_eq!(
        extract_codex_rollout_user_prompt(&json).as_deref(),
        Some("hello\nworld")
    );
    let (prompt, entry_id) =
        extract_codex_rollout_user_prompt_with_entry_id(&json).expect("codex user prompt");
    assert_eq!(prompt, "hello\nworld");
    assert_eq!(entry_id.as_deref(), Some("codex-entry-1"));
}

#[test]
fn extracts_codex_rollout_top_level_entry_id() {
    let json = serde_json::json!({
        "type": "response_item",
        "id": "codex-top-entry",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": "hello from codex" }
            ]
        }
    });

    let (prompt, entry_id) =
        extract_codex_rollout_user_prompt_with_entry_id(&json).expect("codex user prompt");
    assert_eq!(prompt, "hello from codex");
    assert_eq!(entry_id.as_deref(), Some("codex-top-entry"));
}

#[test]
fn codex_distinct_message_entry_ids_publish_distinct_direct_prompts() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let first = serde_json::json!({
        "type": "response_item",
        "id": "codex-turn-container",
        "payload": {
            "id": "codex-message-entry-1",
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "first direct prompt" }]
        }
    });
    let second = serde_json::json!({
        "type": "response_item",
        "id": "codex-turn-container",
        "payload": {
            "id": "codex-message-entry-2",
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "second direct prompt" }]
        }
    });
    let (first_prompt, first_entry_id) =
        extract_codex_rollout_user_prompt_with_entry_id(&first).expect("first codex prompt");
    let (second_prompt, second_entry_id) =
        extract_codex_rollout_user_prompt_with_entry_id(&second).expect("second codex prompt");
    assert_eq!(first_entry_id.as_deref(), Some("codex-message-entry-1"));
    assert_eq!(second_entry_id.as_deref(), Some("codex-message-entry-2"));

    let now = Utc::now();
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "codex",
            "tmux-codex-distinct-ids",
            &first_prompt,
            first_entry_id.as_deref(),
            now,
        ),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "codex",
            "tmux-codex-distinct-ids",
            &second_prompt,
            second_entry_id.as_deref(),
            now,
        ),
        PromptObservation::PublishedSshDirect,
        "distinct Codex message item ids must not collapse separate direct prompts \
         even when the top-level response_item id is shared"
    );
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "codex",
            "tmux-codex-distinct-ids",
            &first_prompt,
            first_entry_id.as_deref(),
            now,
        ),
        PromptObservation::SuppressedReplayedEntry,
        "only the exact already-relayed Codex message item id is replay-suppressed"
    );
}

#[test]
fn ignores_codex_rollout_environment_context_user_message() {
    let json = serde_json::json!({
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [
                {
                    "type": "input_text",
                    "text": "<environment_context>\n  <cwd>/tmp/project</cwd>\n</environment_context>"
                }
            ]
        }
    });

    assert_eq!(extract_codex_rollout_user_prompt(&json), None);
}

#[test]
fn codex_and_qwen_keep_claude_interrupt_text_as_user_prompt() {
    let marker = "[Request interrupted by user]";
    let codex_json = serde_json::json!({
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [
                {
                    "type": "input_text",
                    "text": marker
                }
            ]
        }
    });
    let qwen_json = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                { "type": "text", "text": marker }
            ]
        }
    });

    assert_eq!(
        extract_codex_rollout_user_prompt(&codex_json).as_deref(),
        Some(marker)
    );
    assert_eq!(
        extract_qwen_jsonl_user_prompt(&qwen_json).as_deref(),
        Some(marker)
    );
}

#[test]
fn extracts_claude_transcript_user_message_text() {
    let json = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" }
            ]
        },
        "sessionId": "sess-tui",
    });

    assert_eq!(
        extract_claude_transcript_user_prompt(&json).as_deref(),
        Some("hello\nworld")
    );
}

// Live #3304 reproduction. The duplicate did NOT come from the isMeta
// skill-expansion entry (that is already filtered to None below): the two
// observation paths see ASYMMETRIC text for one submission — the hook path
// records the raw `/loop <args>` invocation echo a ScheduleWakeup writes
// into the terminal, while the idle transcript relay later extracts the
// string-content `<command-*>` wrapper entry. Before the slash canonical
// key their fuzzy keys diverged and the wrapper published a second
// synthetic turn (2026-06-11 05:15 incident).
#[test]
fn suppresses_transcript_command_xml_after_raw_invocation_echo() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let command_args = "매 주기마다: (1) sonnet 모델 서브에이전트를 스폰해 \
        **adk-cc 채널(1479671298497183835)만** 조사시키고 보고받는다";

    // 1st observation (hook path): raw invocation echo, published normally.
    let invocation_echo = format!("/loop {command_args}");
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", &invocation_echo),
        PromptObservation::PublishedSshDirect
    );

    // 2nd observation (idle transcript relay): the same submission as a
    // command-XML wrapper. Without the slash canonical key this fuzzy-
    // mismatched the echo and published a duplicate synthetic turn.
    let wrapper = format!(
        "<command-message>loop</command-message>\n\
         <command-name>/loop</command-name>\n\
         <command-args>{command_args}</command-args>"
    );
    let command_json = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": wrapper,
        },
        "timestamp": "2026-06-10T20:15:20.334Z",
    });
    let prompt = extract_claude_transcript_user_prompt(&command_json).expect("command prompt");
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", &prompt),
        PromptObservation::SuppressedRecentDuplicate,
        "#3304: the XML wrapper form must attribute to the raw invocation echo"
    );

    // The isMeta:true skill-expansion entry is machine context and never
    // reaches dedupe at all (pre-existing filter, unrelated to the bug).
    let skill_expansion_json = serde_json::json!({
        "type": "user",
        "isMeta": true,
        "message": {
            "role": "user",
            "content": [{
                "type": "text",
                "text": format!(
                    "# /loop — schedule a recurring or self-paced prompt\n\n\
                     Parse the input below into `[interval] <prompt…>` and schedule it.\n\n\
                     ## Input\n\n{command_args}"
                ),
            }],
        },
        "timestamp": "2026-06-10T20:15:20.334Z",
    });
    assert_eq!(
        extract_claude_transcript_user_prompt(&skill_expansion_json),
        None,
        "Claude records slash-command skill expansion as isMeta=true machine context"
    );
}

#[test]
fn ignores_claude_transcript_meta_user_message_text() {
    let json = serde_json::json!({
        "type": "user",
        "isMeta": true,
        "message": {
            "role": "user",
            "content": [
                { "type": "text", "text": "_" }
            ]
        },
        "sessionId": "sess-tui",
    });

    assert_eq!(extract_claude_transcript_user_prompt(&json), None);
}

#[test]
fn ignores_claude_transcript_interrupt_marker_user_message_text() {
    for marker in [
        "[Request interrupted by user]",
        "[Request interrupted by user for tool use]",
    ] {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": marker }
                ]
            },
            "sessionId": "sess-tui",
        });

        assert_eq!(
            extract_claude_transcript_user_prompt(&json),
            None,
            "interrupt marker {marker:?} is control output, not external input"
        );
    }

    let user_prompt = "[Request interrupted by user story idea]";
    let json = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                { "type": "text", "text": user_prompt }
            ]
        },
        "sessionId": "sess-tui",
    });

    assert_eq!(
        extract_claude_transcript_user_prompt(&json).as_deref(),
        Some(user_prompt),
        "nearby human text must not be filtered by prefix"
    );
}

#[test]
fn extracts_non_meta_claude_array_user_message_after_slash_command() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let wrapper = "<command-message>loop</command-message>\n\
                   <command-name>/loop</command-name>\n\
                   <command-args>check relay gaps every 30m</command-args>";
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", wrapper),
        PromptObservation::PublishedSshDirect
    );

    let json = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                { "type": "text", "text": "fresh array prompt" },
                { "type": "text", "text": "with attachment context" }
            ],
        },
        "sessionId": "sess-tui",
    });
    let prompt = extract_claude_transcript_user_prompt(&json).expect("array prompt");

    assert_eq!(prompt, "fresh array prompt\nwith attachment context");
    assert_eq!(
        observe_prompt_by_tmux("claude", "tmux-loop", &prompt),
        PromptObservation::PublishedSshDirect,
        "non-command array user content remains a real user submission"
    );
}

#[test]
fn extracts_qwen_jsonl_user_message_text() {
    let json = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" }
            ]
        }
    });

    assert_eq!(
        extract_qwen_jsonl_user_prompt(&json).as_deref(),
        Some("hello\nworld")
    );
}

#[test]
fn ignores_qwen_tool_result_user_messages() {
    let json = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "content": "done",
                "is_error": false
            }]
        }
    });

    assert_eq!(extract_qwen_jsonl_user_prompt(&json), None);
}

// ----------------------------------------------------------------------
// #3540: stable JSONL entry-id (uuid) dedup — root-cause prevention of the
// phantom synthetic inflight on watermark reset / jsonl head rotation.
// ----------------------------------------------------------------------

/// #3540: the SAME entry uuid observed twice (the watermark-reset re-scan)
/// publishes once and is then suppressed by IDENTITY — so the second sighting
/// never mints a synthetic turn. This is the bound the 30s content window
/// could not provide.
#[test]
fn replayed_entry_id_is_suppressed_on_second_observe() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let now = Utc::now();

    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "deploy to make=live",
            Some("uuid-A"),
            now,
        ),
        PromptObservation::PublishedSshDirect,
        "first sighting of a fresh entry relays normally"
    );
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "deploy to make=live",
            Some("uuid-A"),
            now,
        ),
        PromptObservation::SuppressedReplayedEntry,
        "the SAME entry uuid re-encountered (watermark reset / head rotation) \
         is suppressed by identity — no phantom synthetic inflight (#3540)"
    );
}

/// #3540 regression guard (#3459/#3303): a genuinely NEW prompt carries a NEW
/// uuid (Claude Code issues one at type time), so it is NEVER suppressed by
/// the entry-id ledger — missed-prompt regression cannot recur.
#[test]
fn new_entry_id_is_never_suppressed() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let now = Utc::now();

    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "first prompt",
            Some("uuid-1"),
            now,
        ),
        PromptObservation::PublishedSshDirect
    );
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "second prompt",
            Some("uuid-2"),
            now,
        ),
        PromptObservation::PublishedSshDirect,
        "a distinct entry uuid carrying distinct content is a distinct \
         submission — always relayed (#3459/#3303 missed-prompt regression \
         guard). The entry-id ledger only suppresses a RE-ENCOUNTER of the \
         EXACT same uuid; a new uuid never collides."
    );
    // A THIRD distinct prompt under a THIRD uuid also relays — the ledger
    // does not accumulate false suppressions across genuinely new prompts.
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "third prompt",
            Some("uuid-3"),
            now,
        ),
        PromptObservation::PublishedSshDirect,
        "each genuinely new prompt (new uuid + new content) keeps relaying"
    );
}

/// #3540: `entry_id == None` (uuid missing / non-Claude provider) falls back
/// to the pre-#3540 content-keyed 30s recent-observed dedup — no behavior
/// change, no functional regression.
#[test]
fn missing_entry_id_falls_back_to_content_dedup() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let now = Utc::now();

    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "no-uuid prompt",
            None,
            now,
        ),
        PromptObservation::PublishedSshDirect
    );
    // Same content again with no uuid → the content-keyed recent dedup
    // suppresses it (the existing 30s path), NOT the entry-id path.
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "no-uuid prompt",
            None,
            now,
        ),
        PromptObservation::SuppressedRecentDuplicate,
        "with no stable id the legacy content-keyed dedup still applies"
    );
}

/// #3540: a candidate suppressed by the recent-duplicate path must NOT be
/// recorded in the entry-id ledger as 'relayed' — only an ACTUAL relay
/// records the id. (Recording on a dedup-suppressed sighting would be a
/// false 'seen', a subtle correctness bug.)
#[test]
fn dedup_suppressed_candidate_does_not_record_entry_id() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();
    let now = Utc::now();

    // A discord-originated pending prompt is queued first.
    record_discord_originated_prompt("claude", "tmux-3540", "queued prompt");
    // The transcript scanner then observes the SAME text (with a uuid) — it is
    // suppressed as a Discord duplicate, NOT relayed-as-SSH-direct.
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "queued prompt",
            Some("uuid-D"),
            now,
        ),
        PromptObservation::SuppressedDiscordDuplicate
    );
    // Because that sighting was dedup-suppressed (not a real SSH-direct
    // relay), its uuid was NOT recorded. A later genuine SSH-direct sighting
    // of a DIFFERENT prompt under that same uuid would still relay — proving
    // the ledger was not poisoned. (We assert the simpler invariant: the same
    // uuid under fresh content publishes, i.e. is not falsely pre-suppressed.)
    assert_eq!(
        observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            "tmux-3540",
            "different fresh text",
            Some("uuid-D"),
            now,
        ),
        PromptObservation::PublishedSshDirect,
        "a uuid seen only on a dedup-suppressed sighting was not recorded as \
         relayed, so it does not falsely suppress a later real relay (#3540)"
    );
}

/// #3540: purge_expired drops entry ids older than PROMPT_ANCHOR_TTL so the
/// ledger cannot grow without bound; a re-encounter after purge relays again
/// (correct — the watermark-reset window is far shorter than the 30min TTL).
#[test]
fn relayed_entry_id_ledger_purges_after_ttl() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    record_relayed_entry_id("claude", "tmux-3540", "uuid-T");
    assert!(relayed_entry_id_already_seen(
        "claude",
        "tmux-3540",
        "uuid-T"
    ));

    // Force the recorded id to look older than the TTL, then purge.
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(queue) = state
            .relayed_entry_ids_by_tmux
            .get_mut(&PromptKey::new("claude", "tmux-3540"))
        {
            for entry in queue.iter_mut() {
                entry.recorded_at =
                    Instant::now() - (PROMPT_ANCHOR_TTL + Duration::from_secs(1));
            }
        }
        state.purge_expired();
    }
    assert!(
        !relayed_entry_id_already_seen("claude", "tmux-3540", "uuid-T"),
        "entry ids older than PROMPT_ANCHOR_TTL are purged (bounded growth)"
    );
}

/// #3885 follow-up: a long streaming turn's prompt anchor must survive past
/// the legacy 30min purge so the bridge same-input correlation peek still
/// resolves mid-stream (and the #3885 no-response requeue does NOT re-fire a
/// duplicate). An anchor aged beyond the new 4h ceiling is still purged so the
/// idle-pane bound stays bounded. Decoupled from the relayed-entry ledger,
/// which keeps the 30min `PROMPT_ANCHOR_TTL`.
#[test]
fn prompt_anchor_survives_long_streaming_turn_past_legacy_30min_ttl() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let tmux = "tmux-3885-longstream";
    let channel = 7777_u64;
    let streaming_msg = 8_888_u64;

    // An anchor stamped at submit for a turn that has now been streaming 31min
    // (> the legacy 30min purge, < the new 4h ceiling) must STILL resolve.
    record_prompt_anchor_aged_for_tests(
        "claude",
        tmux,
        channel,
        streaming_msg,
        Duration::from_secs(31 * 60),
    );
    assert_eq!(
        prompt_anchor_for_response("claude", tmux, channel),
        Some(TuiPromptAnchor {
            channel_id: channel,
            message_id: streaming_msg,
        }),
        "anchor for a 31min-streaming turn must survive the legacy 30min purge"
    );
    // Sanity: that age is past the OLD 30min TTL (so the win is real) but
    // within the NEW 4h ceiling.
    assert!(Duration::from_secs(31 * 60) > PROMPT_ANCHOR_TTL);
    assert!(Duration::from_secs(31 * 60) < PROMPT_ANCHOR_SUBMIT_TTL);

    // Beyond the 4h ceiling the anchor is purged (bounded idle-pane lifetime).
    record_prompt_anchor_aged_for_tests(
        "claude",
        tmux,
        channel,
        streaming_msg,
        PROMPT_ANCHOR_SUBMIT_TTL + Duration::from_secs(1),
    );
    assert_eq!(
        prompt_anchor_for_response("claude", tmux, channel),
        None,
        "anchor older than PROMPT_ANCHOR_SUBMIT_TTL is purged"
    );
}

/// #3956: re-stamp-on-activity. A turn that streams continuously LONGER than
/// `PROMPT_ANCHOR_SUBMIT_TTL` (4h) must keep a live submit anchor — the watcher
/// calls `touch_prompt_anchor_on_activity` on every observed streamed chunk,
/// advancing `recorded_at` so the anchor never reaches the 4h purge mid-stream.
/// This keeps the #3885 same-input correlation peek resolving for the whole
/// turn (no duplicate-prose requeue), making the correlation TTL-independent.
#[test]
fn streaming_activity_restamps_anchor_so_long_turn_never_loses_it() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let provider = "claude";
    let tmux = "tmux-3956-restamp";
    let channel = 4444_u64;
    let msg = 5_555_u64;

    // The turn has streamed for nearly the whole 4h ceiling.
    record_prompt_anchor_aged_for_tests(
        provider,
        tmux,
        channel,
        msg,
        PROMPT_ANCHOR_SUBMIT_TTL - Duration::from_secs(60),
    );
    // Control: WITHOUT a refresh, a turn that has streamed past the 4h ceiling
    // already loses its anchor (the #3885 residual this fix closes). Pinned on
    // a SEPARATE key so the refreshed-path assertions below are uncontaminated.
    record_prompt_anchor_aged_for_tests(
        provider,
        "tmux-3956-norefresh",
        channel,
        msg,
        PROMPT_ANCHOR_SUBMIT_TTL + Duration::from_secs(1),
    );
    assert_eq!(
        prompt_anchor_for_response(provider, "tmux-3956-norefresh", channel),
        None,
        "without re-stamp, a >4h stream's anchor is purged (the #3885 residual)"
    );

    // Observed streaming activity re-stamps `recorded_at` to ~now.
    assert!(
        touch_prompt_anchor_on_activity(provider, tmux, channel),
        "an existing anchor for this channel is re-stamped on activity"
    );

    // Simulate ANOTHER (4h - 60s) of continuous streaming elapsing AFTER that
    // re-stamp by backdating the refreshed stamp. Because the re-stamp reset the
    // clock, the effective age is now (4h - 60s) < 4h, so the anchor STILL
    // resolves — whereas the un-refreshed control above (~8h wall-age) was purged.
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state
            .prompt_anchor_by_tmux
            .get_mut(&PromptKey::new(provider, tmux))
            .expect("anchor present after touch")
            .recorded_at =
            Instant::now() - (PROMPT_ANCHOR_SUBMIT_TTL - Duration::from_secs(60));
    }
    assert_eq!(
        prompt_anchor_for_response(provider, tmux, channel),
        Some(TuiPromptAnchor {
            channel_id: channel,
            message_id: msg,
        }),
        "re-stamped anchor survives well past the wall-clock 4h a single stamp would not"
    );

    // Channel-scoped: a touch for a DIFFERENT channel must not refresh this anchor.
    assert!(
        !touch_prompt_anchor_on_activity(provider, tmux, channel + 1),
        "touch is a no-op when the stored anchor's channel does not match"
    );
    // Refresh-only: a touch with no anchor recorded must NOT create one.
    assert!(
        !touch_prompt_anchor_on_activity(provider, "tmux-3956-absent", channel),
        "touch never CREATES an anchor — refresh-on-activity only"
    );
}

/// #3956 codex re-review regression guard: `touch_prompt_anchor_on_activity`
/// is a SINGLE-MAP op — it must NOT run the global `purge_expired`, so it can
/// neither scan nor mutate the #3459/#3303 `relayed_entry_ids_by_tmux` ledger
/// (nor any other dedupe map) on the per-chunk hot path. Proven by leaving a
/// ledger entry that a full purge WOULD drop in place ACROSS a touch: it
/// survives the touch byte-for-byte, demonstrating the touch did not trigger
/// the ledger-purging code at all. The ledger still purges on its OWN 30min
/// `PROMPT_ANCHOR_TTL` via the normal (purge-running) paths.
#[test]
fn touch_anchor_on_activity_does_not_run_global_purge_or_touch_ledger() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let provider = "claude";
    let tmux = "tmux-3956-ledger";
    let channel = 5555_u64;
    let msg = 6_666_u64;

    record_relayed_entry_id(provider, tmux, "uuid-LEDGER");
    record_prompt_anchor(provider, tmux, channel, msg);

    // Age the ledger entry PAST its 30min TTL so a full `purge_expired` WOULD
    // drop it; the anchor stays fresh (well within 4h). Done via direct state
    // access so no purge-calling helper runs between here and the touch below.
    // Capture the ledger stamp to prove `touch` leaves it byte-for-byte intact.
    let ledger_stamp_before = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        let aged = Instant::now() - (PROMPT_ANCHOR_TTL + Duration::from_secs(60));
        state
            .relayed_entry_ids_by_tmux
            .get_mut(&PromptKey::new(provider, tmux))
            .and_then(|queue| queue.front_mut())
            .expect("ledger entry present")
            .recorded_at = aged;
        aged
    };

    // Streaming activity re-stamps the SUBMIT anchor (single-map op).
    assert!(touch_prompt_anchor_on_activity(provider, tmux, channel));

    {
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        // The anchor was refreshed to ~now...
        let anchor_age = state
            .prompt_anchor_by_tmux
            .get(&PromptKey::new(provider, tmux))
            .map(|entry| entry.recorded_at.elapsed())
            .expect("anchor present");
        assert!(
            anchor_age < Duration::from_secs(60),
            "anchor was re-stamped on activity"
        );
        // ...but the OVER-TTL ledger entry is STILL present with its original
        // stamp: `touch` did not run the global purge, so the ledger was never
        // scanned or mutated (the #3459/#3303 non-regression is REAL, not just
        // benign). A full `purge_expired` would have dropped this entry.
        let seen = state
            .relayed_entry_ids_by_tmux
            .get(&PromptKey::new(provider, tmux))
            .and_then(|queue| queue.front())
            .expect("ledger entry still present (touch did not purge it)");
        assert_eq!(seen.value, "uuid-LEDGER");
        assert_eq!(
            seen.recorded_at, ledger_stamp_before,
            "touch left the over-TTL ledger entry byte-for-byte untouched"
        );
    }

    // The ledger DOES purge on its own 30min TTL via the normal purge-running
    // path — `touch` simply is not that path. `relayed_entry_id_already_seen`
    // runs `purge_expired`, dropping the over-TTL entry; the freshly-touched
    // anchor (well within 4h) survives that same purge.
    assert!(
        !relayed_entry_id_already_seen(provider, tmux, "uuid-LEDGER"),
        "over-TTL ledger entry is dropped by the normal (purge-running) path"
    );
    assert_eq!(
        prompt_anchor_for_response(provider, tmux, channel),
        Some(TuiPromptAnchor {
            channel_id: channel,
            message_id: msg,
        }),
        "freshly-touched anchor survives the ledger's independent 30min purge"
    );
}

/// #3956 codex re-review: the no-resurrection guarantee must hold WITHOUT the
/// global purge — a matching anchor already past the 4h ceiling is never
/// refreshed by `touch` (it is evicted from the single anchor map instead), so
/// a pane idle 4h+ that suddenly streams cannot revive a long-dead turn's
/// anchor. The eviction touches only `prompt_anchor_by_tmux`.
#[test]
fn touch_anchor_on_activity_evicts_expired_anchor_without_resurrecting_it() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    let provider = "claude";
    let tmux = "tmux-3956-expired";
    let channel = 9999_u64;
    let msg = 1_234_u64;

    // An anchor already past the 4h ceiling (a long-dead turn). Recorded via
    // the aged helper, which does NOT purge, so it is still in the map when the
    // first streaming activity arrives.
    record_prompt_anchor_aged_for_tests(
        provider,
        tmux,
        channel,
        msg,
        PROMPT_ANCHOR_SUBMIT_TTL + Duration::from_secs(1),
    );

    // Activity must NOT refresh the dead anchor...
    assert!(
        !touch_prompt_anchor_on_activity(provider, tmux, channel),
        "an anchor past the 4h ceiling is never re-stamped (no resurrection)"
    );
    // ...and the dead anchor is evicted from the single anchor map.
    {
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        assert!(
            state
                .prompt_anchor_by_tmux
                .get(&PromptKey::new(provider, tmux))
                .is_none(),
            "the over-ceiling anchor was evicted, not resurrected"
        );
    }
    assert_eq!(
        prompt_anchor_for_response(provider, tmux, channel),
        None,
        "no live anchor remains for the dead turn"
    );
}

/// #3540: the ring cap bounds per-key growth even before the TTL fires —
/// the oldest id is evicted once the cap is exceeded.
#[test]
fn relayed_entry_id_ledger_is_ring_capped() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_state();

    record_relayed_entry_id("claude", "tmux-cap", "uuid-oldest");
    for i in 0..RELAYED_ENTRY_ID_RING_CAP {
        record_relayed_entry_id("claude", "tmux-cap", &format!("uuid-{i}"));
    }
    assert!(
        !relayed_entry_id_already_seen("claude", "tmux-cap", "uuid-oldest"),
        "the oldest id is dropped once the ring cap is exceeded"
    );
    assert!(
        relayed_entry_id_already_seen(
            "claude",
            "tmux-cap",
            &format!("uuid-{}", RELAYED_ENTRY_ID_RING_CAP - 1)
        ),
        "the newest ids remain"
    );
}

/// #3540: head-rotation simulation — `extract_claude_transcript_user_prompt_with_entry_id`
/// returns the SAME top-level uuid regardless of where the entry sits, so a
/// surviving entry whose byte offset shifted after a head truncation is still
/// recognized by identity.
#[test]
fn extract_returns_stable_top_level_uuid() {
    let json = serde_json::json!({
        "type": "user",
        "uuid": "6c532800-4c8c-4d1d-9e64-d308fab44a1e",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": "surviving prompt" }],
        },
        "sessionId": "sess-rot",
    });
    let (prompt, entry_id) =
        extract_claude_transcript_user_prompt_with_entry_id(&json).expect("user prompt");
    assert_eq!(prompt, "surviving prompt");
    assert_eq!(
        entry_id.as_deref(),
        Some("6c532800-4c8c-4d1d-9e64-d308fab44a1e"),
        "the stable top-level uuid is extracted; it survives head rotation \
         (offset shifts, uuid does not) so identity dedup recognizes the \
         re-encountered entry (#3540)"
    );
}

/// #3540: a `user` entry with no uuid yields `(prompt, None)` — the scanner
/// then uses the content-keyed fallback (no panic, no regression).
#[test]
fn extract_yields_none_entry_id_when_uuid_absent() {
    let json = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": "no uuid here" }],
        },
        "sessionId": "sess-x",
    });
    let (prompt, entry_id) =
        extract_claude_transcript_user_prompt_with_entry_id(&json).expect("user prompt");
    assert_eq!(prompt, "no uuid here");
    assert_eq!(entry_id, None);
}
