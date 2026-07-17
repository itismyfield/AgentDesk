//! Extracted from `services::discord::health` (#3038 Phase A) — verbatim
//! move; behavior unchanged. Bot-HTTP resolution by name plus the
//! channel-aware direct-meeting runtime/shared resolver pair.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::HealthRegistry;
use crate::services::discord::SharedData;
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::provider::ProviderKind;

/// Resolve the bot HTTP client by alias.
/// Utility aliases are parsed into a stable role before provider lookup.
pub async fn resolve_bot_http(
    registry: &HealthRegistry,
    bot: &str,
) -> Result<Arc<serenity::Http>, (&'static str, String)> {
    if let Some(role) = UtilityBotRole::from_alias(bot) {
        return resolve_utility_bot_http(registry, role).await;
    }

    // Look up provider bot (e.g. "claude", "codex").
    let clients = registry.discord_http.lock().await;
    for (name, http) in clients.iter() {
        if bot_names_match(name, bot) {
            return Ok(http.clone());
        }
    }
    Err((
        "400 Bad Request",
        format!(r#"{{"ok":false,"error":"unknown bot: {bot}"}}"#),
    ))
}

pub(crate) async fn resolve_utility_bot_http(
    registry: &HealthRegistry,
    role: UtilityBotRole,
) -> Result<Arc<serenity::Http>, (&'static str, String)> {
    match registry.utility_bot_http_clone(role).await {
        Some(http) => Ok(http),
        None => Err((
            "503 Service Unavailable",
            format!(
                r#"{{"ok":false,"error":"{} bot not configured (missing {})"}}"#,
                role.alias(),
                role.credential_label()
            ),
        )),
    }
}

fn bot_names_match(registered: &str, requested: &str) -> bool {
    let registered = registered.trim();
    let requested = requested.trim();
    if registered == requested || registered.eq_ignore_ascii_case(requested) {
        return true;
    }

    match (
        ProviderKind::from_str(registered),
        ProviderKind::from_str(requested),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

pub async fn fetch_channel_name(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    let http = resolve_bot_http(registry, provider.as_str()).await.ok()?;
    let channel = channel_id.to_channel(&*http).await.ok()?;
    channel.guild().map(|guild_channel| guild_channel.name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectMeetingRuntimeCandidate {
    index: usize,
    explicit_channel_match: bool,
    live_channel_match: bool,
}

fn select_direct_meeting_runtime_candidate(
    provider_name: &str,
    channel_id: ChannelId,
    candidates: &[DirectMeetingRuntimeCandidate],
) -> Result<Option<usize>, String> {
    let explicit_matches = candidates
        .iter()
        .filter(|candidate| candidate.explicit_channel_match)
        .map(|candidate| candidate.index)
        .collect::<Vec<_>>();
    if explicit_matches.len() > 1 {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!(
                "multiple runtimes explicitly allow channel {} for provider {}",
                channel_id.get(),
                provider_name
            ),
        })
        .to_string());
    }
    if let Some(index) = explicit_matches.first().copied() {
        return Ok(Some(index));
    }

    let live_matches = candidates
        .iter()
        .filter(|candidate| candidate.live_channel_match)
        .map(|candidate| candidate.index)
        .collect::<Vec<_>>();
    if live_matches.len() > 1 {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!(
                "multiple runtimes can handle channel {} for provider {}",
                channel_id.get(),
                provider_name
            ),
        })
        .to_string());
    }
    Ok(live_matches.first().copied())
}

pub(super) async fn resolve_direct_meeting_runtime(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: &ProviderKind,
) -> Result<(Arc<serenity::Http>, Arc<SharedData>), String> {
    let provider_name = owner_provider.as_str();
    let shared_candidates = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.eq_ignore_ascii_case(provider_name))
            .map(|(index, entry)| (index, entry.shared.clone()))
            .collect::<Vec<_>>()
    };

    if shared_candidates.is_empty() {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!("provider runtime not registered: {}", provider_name),
        })
        .to_string());
    }

    let mut candidate_matches = Vec::with_capacity(shared_candidates.len());
    for (index, shared) in &shared_candidates {
        let settings = shared.settings.read().await.clone();
        let explicit_channel_match = settings.allowed_channel_ids.contains(&channel_id.get());
        let live_channel_match = match shared.http.cached_serenity_ctx.get() {
            Some(ctx) => {
                crate::services::discord::provider_handles_channel(
                    ctx,
                    owner_provider,
                    &settings,
                    channel_id,
                )
                .await
            }
            None => false,
        };
        candidate_matches.push(DirectMeetingRuntimeCandidate {
            index: *index,
            explicit_channel_match,
            live_channel_match,
        });
    }

    if let Some(selected_index) =
        select_direct_meeting_runtime_candidate(provider_name, channel_id, &candidate_matches)?
    {
        let (_, shared) = shared_candidates
            .iter()
            .find(|(index, _)| *index == selected_index)
            .cloned()
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "selected runtime index vanished for provider {} on channel {}",
                        provider_name,
                        channel_id.get()
                    ),
                })
                .to_string()
            })?;
        let http = shared
            .http
            .cached_serenity_ctx
            .get()
            .map(|ctx| ctx.http.clone())
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "matched runtime is not ready for provider {} on channel {}",
                        provider_name,
                        channel_id.get()
                    ),
                })
                .to_string()
            })?;
        return Ok((http, shared));
    }

    if shared_candidates.len() == 1 {
        let (_, shared) = shared_candidates[0].clone();
        if let Some(ctx) = shared.http.cached_serenity_ctx.get() {
            return Ok((ctx.http.clone(), shared));
        }
        let http = resolve_bot_http(registry, provider_name)
            .await
            .map_err(|(_, body)| body)?;
        return Ok((http, shared));
    }

    Err(serde_json::json!({
        "ok": false,
        "error": format!(
            "could not resolve a unique runtime for provider {} on channel {}",
            provider_name,
            channel_id.get()
        ),
    })
    .to_string())
}

