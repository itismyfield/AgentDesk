//! Drive the production terminal boundary with a real source file and receipt.

use super::*;

#[cfg(test)]
mod pg_tests;
use crate::services::{
    agent_protocol::RuntimeHandoffKind,
    discord::{
        inflight::{self, CodexRange, InflightTurnIdentity},
        outbound::delivery_record as dr,
    },
    tui_prompt_dedupe::{self, TuiRuntimeBinding},
};

fn receipt_parts(
    driver: &TerminalDeliveryDriver,
    provider: ProviderKind,
) -> (
    TerminalOutcomeDeliveryContext,
    TerminalOutcomeDeliveryState,
    dr::ExactJsonlSourceIdentity,
) {
    let (mut ctx, mut state) = driver.parts();
    let rollout = driver._temp.path().join("receipt-rollout.jsonl");
    std::fs::write(&rollout, [b'x'; 64]).unwrap();
    let rollout = std::fs::canonicalize(rollout).unwrap();
    let tmux = DRIVER_TMUX_SESSION;
    let runtime_kind = if provider == ProviderKind::Codex {
        RuntimeHandoffKind::CodexTui
    } else {
        RuntimeHandoffKind::ClaudeTui
    };
    crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
        tmux,
        &rollout,
        Some("receipt-session"),
        Some(0),
    )
    .unwrap();
    tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        TuiRuntimeBinding {
            runtime_kind,
            output_path: rollout.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("receipt-session".into()),
            last_offset: 64,
            relay_last_offset: None,
        },
    );
    let generation = crate::services::tmux_common::session_temp_path(tmux, "generation");
    std::fs::write(&generation, "g").unwrap();
    filetime::set_file_mtime(
        &generation,
        filetime::FileTime::from_unix_time(1_700_552_100, 1),
    )
    .unwrap();
    let local = &mut state.inflight_state;
    local.provider = provider.as_str().into();
    local.runtime_kind = Some(runtime_kind);
    local.turn_start_offset = Some(0);
    local.last_offset = 64;
    local.turn_nonce = Some("receipt-nonce".into());
    local.session_id = Some("receipt-session".into());
    local.output_path = Some(rollout.display().to_string());
    let source = dr::ExactJsonlSourceIdentity {
        provider: provider.as_str().into(),
        tmux_session_name: tmux.into(),
        turn_nonce: local.turn_nonce.clone().unwrap(),
        range: (0, 64),
        generation_mtime_ns: dr::current_generation_mtime_ns(tmux),
        offset_authority_channel_id: DRIVER_CHANNEL_ID,
        delivery_channel_id: DRIVER_CHANNEL_ID,
    };
    ctx.tmux_last_offset = Some(64);
    if provider == ProviderKind::Codex {
        ctx.codex_tui_terminal_range = Some(CodexRange {
            identity: InflightTurnIdentity::from_state(local),
            result: state.full_response.clone(),
            rollout_path: rollout.display().to_string(),
            session_id: "receipt-session".into(),
            source: source.clone(),
        });
    }
    state.provider = provider;
    // Start with the terminal-owned row absent. The driver's original Claude
    // row is removed as well, so the Claude half exercises the same boundary.
    inflight::clear_inflight_state(&ProviderKind::Claude, DRIVER_CHANNEL_ID);
    (ctx, state, source)
}

async fn run(
    ctx: TerminalOutcomeDeliveryContext,
    state: TerminalOutcomeDeliveryState,
) -> TerminalOutcomeDeliveryOutput {
    tokio::time::timeout(DRIVER_TIMEOUT, run_terminal_outcome_delivery(ctx, state))
        .await
        .expect("rowless terminal must finish without a retry spin")
}

