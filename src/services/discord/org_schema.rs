use std::collections::HashMap;

use poise::serenity_prelude::ChannelId;

use super::runtime_store::org_schema_path;
use super::settings::{RegisteredChannelBinding, RoleBinding, resolve_memory_settings};
use crate::domain::provider_auth_profile::FallbackPolicy;
use crate::services::agent_identity::{self, identity_from_parts, identity_label};
use crate::services::provider::ProviderKind;
use crate::services::provider_auth_profile::ProviderAuthProfileDef;
#[cfg(test)]
use crate::services::provider_auth_profile::fallback::FallbackPolicyRuntime;
use crate::utils::format::expand_tilde_string as expand_tilde;

// ─── Compatibility document access ─────────────────────────────────────────
use crate::services::org_catalog::configured_auth_profile;
use crate::services::org_catalog::{AgentDef, ChannelBinding, OrgSchema};

pub(in crate::services) use crate::services::org_catalog::parse_org_schema;

pub(crate) fn load_org_schema() -> Option<OrgSchema> {
    crate::services::org_catalog::load_runtime_document_at(org_schema_path().as_deref())
}

fn load_org_schema_for_auth()
-> Result<Option<crate::services::org_catalog::AuthCatalogSnapshot>, String> {
    let Some(path) = org_schema_path() else {
        return Ok(None);
    };
    crate::services::org_catalog::read_auth_catalog_at(&path)
}

pub(crate) fn provider_primary_profile(schema: &OrgSchema, provider: Option<&str>) -> String {
    crate::services::org_catalog::provider_primary_profile(
        schema.provider_auth_primary_profiles.as_ref(),
        provider,
    )
}

#[cfg(test)]
#[path = "org_schema/auth_selection_tests.rs"]
mod auth_selection_tests;
#[cfg(test)]
fn list_profile_bindings_from_schema(
    schema: OrgSchema,
) -> Vec<crate::domain::provider_auth_profile::ProfileBinding> {
    crate::services::provider_auth_profile::queries::list_profile_bindings_from_snapshot(
        schema.into(),
    )
}
#[cfg(test)]
pub(crate) use crate::services::provider_auth_profile::queries::provider_auth_catalog;

fn fallback_profile_available(
    provider: &ProviderKind,
    id: &str,
    agent_id: Option<&str>,
    catalog: &HashMap<String, ProviderAuthProfileDef>,
) -> bool {
    !crate::services::dispatch_gate::profile_deferred(provider, id, agent_id)
        && crate::services::provider_auth_profile::resolve(
            provider.clone(),
            Some(id),
            None,
            catalog,
        )
        .is_ok()
}

/// Called only after the bridge has checked the current turn identity.
pub(crate) fn advance_auth_profile(
    provider: &ProviderKind,
    channel: u64,
    request: u64,
) -> Result<Option<(String, String)>, String> {
    let Some(schema) = load_org_schema_for_auth()? else {
        return Ok(None);
    };
    let policy = schema
        .fallback_policies
        .get(provider.as_str())
        .cloned()
        .unwrap_or_default();
    let catalog = schema.profiles;
    let binding = resolve_role_binding(ChannelId::new(channel), None);
    Ok(crate::services::provider_auth_profile::fallback::fail(
        provider,
        channel,
        request,
        &policy,
        &catalog,
        |id| {
            fallback_profile_available(
                provider,
                id,
                binding.as_ref().map(|binding| binding.role_id.as_str()),
                &catalog,
            )
        },
    ))
}

pub(crate) fn spawn_auth_overlay(
    provider: ProviderKind,
    channel_id: Option<u64>,
) -> Result<crate::services::provider_auth_profile::ProviderAuthOverlay, String> {
    let context = crate::services::platform::active_provider_context(provider.as_str());
    let channel_id = channel_id.or_else(|| {
        context
            .as_ref()
            .and_then(|context| context.channel_id.as_deref())
            .and_then(|id| id.parse().ok())
    });
    let agent_id = context
        .as_ref()
        .and_then(|context| context.agent_id.clone())
        .or_else(|| {
            channel_id.and_then(|id| {
                resolve_role_binding(ChannelId::new(id), None).map(|binding| binding.role_id)
            })
        });
    spawn_auth_overlay_for_context(provider, channel_id, agent_id.as_deref())
}

