
/// GET /api/onboarding/status
/// Returns whether onboarding is complete + existing config values.
pub async fn status(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match status_pg(pool).await {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    {
        return match status_config() {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        let conn = match legacy_db(state).lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        // Check whether onboarding created any agents yet.
        let has_bots: bool = conn
            .query_row("SELECT COUNT(*) > 0 FROM agents", [], |row| row.get(0))
            .unwrap_or(false);

        // Get existing config
        let bot_token: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_bot_token'",
                [],
                |row| row.get(0),
            )
            .ok();

        let guild_id: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_guild_id'",
                [],
                |row| row.get(0),
            )
            .ok();

        let owner_id = sanitize_legacy_owner_id(
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_owner_id'",
                [],
                |row| row.get(0),
            )
            .ok(),
        );

        let agent_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))
            .unwrap_or(0);

        // Get channel mappings from agents table
        let mut stmt = conn
            .prepare("SELECT id, name, discord_channel_id FROM agents ORDER BY id")
            .unwrap();
        let agents: Vec<serde_json::Value> = stmt
            .query_map([], |row| {
                Ok(json!({
                    "agent_id": row.get::<_, String>(0)?,
                    "name": row.get::<_, Option<String>>(1)?,
                    "channel_id": row.get::<_, Option<String>>(2)?,
                }))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();

        // Load all bot tokens for pre-fill
        let announce_token: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_announce_token'",
                [],
                |row| row.get(0),
            )
            .ok();
        let notify_token: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_notify_token'",
                [],
                |row| row.get(0),
            )
            .ok();
        let command_token_2: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_command_token_2'",
                [],
                |row| row.get(0),
            )
            .ok();
        let primary_provider: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_provider'",
                [],
                |row| row.get(0),
            )
            .ok();
        let command_provider_2: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_command_provider_2'",
                [],
                |row| row.get(0),
            )
            .ok();

        let completed = has_bots && agent_count > 0;
        let runtime_root = crate::cli::agentdesk_runtime_root();
        let completion_state = runtime_root
            .as_ref()
            .and_then(|root| load_onboarding_completion_state(root).ok().flatten());
        let draft_available = runtime_root
            .as_ref()
            .map(|root| onboarding_draft_path(root).is_file())
            .unwrap_or(false);
        let setup_mode = onboarding_setup_mode(completed);
        let resume_state = onboarding_resume_state(draft_available, completion_state.as_ref());

        // Never return raw onboarding tokens from status.
        // This endpoint can be reachable without auth, so redact all token values.
        let redact = |_t: Option<String>| -> Option<String> { None };

        (
            StatusCode::OK,
            Json(json!({
                "completed": completed,
                "agent_count": agent_count,
                "bot_tokens": {
                    "command": redact(bot_token),
                    "announce": redact(announce_token),
                    "notify": redact(notify_token),
                    "command2": redact(command_token_2),
                },
                "bot_providers": {
                    "command": primary_provider,
                    "command2": command_provider_2,
                },
                "guild_id": guild_id,
                "owner_id": owner_id,
                "agents": agents,
                "draft_available": draft_available,
                "setup_mode": setup_mode,
                "resume_state": resume_state,
                "completion_state": onboarding_completion_state_value(completion_state.as_ref()),
                "partial_apply": completion_state
                    .as_ref()
                    .map(|state| state.partial_apply)
                    .unwrap_or(false),
                "retry_recommended": completion_state
                    .as_ref()
                    .map(|state| state.retry_recommended)
                    .unwrap_or(false),
                "rerun_policy": onboarding_rerun_policy_value(
                    OnboardingRerunPolicy::ReuseExisting,
                    false,
                ),
            })),
        )
    }
}

async fn pg_kv_value(pool: &sqlx::PgPool, key: &str) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres kv_meta {key}: {error}"))
}

async fn onboarding_has_agents_pg(pool: &sqlx::PgPool) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>("SELECT COUNT(*) > 0 FROM agents")
        .fetch_one(pool)
        .await
        .map_err(|error| format!("check postgres onboarding agents: {error}"))
}

