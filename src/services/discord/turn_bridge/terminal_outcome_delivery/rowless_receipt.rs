//! #5521: a row is lifecycle evidence, never a substitute for an exact receipt.

use super::*;
use crate::services::discord::{
    inflight::{InflightTurnIdentity, load_inflight_state_read_only},
    outbound::{delivery_frontier_probe, delivery_record as dr},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalReceiptDisposition {
    Continue,
    AlreadyDelivered,
    ForeignAnchor,
}

pub(super) fn decision(
    ctx: &TerminalOutcomeDeliveryContext,
    state: &TerminalOutcomeDeliveryState,
) -> TerminalReceiptDisposition {
    use TerminalReceiptDisposition::*;
    let local = &state.inflight_state;
    let identity = InflightTurnIdentity::from_state(local);
    let fresh = load_inflight_state_read_only(&state.provider, local.channel_id);
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
        return Continue;
    }
    let Some(tmux) = local.tmux_session_name.as_deref().filter(|s| !s.is_empty()) else {
        return fallback;
    };
    crate::services::tmux_common::with_tmux_source_authority(tmux, |authority| {
        let (source, path) = if let Some(admitted) = ctx.codex_tui_terminal_range.as_ref() {
            // Re-use the captured terminal range even after its row disappears.
            // revalidated_source intentionally refuses a missing row for NEW
            // publication; that refusal does not invalidate a live exact receipt.
            if !admitted.identity.matches_state(local)
                || admitted.result != state.full_response
                || !admitted.source_authority_is_live(authority)
            {
                return fallback;
            }
            let Some(path) = admitted.live_source_path() else {
                return fallback;
            };
            (admitted.source.clone(), path)
        } else {
            // #5264: a non-admitted CodexTui range remains honest legacy/NoRange.
            if state.provider == ProviderKind::Codex
                && local.runtime_kind
                    == Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
            {
                return fallback;
            }
            let Some((start, end)) = local.turn_start_offset.zip(ctx.tmux_last_offset) else {
                return fallback;
            };
            let Some(binding) = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(authority) else {
                return fallback;
            };
            let Some(path) = local
                .output_path
                .as_deref()
                .and_then(|p| std::fs::canonicalize(p).ok())
            else {
                return fallback;
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
                return fallback;
            }
            (
                dr::ExactJsonlSourceIdentity {
                    provider: state.provider.as_str().to_owned(),
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
        if !source.is_authoritative()
            || source.provider != state.provider.as_str()
            || source.tmux_session_name != tmux
            || Some(source.turn_nonce.as_str()) != local.turn_nonce.as_deref()
            || Some(source.range.0) != local.turn_start_offset
            || source.offset_authority_channel_id != ctx.watcher_owner_channel_id.get()
            || source.delivery_channel_id != ctx.channel_id.get()
            || source.generation_mtime_ns != dr::current_generation_mtime_ns(tmux)
        {
            return fallback;
        }
        let eof = std::fs::metadata(path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        if eof.is_none_or(|eof| source.range.1 > eof) {
            return fallback;
        }
        let Some(anchor) = delivery_frontier_probe::current_generation_delivered_anchor(
            &state.provider,
            ctx.watcher_owner_channel_id,
            tmux,
            eof,
        ) else {
            return fallback;
        };
        if anchor.range.0 <= source.range.0
            && anchor.range.1 >= source.range.1
            && anchor.panel_channel_id == ctx.channel_id.get()
            && dr::confirmed_delivery_receipt_exists(
                &state.provider,
                ctx.channel_id,
                anchor.panel_msg_id,
                &source,
            )
        {
            // Both same-anchor retries and a receipt on another anchor are
            // settled without touching either Discord message.
            AlreadyDelivered
        } else {
            fallback
        }
    })
}
