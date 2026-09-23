//! Drive the production terminal boundary with a real source file and receipt.

use super::*;

#[cfg(test)]
mod pg_tests;
mod preloop_cleanup_tests;
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
        ctx.codex_tui_terminal_range = Some(CodexRange::new(
            InflightTurnIdentity::from_state(local),
            state.full_response.clone(),
            rollout.display().to_string(),
            "receipt-session".into(),
            source.clone(),
            None,
        ));
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
        let root = tempfile::tempdir().unwrap();
        let exact = format!(
            "{}::exact_receipt_terminal_decision_records_only_evaluated_frontier_5521",
            module_path!().split_once("::").unwrap().1
        );
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
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
    let generation = crate::services::discord::runtime_store::allocate_process_generation();
    assert_ne!(generation, 0);
    // Keep the real boot allocation bound across this child's per-case roots,
    // as production does; test process_generation otherwise rereads each root.
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(generation));
    for case in [
        "current_receipt",
        "frontier",
        "uncovered",
        "no_range",
        "absent",
    ] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, state, mut source) = receipt_parts(&driver, ProviderKind::Codex);
        let settled = matches!(case, "current_receipt" | "frontier");
        if matches!(case, "no_range" | "absent") {
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
        if case == "absent" {
            ctx.current_msg_id =
                super::super::super::current_message_anchor::detached_current_msg_id_from_durable(
                    0,
                );
            assert_eq!(
                super::super::rowless_receipt::decision(
                    super::super::rowless_receipt::ReceiptDecisionInput::from_terminal(
                        &ctx, &state
                    ),
                ),
                super::super::rowless_receipt::TerminalReceiptDisposition::Continue
            );
        } else {
            let output = run(ctx, state).await;
            assert!(output.terminal_delivery_committed, "{case}");
            assert_eq!(driver.completed_publications() == 0, settled, "{case}");
        }
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
        assert_eq!(record["process_generation"], generation);
        assert_eq!(
            record["current_message_id"],
            if case == "absent" {
                0
            } else {
                DRIVER_CURRENT_MSG_ID
            }
        );
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
        if matches!(case, "no_range" | "absent") {
            assert!(record["source"].is_null());
        } else {
            assert_eq!(record["source"]["range"], serde_json::json!([0, 64]));
            assert_eq!(
                record["source"]["generation_mtime_ns"],
                source.generation_mtime_ns
            );
        }
        assert_eq!(record["anchor"].is_null(), expected.is_none());
        let checked = std::process::Command::new("python3")
            .args([
                "-B",
                "-c",
                r#"
import pathlib, runpy, sys
r = runpy.run_path(sys.argv[1])
events, warnings, _ = r['load_events'](pathlib.Path(sys.argv[2]) / 'relay_authority')
assert not warnings, warnings
metric = r['delivery_boundary_counts'](events)['frontier_already_covers']
expected = sys.argv[3]
assert metric[expected] == 1 and metric['records'] == 1, metric
assert metric['status'] == ('unknown' if expected == 'unknown' else 'measured'), metric
assert r['completion_scope_counts'](events) == {}
"#,
            ])
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/relay_authority_rollout_report.py"
            ))
            .arg(driver._temp.path())
            .arg(match expected {
                Some(true) => "true",
                Some(false) => "false",
                None => "unknown",
            })
            .output()
            .unwrap();
        assert!(checked.status.success(), "{checked:?}");
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_dominates_all_publication_branches_5521() {
    for case in [
        "short",
        "long",
        "fallback",
        "cancel",
        "ptl",
        "empty",
        "recovery",
        "headless",
        "owned_recovery",
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
        let (mut ctx, mut state, source) = receipt_parts(&driver, ProviderKind::Codex);
        if case == "owned_recovery" {
            ctx.entry_was_rowless = false;
            state.inflight_state.full_response = state.full_response.clone();
            inflight::save_inflight_state(&state.inflight_state).unwrap();
        }
        let receipt_anchor = if case == "owned_recovery" {
            DRIVER_FALLBACK_ANCHOR_MSG_ID
        } else {
            DRIVER_CURRENT_MSG_ID
        };
        dr::record_current_pinned_delivery(&source, receipt_anchor).unwrap();
        ctx.single_message_panel_footer_mode = true;
        ctx.cancelled = case == "cancel";
        ctx.is_prompt_too_long = case == "ptl";
        ctx.recovery_retry = case == "recovery";
        if case == "headless" {
            state.gateway = Arc::new(crate::services::discord::gateway::HeadlessGateway);
        }
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
    for post_fails in [false, true] {
        let driver = TerminalDeliveryDriver::new(
            if post_fails {
                ReplaceBehaviour::FailedPost
            } else {
                ReplaceBehaviour::Edited
            },
            1,
        );
        let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor".into());
        successor.turn_start_offset = Some(64);
        inflight::save_inflight_state(&successor).unwrap();
        ctx.codex_tui_terminal_range = None;
        let output = run(ctx, state).await;
        assert!(matches!(
            output.outcome,
            TerminalOutcomeDeliveryOutcome::DeferredToCustody { .. }
        ));
        assert!(!output.terminal_delivery_committed && !output.bridge_skip_holder_owns_inflight);
        assert!(output.preserve_inflight_for_cleanup_retry);
        let mut signals = driver.shared.inflight_signals.subscribe();
        run_postlude(&driver, output, false, false).await;
        assert_no_completed_signal(&mut signals);
        assert!(
            driver.observations().is_empty(),
            "handoff itself never publishes"
        );
        let drained = drain_custody(&driver).await;
        if post_fails {
            assert!(drained.is_err());
            assert_eq!(
                custody_records(&driver).len(),
                1,
                "failed POST survives restart as a real payload"
            );
        } else {
            assert_eq!(drained.unwrap(), 1);
            assert_eq!(driver.completed_publications(), 1);
            assert!(custody_records(&driver).is_empty());
        }
        assert!(
            driver
                .observations()
                .iter()
                .all(|o| o.call == DriverCall::Send)
        );
        let fresh =
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap();
        assert_eq!(fresh.turn_nonce, successor.turn_nonce);
    }
}

fn custody_records(driver: &TerminalDeliveryDriver) -> Vec<serde_json::Value> {
    let root = driver
        ._temp
        .path()
        .join("runtime/discord_terminal_delivery_custody");
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .map(|entry| serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap())
        .collect()
}

async fn drain_custody(driver: &TerminalDeliveryDriver) -> Result<usize, String> {
    crate::services::discord::terminal_delivery_custody::drain_for_test(
        |mut payload, checkpoint| {
            let shared = driver.shared.clone();
            let gateway = driver.gateway.clone();
            async move {
                let outcome = super::super::foreign_terminal_handoff::resume_payload_with_gateway(
                    &shared,
                    gateway.as_ref(),
                    &mut payload,
                    &checkpoint,
                )
                .await;
                (payload, outcome)
            }
        },
    )
    .await
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
    let is_external_input_tui_direct = output.inflight_state.turn_source == inflight::TurnSource::ExternalInput;
    let mut completion_guard = guards::CompletionGuard::for_completion_test(driver.shared.clone(), channel_id, user_id);
    output.handoff_completion_authority(&mut completion_guard);
    let inflight_guard = guards::InflightCleanupGuard::for_completion_test(&output.inflight_state, driver.shared.token_hash.clone());
    let ctx = postlude::CompletionPostludeContext {
        shared_owned: output.shared_owned, gateway: output.gateway, channel_id,
        provider: output.provider, cancel_token: output.cancel_token,
        user_msg_id: (user_id != 0).then(|| MessageId::new(user_id)), turn_id: output.turn_id,
        request_owner_name: String::new(), final_session_status: "idle", status_panel_started_at: 0,
        has_queued_turns: false, defer_watcher_resume: true, can_chain_locally: true,
        single_message_panel_footer_mode: footer, is_external_input_tui_direct,
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

#[tokio::test]
async fn exact_receipt_rowless_terminal_custody_respects_owner_and_live_lease_5521() {
    for owner in [
        Some(BridgeOutputOwner::WatcherRelay),
        Some(BridgeOutputOwner::StandbyRelay),
        None,
    ] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
        let held = bridge_delivery_lease_for_inflight(
            &driver.shared,
            ctx.watcher_owner_channel_id,
            driver.shared.restart.current_generation,
            &state.inflight_state,
            ctx.tmux_last_offset,
        );
        assert!(matches!(held, BridgeLeaseAcquire::Held(_)));
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor".into());
        inflight::save_inflight_state(&successor).unwrap();
        ctx.bridge_output_owner = owner;
        let output = run(ctx, state).await;
        if owner.is_some() {
            assert!(matches!(
                output.outcome,
                TerminalOutcomeDeliveryOutcome::DeferredToOwner
            ));
        } else {
            assert!(matches!(
                output.outcome,
                TerminalOutcomeDeliveryOutcome::DeferredToCustody { .. }
            ));
        }
        run_postlude(&driver, output, false, false).await;
        assert_eq!(drain_custody(&driver).await.unwrap(), 0);
        assert!(driver.observations().is_empty());
        drop(held);
        if owner.is_none() {
            assert_eq!(drain_custody(&driver).await.unwrap(), 1);
            assert_eq!(driver.completed_publications(), 1);
        } else {
            assert!(custody_records(&driver).is_empty());
        }
        assert_eq!(
            inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                .unwrap()
                .turn_nonce,
            successor.turn_nonce
        );
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_custody_long_partial_ack_survives_retry_5521() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::FailSecondPostOnce, 1)
        .with_body("long answer ".repeat(700));
    let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
    let expected_chunks =
        crate::services::discord::formatting::split_message(&state.full_response).len();
    assert!(expected_chunks > 2);
    let mut successor = state.inflight_state.clone();
    successor.turn_nonce = Some("successor".into());
    inflight::save_inflight_state(&successor).unwrap();
    ctx.codex_tui_terminal_range = None;
    let output = run(ctx, state).await;
    run_postlude(&driver, output, false, false).await;
    assert!(drain_custody(&driver).await.is_err());
    let records = custody_records(&driver);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["payload"]["delivery_receipts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(driver.completed_publications(), 1);
    assert_eq!(drain_custody(&driver).await.unwrap(), 1);
    assert_eq!(
        driver.completed_publications(),
        expected_chunks,
        "ACKed first chunk must not be re-POSTed"
    );
    assert!(custody_records(&driver).is_empty());
    assert!(
        driver
            .observations()
            .iter()
            .all(|o| o.call == DriverCall::Send)
    );
    assert_eq!(
        inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
            .unwrap()
            .turn_nonce,
        successor.turn_nonce
    );
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_custody_io_failure_never_completes_5521() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
    let mut successor = state.inflight_state.clone();
    successor.turn_nonce = Some("successor".into());
    inflight::save_inflight_state(&successor).unwrap();
    std::fs::write(
        driver
            ._temp
            .path()
            .join("runtime/discord_terminal_delivery_custody"),
        "not a directory",
    )
    .unwrap();
    let output = run(ctx, state).await;
    assert!(
        matches!(&output.outcome, TerminalOutcomeDeliveryOutcome::Unresolved { error } if error.contains("custody"))
    );
    assert!(!output.terminal_delivery_committed && !output.bridge_skip_holder_owns_inflight);
    let mut signals = driver.shared.inflight_signals.subscribe();
    run_postlude(&driver, output, false, false).await;
    assert_no_completed_signal(&mut signals);
    assert!(driver.observations().is_empty());
    assert_eq!(
        inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
            .unwrap()
            .turn_nonce,
        successor.turn_nonce
    );
}

fn assert_no_completed_signal(
    signals: &mut tokio::sync::broadcast::Receiver<inflight::InflightSignal>,
) {
    while let Ok(signal) = signals.try_recv() {
        assert!(
            !matches!(signal, inflight::InflightSignal::Completed { .. }),
            "deferred custody must not announce Completed"
        );
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_custody_empty_cancel_and_ptl_match_normal_body_5521() {
    for cancel in [true, false] {
        let mut normal_body = None;
        for foreign in [false, true] {
            let driver =
                TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1).with_body(String::new());
            let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
            ctx.cancelled = cancel;
            ctx.is_prompt_too_long = !cancel;
            ctx.codex_tui_terminal_range = None;
            let mut row = state.inflight_state.clone();
            if foreign {
                row.turn_nonce = Some("successor".into());
            }
            inflight::save_inflight_state(&row).unwrap();
            let output = run(ctx, state).await;
            if foreign {
                assert!(matches!(
                    output.outcome,
                    TerminalOutcomeDeliveryOutcome::DeferredToCustody { .. }
                ));
                let record = custody_records(&driver).pop().unwrap();
                assert_eq!(
                    record["payload"]["full_response"], "",
                    "guidance must not forge provider source text"
                );
                assert_eq!(
                    record["payload"]["delivery_body"],
                    normal_body.as_deref().unwrap()
                );
            }
            run_postlude(&driver, output, false, cancel).await;
            if foreign {
                assert_eq!(drain_custody(&driver).await.unwrap(), 1);
            }
            let body = driver
                .published_bodies
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("actual terminal body published");
            if foreign {
                assert_eq!(body, normal_body.as_deref().unwrap());
                assert!(custody_records(&driver).is_empty());
                assert_eq!(
                    inflight::load_inflight_state_read_only(
                        &ProviderKind::Codex,
                        DRIVER_CHANNEL_ID
                    )
                    .unwrap()
                    .turn_nonce,
                    row.turn_nonce
                );
            } else {
                normal_body = Some(body);
            }
        }
    }
}

#[tokio::test]
async fn exact_receipt_rowless_terminal_custody_empty_recovery_stays_inside_source_range_5521() {
    for (provider, recovered, completed, native_claude) in [
        (ProviderKind::Claude, "", true, false),
        (ProviderKind::Claude, "A answer", true, false),
        (ProviderKind::Claude, "", true, true),
        (ProviderKind::Claude, "A answer", true, true),
        (ProviderKind::Codex, "A answer", true, false),
        (ProviderKind::Claude, "", false, false),
        (ProviderKind::Codex, "", false, false),
    ] {
        let driver =
            TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1).with_body(String::new());
        let (mut ctx, mut state, _) = receipt_parts(&driver, provider.clone());
        let line = if !completed {
            serde_json::json!({"type":"unknown_record"})
        } else if provider == ProviderKind::Codex {
            serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":recovered}})
        } else {
            serde_json::json!({"type":"result","subtype":"success","result":recovered})
        };
        let mut bytes = if native_claude {
            state.adk_session_key = None;
            state.inflight_state.session_id = None;
            let assistant = serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":recovered}]}});
            let terminal = serde_json::json!({"type":"system","subtype":"stop_hook_summary"});
            format!("{assistant}\n{terminal}").into_bytes()
        } else {
            line.to_string().into_bytes()
        };
        assert!(bytes.len() < 256);
        bytes.resize(255, b' ');
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"B must never be attributed to A\"}\n");
        let path = state.inflight_state.output_path.clone().unwrap();
        std::fs::write(&path, bytes).unwrap();
        state.inflight_state.last_offset = 256;
        ctx.tmux_last_offset = Some(256);
        if let Some(admitted) = ctx.codex_tui_terminal_range.as_mut() {
            admitted.source.range.1 = 256;
        }
        tui_prompt_dedupe::register_tmux_runtime_binding(
            DRIVER_TMUX_SESSION,
            TuiRuntimeBinding {
                runtime_kind: state.inflight_state.runtime_kind.unwrap(),
                output_path: path,
                relay_output_path: None,
                input_fifo_path: None,
                session_id: (!native_claude).then(|| "receipt-session".into()),
                last_offset: 256,
                relay_last_offset: None,
            },
        );
        #[cfg(unix)]
        if native_claude {
            // Native TUI has no session marker. Carry an actual decoded reader
            // range (including the opened FD) before replacing its durable row.
            state.cancel_token = Arc::new(
                crate::services::provider::CancelToken::from_persisted_turn_nonce(
                    state.inflight_state.turn_nonce.clone(),
                ),
            );
            crate::services::discord::mailbox_recovery_kickoff(
                &driver.shared,
                ctx.channel_id,
                state.cancel_token.clone(),
                serenity::all::UserId::new(state.inflight_state.request_owner_user_id),
                ctx.user_msg_id,
            )
            .await;
            inflight::save_inflight_state(&state.inflight_state).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let (read, stats) =
                crate::services::session_backend::read_output_file_until_result_with_harvest(
                    state.inflight_state.output_path.as_deref().unwrap(),
                    0,
                    tx,
                    None,
                    crate::services::provider::SessionProbe::process(|| false),
                )
                .unwrap();
            let crate::services::provider::ReadOutputResult::Completed { offset } = read else {
                panic!("native source must contain its decoded terminal")
            };
            assert!(stats.decoded_terminal);
            assert_eq!(
                offset, 256,
                "the reader stops at A before successor B's source bytes"
            );
            let crate::services::cluster::stream_relay::SourceFileIdentity::Unix { dev, ino } =
                stats.source_file.unwrap()
            else {
                panic!("reader must carry the opened file identity")
            };
            let session_id = rx
                .into_iter()
                .find_map(|frame| match frame {
                    crate::services::agent_protocol::StreamMessage::Done { session_id, .. } => {
                        Some(session_id)
                    }
                    _ => None,
                })
                .unwrap();
            let frame = crate::services::agent_protocol::StreamMessage::ClaudeTuiTerminalDone {
                result: String::new(),
                session_id,
                transcript_path: state.inflight_state.output_path.clone().unwrap(),
                tmux_session_name: DRIVER_TMUX_SESSION.into(),
                turn_nonce: state.inflight_state.turn_nonce.clone().unwrap(),
                source_start: 0,
                complete_record_end: offset,
                generation_mtime_ns: dr::current_generation_mtime_ns(DRIVER_TMUX_SESSION),
                source_file_dev: dev,
                source_file_ino: ino,
                actor: Arc::downgrade(&state.cancel_token),
            };
            let mut baseline = state.inflight_state.clone();
            let expected = InflightTurnIdentity::from_state(&baseline);
            let (_, admitted, _) = state
                .inflight_state
                .admit_tui_terminal_frame(
                    &mut baseline,
                    &expected,
                    true,
                    (&driver.shared, &state.cancel_token),
                    "",
                    frame,
                )
                .await
                .unwrap();
            ctx.codex_tui_terminal_range = admitted;
        }
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor".into());
        successor.turn_start_offset = Some(256);
        inflight::save_inflight_state(&successor).unwrap();
        let output = run(ctx, state).await;
        assert!(matches!(
            output.outcome,
            TerminalOutcomeDeliveryOutcome::DeferredToCustody { .. }
        ));
        run_postlude(&driver, output, false, false).await;
        if !completed {
            assert!(drain_custody(&driver).await.is_err());
            assert_eq!(custody_records(&driver).len(), 1);
            assert!(driver.published_bodies.lock().unwrap().is_empty());
            assert_eq!(
                inflight::load_inflight_state_read_only(&provider, DRIVER_CHANNEL_ID)
                    .unwrap()
                    .turn_nonce,
                successor.turn_nonce
            );
            continue;
        }
        assert_eq!(drain_custody(&driver).await.unwrap(), 1);
        let body = driver
            .published_bodies
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap();
        if recovered.is_empty() {
            assert_eq!(
                body,
                super::super::empty_response_recovery::empty_response_guidance(false)
            );
        } else {
            assert_eq!(body, recovered);
        }
        assert!(!body.contains("B must never"));
        assert!(custody_records(&driver).is_empty());
        assert_eq!(
            inflight::load_inflight_state_read_only(&provider, DRIVER_CHANNEL_ID)
                .unwrap()
                .turn_nonce,
            successor.turn_nonce
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn exact_receipt_rowless_terminal_consumes_captured_claude_source_without_session_5521() {
    use std::os::unix::fs::MetadataExt;
    for valid_file_identity in [true, false] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, mut state, source) = receipt_parts(&driver, ProviderKind::Claude);
        state.inflight_state.session_id = None;
        std::fs::remove_file(crate::services::tmux_common::session_temp_path(
            DRIVER_TMUX_SESSION,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ))
        .unwrap();
        let path = state.inflight_state.output_path.clone().unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        tui_prompt_dedupe::register_tmux_runtime_binding(
            DRIVER_TMUX_SESSION,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.clone(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 64,
                relay_last_offset: None,
            },
        );
        // The alias retains the captured provider, FD identity and nullable
        // session; no Codex marker or invented session grants Claude authority.
        ctx.codex_tui_terminal_range = Some(serde_json::from_value(serde_json::json!({
            "identity": InflightTurnIdentity::from_state(&state.inflight_state),
            "result": state.full_response,
            "rollout_path": path,
            "session_id": "",
            "source": source,
            "source_file_identity": [metadata.dev(), metadata.ino() + u64::from(!valid_file_identity)],
        })).unwrap());
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("claude-successor".into());
        successor.turn_start_offset = Some(64);
        inflight::save_inflight_state(&successor).unwrap();
        dr::record_current_pinned_delivery(&source, DRIVER_CURRENT_MSG_ID).unwrap();
        ctx.single_message_panel_footer_mode = true;
        let output = run(ctx, state).await;
        assert_eq!(output.terminal_delivery_committed, valid_file_identity);
        if !valid_file_identity {
            assert!(matches!(
                output.outcome,
                TerminalOutcomeDeliveryOutcome::DeferredToCustody { .. }
            ));
        }
        run_postlude(&driver, output, true, false).await;
        assert!(driver.observations().is_empty());
        assert_eq!(
            custody_records(&driver).len(),
            usize::from(!valid_file_identity)
        );
        let fresh =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, DRIVER_CHANNEL_ID)
                .unwrap();
        assert_eq!(fresh.turn_nonce, successor.turn_nonce);
        assert!(!fresh.terminal_delivery_committed);
    }
}

#[tokio::test]
async fn exact_receipt_short_fallback_settles_original_actor_and_preserves_successor_5521() {
    use crate::services::discord::turn_finalizer::{CompletionAdmissionPlan, TurnKey};
    for replace_actor in [false, true] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::FallbackAfterEditFailure, 2);
        let (mut ctx, mut state, source) = receipt_parts(&driver, ProviderKind::Codex);
        let channel = ctx.channel_id;
        state.inflight_state.turn_source = inflight::TurnSource::ExternalInput;
        // The real terminal entry has already persisted the admitted raw body.
        state.inflight_state.full_response = state.full_response.clone();
        state.cancel_token = Arc::new(
            crate::services::provider::CancelToken::from_persisted_turn_nonce(
                state.inflight_state.turn_nonce.clone(),
            ),
        );
        let original_actor = state.cancel_token.clone();
        let original = state.inflight_state.clone();
        inflight::save_inflight_state(&original).unwrap();
        assert!(
            ctx.codex_tui_terminal_range
                .as_ref()
                .unwrap()
                .revalidated_source(&original)
                .unwrap()
                .is_some(),
            "the lifecycle fixture must exercise pinned delivery, not Unknown/NoRange"
        );
        ctx.entry_was_rowless = true;
        crate::services::discord::mailbox_recovery_kickoff(
            &driver.shared,
            channel,
            original_actor.clone(),
            serenity::UserId::new(DRIVER_USER_MSG_ID),
            Some(MessageId::new(original.user_msg_id)),
        )
        .await;
        let key = TurnKey::new(
            channel,
            original.effective_finalizer_turn_id(),
            driver.shared.restart.current_generation,
        )
        .with_episode_nonce(original.turn_nonce.as_deref());
        // This driver starts at terminal delivery; adapter admission ordering is
        // covered by synthetic_terminal_ordering_tests at the actual reader.
        driver
            .shared
            .turn_finalizer
            .register_start_with_completion_admission(
                key,
                ProviderKind::Codex,
                original.effective_relay_owner_kind(),
                CompletionAdmissionPlan::Immediate,
                &driver.shared,
            );
        let mut delivery = Box::pin(run_terminal_outcome_delivery(ctx, state));
        for _ in 0..20 {
            assert!(
                !poll_at_most(&mut delivery, 1),
                "fixture must suspend inside the actual gateway"
            );
            if !driver.publish_entries().is_empty() {
                break;
            }
        }
        assert_eq!(driver.publish_entries().len(), 1);
        let successor = if replace_actor {
            let actor = Arc::new(
                crate::services::provider::CancelToken::from_persisted_turn_nonce(
                    original.turn_nonce.clone(),
                ),
            );
            crate::services::discord::mailbox_recovery_kickoff(
                &driver.shared,
                channel,
                actor.clone(),
                serenity::UserId::new(DRIVER_USER_MSG_ID),
                Some(MessageId::new(original.user_msg_id)),
            )
            .await;
            Some(actor)
        } else {
            None
        };
        let output = tokio::time::timeout(DRIVER_TIMEOUT, delivery)
            .await
            .unwrap();
        assert!(output.terminal_delivery_committed);
        assert!(!output.preserve_inflight_for_cleanup_retry);
        assert!(
            output.completion_footer_terminal_text.is_none(),
            "failed original anchor must not get a completion footer"
        );
        assert!(dr::confirmed_delivery_receipt_exists(
            &ProviderKind::Codex,
            channel,
            DRIVER_FALLBACK_ANCHOR_MSG_ID,
            &source
        ));
        assert_eq!(driver.completed_publications(), 1);
        run_postlude(&driver, output, false, false).await;
        let after = crate::services::discord::mailbox_snapshot(&driver.shared, channel).await;
        if let Some(successor) = successor {
            assert!(
                after
                    .cancel_token
                    .as_ref()
                    .is_some_and(|actor| Arc::ptr_eq(actor, &successor))
            );
            assert!(!successor.cancelled.load(Ordering::Acquire));
            let row =
                inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID)
                    .unwrap();
            assert_eq!(row.current_msg_id, original.current_msg_id);
            assert_eq!(row.turn_nonce, original.turn_nonce);
        } else {
            assert!(
                after.cancel_token.is_none(),
                "confirmed fallback releases the original actor"
            );
            let next = Arc::new(crate::services::provider::CancelToken::new());
            assert!(
                crate::services::discord::mailbox_try_start_turn(
                    &driver.shared,
                    channel,
                    next,
                    serenity::UserId::new(DRIVER_USER_MSG_ID),
                    MessageId::new(DRIVER_USER_MSG_ID + 1)
                )
                .await
            );
        }
        assert_eq!(driver.completed_publications(), 1);
    }
}