/// The account a launch for the channel and agent is configured to use: a recovery pin first,
/// then the channel or agent setting, then the provider primary; `true` when a pin chose it.
fn configured_launch_profile(
    schema: Option<&crate::services::org_catalog::AuthCatalogSnapshot>,
    provider: &ProviderKind,
    channel_id: Option<u64>,
    agent_id: Option<&str>,
) -> Result<(Option<String>, bool), String> {
    let channel_profile = channel_id.and_then(|id| {
        let snapshot = schema?;
        let channel = id.to_string();
        snapshot
            .bindings
            .iter()
            .find(|input| {
                input.channel_id.as_deref() == Some(channel.as_str())
                    && agent_id.is_none_or(|agent| agent == input.agent_id)
                    && snapshot
                        .bindings
                        .iter()
                        .any(|agent| agent.channel_id.is_none() && agent.agent_id == input.agent_id)
            })
            .and_then(|input| input.channel_profile.clone())
    });
    let agent_profile = agent_id.and_then(|id| {
        schema?
            .bindings
            .iter()
            .find(|input| input.channel_id.is_none() && input.agent_id == id)
            .and_then(|input| input.agent_profile.clone())
    });
    let pinned_profile = channel_id
        .map(|id| {
            crate::services::agent_recovery::pinned_auth_profile(
                &id.to_string(),
                provider,
                agent_id,
            )
        })
        .transpose()?
        .flatten();
    let recovery_pinned = pinned_profile.is_some();
    let profile = pinned_profile.or_else(|| {
        configured_auth_profile(channel_profile.as_deref(), agent_profile.as_deref())
            .map(str::to_string)
            .or_else(|| {
                schema.map(|schema| {
                    crate::services::org_catalog::provider_primary_profile(
                        Some(&schema.primary_profiles),
                        Some(provider.as_str()),
                    )
                })
            })
    });
    Ok((profile, recovery_pinned))
}

/// The account a launch for the channel and agent would take, read without the fallback router a
/// launch updates; `None` when a fallback policy may choose another account at launch.
pub(crate) fn configured_launch_overlay(
    provider: ProviderKind,
    channel_id: u64,
    agent_id: Option<&str>,
) -> Result<Option<crate::services::provider_auth_profile::ProviderAuthOverlay>, String> {
    use crate::services::provider_auth_profile::resolve;
    let schema = load_org_schema_for_auth()?;
    let catalog = schema
        .as_ref()
        .map(|schema| schema.profiles.clone())
        .unwrap_or_default();
    let (profile, pinned) =
        configured_launch_profile(schema.as_ref(), &provider, Some(channel_id), agent_id)?;
    let fallback = schema
        .as_ref()
        .and_then(|schema| schema.fallback_policies.get(provider.as_str()))
        .is_some_and(|policy| policy.enabled);
    if fallback
        && !pinned
        && crate::services::provider_auth_profile::extra_account_login_supported(&provider)
    {
        return Ok(None);
    }
    let overlay = resolve(provider, profile.as_deref(), None, &catalog);
    overlay.map(Some).map_err(|error| error.to_string())
}

/// The configured launch account before fallback routing, validated so a typo never picks an account,
/// plus the channel and policy fallback routing would apply to it.
type LaunchParts = (
    crate::services::provider_auth_profile::ProviderAuthOverlay,
    Option<(u64, FallbackPolicy)>,
);

fn launch_overlay_parts(
    schema: Option<&crate::services::org_catalog::AuthCatalogSnapshot>,
    provider: &ProviderKind,
    channel_id: Option<u64>,
    agent_id: Option<&str>,
    catalog: &HashMap<String, ProviderAuthProfileDef>,
) -> Result<LaunchParts, String> {
    let (profile, recovery_pinned) =
        configured_launch_profile(schema, provider, channel_id, agent_id)?;
    let primary = crate::services::provider_auth_profile::resolve(
        provider.clone(),
        profile.as_deref(),
        None,
        catalog,
    )
    .map_err(|error| error.to_string())?;
    let route = channel_id.filter(|_| !recovery_pinned).map(|channel| {
        let policy = schema
            .and_then(|schema| schema.fallback_policies.get(provider.as_str()))
            .cloned()
            .unwrap_or_default();
        (channel, policy)
    });
    Ok((primary, route))
}

fn fallback_selects(provider: &ProviderKind, policy: &FallbackPolicy) -> bool {
    policy.enabled
        && crate::services::provider_auth_profile::extra_account_login_supported(provider)
}