pub(super) async fn resolve_direct_meeting_shared(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: &ProviderKind,
) -> Result<Arc<SharedData>, String> {
    let provider_name = owner_provider.as_str();
    let shared_candidates = {
        let providers = registry.providers.lock().await;
        providers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.eq_ignore_ascii_case(provider_name))
            .map(|(index, entry)| (index, entry.shared.clone()))
            .collect::<Vec<_>>()
    };

    if shared_candidates.is_empty() {
        return Err(serde_json::json!({
            "ok": false,
            "error": format!("provider runtime not registered: {}", provider_name),
        })
        .to_string());
    }

    let mut candidate_matches = Vec::with_capacity(shared_candidates.len());
    for (index, shared) in &shared_candidates {
        let settings = shared.settings.read().await.clone();
        let explicit_channel_match = settings.allowed_channel_ids.contains(&channel_id.get());
        let live_channel_match = match shared.http.cached_serenity_ctx.get() {
            Some(ctx) => {
                crate::services::discord::provider_handles_channel(
                    ctx,
                    owner_provider,
                    &settings,
                    channel_id,
                )
                .await
            }
            None => false,
        };
        candidate_matches.push(DirectMeetingRuntimeCandidate {
            index: *index,
            explicit_channel_match,
            live_channel_match,
        });
    }

    if let Some(selected_index) =
        select_direct_meeting_runtime_candidate(provider_name, channel_id, &candidate_matches)?
    {
        let (_, shared) = shared_candidates
            .iter()
            .find(|(index, _)| *index == selected_index)
            .cloned()
            .ok_or_else(|| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "selected runtime index vanished for provider {} on channel {}",
                        provider_name,
                        channel_id.get()
                    ),
                })
                .to_string()
            })?;
        return Ok(shared);
    }

    if shared_candidates.len() == 1 {
        return Ok(shared_candidates[0].1.clone());
    }

    Err(serde_json::json!({
        "ok": false,
        "error": format!(
            "could not resolve a unique runtime for provider {} on channel {}",
            provider_name,
            channel_id.get()
        ),
    })
    .to_string())
}

#[cfg(test)]
mod direct_meeting_candidate_tests {
    //! #3038 Phase A characterization tests — pin the runtime-candidate
    //! selection behavior of `select_direct_meeting_runtime_candidate`
    //! before the health.rs directory decomposition.

    use poise::serenity_prelude::ChannelId;

    use super::{DirectMeetingRuntimeCandidate, select_direct_meeting_runtime_candidate};

    fn candidate(index: usize, explicit: bool, live: bool) -> DirectMeetingRuntimeCandidate {
        DirectMeetingRuntimeCandidate {
            index,
            explicit_channel_match: explicit,
            live_channel_match: live,
        }
    }

    #[test]
    fn no_candidates_resolves_to_none() {
        let selected = select_direct_meeting_runtime_candidate("claude", ChannelId::new(42), &[]);
        assert_eq!(selected, Ok(None));
    }

    #[test]
    fn single_explicit_match_wins_over_live_matches() {
        let candidates = [candidate(0, false, true), candidate(3, true, true)];
        let selected =
            select_direct_meeting_runtime_candidate("claude", ChannelId::new(42), &candidates);
        assert_eq!(selected, Ok(Some(3)));
    }

    #[test]
    fn multiple_explicit_matches_are_ambiguous() {
        let candidates = [candidate(0, true, false), candidate(1, true, false)];
        let error =
            select_direct_meeting_runtime_candidate("claude", ChannelId::new(42), &candidates)
                .unwrap_err();
        let body: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(
            body["error"],
            "multiple runtimes explicitly allow channel 42 for provider claude"
        );
    }

    #[test]
    fn single_live_match_is_selected_without_explicit_match() {
        let candidates = [candidate(0, false, false), candidate(2, false, true)];
        let selected =
            select_direct_meeting_runtime_candidate("claude", ChannelId::new(42), &candidates);
        assert_eq!(selected, Ok(Some(2)));
    }

    #[test]
    fn multiple_live_matches_are_ambiguous() {
        let candidates = [candidate(0, false, true), candidate(1, false, true)];
        let error =
            select_direct_meeting_runtime_candidate("claude", ChannelId::new(42), &candidates)
                .unwrap_err();
        let body: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(
            body["error"],
            "multiple runtimes can handle channel 42 for provider claude"
        );
    }
}
