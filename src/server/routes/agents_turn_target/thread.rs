use std::time::Duration;

use axum::{Json, http::StatusCode};
use poise::serenity_prelude::ChannelId;
use serde_json::json;

use super::{AgentTurnTarget, resolve_direct_bound_target, target_error};
use crate::db::agents::AgentChannelBindings;
use crate::services::discord::health::{HealthRegistry, ThreadTurnRuntime};

type TargetError = (StatusCode, Json<serde_json::Value>);

pub(super) async fn resolve(
    registry: Option<&HealthRegistry>,
    bindings: &AgentChannelBindings,
    agent: &str,
    provider: Option<&str>,
    raw: &str,
) -> Result<AgentTurnTarget, TargetError> {
    let forbidden = || {
        target_error(
            StatusCode::FORBIDDEN,
            format!("channel override {raw} is not allowed for agent {agent}"),
        )
    };
    let channel = raw
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .map(ChannelId::new)
        .ok_or_else(forbidden)?;
    let candidates: Vec<_> = bindings
        .all_channels()
        .iter()
        .filter_map(|bound| resolve_direct_bound_target(bindings, agent, None, Some(bound)).ok())
        .filter(|target| target.channel_id != 0)
        .collect();
    if candidates.is_empty() {
        return Err(forbidden());
    }
    let registry = registry.ok_or_else(parent_unverified)?;
    let reads = futures::future::join_all(candidates.iter().map(|target| {
        ThreadTurnRuntime::read(
            registry,
            ChannelId::new(target.channel_id),
            &target.provider,
            channel,
        )
    }));
    let reads = tokio::time::timeout(Duration::from_secs(5), reads)
        .await
        .map_err(|_| parent_unverified())?;
    let mut observed = None;
    let mut verified = None;
    let mut unread = false;
    for (candidate, read) in candidates.into_iter().zip(reads) {
        let Ok((parent, runtime)) = read else {
            unread = true;
            continue;
        };
        if observed.is_some_and(|previous| previous != parent) {
            return Err(parent_unverified());
        }
        observed = Some(parent);
        if parent == Some(candidate.channel_id) {
            verified = Some((candidate, runtime));
        }
    }
    let Some((matched, runtime)) = verified else {
        if unread
            || observed.flatten().is_some_and(|parent| {
                super::super::agents::channel_override_is_allowed(&parent.to_string(), bindings)
            })
        {
            return Err(parent_unverified());
        }
        return Err(forbidden());
    };
    if !runtime.parent_consistent() {
        return Err(parent_unverified());
    }
    let mut target =
        resolve_direct_bound_target(bindings, agent, provider, Some(&matched.primary_channel))?;
    target.channel_id = channel.get();
    target.thread = Some(runtime);
    Ok(target)
}

fn parent_unverified() -> TargetError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"ok":false,"status":"thread_parent_unverified","retryable":true})),
    )
}