fn spawn_auth_overlay_for_context(
    provider: ProviderKind,
    channel_id: Option<u64>,
    agent_id: Option<&str>,
) -> Result<crate::services::provider_auth_profile::ProviderAuthOverlay, String> {
    use crate::services::provider_auth_profile::resolve;

    let schema = load_org_schema_for_auth()?;
    let catalog = schema
        .as_ref()
        .map(|schema| schema.profiles.clone())
        .unwrap_or_default();
    let (primary_overlay, fallback_route) =
        launch_overlay_parts(schema.as_ref(), &provider, channel_id, agent_id, &catalog)?;
    let overlay = if let Some((channel, policy)) = fallback_route {
        let candidates = crate::services::org_catalog::fallback_candidates(
            &policy,
            &provider,
            &primary_overlay.profile_id,
            &catalog,
        );
        let selected = if fallback_selects(&provider, &policy) {
            crate::services::provider_auth_profile::fallback::select_for_launch(
                &provider,
                channel,
                &candidates,
                |id| fallback_profile_available(&provider, id, agent_id, &catalog),
            )
            .ok_or_else(|| format!("no configured {} auth profile", provider.as_str()))?
        } else {
            primary_overlay.profile_id.clone()
        };
        resolve(provider.clone(), Some(&selected), None, &catalog)
            .map_err(|error| error.to_string())?
    } else {
        primary_overlay
    };
    if let Some(binding) = channel_id.and_then(|id| resolve_role_binding(ChannelId::new(id), None))
    {
        if let Some(bound_provider) = binding.provider.clone() {
            let identity = identity_from_parts(
                &binding.role_id,
                &binding.role_id,
                bound_provider,
                binding.model.clone(),
                Some(binding.auth_profile.as_str()),
            );
            tracing::info!(
                identity = %identity_label(&identity),
                profile_id = %overlay.profile_id,
                home = overlay.home.as_ref().map(|path| path.display().to_string()),
                "provider spawn identity"
            );
        }
    }
    crate::services::turn_summary_line::quota::record_launch(channel_id, overlay)
}

pub(crate) fn overlay_from_tmux_session(
    provider: ProviderKind,
    session_name: &str,
) -> Result<crate::services::provider_auth_profile::ProviderAuthOverlay, String> {
    let context = crate::services::platform::active_provider_context(provider.as_str());
    let channel_id = context
        .as_ref()
        .and_then(|context| context.channel_id.as_deref())
        .and_then(|channel_id| channel_id.parse().ok())
        .or_else(|| {
            crate::services::provider::parse_provider_and_channel_from_tmux_name(session_name)
                .and_then(|(_, channel)| channel.parse().ok())
        });
    spawn_auth_overlay_for_context(
        provider,
        channel_id,
        context
            .as_ref()
            .and_then(|context| context.agent_id.as_deref()),
    )
}

/// Home the next launch reads, without touching the fallback router or quota record. Intake picks the
/// agent per message, so a home is promised only when every agent the launch could carry yields it.
pub(crate) fn predicted_launch_home(
    provider: ProviderKind,
    channel_id: Option<u64>,
    session_name: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    let channel_id = channel_id.or_else(|| {
        crate::services::provider::parse_provider_and_channel_from_tmux_name(session_name)
            .and_then(|(_, channel)| channel.parse().ok())
    });
    let (loaded, name) = (load_org_schema_for_auth()?, provider.as_str());
    let schema = loaded.as_ref();
    let catalog = schema
        .map(|schema| schema.profiles.clone())
        .unwrap_or_default();
    // No agent, an agent outside the schema, and each schema agent cover every launch context.
    let agents = [None, Some("\0unbound")]
        .into_iter()
        .chain(schema.into_iter().flat_map(|schema| {
            schema
                .bindings
                .iter()
                .filter(|input| input.channel_id.is_none())
                .map(|input| Some(input.agent_id.as_str()))
        }));
    let mut homes = Vec::new();
    for agent in agents {
        // An agent whose account cannot resolve for this provider never launches it, so it adds no home.
        let (primary, route) =
            match launch_overlay_parts(schema, &provider, channel_id, agent, &catalog) {
                Ok(resolved) => resolved,
                Err(_) if agent.is_some() => continue,
                Err(error) => return Err(error),
            };
        // Launch-time routing can only move off the primary when it has another candidate.
        let routed = route.is_some_and(|(_, policy)| {
            fallback_selects(&provider, &policy)
                && crate::services::org_catalog::fallback_candidates(
                    &policy,
                    &provider,
                    &primary.profile_id,
                    &catalog,
                )
                .len()
                    > 1
        });
        if routed {
            return Err(format!("{name} auth fallback picks the launch account"));
        }
        homes.push(primary.home);
    }
    homes.sort();
    homes.dedup();
    match homes.as_slice() {
        [home] => Ok(home.clone()),
        many => Err(format!(
            "the next {name} account depends on which agent the channel resolves to ({} homes)",
            many.len()
        )),
    }
}

