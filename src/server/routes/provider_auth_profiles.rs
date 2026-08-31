//! Extra CLI auth-profile catalog + isolated vendor login.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::discord::org_writer;
use crate::services::provider::ProviderKind;
use crate::services::provider_auth::{ProviderAuthSpec, detect_provider_credentials_with_overlay};
use crate::services::provider_auth_profile::{
    self, AuthProfileError, EXTRA_ACCOUNT_PROVIDER_IDS, ManagedHomeValidationMode,
    ProviderAuthProfileDef, compact_home_for_yaml, create_empty_profile_home, extra_account_home,
    extra_account_login_supported, intern_provider, login_tmux_session_name, overlay_for_home,
    validate_managed_profile_home, validate_profile_id, vendor_login_argv, write_login_script,
};

#[derive(Debug, Deserialize, Default)]
pub struct LoginStartBody {
    #[serde(default)]
    pub profile_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginCompleteBody {
    pub profile_id: String,
    #[serde(default)]
    pub home: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthProfilePatchBody {
    #[serde(default)]
    pub auth_profile: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct PrimaryProfileBody {
    #[serde(default)]
    pub profile_id: Option<String>,
}

/// GET /api/provider-auth-profiles
pub async fn list_provider_auth_profiles(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let catalog = crate::services::discord::provider_auth_catalog();
    let primary_profiles = crate::services::discord::provider_auth_primary_profiles();
    let bindings = crate::services::discord::list_profile_bindings();
    let usage_by_key = if let Some(pool) = state.pg_pool_ref() {
        let now = chrono::Utc::now().timestamp();
        crate::services::analytics::build_rate_limit_provider_payloads_pg(pool, now)
            .await
            .into_iter()
            .filter_map(|entry| {
                let provider = entry.get("provider")?.as_str()?.to_string();
                let profile_id = entry
                    .get("profile_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("default")
                    .to_string();
                Some(((provider, profile_id), entry))
            })
            .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };

    let mut providers = Vec::new();
    for id in EXTRA_ACCOUNT_PROVIDER_IDS {
        let Some(kind) = ProviderKind::from_str(id) else {
            continue;
        };
        let default_home = provider_auth_profile::default_home_display(&kind)
            .unwrap_or("")
            .to_string();
        let mut accounts = vec![account_payload(
            "default",
            &default_home,
            &kind,
            &bindings,
            &usage_by_key,
        )];
        let mut extras: Vec<_> = catalog
            .iter()
            .filter(|(_, def)| intern_provider(&def.provider).ok().as_ref() == Some(&kind))
            .collect();
        extras.sort_by_key(|(profile_id, _)| *profile_id);
        for (profile_id, def) in extras {
            accounts.push(account_payload(
                profile_id,
                def.home.as_deref().unwrap_or(""),
                &kind,
                &bindings,
                &usage_by_key,
            ));
        }
        providers.push(json!({
            "id": id,
            "default_home": default_home,
            "primary_profile_id": primary_profiles.get(*id).cloned().unwrap_or_else(|| "default".to_string()),
            "accounts": accounts,
        }));
    }

    let agent_profile_overrides: Vec<Value> = bindings
        .iter()
        .filter(|binding| binding.channel_id.is_none())
        .map(|binding| {
            json!({
                "agent_id": binding.agent_id.clone(),
                "provider": binding.provider.clone(),
                "profile_id": if binding.is_explicit { Value::String(binding.profile_id.clone()) } else { Value::Null },
            })
        })
        .collect();
    Ok((
        StatusCode::OK,
        Json(json!({
            "providers": providers,
            "agent_profile_overrides": agent_profile_overrides,
        })),
    ))
}

/// PUT /api/provider-auth-profiles/{provider}/primary
pub async fn set_primary_profile(
    Path(provider): Path<String>,
    Json(body): Json<PrimaryProfileBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let kind = intern_provider(&provider).map_err(profile_error)?;
    let profile_id = body
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(provider_auth_profile::DEFAULT_PROFILE_ID);
    if profile_id != provider_auth_profile::DEFAULT_PROFILE_ID {
        validate_profile_id(profile_id).map_err(profile_error)?;
    }
    org_writer::set_provider_primary_profile(kind.as_str(), profile_id)
        .map_err(|error| AppError::new(StatusCode::BAD_REQUEST, ErrorCode::Config, error))?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "provider": kind.as_str(),
            "primary_profile_id": profile_id,
        })),
    ))
}

