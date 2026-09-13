//! Drive the production terminal boundary with a real source file and receipt.

use super::*;
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
async fn exact_receipt_rowless_terminal_unknown_foreign_anchor_preserves_retry_5521() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (mut ctx, state, _) = receipt_parts(&driver, ProviderKind::Codex);
    let mut successor = state.inflight_state.clone();
    successor.user_msg_id += 1;
    successor.turn_nonce = Some("successor".into());
    inflight::save_inflight_state(&successor).unwrap();
    ctx.codex_tui_terminal_range = None;
    let output = run(ctx, state).await;
    assert!(!output.terminal_delivery_committed);
    assert!(output.preserve_inflight_for_cleanup_retry && output.bridge_skip_holder_owns_inflight);
    assert!(driver.observations().is_empty());
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
            "stale" => receipt.generation_mtime_ns -= 1,
            "nonce" => receipt.turn_nonce.push_str("-other"),
            "no_range" => ctx.codex_tui_terminal_range = None,
            "empty_range" => ctx.codex_tui_terminal_range.as_mut().unwrap().source.range = (0, 0),
            "reversed" => ctx.codex_tui_terminal_range.as_mut().unwrap().source.range = (64, 0),
            _ => {}
        }
        if case != "no_receipt" {
            dr::record_current_pinned_delivery(&receipt, DRIVER_CURRENT_MSG_ID).unwrap();
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