async fn status_pg(pool: &sqlx::PgPool) -> Result<serde_json::Value, String> {
    let has_bots = onboarding_has_agents_pg(pool).await?;
    let bot_token = pg_kv_value(pool, "onboarding_bot_token").await?;
    let guild_id = pg_kv_value(pool, "onboarding_guild_id").await?;
    let owner_id = sanitize_legacy_owner_id(pg_kv_value(pool, "onboarding_owner_id").await?);
    let agent_count: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM agents")
        .fetch_one(pool)
        .await
        .map_err(|error| format!("count postgres onboarding agents: {error}"))?;
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT id, name, discord_channel_id FROM agents ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres onboarding agents: {error}"))?;
    let agents = rows
        .into_iter()
        .map(|(agent_id, name, channel_id)| {
            json!({
                "agent_id": agent_id,
                "name": name,
                "channel_id": channel_id,
            })
        })
        .collect::<Vec<_>>();

    let announce_token = pg_kv_value(pool, "onboarding_announce_token").await?;
    let notify_token = pg_kv_value(pool, "onboarding_notify_token").await?;
    let command_token_2 = pg_kv_value(pool, "onboarding_command_token_2").await?;
    let primary_provider = pg_kv_value(pool, "onboarding_provider").await?;
    let command_provider_2 = pg_kv_value(pool, "onboarding_command_provider_2").await?;

    let completed = has_bots && agent_count > 0;
    let runtime_root = crate::cli::agentdesk_runtime_root();
    let completion_state = runtime_root
        .as_ref()
        .and_then(|root| load_onboarding_completion_state(root).ok().flatten());
    let draft_available = runtime_root
        .as_ref()
        .map(|root| onboarding_draft_path(root).is_file())
        .unwrap_or(false);
    let setup_mode = onboarding_setup_mode(completed);
    let resume_state = onboarding_resume_state(draft_available, completion_state.as_ref());
    let redact = |_t: Option<String>| -> Option<String> { None };

    Ok(json!({
        "completed": completed,
        "agent_count": agent_count,
        "bot_tokens": {
            "command": redact(bot_token),
            "announce": redact(announce_token),
            "notify": redact(notify_token),
            "command2": redact(command_token_2),
        },
        "bot_providers": {
            "command": primary_provider,
            "command2": command_provider_2,
        },
        "guild_id": guild_id,
        "owner_id": owner_id,
        "agents": agents,
        "draft_available": draft_available,
        "setup_mode": setup_mode,
        "resume_state": resume_state,
        "completion_state": onboarding_completion_state_value(completion_state.as_ref()),
        "partial_apply": completion_state
            .as_ref()
            .map(|state| state.partial_apply)
            .unwrap_or(false),
        "retry_recommended": completion_state
            .as_ref()
            .map(|state| state.retry_recommended)
            .unwrap_or(false),
        "rerun_policy": onboarding_rerun_policy_value(
            OnboardingRerunPolicy::ReuseExisting,
            false,
        ),
    }))
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn status_config() -> Result<serde_json::Value, String> {
    let runtime_root = crate::cli::agentdesk_runtime_root();
    let config = match runtime_root.as_ref() {
        Some(root) => load_onboarding_config(root)?,
        None => crate::config::Config::default(),
    };
    let agent_count = config.agents.len() as i64;
    let agents = config
        .agents
        .iter()
        .map(|agent| {
            let channel_id = agent.channels.iter().into_iter().find_map(|(_, channel)| {
                channel.and_then(|channel| {
                    channel
                        .channel_id()
                        .or_else(|| channel.channel_name())
                        .or_else(|| channel.target())
                })
            });
            json!({
                "agent_id": agent.id,
                "name": agent.name,
                "channel_id": channel_id,
            })
        })
        .collect::<Vec<_>>();
    let completion_state = runtime_root
        .as_ref()
        .and_then(|root| load_onboarding_completion_state(root).ok().flatten());
    let draft_available = runtime_root
        .as_ref()
        .map(|root| onboarding_draft_path(root).is_file())
        .unwrap_or(false);
    let completed = config.discord.guild_id.is_some() && agent_count > 0;
    let setup_mode = onboarding_setup_mode(completed);
    let resume_state = onboarding_resume_state(draft_available, completion_state.as_ref());

    Ok(json!({
        "completed": completed,
        "agent_count": agent_count,
        "bot_tokens": {
            "command": Option::<String>::None,
            "announce": Option::<String>::None,
            "notify": Option::<String>::None,
            "command2": Option::<String>::None,
        },
        "bot_providers": {
            "command": config.discord.bots.get("command").and_then(|bot| bot.provider.clone()),
            "command2": config.discord.bots.get("command_2").and_then(|bot| bot.provider.clone()),
        },
        "guild_id": config.discord.guild_id,
        "owner_id": config.discord.owner_id.map(|id| id.to_string()),
        "agents": agents,
        "draft_available": draft_available,
        "setup_mode": setup_mode,
        "resume_state": resume_state,
        "completion_state": onboarding_completion_state_value(completion_state.as_ref()),
        "partial_apply": completion_state
            .as_ref()
            .map(|state| state.partial_apply)
            .unwrap_or(false),
        "retry_recommended": completion_state
            .as_ref()
            .map(|state| state.retry_recommended)
            .unwrap_or(false),
        "rerun_policy": onboarding_rerun_policy_value(
            OnboardingRerunPolicy::ReuseExisting,
            false,
        ),
    }))
}

/// GET /api/onboarding/draft
/// Returns the in-progress onboarding draft, distinct from completed setup summary.
pub async fn draft_get(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    let completed = if let Some(pool) = state.pg_pool_ref() {
        match onboarding_has_agents_pg(pool).await {
            Ok(completed) => completed,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    } else {
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        {
            crate::cli::agentdesk_runtime_root()
                .as_ref()
                .and_then(|root| load_onboarding_config(root).ok())
                .map(|config| config.discord.guild_id.is_some() && !config.agents.is_empty())
                .unwrap_or(false)
        }
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            match legacy_db(state).lock() {
                Ok(conn) => conn
                    .query_row("SELECT COUNT(*) > 0 FROM agents", [], |row| row.get(0))
                    .unwrap_or(false),
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            }
        }
    };

    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "cannot determine runtime root"})),
        );
    };

    let draft = match load_onboarding_draft(&root) {
        Ok(draft) => draft,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    }
    .map(OnboardingDraft::redact_secrets);
    let completion_state = match load_onboarding_completion_state(&root) {
        Ok(state) => state,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };
    let available = draft.is_some();

    (
        StatusCode::OK,
        Json(json!({
            "available": available,
            "completed": completed,
            "draft": draft,
            "setup_mode": onboarding_setup_mode(completed),
            "resume_state": onboarding_resume_state(available, completion_state.as_ref()),
            "completion_state": onboarding_completion_state_value(completion_state.as_ref()),
            "secret_policy": onboarding_draft_secret_policy_value(),
        })),
    )
}

/// PUT /api/onboarding/draft
/// Persists the in-progress onboarding draft required to resume across browsers.
pub async fn draft_put(body: OnboardingDraft) -> (StatusCode, Json<serde_json::Value>) {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "cannot determine runtime root"})),
        );
    };

    if let Err(error) = crate::runtime_layout::ensure_runtime_layout(&root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to prepare runtime layout: {error}")})),
        );
    }

    let draft = match body.normalize() {
        Ok(draft) => draft,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let draft = draft.redact_secrets();

    if let Err(error) = save_onboarding_draft(&root, &draft) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "available": true,
            "draft": draft,
            "secret_policy": onboarding_draft_secret_policy_value(),
        })),
    )
}

/// DELETE /api/onboarding/draft
/// Explicitly removes the in-progress onboarding draft.
pub async fn draft_delete() -> (StatusCode, Json<serde_json::Value>) {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "cannot determine runtime root"})),
        );
    };

    if let Err(error) = clear_onboarding_draft(&root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "available": false,
            "secret_policy": onboarding_draft_secret_policy_value(),
        })),
    )
}