/// POST /api/provider-auth-profiles/{provider}/login-start
pub async fn login_start(
    Path(provider): Path<String>,
    body: Option<Json<LoginStartBody>>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let kind = intern_provider(&provider).map_err(profile_error)?;
    if !extra_account_login_supported(&kind) {
        return Err(profile_error(AuthProfileError::UnsupportedLogin(
            kind.as_str().to_string(),
        )));
    }
    let catalog = crate::services::discord::provider_auth_catalog();
    let profile_id = match body
        .profile_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(id) => {
            validate_profile_id(id).map_err(profile_error)?;
            if catalog.contains_key(id) {
                return Err(profile_error(AuthProfileError::AlreadyCataloged(
                    id.to_string(),
                )));
            }
            id.to_string()
        }
        None => {
            let existing_homes =
                provider_auth_profile::list_existing_profile_homes(&kind).map_err(profile_error)?;
            provider_auth_profile::allocate_profile_id(&kind, &catalog, &existing_homes)
                .map_err(profile_error)?
        }
    };

    let home = create_empty_profile_home(&kind, &profile_id).map_err(profile_error)?;
    let overlay = overlay_for_home(kind.clone(), &profile_id, &home);
    let argv = vendor_login_argv(&kind).ok_or_else(|| {
        profile_error(AuthProfileError::UnsupportedLogin(
            kind.as_str().to_string(),
        ))
    })?;
    let script = write_login_script(&home, &overlay, argv).map_err(profile_error)?;
    let session = login_tmux_session_name(&kind, &profile_id);
    spawn_login_tmux(&session, &home, &script).map_err(|error| {
        AppError::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Internal, error)
    })?;

    tracing::info!(
        profile_id = %profile_id,
        home = %home.display(),
        tmux_session = %session,
        provider = kind.as_str(),
        "provider extra-account login started"
    );

    Ok((
        StatusCode::OK,
        Json(json!({
            "profile_id": profile_id,
            "home": home.display().to_string(),
            "tmux_session": session,
            "attach": format!("tmux attach -t {session}"),
        })),
    ))
}

/// POST /api/provider-auth-profiles/{provider}/login-complete
pub async fn login_complete(
    Path(provider): Path<String>,
    Json(body): Json<LoginCompleteBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let kind = intern_provider(&provider).map_err(profile_error)?;
    if !extra_account_login_supported(&kind) {
        return Err(profile_error(AuthProfileError::UnsupportedLogin(
            kind.as_str().to_string(),
        )));
    }
    let profile_id = body.profile_id.trim();
    validate_profile_id(profile_id).map_err(profile_error)?;
    let expected_home = extra_account_home(&kind, profile_id);
    let home = match body
        .home
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(raw) => PathBuf::from(crate::utils::format::expand_tilde_string(raw)),
        None => expected_home.clone(),
    };
    validate_managed_profile_home(
        &kind,
        profile_id,
        &home,
        ManagedHomeValidationMode::Existing,
    )
    .map_err(profile_error)?;

    let overlay = overlay_for_home(kind.clone(), profile_id, &home);
    let spec = kind
        .registry_entry()
        .map(|entry| entry.auth)
        .unwrap_or(ProviderAuthSpec {
            credential_paths: &[],
            env_keys: &[],
            auth_check_argv: None,
        });
    let status = detect_provider_credentials_with_overlay(kind.as_str(), &spec, Some(&overlay));
    if !status.credential_present {
        return Err(profile_error(AuthProfileError::CredentialsMissing {
            profile_id: profile_id.to_string(),
            home: home.display().to_string(),
        }));
    }

    org_writer::append_provider_auth_profile(
        profile_id,
        ProviderAuthProfileDef {
            provider: kind.as_str().to_string(),
            home: Some(compact_home_for_yaml(&home)),
            env: BTreeMap::new(),
        },
    )
    .map_err(|error| AppError::new(StatusCode::BAD_REQUEST, ErrorCode::Config, error))?;

    tracing::info!(
        profile_id = %profile_id,
        home = %home.display(),
        provider = kind.as_str(),
        "provider extra-account login completed"
    );

    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "profile_id": profile_id,
            "home": compact_home_for_yaml(&home),
        })),
    ))
}

