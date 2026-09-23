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

const CHANNEL_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

// Explicit channel ownership is authoritative. Do not wait for Discord
// lookups on unrelated bots before resolving a configured owner. In particular,
// health snapshots have a short deadline and would otherwise report a detached
// watcher when an unrelated bot's channel lookup stalls.
async fn select_candidate_with_live_probe<F, Fut>(
    provider_name: &str,
    channel_id: ChannelId,
    mut candidates: Vec<DirectMeetingRuntimeCandidate>,
    mut probe: F,
) -> Result<Option<usize>, String>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if candidates
        .iter()
        .any(|candidate| candidate.explicit_channel_match)
    {
        return select_direct_meeting_runtime_candidate(provider_name, channel_id, &candidates);
    }
    for candidate in &mut candidates {
        candidate.live_channel_match = probe(candidate.index).await;
    }
    select_direct_meeting_runtime_candidate(provider_name, channel_id, &candidates)
}

async fn resolve_channel_candidate(
    provider_name: &str,
    channel_id: ChannelId,
    owner_provider: &ProviderKind,
    shared_candidates: &[(usize, Arc<SharedData>)],
) -> Result<Option<usize>, String> {
    let mut snapshots = Vec::with_capacity(shared_candidates.len());
    let mut candidates = Vec::with_capacity(shared_candidates.len());
    for (index, shared) in shared_candidates {
        let settings = shared.settings.read().await.clone();
        candidates.push(DirectMeetingRuntimeCandidate {
            index: *index,
            explicit_channel_match: settings.allowed_channel_ids.contains(&channel_id.get()),
            live_channel_match: false,
        });
        snapshots.push((*index, shared, settings));
    }
    // A worker has no gateway Context. Resolve a thread's authoritative parent
    // once, then reuse the configured allowlists instead of probing every bot.
    // Keep explicit child bindings ahead of inherited parent bindings.
    if !candidates
        .iter()
        .any(|candidate| candidate.explicit_channel_match)
        && snapshots.iter().any(|(_, shared, settings)| {
            shared.http.cached_serenity_ctx.get().is_none()
                && !settings.allowed_channel_ids.is_empty()
        })
    {
        let lookup = async {
            for (_, shared, _) in &snapshots {
                if let Some(http) = shared.serenity_http_or_token_fallback()
                    && let Ok(channel) = channel_id.to_channel(&http).await
                {
                    return Ok(channel);
                }
            }
            Err("no registered bot can resolve the runner channel".to_string())
        };
        let channel = tokio::time::timeout(CHANNEL_LOOKUP_TIMEOUT, lookup)
            .await
            .map_err(|_| "runner channel ownership lookup timed out".to_string())??;
        if let serenity::Channel::Guild(channel) = channel
            && matches!(
                channel.kind,
                serenity::ChannelType::PublicThread
                    | serenity::ChannelType::PrivateThread
                    | serenity::ChannelType::NewsThread
            )
            && let Some(parent) = channel.parent_id
        {
            for candidate in &mut candidates {
                candidate.explicit_channel_match = snapshots.iter().any(|(index, _, settings)| {
                    *index == candidate.index
                        && settings.allowed_channel_ids.contains(&parent.get())
                });
            }
        }
    }
    select_candidate_with_live_probe(provider_name, channel_id, candidates, |index| {
        let snapshot = snapshots
            .iter()
            .find(|(candidate_index, _, _)| *candidate_index == index);
        async move {
            let Some((_, shared, settings)) = snapshot else {
                return false;
            };
            match shared.http.cached_serenity_ctx.get() {
                Some(ctx) => {
                    crate::services::discord::provider_handles_channel(
                        ctx,
                        owner_provider,
                        settings,
                        channel_id,
                    )
                    .await
                }
                None => false,
            }
        }
    })
    .await
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
        let http = shared.serenity_http_or_token_fallback().ok_or_else(|| {
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

    if shared_candidates.len() == 1 && unbound_single_runtime_allowed(&shared_candidates[0].1).await
    {
        let (_, shared) = shared_candidates[0].clone();
        if let Some(http) = shared.serenity_http_or_token_fallback() {
            return Ok((http, shared));
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

pub(crate) struct IntakeWorkerRuntime {
    pub(crate) http: Arc<serenity::Http>,
    pub(crate) shared: Arc<SharedData>,
    pub(crate) token: String,
}

/// A provider queue may be polled by several bots. The claimant is not the
/// channel owner: all execution state and REST credentials must come from the
/// same channel-resolved runtime before the worker accepts the row.
pub(crate) async fn resolve_intake_worker_runtime(
    claimant: &SharedData,
    channel_id: ChannelId,
) -> Result<IntakeWorkerRuntime, String> {
    let registry = claimant
        .health_registry
        .upgrade()
        .ok_or_else(|| "runner runtime registry unavailable".to_string())?;
    let (http, shared) =
        resolve_direct_meeting_runtime(&registry, channel_id, &claimant.provider).await?;
    let token = shared
        .http
        .cached_bot_token
        .get()
        .cloned()
        .ok_or_else(|| "channel owner Discord REST credentials unavailable".to_string())?;
    Ok(IntakeWorkerRuntime {
        http,
        shared,
        token,
    })
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

    if shared_candidates.len() == 1 && unbound_single_runtime_allowed(&shared_candidates[0].1).await
    {
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

async fn unbound_single_runtime_allowed(shared: &SharedData) -> bool {
    // During worker startup the eventual channel owner may not be registered
    // yet. A single restricted REST runtime is not proof that it owns every
    // channel. Preserve legacy gateway and unrestricted single-bot behavior.
    shared.http.cached_serenity_ctx.get().is_some()
        || shared.settings.read().await.allowed_channel_ids.is_empty()
}

#[cfg(test)]
mod direct_meeting_candidate_tests {
    //! #3038 Phase A characterization tests — pin the runtime-candidate
    //! selection behavior of `select_direct_meeting_runtime_candidate`
    //! before the health.rs directory decomposition.

    use poise::serenity_prelude::ChannelId;

    use super::{
        DirectMeetingRuntimeCandidate, select_candidate_with_live_probe,
        select_direct_meeting_runtime_candidate,
    };

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
    #[tokio::test]
    async fn explicit_owner_does_not_wait_for_unrelated_live_probes() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            select_candidate_with_live_probe(
                "codex",
                ChannelId::new(42),
                vec![candidate(0, false, false), candidate(3, true, false)],
                |_| std::future::pending::<bool>(),
            ),
        )
        .await
        .expect("explicit ownership must not await Discord");
        assert_eq!(result, Ok(Some(3)));
    }

    #[tokio::test]
    async fn duplicate_explicit_owners_fail_without_live_probes() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            select_candidate_with_live_probe(
                "codex",
                ChannelId::new(42),
                vec![candidate(0, true, false), candidate(3, true, false)],
                |_| std::future::pending::<bool>(),
            ),
        )
        .await
        .expect("ambiguous configuration must not await Discord");
        assert!(
            result
                .unwrap_err()
                .contains("multiple runtimes explicitly allow")
        );
    }

    #[tokio::test]
    async fn unconfigured_channel_still_checks_all_live_candidates() {
        let mut probed = Vec::new();
        let result = select_candidate_with_live_probe(
            "codex",
            ChannelId::new(42),
            vec![candidate(0, false, false), candidate(3, false, false)],
            |index| {
                probed.push(index);
                std::future::ready(true)
            },
        )
        .await;
        assert_eq!(probed, vec![0, 3]);
        assert!(result.unwrap_err().contains("multiple runtimes can handle"));
    }

    #[tokio::test]
    async fn worker_claimant_uses_channel_owner_credentials_without_gateway() {
        use crate::services::discord::{health::HealthRegistry, make_shared_data_for_tests};
        use std::sync::Arc;
        let registry = Arc::new(HealthRegistry::new());
        let mut claimant = make_shared_data_for_tests();
        Arc::get_mut(&mut claimant).unwrap().health_registry = Arc::downgrade(&registry);
        let owner = make_shared_data_for_tests();
        claimant.settings.write().await.allowed_channel_ids = vec![41];
        owner.settings.write().await.allowed_channel_ids = vec![42];
        claimant
            .http
            .cached_bot_token
            .set("claimant-test-token".into())
            .unwrap();
        owner
            .http
            .cached_bot_token
            .set("owner-test-token".into())
            .unwrap();
        registry.register("claude".into(), claimant.clone()).await;
        registry.register("claude".into(), owner.clone()).await;
        let runtime = super::resolve_intake_worker_runtime(&claimant, ChannelId::new(42))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&runtime.shared, &owner));
        assert_eq!(runtime.token, "owner-test-token");
        assert!(runtime.shared.http.cached_serenity_ctx.get().is_none());
        claimant.settings.write().await.allowed_channel_ids.push(42);
        assert!(
            super::resolve_intake_worker_runtime(&claimant, ChannelId::new(42))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn worker_owner_without_rest_credentials_is_rejected() {
        use crate::services::discord::{health::HealthRegistry, make_shared_data_for_tests};
        use std::sync::Arc;
        let registry = Arc::new(HealthRegistry::new());
        let mut claimant = make_shared_data_for_tests();
        Arc::get_mut(&mut claimant).unwrap().health_registry = Arc::downgrade(&registry);
        let owner = make_shared_data_for_tests();
        claimant.settings.write().await.allowed_channel_ids = vec![41];
        owner.settings.write().await.allowed_channel_ids = vec![42];
        claimant
            .http
            .cached_bot_token
            .set("claimant-test-token".into())
            .unwrap();
        registry.register("claude".into(), claimant.clone()).await;
        registry.register("claude".into(), owner).await;
        assert!(
            super::resolve_intake_worker_runtime(&claimant, ChannelId::new(42))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn restricted_worker_cannot_take_an_unbound_channel_during_startup() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        assert!(super::unbound_single_runtime_allowed(&shared).await);
        shared.settings.write().await.allowed_channel_ids = vec![41];
        assert!(!super::unbound_single_runtime_allowed(&shared).await);
    }
}
