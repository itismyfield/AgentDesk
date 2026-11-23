//! Completes Codex restore-parked owners whose terminal delivery this node already confirmed.

use std::path::Path;

use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;

pub(in crate::services::discord) fn finish_confirmed_parked_owners(
    tmux: &str,
    expected: Option<&crate::services::discord::inflight::CodexRange>,
) {
    use crate::services::{discord::outbound::delivery_record as dr, tui_prompt_dedupe as dedupe};
    for (turn, owner, source) in dedupe::codex_restore_parked_owners(tmux) {
        if expected.is_some_and(|range| {
            !source.matches_path(Path::new(&range.rollout_path))
                || source.session_id.as_deref() != Some(range.session_id.as_str())
                || owner.0 != range.source.delivery_channel_id
                || owner.1 != range.source.turn_nonce
                || owner.4 != range.identity.user_msg_id
                || owner.5 != range.source.range.0
        }) {
            continue;
        }
        let authority = expected.map_or_else(
            || {
                dr::watcher_owner_channel_for_delivery_channel(
                    &ProviderKind::Codex,
                    ChannelId::new(owner.0),
                    tmux,
                )
                .map_or(owner.0, |channel| channel.get())
            },
            |range| range.source.offset_authority_channel_id,
        );
        let Some(record) = dr::read_record(&ProviderKind::Codex, authority) else {
            continue;
        };
        let delivered = record.confirmed_deliveries.iter().find(|receipt| {
            let exact = &receipt.source;
            expected.is_none_or(|range| exact == &range.source)
                && exact.is_authoritative()
                && receipt.message_id != 0
                && receipt.delivery_channel_id == owner.0
                && exact.provider == ProviderKind::Codex.as_str()
                && exact.tmux_session_name == tmux
                && exact.turn_nonce == owner.1
                && exact.offset_authority_channel_id == authority
                && exact.delivery_channel_id == owner.0
                && exact.generation_mtime_ns == dr::current_generation_mtime_ns(tmux)
                && exact.range.0 == owner.5
                && source.record(tmux, exact.range.1).is_ok_and(|terminal| {
                    terminal["type"] == "event_msg"
                        && terminal["payload"]["type"] == "task_complete"
                        && terminal["payload"]["turn_id"].as_str() == Some(turn.as_str())
                })
        });
        if let Some(receipt) = delivered {
            dedupe::complete_codex_parked_owner(
                tmux,
                &turn,
                &owner,
                &source,
                receipt.source.range.1,
            );
        }
    }
}
