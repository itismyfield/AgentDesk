//! #5521: a row is lifecycle evidence, never a substitute for an exact receipt.

use super::*;
use crate::services::discord::{
    inflight::{InflightTurnIdentity, load_inflight_state_read_only},
    outbound::{delivery_frontier_probe, delivery_record as dr},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge) enum TerminalReceiptDisposition {
    Continue,
    AlreadyDelivered,
    ForeignAnchor,
}

pub(in crate::services::discord::turn_bridge) struct ReceiptDecisionInput<'a> {
    pub provider: &'a ProviderKind,
    pub channel_id: ChannelId,
    pub current_msg_id: MessageId,
    pub watcher_owner_channel_id: ChannelId,
    pub entry_was_rowless: bool,
    pub codex_tui_terminal_range: Option<&'a crate::services::discord::inflight::CodexRange>,
    pub tmux_last_offset: Option<u64>,
    pub inflight_state: &'a InflightTurnState,
    pub full_response: &'a str,
}

impl<'a> ReceiptDecisionInput<'a> {
    pub(super) fn from_terminal(
        ctx: &'a TerminalOutcomeDeliveryContext,
        state: &'a TerminalOutcomeDeliveryState,
    ) -> Self {
        Self {
            provider: &state.provider,
            channel_id: ctx.channel_id,
            current_msg_id: ctx.current_msg_id,
            watcher_owner_channel_id: ctx.watcher_owner_channel_id,
            entry_was_rowless: ctx.entry_was_rowless,
            codex_tui_terminal_range: ctx.codex_tui_terminal_range.as_ref(),
            tmux_last_offset: ctx.tmux_last_offset,
            inflight_state: &state.inflight_state,
            full_response: &state.full_response,
        }
    }
}

pub(in crate::services::discord::turn_bridge) fn decision(
    ctx: ReceiptDecisionInput<'_>,
) -> TerminalReceiptDisposition {
    use crate::services::discord::relay_recovery::authority_observation::delivery_boundary::{
        TerminalReceiptDecisionRecord, record_terminal_receipt_decision,
    };
    let (disposition, source, anchor, frontier_already_covers) = decision_with_evidence(&ctx);
    record_terminal_receipt_decision(TerminalReceiptDecisionRecord {
        provider: ctx.provider,
        channel_id: ctx.channel_id.get(),
        turn_id: ctx.inflight_state.effective_finalizer_turn_id(),
        source: source.as_ref(),
        anchor,
        current_message_id:
            super::super::current_message_anchor::durable_current_msg_id_from_detached(
                ctx.current_msg_id,
            ),
        frontier_already_covers,
        disposition: match disposition {
            TerminalReceiptDisposition::Continue => "continue",
            TerminalReceiptDisposition::AlreadyDelivered => "already_delivered",
            TerminalReceiptDisposition::ForeignAnchor => "foreign_anchor",
        },
    });
    disposition
}

type DecisionEvidence = (
    TerminalReceiptDisposition,
    Option<dr::ExactJsonlSourceIdentity>,
    Option<delivery_frontier_probe::CurrentGenerationAnchor>,
    Option<bool>,
);

fn decision_with_evidence(ctx: &ReceiptDecisionInput<'_>) -> DecisionEvidence {
    use TerminalReceiptDisposition::*;
    let unknown = |disposition| (disposition, None, None, None);
    let local = ctx.inflight_state;
    let identity = InflightTurnIdentity::from_state(local);
    let fresh = load_inflight_state_read_only(ctx.provider, local.channel_id);
    let own_row = fresh
        .as_ref()
        .is_some_and(|row| identity.matches_state(row) && row.turn_nonce == local.turn_nonce);
    // A successor's anchor cannot be edited even when this source is unknown.
    let fallback = if !own_row
        && fresh
            .as_ref()
            .is_some_and(|row| row.current_msg_id == ctx.current_msg_id.get())
    {
        ForeignAnchor
    } else {
        Continue
    };
    if own_row && !ctx.entry_was_rowless {
        return unknown(Continue);
    }
    let Some(tmux) = local.tmux_session_name.as_deref().filter(|s| !s.is_empty()) else {
        return unknown(fallback);
    };
    crate::services::tmux_common::with_tmux_source_authority(tmux, |authority| {
        let Some((source, _path, eof)) = verified_source_under_authority(&ctx, authority) else {
            return unknown(fallback);
        };
        let eof = Some(eof);
        // An exact current-source receipt survives advancement of the frontier
        // to a later range/anchor. The frontier is an anchor discovery hint,
        // not a veto over that confirmed transport result.
        let has_receipt = |message_id| {
            dr::confirmed_delivery_receipt_exists(ctx.provider, ctx.channel_id, message_id, &source)
        };
        if has_receipt(ctx.current_msg_id.get()) {
            // The exact receipt settles this retry before a frontier read.
            // Preserve that unmeasured frontier instead of synthesizing true.
            return (AlreadyDelivered, Some(source), None, None);
        }
        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            ctx.provider,
            ctx.watcher_owner_channel_id,
            tmux,
            eof,
        );
        let frontier_covers = anchor.is_some_and(|anchor| {
            anchor.range.0 <= source.range.0
                && anchor.range.1 >= source.range.1
                && anchor.panel_channel_id == ctx.channel_id.get()
                && has_receipt(anchor.panel_msg_id)
        });
        let disposition = if frontier_covers
            || dr::read_record(ctx.provider, source.offset_authority_channel_id).is_some_and(
                |record| {
                    record.confirmed_deliveries.iter().any(|receipt| {
                        receipt.source == source
                            && receipt.delivery_channel_id == ctx.channel_id.get()
                            && has_receipt(receipt.message_id)
                    })
                },
            ) {
            // Both same-anchor retries and a receipt on another anchor are
            // settled without touching either Discord message.
            AlreadyDelivered
        } else {
            fallback
        };
        (
            disposition,
            Some(source),
            anchor,
            anchor.map(|_| frontier_covers),
        )
    })
}