pub(crate) fn api_agent_identity(
    agent_id: &str,
    db_provider: Option<&str>,
    db_name: Option<&str>,
) -> serde_json::Value {
    let schema = load_org_schema();
    let def = schema
        .as_ref()
        .and_then(|schema| schema.agents.get(agent_id));
    let provider = def
        .and_then(|def| def.provider.as_deref())
        .or(db_provider)
        .and_then(ProviderKind::from_str)
        .unwrap_or_else(|| ProviderKind::Unsupported(db_provider.unwrap_or("unknown").to_string()));
    let effective_profile =
        configured_auth_profile(None, def.and_then(|def| def.auth_profile.as_deref()))
            .map(str::to_string)
            .unwrap_or_else(|| {
                schema
                    .as_ref()
                    .map(|schema| provider_primary_profile(schema, Some(provider.as_str())))
                    .unwrap_or_else(|| "default".to_string())
            });
    let identity = identity_from_parts(
        agent_id,
        def.map(|def| def.display_name.clone())
            .or_else(|| db_name.map(str::to_string))
            .unwrap_or_else(|| agent_id.to_string()),
        provider,
        def.and_then(|def| def.model.clone()),
        Some(&effective_profile),
    );
    agent_identity::identity_json(&identity)
}

pub(super) fn org_schema_exists() -> bool {
    org_schema_path().map(|p| p.exists()).unwrap_or(false)
}

/// Check if a role_id exists in the org schema's agents map.
pub(super) fn is_known_agent(role_id: &str) -> Option<bool> {
    let schema = load_org_schema()?;
    Some(schema.agents.contains_key(role_id))
}

// ─── Resolution functions (mirror role_map.rs API) ──────────────────────────

/// Resolve a channel binding from org schema, returning the ChannelBinding
/// and the agent definition it refers to.
fn resolve_channel_binding<'a>(
    schema: &'a OrgSchema,
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<(&'a ChannelBinding, &'a AgentDef)> {
    let channels = schema.channels.as_ref()?;

    // 1. Try by_id
    if let Some(by_id) = &channels.by_id {
        let key = channel_id.get().to_string();
        if let Some(binding) = by_id.get(&key) {
            if let Some(agent_def) = schema.agents.get(&binding.agent) {
                return Some((binding, agent_def));
            }
        }
    }

    // 2. Try by_name (if enabled)
    if let Some(by_name) = &channels.by_name {
        let enabled = by_name.enabled.unwrap_or(false);
        if enabled {
            if let (Some(mappings), Some(cname)) = (&by_name.mappings, channel_name) {
                if let Some(binding) = mappings.get(cname) {
                    if let Some(agent_def) = schema.agents.get(&binding.agent) {
                        return Some((binding, agent_def));
                    }
                }
            }
        }
    }

    None
}

pub(super) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleBinding> {
    let schema = load_org_schema()?;
    let (ch_binding, agent_def) = resolve_channel_binding(&schema, channel_id, channel_name)?;

    // Channel-level overrides take priority over agent-level defaults
    let provider = ch_binding
        .provider
        .as_deref()
        .or(agent_def.provider.as_deref())
        .and_then(ProviderKind::from_str);

    let model = ch_binding.model.clone().or_else(|| agent_def.model.clone());
    let peer_agents_enabled = ch_binding
        .peer_agents
        .or(agent_def.peer_agents)
        .unwrap_or(true);
    let memory = resolve_memory_settings(agent_def.memory.as_ref(), ch_binding.memory.as_ref());

    // Explicit prompt_file > auto-derived from prompts_root > empty
    let prompt_file = agent_def
        .prompt_file
        .as_deref()
        .map(expand_tilde)
        .or_else(|| {
            schema.prompts_root.as_deref().map(|root| {
                let base = expand_tilde(root);
                format!("{}/agents/{}/IDENTITY.md", base, ch_binding.agent)
            })
        })
        .unwrap_or_default();

    let auth_profile = configured_auth_profile(
        ch_binding.auth_profile.as_deref(),
        agent_def.auth_profile.as_deref(),
    )
    .map(str::to_string)
    .unwrap_or_else(|| {
        provider_primary_profile(&schema, provider.as_ref().map(ProviderKind::as_str))
    });

    Some(RoleBinding {
        role_id: ch_binding.agent.clone(),
        prompt_file,
        provider,
        model,
        reasoning_effort: None,
        peer_agents_enabled,
        auth_profile,
        memory,
    })
}