/// DELETE /api/provider-auth-profiles/{provider}/{profile_id}
///
/// Unlinks a named account from org.yaml. This deliberately retains the
/// profile's credential directory; deleting credentials needs an explicit,
/// separate operator action and must never be the side effect of an UI ×.
pub async fn remove_profile(
    Path((provider, profile_id)): Path<(String, String)>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let kind = intern_provider(&provider).map_err(profile_error)?;
    let profile_id = profile_id.trim();
    validate_profile_id(profile_id).map_err(profile_error)?;
    org_writer::remove_provider_auth_profile(profile_id, kind.as_str()).map_err(|error| {
        let status = if error.contains("not found") {
            StatusCode::NOT_FOUND
        } else if error.contains("still bound") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        AppError::new(status, ErrorCode::Config, error)
    })?;
    let session = login_tmux_session_name(&kind, profile_id);
    if crate::services::platform::tmux::has_session(&session) {
        let _ =
            crate::services::platform::tmux::kill_session(&session, "provider-auth-profile-unlink");
    }
    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "provider": kind.as_str(),
            "profile_id": profile_id,
            "credentials_retained": true,
        })),
    ))
}

/// PATCH /api/agents/{id} auth_profile helper used by agents_crud.
pub fn apply_agent_auth_profile_patch(
    agent_id: &str,
    auth_profile: &Value,
) -> Result<(), AppError> {
    org_writer::set_agent_auth_profile(agent_id, parse_auth_profile_patch(auth_profile)?).map_err(
        |error| {
            let status = if error.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            AppError::new(status, ErrorCode::Config, error)
        },
    )
}

/// PATCH /api/channels/{id}
pub async fn patch_channel_auth_profile(
    Path(channel_id): Path<String>,
    Json(body): Json<AuthProfilePatchBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let Some(value) = body.auth_profile.as_ref() else {
        return Err(AppError::bad_request("auth_profile is required"));
    };
    org_writer::set_channel_auth_profile(&channel_id, parse_auth_profile_patch(value)?).map_err(
        |error| {
            let status = if error.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            AppError::new(status, ErrorCode::Config, error)
        },
    )?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "channel_id": channel_id,
            "auth_profile": value,
        })),
    ))
}

fn parse_auth_profile_patch(value: &Value) -> Result<Option<&str>, AppError> {
    match value {
        Value::Null => Ok(None),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed == provider_auth_profile::DEFAULT_PROFILE_ID {
                Ok(None)
            } else {
                validate_profile_id(trimmed).map_err(profile_error)?;
                Ok(Some(trimmed))
            }
        }
        _ => Err(AppError::bad_request(
            "auth_profile must be a string, empty, or null",
        )),
    }
}

fn account_payload(
    profile_id: &str,
    home: &str,
    provider: &ProviderKind,
    bindings: &[crate::services::discord::ProfileBinding],
    usage_by_key: &HashMap<(String, String), Value>,
) -> Value {
    let mut bound_agents: Vec<String> = bindings
        .iter()
        .filter(|binding| {
            binding.channel_id.is_none()
                && binding.provider == provider.as_str()
                && binding.profile_id == profile_id
        })
        .map(|binding| binding.agent_id.clone())
        .collect();
    bound_agents.sort();
    bound_agents.dedup();
    let mut bound_channels: Vec<String> = bindings
        .iter()
        .filter(|binding| binding.provider == provider.as_str() && binding.profile_id == profile_id)
        .filter_map(|binding| binding.channel_id.clone())
        .collect();
    bound_channels.sort();
    bound_channels.dedup();
    let usage = usage_by_key
        .get(&(provider.as_str().to_string(), profile_id.to_string()))
        .cloned()
        .map(|entry| {
            json!({
                "buckets": entry.get("buckets").cloned().unwrap_or_else(|| json!([])),
                "stale": entry.get("stale").and_then(Value::as_bool).unwrap_or(false),
                "unsupported": entry.get("unsupported").and_then(Value::as_bool).unwrap_or(false),
                "reason": entry.get("reason").cloned().unwrap_or(Value::Null),
            })
        })
        .or_else(|| unsupported_usage(provider));
    json!({
        "id": profile_id,
        "home": home,
        "bound_agents": bound_agents,
        "bound_channels": bound_channels,
        "usage": usage,
    })
}

