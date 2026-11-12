//! HTTP ingress supplies original envelope evidence after source admission and local Stop handling.
use super::{Source, entry, fold, installable, ledger, read_source};
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_prompt_dedupe::binding_context::{self as context, CapturedContext};
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde_json::Value;

pub(crate) fn note_hook(
    accepted: bool,
    headers: &HeaderMap,
    provider: &str,
    kind: &HookEventKind,
    payload: &Value,
    continuing: bool,
) {
    if !accepted || provider != "claude" {
        return;
    }
    let Some(envelope) = headers
        .get(context::BINDING_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| context::decode_binding_header(h).ok())
    else {
        return;
    };
    let CapturedContext::Captured(ctx) = envelope.context else {
        return;
    };
    let Some(at) = headers
        .get("x-agentdesk-relay-published-at")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
    else {
        return;
    };
    if at > Utc::now()
        || at < ctx.created_at
        || !context::execution_context("claude", &ctx.execution_nonce)
            .is_ok_and(|canonical| canonical == ctx)
    {
        return;
    }
    let Some(source) = Source::current(&ctx.tmux_session) else {
        return;
    };
    if source.nonce != ctx.execution_nonce
        || at < source.visit
        || payload["session_id"].as_str() != Some(&source.native)
        || payload["transcript_path"]
            .as_str()
            .map(std::path::Path::new)
            != Some(source.path.as_path())
        || ((payload["agent_id"].is_string() || payload["agent_transcript_path"].is_string())
            && *kind != HookEventKind::SubagentStop)
    {
        return;
    }
    super::poll::start();
    let Some(mut frame) = read_source(&source) else {
        return;
    };
    if !matches!(
        kind,
        HookEventKind::PreToolUse | HookEventKind::PostToolUse | HookEventKind::SubagentStop
    ) && frame.caught_up()
        && frame.turn.as_ref().is_some_and(|turn| {
            at < turn.at
                || payload["prompt_id"]
                    .as_str()
                    .is_some_and(|id| Some(id) != turn.prompt.as_deref())
        })
    {
        return;
    }
    if *kind == HookEventKind::SubagentStop {
        frame.children.stop(&source, payload, at);
    }
    let mut state = ledger();
    if Source::current(&source.session).as_ref() != Some(&source) {
        return;
    }
    let Some(entry) = entry(&mut state, &source) else {
        return;
    };
    if !installable(entry, &source, &frame) {
        return;
    }
    let before = entry.clone();
    fold(entry, &frame);
    if !entry.tainted {
        if continuing && *kind == HookEventKind::Stop {
            entry.continuation = frame.stamp.len;
        }
        if matches!(kind, HookEventKind::PreToolUse | HookEventKind::PostToolUse)
            && (!frame.caught_up() || frame.terminal == 0)
            && let Some(id) = payload["tool_use_id"].as_str()
        {
            entry.tools.record(
                &frame,
                id,
                at,
                *kind == HookEventKind::PostToolUse,
                payload["prompt_id"].as_str(),
            );
        }
    }
    entry.tools.reconcile(&frame);
    if *entry != before {
        entry.revision += 1;
        super::poll::CHANGED.notify_one();
    }
}