pub(in crate::services::discord::turn_bridge) fn source_matches_episode(
    source: &dr::ExactJsonlSourceIdentity,
    provider: &ProviderKind,
    local: &InflightTurnState,
    owner: ChannelId,
    delivery: ChannelId,
) -> bool {
    source.is_authoritative()
        && source.provider == provider.as_str()
        && Some(source.tmux_session_name.as_str()) == local.tmux_session_name.as_deref()
        && Some(source.turn_nonce.as_str()) == local.turn_nonce.as_deref()
        && Some(source.range.0) == local.turn_start_offset
        && source.offset_authority_channel_id == owner.get()
        && source.delivery_channel_id == delivery.get()
        && source.generation_mtime_ns == dr::current_generation_mtime_ns(&source.tmux_session_name)
}

pub(super) fn verified_source_under_authority(
    ctx: &ReceiptDecisionInput<'_>,
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
) -> Option<(dr::ExactJsonlSourceIdentity, std::path::PathBuf, u64)> {
    let local = ctx.inflight_state;
    let tmux = authority.session();
    let (source, path) = if let Some(admitted) = ctx.codex_tui_terminal_range.as_ref() {
        // Re-use the captured terminal range even after its row disappears.
        // revalidated_source intentionally refuses a missing row for NEW
        // publication; that refusal does not invalidate a live exact receipt.
        if !admitted.identity.matches_state(local)
            || admitted.result != ctx.full_response
            || !admitted.source_receipt_is_live(authority)
        {
            return None;
        }
        let Some(path) = admitted.receipt_source_path() else {
            return None;
        };
        (admitted.source.clone(), path)
    } else {
        // #5264: a non-admitted CodexTui range remains honest legacy/NoRange.
        if *ctx.provider == ProviderKind::Codex
            && local.runtime_kind
                == Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
        {
            return None;
        }
        let Some((start, end)) = local.turn_start_offset.zip(ctx.tmux_last_offset) else {
            return None;
        };
        let Some(binding) = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(authority) else {
            return None;
        };
        let Some(path) = local
            .output_path
            .as_deref()
            .and_then(|p| std::fs::canonicalize(p).ok())
        else {
            return None;
        };
        if local.runtime_kind != Some(binding.runtime_kind)
            || local.session_id.as_deref().filter(|s| !s.is_empty())
                != binding.session_id.as_deref().filter(|s| !s.is_empty())
            || local.session_id.as_deref().is_none_or(str::is_empty)
            || std::fs::canonicalize(binding.relay_output_path())
                .ok()
                .as_ref()
                != Some(&path)
            || binding.relay_last_offset() < end
        {
            return None;
        }
        (
            dr::ExactJsonlSourceIdentity {
                provider: ctx.provider.as_str().to_owned(),
                tmux_session_name: tmux.to_owned(),
                turn_nonce: local.turn_nonce.clone().unwrap_or_default(),
                range: (start, end),
                generation_mtime_ns: dr::current_generation_mtime_ns(tmux),
                offset_authority_channel_id: ctx.watcher_owner_channel_id.get(),
                delivery_channel_id: ctx.channel_id.get(),
            },
            path,
        )
    };
    if !source_matches_episode(
        &source,
        ctx.provider,
        local,
        ctx.watcher_owner_channel_id,
        ctx.channel_id,
    ) {
        return None;
    }
    let eof = std::fs::metadata(&path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len());
    if eof.is_none_or(|eof| source.range.1 > eof) {
        return None;
    }
    Some((source, path, eof?))
}

pub(super) fn recover_empty_body(ctx: ReceiptDecisionInput<'_>) -> Result<String, String> {
    if ctx.inflight_state.output_path.is_none() {
        return Ok(String::new());
    }
    let tmux = ctx
        .inflight_state
        .tmux_session_name
        .as_deref()
        .ok_or("empty terminal has no captured source session")?;
    crate::services::tmux_common::with_tmux_source_authority(tmux, |authority| {
        let (source, path, _) = verified_source_under_authority(&ctx, authority)
            .ok_or("empty terminal recovery has no verified captured source range")?;
        crate::services::discord::recovery_engine::extract_response_from_output_range(
            &path,
            source.range.0,
            source.range.1,
            ctx.inflight_state.runtime_kind
                == Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
        )
    })
}
