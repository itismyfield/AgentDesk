use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;

use super::super::super::SharedData;
use super::super::super::outbound::delivery_frontier_probe;
use super::GuardedCleanupTargetAuthor;
use super::ops::strip_placeholder_indicators_for_preserve;
use crate::services::discord;

pub(super) fn placeholder_real_body_exposure_evidence(
    provider: &ProviderKind,
    _response_sent_offset: usize,
    last_edit_text: &str,
) -> Option<&'static str> {
    let stripped_body =
        discord::single_message_panel::strip_placeholder_terminal_status(last_edit_text, provider);
    if stripped_body.trim().is_empty() {
        None
    } else {
        Some("last_edit_text_body")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GuardedDeliveredElsewhereSignal {
    Protected { evidence: &'static str },
    Found { evidence: &'static str },
    NotFound,
    Ambiguous { evidence: &'static str },
}

impl GuardedDeliveredElsewhereSignal {
    fn evidence(self) -> &'static str {
        match self {
            Self::Protected { evidence }
            | Self::Found { evidence }
            | Self::Ambiguous { evidence } => evidence,
            Self::NotFound => "no_delivered_elsewhere_signal",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GuardedNonterminalDeleteDecision {
    PreserveNoEdit {
        evidence: &'static str,
    },
    PreserveWithStrip {
        evidence: &'static str,
        cleaned_body: String,
    },
    Delete {
        evidence: &'static str,
    },
}
fn delivered_frontier_same_coordinate_space(
    anchor: delivery_frontier_probe::CurrentGenerationAnchor,
    current_output_eof: Option<u64>,
    live_committed_end: Option<u64>,
) -> Result<(), &'static str> {
    let Some(current_output_eof) = current_output_eof else {
        return Err("current_generation_anchor_coordinate_unproven");
    };
    if anchor.range.1 > current_output_eof {
        return Err("current_generation_anchor_exceeds_current_eof");
    }
    match live_committed_end {
        Some(committed_end) if committed_end == anchor.range.1 => Ok(()),
        _ => Err("current_generation_anchor_live_frontier_mismatch"),
    }
}

pub(super) fn guarded_cleanup_delivered_elsewhere_signal_from_anchor(
    channel_id: ChannelId,
    message_id: MessageId,
    delivered_range: (u64, u64),
    anchor: delivery_frontier_probe::CurrentGenerationAnchor,
    current_output_eof: Option<u64>,
    live_committed_end: Option<u64>,
) -> GuardedDeliveredElsewhereSignal {
    let (range_start, range_end) = delivered_range;
    if range_end <= range_start {
        return GuardedDeliveredElsewhereSignal::NotFound;
    }
    if anchor.panel_channel_id == channel_id.get() && anchor.panel_msg_id == message_id.get() {
        return GuardedDeliveredElsewhereSignal::Protected {
            evidence: "current_generation_anchor_same_message",
        };
    }
    if range_start >= anchor.range.0 && range_end <= anchor.range.1 {
        return match delivered_frontier_same_coordinate_space(
            anchor,
            current_output_eof,
            live_committed_end,
        ) {
            Ok(()) => GuardedDeliveredElsewhereSignal::Found {
                evidence: "current_generation_anchor_different_message",
            },
            Err(evidence) => GuardedDeliveredElsewhereSignal::Ambiguous { evidence },
        };
    }
    GuardedDeliveredElsewhereSignal::Ambiguous {
        evidence: "current_generation_anchor_range_mismatch",
    }
}

pub(super) fn guarded_cleanup_delivered_elsewhere_signal(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    message_id: MessageId,
    delivered_range: Option<(u64, u64)>,
    current_output_eof: Option<u64>,
    live_committed_end: Option<u64>,
) -> GuardedDeliveredElsewhereSignal {
    let Some(delivered_range) = delivered_range else {
        return GuardedDeliveredElsewhereSignal::NotFound;
    };
    let Some(anchor) = delivery_frontier_probe::current_generation_delivered_anchor(
        provider,
        channel_id,
        tmux_session_name,
        current_output_eof,
    ) else {
        return GuardedDeliveredElsewhereSignal::NotFound;
    };
    guarded_cleanup_delivered_elsewhere_signal_from_anchor(
        channel_id,
        message_id,
        delivered_range,
        anchor,
        current_output_eof,
        live_committed_end,
    )
}

pub(super) fn guarded_nonterminal_delete_decision(
    provider: &ProviderKind,
    response_sent_offset: usize,
    last_edit_text: &str,
    committed_terminal_anchor: bool,
    delivered_elsewhere: GuardedDeliveredElsewhereSignal,
    target_author: GuardedCleanupTargetAuthor,
) -> GuardedNonterminalDeleteDecision {
    // Decision table (rows are evaluated in this order):
    // terminal anchor | * | * | * => PreserveNoEdit
    // no terminal anchor | Protected(same-message frontier) | * | * => PreserveNoEdit
    // no terminal anchor | Found(different message + same coordinate) | * | * => Delete
    // no terminal anchor | Ambiguous | body present | Watcher => PreserveWithStrip
    // no terminal anchor | Ambiguous | body present | CrossActor/Unknown => PreserveNoEdit
    // no terminal anchor | Ambiguous | body absent | * => PreserveNoEdit
    // no terminal anchor | NotFound | body present | Watcher => PreserveWithStrip
    // no terminal anchor | NotFound | body present | CrossActor/Unknown => PreserveNoEdit
    // no terminal anchor | NotFound | body absent | * => Delete
    //
    // The Found+absent row is the disposable chrome case where a different
    // message is positively proven to hold the delivered body. Ambiguous+absent
    // preserves fail-safe because there is no positive proof deletion is safe.
    if committed_terminal_anchor {
        return GuardedNonterminalDeleteDecision::PreserveNoEdit {
            evidence: "committed_terminal_anchor",
        };
    }
    if let GuardedDeliveredElsewhereSignal::Protected { evidence } = delivered_elsewhere {
        return GuardedNonterminalDeleteDecision::PreserveNoEdit { evidence };
    }
    if let GuardedDeliveredElsewhereSignal::Found { evidence } = delivered_elsewhere {
        return GuardedNonterminalDeleteDecision::Delete { evidence };
    }
    if let Some(evidence) =
        placeholder_real_body_exposure_evidence(provider, response_sent_offset, last_edit_text)
    {
        return match target_author {
            GuardedCleanupTargetAuthor::Watcher => {
                GuardedNonterminalDeleteDecision::PreserveWithStrip {
                    evidence,
                    cleaned_body: strip_placeholder_indicators_for_preserve(
                        last_edit_text,
                        provider,
                    ),
                }
            }
            GuardedCleanupTargetAuthor::CrossActor | GuardedCleanupTargetAuthor::Unknown => {
                GuardedNonterminalDeleteDecision::PreserveNoEdit { evidence }
            }
        };
    }
    if let GuardedDeliveredElsewhereSignal::Ambiguous { evidence } = delivered_elsewhere {
        return GuardedNonterminalDeleteDecision::PreserveNoEdit { evidence };
    }
    GuardedNonterminalDeleteDecision::Delete {
        evidence: delivered_elsewhere.evidence(),
    }
}

/// #4158 hardening — the terminal-committed cleanup gate.
///
/// The SkipAlreadyCommitted arm runs only AFTER the offset authority proved a
/// terminal delivery committed this range, so a LIVE placeholder there MIGHT be
/// the very message the session-bound sink delivered into (the sink's
/// `PlaceholderEdit(current_msg_id)` route, `session_relay_sink.rs`). The base
/// decision table's final `NotFound + body-absent → Delete` fallthrough is
/// correct for the no-response arm (nothing was delivered, so the placeholder is
/// disposable chrome), but in the terminal-committed arm that same row would
/// delete a sink-delivered body whenever no positive delivered-elsewhere anchor
/// exists — e.g. the durable delivered-frontier shadow is disabled
/// (`AGENTDESK_DELIVERY_RECORD_SHADOW` default OFF), so
/// `current_generation_delivered_anchor` returns `None` → `NotFound`, and the
/// sink's delivery marks `session_bound_delivered`, NOT the
/// `terminal_delivery_committed` that `committed_terminal_anchor_protects_delete`
/// keys on. So neither guard fires and the body-bearing placeholder is deleted.
///
/// Callers that pass `require_delivered_elsewhere_proof = true` therefore delete
/// ONLY on POSITIVE `Found` proof (a DIFFERENT message demonstrably holds the
/// committed range); every non-`Found` `Delete` is downgraded to a fail-safe
/// preserve. `Protected`/`Ambiguous`/body-exposure preserve paths are unchanged.
/// With the shadow anchor ENABLED (production) a real #4158 residue still yields
/// `Found` → delete, so the fix is unchanged there; this only closes the
/// shadow-disabled message-loss window.
pub(super) fn apply_terminal_committed_delete_proof_gate(
    decision: GuardedNonterminalDeleteDecision,
    has_positive_delivered_elsewhere: bool,
    require_delivered_elsewhere_proof: bool,
) -> GuardedNonterminalDeleteDecision {
    if require_delivered_elsewhere_proof
        && !has_positive_delivered_elsewhere
        && matches!(decision, GuardedNonterminalDeleteDecision::Delete { .. })
    {
        return GuardedNonterminalDeleteDecision::PreserveNoEdit {
            evidence: "terminal_committed_requires_delivered_elsewhere_proof",
        };
    }
    decision
}
fn cleanup_output_eof_from_path(path: &str) -> Option<u64> {
    if path.trim().is_empty() {
        return None;
    }
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

pub(super) fn cleanup_current_output_eof(
    shared: &SharedData,
    live_inflight: Option<&discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<u64> {
    if let Some(eof) = live_inflight
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .and_then(|state| state.output_path.as_deref())
        .and_then(cleanup_output_eof_from_path)
    {
        return Some(eof);
    }
    shared
        .tmux_watchers
        .watcher_output_path(tmux_session_name)
        .and_then(|path| cleanup_output_eof_from_path(&path))
}