/// Keep a missing telemetry source distinguishable from an empty rate-limit
/// response.  This is per account too: a profile must not look healthy merely
/// because its provider has no supported usage endpoint.
fn unsupported_usage(provider: &ProviderKind) -> Option<Value> {
    let reason = match provider {
        ProviderKind::OpenCode => "No OpenCode rate-limit telemetry source is implemented yet.",
        ProviderKind::Qwen => "No Qwen rate-limit telemetry source is implemented yet.",
        _ => return None,
    };
    Some(json!({
        "buckets": [],
        "stale": false,
        "unsupported": true,
        "reason": reason,
    }))
}

fn spawn_login_tmux(
    session: &str,
    home: &std::path::Path,
    script: &std::path::Path,
) -> Result<(), String> {
    if crate::services::platform::tmux::has_session(session) {
        let _ =
            crate::services::platform::tmux::kill_session(session, "provider-auth-login-restart");
    }
    let command = format!(
        "bash {}",
        crate::services::process::shell_escape(&script.display().to_string())
    );
    let output = crate::services::platform::tmux::create_session(
        session,
        Some(&home.display().to_string()),
        &command,
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tmux login session failed: {stderr}"));
    }
    crate::services::platform::tmux::set_option(session, "remain-on-exit", "on");
    Ok(())
}

fn profile_error(error: AuthProfileError) -> AppError {
    let status = match &error {
        AuthProfileError::UnknownProfile(_) | AuthProfileError::HomeMissing { .. } => {
            StatusCode::NOT_FOUND
        }
        AuthProfileError::AlreadyCataloged(_) | AuthProfileError::CredentialsMissing { .. } => {
            StatusCode::CONFLICT
        }
        AuthProfileError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    AppError::new(status, ErrorCode::Validation, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider_auth_profile::{
        login_script_contents, overlay_for_home, vendor_login_argv,
    };

    #[test]
    fn test_012_login_payload_and_script_have_no_secrets() {
        let dir = tempfile::tempdir().expect("home");
        let overlay = overlay_for_home(ProviderKind::Codex, "work", dir.path());
        let script =
            login_script_contents(&overlay, vendor_login_argv(&ProviderKind::Codex).unwrap());
        let payload = json!({
            "profile_id": "work",
            "home": dir.path().display().to_string(),
            "tmux_session": login_tmux_session_name(&ProviderKind::Codex, "work"),
        });
        let encoded = format!("{payload}{script}");
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("sk-"));
        assert!(
            !script
                .lines()
                .any(|line| line.starts_with("export HOME=") || line.starts_with("HOME=")),
            "extra-account login must not rewrite HOME"
        );
        assert!(script.contains("CODEX_HOME="));
        assert!(
            payload["tmux_session"]
                .as_str()
                .unwrap()
                .starts_with("adk-login-codex-")
        );
    }

    #[test]
    fn test_013_account_payload_includes_usage_without_secrets() {
        let bindings = vec![crate::services::discord::ProfileBinding {
            agent_id: "coder".into(),
            provider: "codex".into(),
            profile_id: "work".into(),
            channel_id: None,
            is_explicit: true,
        }];
        let mut usage = HashMap::new();
        usage.insert(
            ("codex".into(), "work".into()),
            json!({
                "provider": "codex",
                "profile_id": "work",
                "buckets": [{"name": "5h", "limit": 100, "used": 20, "remaining": 80, "reset": 0}],
                "stale": false,
                "unsupported": false,
                "reason": null,
            }),
        );
        let account = account_payload(
            "work",
            "~/.adk/profiles/codex/work",
            &ProviderKind::Codex,
            &bindings,
            &usage,
        );
        assert_eq!(account["id"], "work");
        assert_eq!(account["bound_agents"], json!(["coder"]));
        assert_eq!(account["usage"]["buckets"][0]["used"], 20);
        let encoded = account.to_string();
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("sk-"));
    }

    #[test]
    fn unsupported_provider_account_is_not_reported_as_empty_usage() {
        let account = account_payload(
            "qwen-alt",
            "~/.adk/profiles/qwen/qwen-alt",
            &ProviderKind::Qwen,
            &[],
            &HashMap::new(),
        );
        assert_eq!(account["usage"]["unsupported"], true);
        assert!(
            account["usage"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("Qwen"))
        );
    }

    #[test]
    fn auth_profile_patch_null_clears() {
        assert_eq!(parse_auth_profile_patch(&Value::Null).unwrap(), None);
        assert_eq!(parse_auth_profile_patch(&json!("default")).unwrap(), None);
        assert_eq!(
            parse_auth_profile_patch(&json!("work")).unwrap(),
            Some("work")
        );
    }
}