pub(super) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    let schema = load_org_schema()?;
    let (ch_binding, agent_def) = resolve_channel_binding(&schema, channel_id, channel_name)?;

    // Channel-level workspace overrides agent-level default
    let ws = ch_binding
        .workspace
        .as_deref()
        .or(agent_def.workspace.as_deref())?;

    Some(expand_tilde(ws))
}

pub(super) fn load_shared_prompt_path() -> Option<String> {
    let schema = load_org_schema()?;
    // Explicit shared_prompt > auto-derived from prompts_root/agents/_shared.prompt.md
    schema
        ._shared_prompt
        .as_deref()
        .map(expand_tilde)
        .or_else(|| {
            let root = expand_tilde(schema.prompts_root.as_deref()?);
            let root = std::path::Path::new(&root);
            let canonical = root.join("agents").join("_shared.prompt.md");
            if canonical.exists() {
                return Some(canonical.display().to_string());
            }
            let legacy = root.join("_shared.md");
            legacy.exists().then(|| legacy.display().to_string())
        })
}

pub(super) fn list_registered_channel_bindings() -> Vec<RegisteredChannelBinding> {
    let Some(schema) = load_org_schema() else {
        return Vec::new();
    };

    let mut bindings = Vec::new();
    if let Some(by_id) = schema
        .channels
        .as_ref()
        .and_then(|channels| channels.by_id.as_ref())
    {
        for (channel_id_raw, binding) in by_id {
            let Ok(channel_id) = channel_id_raw.parse::<u64>() else {
                continue;
            };
            let owner_provider = binding
                .provider
                .as_deref()
                .or_else(|| {
                    schema
                        .agents
                        .get(&binding.agent)
                        .and_then(|agent| agent.provider.as_deref())
                })
                .and_then(ProviderKind::from_str);
            let Some(owner_provider) = owner_provider.filter(ProviderKind::is_supported) else {
                continue;
            };
            bindings.push(RegisteredChannelBinding {
                channel_id,
                owner_provider,
                fallback_name: None,
            });
        }
    }

    bindings.sort_by_key(|binding| binding.channel_id);
    bindings
}

