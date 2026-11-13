//! Subagent rollout consumer (#3086).
//!
//! Reconstructs the Claude TUI's `Done (N tool uses · M tokens · Xs)` summary
//! for a finished subagent. The live relay/status hot path uses ONLY the
//! in-stream fast path (no disk IO): the parent transcript's Task `tool_result`
//! record carries a top-level `toolUseResult` object with `totalToolUseCount`
//! / `totalTokens` / `totalDurationMs` (and `agentId`). This is the exact same
//! accounting the TUI renders and needs no disk IO. Any field the aggregate
//! omits is simply left empty and the render layer degrades to a partial
//! `Done (...)` line.
//!
//! Reading the (potentially large) per-subagent `agent-<id>.jsonl` rollout is
//! deliberately NOT an option here: it would be an unbounded, blocking read on
//! the async relay loop (#3086 P1).

pub(super) use crate::services::claude_tui::activity::events::{
    agent_id_from_tool_use_result, summary_from_tool_use_result, tool_use_result_is_async_launch,
};
use serde_json::Value;

use crate::services::agent_protocol::{StatusEvent, SubagentSummary};

pub(super) fn description_from_tool_use_result(value: &Value) -> Option<String> {
    value
        .get("toolUseResult")?
        .as_object()?
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|desc| !desc.is_empty())
        .map(str::to_string)
}

/// Builds the subagent [`SubagentSummary`] `(summary, agent_id, desc)` triple
/// from a JSON object's `toolUseResult` aggregate — an individual `tool_result`
/// block (batched) or the whole `user` record (legacy single). `None` for
/// ordinary results. #3086 P1: live hot path — in-stream aggregate only (no disk
/// IO); the prior synchronous rollout `read_to_string` was removed.
pub(super) fn subagent_completion_from_record(
    value: &Value,
) -> Option<(SubagentSummary, Option<String>, Option<String>)> {
    let (summary, agent_id) = summary_from_tool_use_result(value)?;
    let desc = description_from_tool_use_result(value);
    Some((summary, agent_id, desc))
}

/// #4396: terminal events for an id-bearing `tool_result` block that carries NO
/// aggregate; `None` for an id-less block (the caller's plain-`ToolEnd` path).
///
/// A batched record carries ONE record-level `toolUseResult` aggregate, owned
/// by the first id-bearing block — the OTHER parallel Tasks' results used to
/// fall through to a bare `ToolEnd`, leaving their slots permanently unfinished
/// (no footer ✓/✗, and the #4367 live filter never hides them). The record does
/// not name the block's tool, so emit a summary-less id-bearing `SubagentEnd`
/// and let the panel decide: it applies an id-bearing end ONLY on an exact
/// `tool_use_id` match against a slot a real Task launch opened (agent_id/desc
/// are `None`, so the #4177 fallback cannot fire) — an ordinary tool's result
/// id matches no subagent slot and is a no-op. `ack_only: !is_error` mirrors
/// `status_events_from_tool_result_with_id`: a successful BACKGROUND launch ack
/// must not finalize its still-running slot, while a foreground completion (or
/// a failed launch) finalizes.
pub(super) fn idful_tool_result_close_events(
    block: &Value,
    is_error: bool,
) -> Option<Vec<StatusEvent>> {
    let id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    Some(vec![
        StatusEvent::ToolEnd { success: !is_error },
        StatusEvent::SubagentEnd {
            success: !is_error,
            agent_id: None,
            desc: None,
            tool_use_id: Some(id.to_string()),
            summary: None,
            ack_only: !is_error,
        },
    ])
}

/// #3920: background-promotion [`StatusEvent`]s for a `user`-record `tool_result`
/// block (`blocks[idx]`) that is a SUCCESSFUL async/background Agent launch ack —
/// either its OWN `toolUseResult` is async (batched per-block shape) or the
/// RECORD-level `toolUseResult` is async and this is the first id-bearing block
/// (legacy single-subagent shape). Such an ack is NOT a completion: the subagent
/// keeps running detached and outlives the launching turn, so the panel re-affirms
/// its slot as `background: true` (keyed by the block's tool-use id). That keeps
/// it alive across turn-boundary resets, parity with a Bash `run_in_background`
/// task; without it the foreground-looking slot is dropped a turn later (the
/// #3920 root cause). A FAILED launch returns `None` — the agent never started.
pub(super) fn async_launch_promote_events(
    value: &Value,
    blocks: &[Value],
    idx: usize,
    is_error: bool,
) -> Option<Vec<StatusEvent>> {
    let block = blocks.get(idx)?;
    if is_error {
        return None;
    }
    let record_owns_launch =
        tool_use_result_is_async_launch(value) && Some(idx) == first_idful_tool_result_idx(blocks);
    let is_async_launch = tool_use_result_is_async_launch(block) || record_owns_launch;
    if !is_async_launch {
        return None;
    }
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let agent_id = agent_id_from_tool_use_result(block).or_else(|| {
        record_owns_launch
            .then(|| agent_id_from_tool_use_result(value))
            .flatten()
    });
    let desc = tool_use_id
        .as_ref()
        .is_none()
        .then(|| {
            description_from_tool_use_result(block).or_else(|| {
                record_owns_launch
                    .then(|| description_from_tool_use_result(value))
                    .flatten()
            })
        })
        .flatten();
    Some(vec![
        StatusEvent::ToolEnd { success: true },
        StatusEvent::SubagentStart {
            subagent_type: None,
            desc,
            agent_id,
            tool_use_id,
            background: true,
        },
    ])
}