#[tokio::test]
async fn exact_receipt_terminal_decision_records_only_evaluated_frontier_5521() {
    const CHILD: &str = "ADK_5071_TERMINAL_OBSERVATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let exact = format!(
            "{}::exact_receipt_terminal_decision_records_only_evaluated_frontier_5521",
            module_path!().split_once("::").unwrap().1
        );
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(child.status.success(), "{child:?}");
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed; 0 failed"));
        return;
    }
    // install has no uninstall: this dial exists only in the isolated child.
    let mut config = crate::config::Config::default();
    config.runtime.relay_authority_mode = crate::config::RelayAuthorityMode::Enforce;
    config.runtime.relay_authority_cohort_percent = 100;
    crate::config_live_reload::install(config);
    for case in ["current_receipt", "frontier", "uncovered", "no_range"] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, state, mut source) = receipt_parts(&driver, ProviderKind::Codex);
        let settled = matches!(case, "current_receipt" | "frontier");
        if case == "no_range" {
            ctx.codex_tui_terminal_range = None;
        } else {
            if case == "uncovered" {
                source.range.1 -= 1;
            }
            let anchor = if case == "frontier" {
                DRIVER_FALLBACK_ANCHOR_MSG_ID
            } else {
                DRIVER_CURRENT_MSG_ID
            };
            dr::record_current_pinned_delivery(&source, anchor).unwrap();
        }
        let output = run(ctx, state).await;
        assert!(output.terminal_delivery_committed, "{case}");
        assert_eq!(driver.completed_publications() == 0, settled, "{case}");
        let file = std::fs::read_dir(driver._temp.path().join("relay_authority"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let records: Vec<serde_json::Value> = std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["site"] == "completion_terminal_receipt")
            .collect();
        assert_eq!(records.len(), 1, "{case}: one actual decision");
        let record = &records[0];
        let expected = match case {
            "frontier" => Some(true),
            "uncovered" => Some(false),
            _ => None,
        };
        assert_eq!(
            record["frontier_already_covers"].as_bool(),
            expected,
            "{case}"
        );
        assert_eq!(
            record["disposition"],
            if settled {
                "already_delivered"
            } else {
                "continue"
            }
        );
        if case == "no_range" {
            assert!(record["source"].is_null());
        } else {
            assert_eq!(record["source"]["range"], serde_json::json!([0, 64]));
            assert_eq!(
                record["source"]["generation_mtime_ns"],
                source.generation_mtime_ns
            );
        }
        assert_eq!(record["anchor"].is_null(), expected.is_none());
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_dominates_all_publication_branches_5521() {
    for case in [
        "short", "long", "fallback", "cancel", "ptl", "empty", "recovery", "headless",
    ] {
        let body = match case {
            "long" => "chunk ".repeat(1_200),
            "empty" => String::new(),
            _ => DRIVER_BODY.into(),
        };
        let replace = if case == "fallback" {
            ReplaceBehaviour::FallbackAfterEditFailure
        } else {
            ReplaceBehaviour::Edited
        };
        let driver = TerminalDeliveryDriver::new(replace, 1).with_body(body);
        let (mut ctx, state, source) = receipt_parts(&driver, ProviderKind::Codex);
        dr::record_current_pinned_delivery(&source, DRIVER_CURRENT_MSG_ID).unwrap();
        ctx.single_message_panel_footer_mode = true;
        ctx.cancelled = case == "cancel";
        ctx.is_prompt_too_long = case == "ptl";
        ctx.recovery_retry = case == "recovery";
        ctx.can_chain_locally = case != "headless";
        let output = run(ctx, state).await;
        assert!(
            driver.observations().is_empty(),
            "{case}: no send/edit/delete/replace"
        );
        assert_eq!(driver.completed_publications(), 0, "{case}");
        assert!(
            output.terminal_delivery_committed && output.status_panel_terminal_committed,
            "{case}"
        );
        assert!(
            !output.bridge_should_emit_completion,
            "{case}: completion footer must stay untouched"
        );
        assert!(
            !output.preserve_inflight_for_cleanup_retry,
            "{case}: receipt settles the obligation"
        );
        assert!(
            driver.marker(),
            "{case}: current watcher incarnation learns completion"
        );
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_supports_claude_and_entry_witness_5521() {
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, state, source) = receipt_parts(&driver, provider);
        // It was rowless at entry but a matching row appeared before terminal.
        ctx.entry_was_rowless = true;
        inflight::save_inflight_state(&state.inflight_state).unwrap();
        dr::record_current_pinned_delivery(&source, DRIVER_CURRENT_MSG_ID).unwrap();
        let output = run(ctx, state).await;
        assert!(output.terminal_delivery_committed);
        assert!(driver.observations().is_empty());
        assert!(!output.bridge_should_emit_completion);
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_preserves_foreign_anchor_and_successor_5521() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (ctx, state, source) = receipt_parts(&driver, ProviderKind::Codex);
    let mut successor = state.inflight_state.clone();
    successor.user_msg_id += 1;
    successor.turn_nonce = Some("successor".into());
    inflight::save_inflight_state(&successor).unwrap();
    dr::record_current_pinned_delivery(&source, DRIVER_FALLBACK_ANCHOR_MSG_ID).unwrap();
    let output = run(ctx, state).await;
    assert!(output.terminal_delivery_committed);
    assert!(
        driver.observations().is_empty(),
        "neither original nor receipt anchor may mutate"
    );
    let fresh =
        inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID).unwrap();
    assert_eq!(fresh.user_msg_id, successor.user_msg_id);
    assert_eq!(fresh.turn_nonce, successor.turn_nonce);
    assert!(!fresh.terminal_delivery_committed);
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_foreign_anchor_fallback_and_dual_failure_5521() {
    for (post_fails, live_gateway) in [(false, true), (true, true), (false, false)] {
        let replace = if post_fails {
            ReplaceBehaviour::FailedPost
        } else {
            ReplaceBehaviour::Edited
        };
        let driver = TerminalDeliveryDriver::new(replace, 1);
        let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor".into());
        successor.turn_start_offset = Some(64);
        inflight::save_inflight_state(&successor).unwrap();
        ctx.codex_tui_terminal_range = None;
        ctx.can_chain_locally = live_gateway;
        let output = run(ctx, state).await;
        assert!(
            !output.bridge_skip_holder_owns_inflight && !output.preserve_inflight_for_cleanup_retry
        );
        if post_fails || !live_gateway {
            assert!(!output.terminal_delivery_committed);
            assert!(
                matches!(&output.outcome, TerminalOutcomeDeliveryOutcome::Unresolved { error } if error.contains("outbox") && (error.contains("POST failed") || error.contains("no live Discord")))
            );
        } else {
            assert!(output.terminal_delivery_committed);
            assert_eq!(driver.completed_publications(), 1);
        }
        run_postlude(&driver, output, false, false).await;
        assert!(
            driver
                .observations()
                .iter()
                .all(|o| o.call == DriverCall::Send),
            "foreign anchor is never an edit/delete target"
        );
        let fresh =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap();
        assert_eq!(fresh.turn_nonce, successor.turn_nonce);
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_uncovered_or_stale_still_publishes_5521() {
    for case in [
        "uncovered",
        "stale",
        "nonce",
        "no_receipt",
        "no_range",
        "empty_range",
        "reversed",
    ] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, state, mut receipt) = receipt_parts(&driver, ProviderKind::Codex);
        match case {
            "uncovered" => receipt.range.1 -= 1,

            "nonce" => receipt.turn_nonce.push_str("-other"),
            "no_range" => ctx.codex_tui_terminal_range = None,
            "empty_range" => ctx.codex_tui_terminal_range.as_mut().unwrap().source.range = (0, 0),
            "reversed" => ctx.codex_tui_terminal_range.as_mut().unwrap().source.range = (64, 0),
            _ => {}
        }
        if case != "no_receipt" {
            dr::record_current_pinned_delivery(&receipt, DRIVER_CURRENT_MSG_ID).unwrap();
        }
        if case == "stale" {
            let generation =
                crate::services::tmux_common::session_temp_path(DRIVER_TMUX_SESSION, "generation");
            filetime::set_file_mtime(
                generation,
                filetime::FileTime::from_unix_time(1_700_552_200, 1),
            )
            .unwrap();
        }
        let output = run(ctx, state).await;
        assert!(
            driver.completed_publications() > 0,
            "{case}: unknown is not delivered proof"
        );
        assert!(
            output.terminal_delivery_committed,
            "{case}: legitimate legacy delivery remains available"
        );
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_survives_newer_frontier_at_another_anchor_5521() {
    for delivered_anchor in [DRIVER_CURRENT_MSG_ID, DRIVER_FALLBACK_ANCHOR_MSG_ID] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (ctx, state, source) = receipt_parts(&driver, ProviderKind::Codex);
        dr::record_current_pinned_delivery(&source, delivered_anchor).unwrap();
        // The original exact receipt stays in the same-generation bounded
        // record after another range becomes the latest frontier.
        let path = state.inflight_state.output_path.as_ref().unwrap();
        std::fs::write(path, [b'x'; 128]).unwrap();
        let mut later = source.clone();
        later.range = (64, 128);
        later.turn_nonce.push_str("-later");
        dr::record_current_pinned_delivery(&later, DRIVER_STALE_PREFIX_MSG_ID).unwrap();
        crate::services::codex_tui::session::advance_codex_tui_runtime_binding_and_marker_offset(
            DRIVER_TMUX_SESSION,
            std::path::Path::new(path),
            128,
        );
        assert_eq!(
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(DRIVER_TMUX_SESSION)
                .unwrap()
                .rollout_start_offset,
            Some(128)
        );
        crate::services::tmux_common::with_tmux_source_authority(
            DRIVER_TMUX_SESSION,
            |authority| {
                let admitted = ctx.codex_tui_terminal_range.as_ref().unwrap();
                assert!(
                    !admitted.source_authority_is_live(authority),
                    "new publication still needs its exact cursor"
                );
                assert!(admitted.source_receipt_is_live(authority));
            },
        );
        let output = run(ctx, state).await;
        assert!(output.terminal_delivery_committed);
        run_postlude(&driver, output, false, false).await;
        assert!(driver.observations().is_empty());
    }
}

// Re-use the actual terminal driver's output as the postlude input. Only the
// unrelated transcript/accounting inputs are neutral; projection and inflight
// settlement run through the production caller.
#[rustfmt::skip]
async fn run_postlude(driver: &TerminalDeliveryDriver, output: TerminalOutcomeDeliveryOutput, footer: bool, cancelled: bool) {
    use super::super::super::{completion_postlude as postlude, guards};
    let channel_id = ChannelId::new(DRIVER_CHANNEL_ID);
    let (_, rx) = std::sync::mpsc::channel();
    let fence = tokio::sync::OnceCell::new();
    let _ = super::super::super::capture_bridge_clear_fence(&driver.shared, channel_id, rx, &fence).await;
    let user_id = output.inflight_state.user_msg_id;
    let completion_guard = guards::CompletionGuard::for_completion_test(driver.shared.clone(), channel_id, user_id);
    let inflight_guard = guards::InflightCleanupGuard::for_completion_test(&output.inflight_state, driver.shared.token_hash.clone());
    let ctx = postlude::CompletionPostludeContext {
        shared_owned: output.shared_owned, gateway: output.gateway, channel_id,
        provider: output.provider, cancel_token: output.cancel_token,
        user_msg_id: (user_id != 0).then(|| MessageId::new(user_id)), turn_id: output.turn_id,
        request_owner_name: String::new(), final_session_status: "idle", status_panel_started_at: 0,
        has_queued_turns: false, defer_watcher_resume: true, can_chain_locally: true,
        single_message_panel_footer_mode: footer, is_external_input_tui_direct: false,
        context_window_tokens: 0, context_compact_percent: 0,
        clear_fence: fence.into_inner().unwrap(), turn_start: output.turn_start,
    };
    let state = postlude::CompletionPostludeState {
        watcher_delivery_pin: driver.parts().0.watcher_delivery_pin,
        full_response: output.full_response, user_text_owned: output.user_text_owned,
        role_binding: None, adk_session_key: None, adk_session_name: None, adk_session_info: None,
        adk_cwd: None, dispatch_id: None, dispatch_kind: None, new_session_id: None,
        new_raw_provider_session_id: None,
        status_panel_terminal_committed: output.status_panel_terminal_committed,
        bridge_should_emit_completion: output.bridge_should_emit_completion,
        current_msg_id: MessageId::new(DRIVER_CURRENT_MSG_ID),
        status_panel_msg_id: Some(MessageId::new(DRIVER_CURRENT_MSG_ID)),
        last_status_panel_text: "working".into(),
        completion_footer_terminal_text: output.completion_footer_terminal_text,
        busy_requeue_outcome: output.busy_requeue_outcome, spin_idx: 0, status_panel_generation: 0,
        preserve_inflight_for_cleanup_retry: output.preserve_inflight_for_cleanup_retry,
        tmux_last_offset: Some(64), watcher_owner_channel_id: channel_id,
        bridge_relay_delegated_to_watcher: false, is_prompt_too_long: false,
        resume_failure_detected: false, recovery_retry: false, rx_disconnected: false,
        tmux_handed_off: false, bridge_output_owner: None,
        terminal_delivery_committed: output.terminal_delivery_committed,
        terminal_session_reset_required: false, transcript_events: Vec::new(),
        accumulated_input_tokens: 0, accumulated_cache_create_tokens: 0,
        accumulated_cache_read_tokens: 0, accumulated_output_tokens: 0,
        accumulated_memory_input_tokens: 0, accumulated_memory_output_tokens: 0,
        transport_error: false, api_friction_reports: Vec::new(), cancelled,
        restart_followup_pending: None,
        bridge_skip_holder_owns_inflight: output.bridge_skip_holder_owns_inflight,
        completion_guard, inflight_guard, inflight_state: output.inflight_state,
    };
    tokio::time::timeout(DRIVER_TIMEOUT, postlude::run_completion_postlude(ctx, state)).await.unwrap();
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_runs_postlude_without_footer_or_status_mutation_5521() {
    for footer in [false, true] {
        let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        Arc::get_mut(&mut driver.shared)
            .unwrap()
            .ui
            .status_panel_v2_enabled = true;
        let _mailbox = driver.shared.mailbox(ChannelId::new(DRIVER_CHANNEL_ID));
        let (mut ctx, state, source) = receipt_parts(&driver, ProviderKind::Codex);
        ctx.entry_was_rowless = true;
        ctx.single_message_panel_footer_mode = footer;
        inflight::save_inflight_state(&state.inflight_state).unwrap();
        dr::record_current_pinned_delivery(&source, DRIVER_CURRENT_MSG_ID).unwrap();
        let output = run(ctx, state).await;
        run_postlude(&driver, output, footer, false).await;
        assert!(
            driver.observations().is_empty(),
            "footer={footer}: terminal and postlude perform zero gateway mutations"
        );
        assert!(
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .is_none(),
            "receipt settles and clears this actor's own row"
        );
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_postlude_preserves_same_user_and_zero_id_successor_5521() {
    for (user_id, successor_offset) in [
        (DRIVER_USER_MSG_ID, 0),
        (DRIVER_USER_MSG_ID, 64),
        (0, 0),
        (0, 64),
    ] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, mut state, source) = receipt_parts(&driver, ProviderKind::Codex);
        state.inflight_state.user_msg_id = user_id;
        ctx.user_msg_id = (user_id != 0).then(|| MessageId::new(user_id));
        ctx.codex_tui_terminal_range.as_mut().unwrap().identity =
            InflightTurnIdentity::from_state(&state.inflight_state);
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("same-user-successor".into());
        successor.turn_start_offset = Some(successor_offset);
        inflight::save_inflight_state(&successor).unwrap();
        dr::record_current_pinned_delivery(&source, DRIVER_CURRENT_MSG_ID).unwrap();
        let output = run(ctx, state).await;
        assert!(output.terminal_delivery_committed);
        run_postlude(&driver, output, false, false).await;
        let fresh =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap();
        assert_eq!(fresh.turn_nonce, successor.turn_nonce);
        assert_eq!(fresh.turn_start_offset, successor.turn_start_offset);
        assert!(!fresh.terminal_delivery_committed);
        assert!(driver.observations().is_empty());
    }
}