/// Look up the provider for a channel name suffix from org schema suffix_map.
pub(super) fn lookup_suffix_provider(channel_name: &str) -> Option<ProviderKind> {
    let schema = load_org_schema()?;
    let suffix_map = schema.suffix_map.as_ref()?;
    for (suffix, provider_str) in suffix_map {
        if channel_name.ends_with(suffix.as_str()) {
            return Some(ProviderKind::from_str_or_unsupported(provider_str));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_identity::identity_label;

    #[test]
    fn predicted_launch_home_is_promised_only_when_every_launch_agent_reads_the_same_home() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let home = tempfile::tempdir().unwrap();
        let home_path = std::fs::canonicalize(home.path()).unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let _home = set("HOME", &home_path);
        let accounts = home_path.join(".adk/profiles");
        let profile = accounts.join("claude/acct-b");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(accounts.join("codex/acct-x")).unwrap();
        let root = crate::config::runtime_root().unwrap();
        let org = super::super::runtime_store::org_schema_path_for_root(&root);
        std::fs::create_dir_all(org.parent().unwrap()).unwrap();
        let catalog = format!(
            "version: 1\nprovider_auth_profiles:\n  acct-b:\n    provider: claude\n    home: {}\n  acct-x:\n    provider: codex\n    home: {}\n",
            profile.display(),
            accounts.join("codex/acct-x").display()
        );
        // The channel reaches `coder` only by name; a Codex-account agent never launches Claude.
        let org_with = |coder: &str, extra: &str| {
            let yaml = format!(
                "{catalog}agents:\n  coder:\n    display_name: Coder\n{coder}  reviewer:\n    display_name: Reviewer\n    auth_profile: acct-x\nchannels:\n  by_name:\n    enabled: true\n    mappings:\n      coder-room:\n        agent: coder\n{extra}"
            );
            std::fs::write(&org, yaml).unwrap();
            predicted_launch_home(ProviderKind::Claude, Some(42), "")
        };
        let off = "provider_auth_fallbacks:\n  claude:\n    enabled: false\n";
        let named = org_with("    auth_profile: acct-b\n", off).unwrap_err();
        assert!(named.contains("depends on which agent"), "{named}");
        assert_eq!(org_with("", off).unwrap(), None);
        let primary = format!("{off}provider_auth_primary_profiles:\n  claude: acct-b\n");
        assert_eq!(org_with("", &primary).unwrap(), Some(profile));
        // With no policy entry, launch routing may still move to the other Claude account.
        assert!(org_with("", "").unwrap_err().contains("fallback"));
    }

    #[test]
    fn profile_fallback_configuration_validates_and_defaults_to_auto() {
        let yaml = "version: 1\nagents: {}\nprovider_auth_fallbacks:\n  codex:\n    fallback_profile: default\n    include_remaining: false\n";
        let schema = parse_org_schema(yaml).unwrap();
        let policy = &schema.provider_auth_fallbacks["codex"];
        assert!(policy.enabled);
        assert!(!policy.include_remaining);
        let invalid = yaml.replace("fallback_profile: default", "fallback_profile: missing");
        assert!(parse_org_schema(&invalid).is_err());
        assert!(parse_org_schema(&yaml.replace("codex:", "gemini:")).is_err());
        let defaults = parse_org_schema("version: 1\nagents: {}\n").unwrap();
        assert!(defaults.provider_auth_fallbacks.is_empty());
    }

    #[test]
    fn test_010_peer_identity_label_uses_formatter() {
        let yaml = r#"
version: 1
agents:
  coder:
    display_name: Coder
    provider: codex
    model: gpt-5.6-terra
    keywords: [code]
"#;
        let schema = parse_org_schema(yaml).expect("schema");
        let def = schema.agents.get("coder").expect("coder");
        let provider = ProviderKind::from_str(def.provider.as_deref().unwrap()).unwrap();
        let identity = identity_from_parts(
            "coder",
            def.display_name.clone(),
            provider,
            def.model.clone(),
            def.auth_profile.as_deref(),
        );
        assert_eq!(identity_label(&identity), "coder · codex terra · default");
        let json = agent_identity::identity_json(&identity);
        assert_eq!(json["label"], "coder · codex terra · default");
        assert!(json.get("token").is_none());
    }

    #[test]
    fn unknown_catalog_id_fails_org_load() {
        let yaml = r#"
version: 1
provider_auth_profiles:
  default:
    provider: codex
    home: ~/.adk/profiles/codex/default
agents:
  coder:
    display_name: Coder
    provider: codex
"#;
        let err = parse_org_schema(yaml).unwrap_err();
        assert!(err.contains("reserved") || err.contains("default"));
    }

    #[test]
    fn primary_profile_is_inherited_until_agent_or_channel_explicitly_overrides_it() {
        let yaml = r#"
version: 1
provider_auth_profiles:
  codex-work:
    provider: codex
    home: ~/.adk/profiles/codex/codex-work
  codex-personal:
    provider: codex
    home: ~/.adk/profiles/codex/codex-personal
provider_auth_primary_profiles:
  codex: codex-work
agents:
  coder:
    display_name: Coder
    provider: codex
channels:
  by_id:
    "123":
      agent: coder
      auth_profile: codex-personal
"#;
        let schema = parse_org_schema(yaml).expect("schema");
        let bindings = list_profile_bindings_from_schema(schema);
        let agent = bindings
            .iter()
            .find(|binding| binding.channel_id.is_none())
            .expect("agent binding");
        assert_eq!(agent.profile_id, "codex-work");
        assert!(!agent.is_explicit);

        let channel = bindings
            .iter()
            .find(|binding| binding.channel_id.as_deref() == Some("123"))
            .expect("channel binding");
        assert_eq!(channel.profile_id, "codex-personal");
        assert!(channel.is_explicit);
    }

    #[test]
    fn provider_primary_rejects_profile_from_another_provider() {
        let yaml = r#"
version: 1
provider_auth_profiles:
  claude-work:
    provider: claude
    home: ~/.adk/profiles/claude/claude-work
provider_auth_primary_profiles:
  codex: claude-work
agents: {}
"#;
        let error = parse_org_schema(yaml).expect_err("provider mismatch must fail");
        assert!(error.contains("belongs to"));
    }
}