/// First id-bearing `tool_result` block — the owner of any RECORD-level
/// `toolUseResult` aggregate (legacy single-subagent shape).
fn first_idful_tool_result_idx(blocks: &[Value]) -> Option<usize> {
    blocks.iter().position(|block| {
        block.get("type").and_then(Value::as_str) == Some("tool_result")
            && block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty())
    })
}

/// Feed exact child observations into the existing footer owner; no description fallback.
pub(super) fn refresh_observed_children(
    live: &super::PlaceholderLiveEvents,
    channel: poise::serenity_prelude::ChannelId,
    provider: &crate::services::provider::ProviderKind,
) {
    use crate::services::claude_tui::activity::{self, Background};
    if !matches!(provider, crate::services::provider::ProviderKind::Claude) {
        return;
    }
    let Some(snapshot) = activity::cached_for_channel(channel.get()) else {
        return;
    };
    let Some(entry) = live.status_by_channel.get(&channel) else {
        return;
    };
    let mut state = entry.lock().unwrap_or_else(|e| e.into_inner());
    if !snapshot.current() {
        return;
    }
    // Child observations do not change the parent's status or completion clock.
    let parent_status = state.status.clone();
    let parent_completed_at = state.completed_at;
    for child in snapshot.children {
        let event = match (child.subagent, child.finished) {
            (true, Some(success)) => StatusEvent::SubagentEnd {
                success,
                agent_id: None,
                desc: None,
                tool_use_id: Some(child.tool_id),
                summary: None,
                ack_only: false,
            },
            (true, None) => StatusEvent::SubagentStart {
                subagent_type: None,
                desc: None,
                agent_id: child.agent_id,
                tool_use_id: Some(child.tool_id),
                background: true,
            },
            (false, Some(success)) => StatusEvent::BackgroundTaskEnd {
                tool_use_id: child.tool_id,
                success,
            },
            (false, None) => StatusEvent::BackgroundTaskStart {
                name: child.tool_name,
                summary: "Background task".into(),
                tool_use_id: child.tool_id,
            },
        };
        state.apply(event);
    }
    state.status = parent_status;
    state.completed_at = parent_completed_at;
    state.background_agent_pending = snapshot.background != Background::Clear;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_use_result_summary_extracts_all_fields() {
        let value = json!({
            "type": "user",
            "toolUseResult": {
                "agentId": "a5e810f97737bf4bd",
                "totalToolUseCount": 38,
                "totalTokens": 90157,
                "totalDurationMs": 109275,
            }
        });
        let (summary, agent_id) = summary_from_tool_use_result(&value).expect("subagent result");
        assert_eq!(agent_id.as_deref(), Some("a5e810f97737bf4bd"));
        assert_eq!(summary.tool_count, Some(38));
        assert_eq!(summary.tokens, Some(90157));
        // 109275ms → 110s (ceil) — TUI rounds up partial seconds.
        assert_eq!(summary.duration_secs, Some(110));
    }

    #[test]
    fn tool_use_result_async_launch_ack_is_not_a_completion_summary() {
        let value = json!({
            "type": "user",
            "toolUseResult": {
                "isAsync": true,
                "status": "async_launched",
                "agentId": "a31353d794c259eb9",
                "description": "...",
                "prompt": "...",
                "outputFile": "...",
                "canReadOutputFile": true
            }
        });

        assert!(summary_from_tool_use_result(&value).is_none());
    }

    #[test]
    fn tool_use_result_agent_id_without_accounting_is_not_a_completion_summary() {
        let values = [
            json!({
                "type": "user",
                "toolUseResult": {
                    "isAsync": false,
                    "status": "async_launched-ish",
                    "agentId": "a31353d794c259eb9"
                }
            }),
            json!({
                "type": "user",
                "toolUseResult": {
                    "isAsync": false,
                    "agentId": "a31353d794c259eb9"
                }
            }),
            json!({
                "type": "user",
                "toolUseResult": {
                    "isAsync": false,
                    "status": "async_launched",
                    "agentId": "a31353d794c259eb9"
                }
            }),
        ];

        for value in values {
            assert!(
                summary_from_tool_use_result(&value).is_none(),
                "agentId-only result must not classify as completion: {value}"
            );
        }
    }

    #[test]
    fn tool_use_result_without_agent_id_but_with_accounting_is_a_completion_summary() {
        let value = json!({
            "type": "user",
            "toolUseResult": {
                "totalToolUseCount": 2,
                "totalTokens": 1234,
                "totalDurationMs": 1500
            }
        });

        let (summary, agent_id) = summary_from_tool_use_result(&value).expect("completion summary");
        assert_eq!(agent_id, None);
        assert_eq!(summary.tool_count, Some(2));
        assert_eq!(summary.tokens, Some(1234));
        assert_eq!(summary.duration_secs, Some(2));
    }

    #[test]
    fn tool_use_result_string_is_not_a_subagent() {
        let value = json!({ "type": "user", "toolUseResult": "ok" });
        assert!(summary_from_tool_use_result(&value).is_none());
    }

    #[test]
    fn tool_use_result_missing_is_none() {
        let value = json!({ "type": "user" });
        assert!(summary_from_tool_use_result(&value).is_none());
    }
}
