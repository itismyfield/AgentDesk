
async fn persist_onboarding_pg(
    pool: &sqlx::PgPool,
    body: &CompleteBody,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("failed to start postgres onboarding transaction: {error}"))?;

    for (key, value) in [
        ("onboarding_bot_token", Some(body.token.trim())),
        ("onboarding_guild_id", Some(body.guild_id.trim())),
        ("onboarding_provider", Some(provider)),
        (
            "onboarding_owner_id",
            body.owner_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        (
            "onboarding_announce_token",
            body.announce_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        (
            "onboarding_notify_token",
            body.notify_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        (
            "onboarding_command_token_2",
            body.command_token_2
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        (
            "onboarding_command_provider_2",
            body.command_provider_2
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        ("onboarding_complete", Some("true")),
    ] {
        match value {
            Some(value) => {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value)
                     VALUES ($1, $2)
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, expires_at = NULL",
                )
                .bind(key)
                .bind(value)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("failed to persist postgres kv_meta {key}: {error}"))?;
            }
            None => {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| format!("failed to clear postgres kv_meta {key}: {error}"))?;
            }
        }
    }

    for mapping in resolved_channels {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id, description, system_prompt, status, xp)
             VALUES ($1, $2, $3, $4, $5, $6, 'active', 0)
             ON CONFLICT (id) DO UPDATE SET
               name = COALESCE(EXCLUDED.name, agents.name),
               provider = COALESCE(EXCLUDED.provider, agents.provider),
               discord_channel_id = EXCLUDED.discord_channel_id,
               description = COALESCE(EXCLUDED.description, agents.description),
               system_prompt = COALESCE(EXCLUDED.system_prompt, agents.system_prompt),
               updated_at = NOW()",
        )
        .bind(&mapping.role_id)
        .bind(&mapping.role_id)
        .bind(provider)
        .bind(&mapping.channel_id)
        .bind(&mapping.description)
        .bind(&mapping.system_prompt)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("failed to upsert postgres agent {}: {error}", mapping.role_id))?;
    }

    if !resolved_channels.is_empty() {
        let (template_name, template_name_ko, template_icon, template_color) =
            match body.template.as_deref() {
                Some("delivery") => ("Delivery Squad", "전달 스쿼드", "🚀", "#8b5cf6"),
                Some("operations") => ("Operations Cell", "운영 셀", "🛠️", "#10b981"),
                Some("insight") => ("Insight Desk", "인사이트 데스크", "📚", "#3b82f6"),
                _ => ("General", "일반", "📁", "#6b7280"),
            };

        let office_id = "hq";
        sqlx::query(
            "INSERT INTO offices (id, name, name_ko, icon)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(office_id)
        .bind("Headquarters")
        .bind("본사")
        .bind("🏛️")
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("failed to upsert postgres default office: {error}"))?;

        let dept_id = body.template.as_deref().unwrap_or("general").to_string();
        sqlx::query(
            "INSERT INTO departments (id, name, name_ko, icon, color, office_id, sort_order)
             VALUES ($1, $2, $3, $4, $5, $6, 0)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&dept_id)
        .bind(template_name)
        .bind(template_name_ko)
        .bind(template_icon)
        .bind(template_color)
        .bind(office_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("failed to upsert postgres onboarding department: {error}"))?;

        for mapping in resolved_channels {
            sqlx::query(
                "INSERT INTO office_agents (office_id, agent_id, department_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (office_id, agent_id)
                 DO UPDATE SET department_id = EXCLUDED.department_id",
            )
            .bind(office_id)
            .bind(&mapping.role_id)
            .bind(&dept_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!(
                    "failed to assign postgres office agent {}: {error}",
                    mapping.role_id
                )
            })?;

            sqlx::query("UPDATE agents SET department = $1, updated_at = NOW() WHERE id = $2")
                .bind(&dept_id)
                .bind(&mapping.role_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    format!(
                        "failed to set postgres agent department {}: {error}",
                        mapping.role_id
                    )
                })?;
        }
    }

    tx.commit()
        .await
        .map_err(|error| format!("failed to commit postgres onboarding transaction: {error}"))
}

/// POST /api/onboarding/complete
/// Saves onboarding configuration and sets up agents.
pub async fn complete(
    state: &AppState,
    body: CompleteBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, response) =
        complete_with_options(state, &body, &CompleteExecutionOptions::default()).await;
    (status, Json(response))
}

async fn complete_with_options(
    state: &AppState,
    body: &CompleteBody,
    options: &CompleteExecutionOptions,
) -> (StatusCode, serde_json::Value) {
    let provider = body.provider.as_deref().unwrap_or("claude");
    if body.guild_id.trim().is_empty() {
        return completion_response(
            StatusCode::BAD_REQUEST,
            false,
            provider,
            OnboardingRerunPolicy::ReuseExisting,
            false,
            None,
            Some("guild_id is required for onboarding completion".to_string()),
            Vec::new(),
            serde_json::Map::new(),
        );
    }
    if let Err(error) = parse_owner_id(body.owner_id.as_deref()) {
        return completion_response(
            StatusCode::BAD_REQUEST,
            false,
            provider,
            OnboardingRerunPolicy::ReuseExisting,
            false,
            None,
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }
    let explicit_rerun_policy = body
        .rerun_policy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    let rerun_policy = match OnboardingRerunPolicy::parse(body.rerun_policy.as_deref()) {
        Ok(policy) => policy,
        Err(error) => {
            return completion_response(
                StatusCode::BAD_REQUEST,
                false,
                provider,
                OnboardingRerunPolicy::ReuseExisting,
                explicit_rerun_policy,
                None,
                Some(error),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };
    let request_fingerprint = match requested_channel_fingerprint(body, provider) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return completion_response(
                StatusCode::BAD_REQUEST,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                None,
                Some(error),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };
    let discord_token = body
        .announce_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(body.token.as_str());

    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            None,
            Some("cannot determine runtime root".to_string()),
            Vec::new(),
            serde_json::Map::new(),
        );
    };

    if let Err(error) = crate::runtime_layout::ensure_runtime_layout(&root) {
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            None,
            Some(format!("failed to prepare runtime layout: {error}")),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let existing_completion_state = match load_onboarding_completion_state(&root) {
        Ok(state) => state,
        Err(error) => {
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                None,
                Some(error),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };

    if let Some(existing_state) = existing_completion_state
        .as_ref()
        .filter(|state| state.partial_apply && state.request_fingerprint != request_fingerprint)
    {
        return completion_response(
            StatusCode::CONFLICT,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(existing_state),
            Some(
                "an incomplete onboarding attempt exists for a different channel plan; retry the same payload or reset the previous partial apply before changing channel mappings".to_string(),
            ),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let checkpoint_state = existing_completion_state
        .as_ref()
        .filter(|state| state.request_fingerprint == request_fingerprint);

    let client = reqwest::Client::new();
    let mut resolved_channels = Vec::with_capacity(body.channels.len());
    for mapping in &body.channels {
        let checkpoint = checkpoint_state.and_then(|state| {
            let requested_name = desired_channel_name(mapping).ok()?;
            state.channels.iter().find(|channel| {
                channel.role_id == mapping.role_id
                    && channel.requested_channel_name == requested_name
            })
        });
        let resolved = match resolve_channel_mapping(
            &client,
            discord_token,
            &options.discord_api_base,
            &body.guild_id,
            mapping,
            checkpoint,
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                return completion_response(
                    StatusCode::BAD_REQUEST,
                    false,
                    provider,
                    rerun_policy,
                    explicit_rerun_policy,
                    existing_completion_state.as_ref(),
                    Some(format!(
                        "failed to resolve channel for agent '{}': {}",
                        mapping.role_id, error
                    )),
                    Vec::new(),
                    serde_json::Map::new(),
                );
            }
        };
        resolved_channels.push(resolved);
    }

    if let Err(error) = validate_unique_resolved_channels(&resolved_channels) {
        return completion_response(
            StatusCode::BAD_REQUEST,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            existing_completion_state.as_ref(),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let channels_created = resolved_channels
        .iter()
        .filter(|mapping| mapping.resolution == ChannelResolutionKind::CreatedChannel)
        .count();
    let checkpoint_reused = resolved_channels
        .iter()
        .filter(|mapping| mapping.resolution == ChannelResolutionKind::Checkpoint)
        .count();
    let has_partial_apply = channels_created > 0
        || checkpoint_state
            .map(|state| state.partial_apply)
            .unwrap_or(false);

    let mut completion_state = build_onboarding_completion_state(
        &request_fingerprint,
        &body.guild_id,
        provider,
        rerun_policy,
        OnboardingCompletionStage::ChannelsResolved,
        has_partial_apply,
        has_partial_apply,
        None,
        &resolved_channels,
    );
    if let Err(error) = save_onboarding_completion_state(&root, &completion_state) {
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if options.fail_after_stage == Some(OnboardingCompletionStage::ChannelsResolved) {
        let error = format!(
            "test failpoint triggered after stage {}",
            OnboardingCompletionStage::ChannelsResolved.as_str()
        );
        completion_state.last_error = Some(error.clone());
        completion_state.retry_recommended = true;
        if let Err(save_error) = save_onboarding_completion_state(&root, &completion_state) {
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                Some(format!(
                    "{error}; additionally failed to persist completion state: {save_error}"
                )),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let conflicts = if let Some(pool) = state.pg_pool_ref() {
        collect_onboarding_conflicts_pg(pool, &root, provider, &resolved_channels, rerun_policy)
            .await
    } else {
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        {
            Err("Postgres pool is required to check onboarding database conflicts".to_string())
        }
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            let conn = match legacy_db(state).lock() {
                Ok(conn) => conn,
                Err(error) => {
                    completion_state.last_error = Some(format!("{error}"));
                    let _ = save_onboarding_completion_state(&root, &completion_state);
                    return completion_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        false,
                        provider,
                        rerun_policy,
                        explicit_rerun_policy,
                        Some(&completion_state),
                        Some(format!("{error}")),
                        Vec::new(),
                        serde_json::Map::new(),
                    );
                }
            };
            collect_onboarding_conflicts(&conn, &root, provider, &resolved_channels, rerun_policy)
        }
    };

    let conflicts = match conflicts {
        Ok(conflicts) => conflicts,
        Err(error) => {
            completion_state.last_error = Some(error.clone());
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                Some(error),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };
    if !conflicts.is_empty() {
        let error = "onboarding rerun would overwrite existing agent/channel bindings; re-run with rerun_policy=replace_existing only if you intend to replace them".to_string();
        completion_state.last_error = Some(error.clone());
        completion_state.retry_recommended = false;
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::CONFLICT,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            conflicts,
            serde_json::Map::new(),
        );
    }

    let config_dir = crate::runtime_layout::config_dir(&root);
    if let Err(error) = std::fs::create_dir_all(&config_dir) {
        completion_state.last_error = Some(format!(
            "failed to create config dir {}: {error}",
            config_dir.display()
        ));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let workspaces_dir = root.join("workspaces");
    if let Err(error) = std::fs::create_dir_all(&workspaces_dir) {
        completion_state.last_error = Some(format!(
            "failed to create workspaces dir {}: {error}",
            workspaces_dir.display()
        ));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }
    for mapping in &resolved_channels {
        let ws_dir = workspaces_dir.join(&mapping.role_id);
        if let Err(error) = std::fs::create_dir_all(&ws_dir) {
            completion_state.last_error = Some(format!(
                "failed to create workspace {}: {error}",
                ws_dir.display()
            ));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    }

    if let Err(error) = write_onboarding_role_map(&root, provider, &resolved_channels) {
        completion_state.last_error = Some(error);
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Err(error) = write_agentdesk_channel_bindings(&root, provider, &resolved_channels) {
        completion_state.last_error = Some(format!("failed to write agentdesk.yaml: {error}"));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Err(error) = write_agentdesk_discord_config(
        &root,
        &body.guild_id,
        &body.token,
        provider,
        body.command_token_2.as_deref(),
        body.command_provider_2.as_deref(),
        body.owner_id.as_deref(),
    ) {
        completion_state.last_error = Some(format!(
            "failed to write agentdesk.yaml discord config: {error}"
        ));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Err(error) = write_credential_token(&root, "announce", body.announce_token.as_deref()) {
        completion_state.last_error = Some(format!("failed to write announce credential: {error}"));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Err(error) = write_credential_token(&root, "notify", body.notify_token.as_deref()) {
        completion_state.last_error = Some(format!("failed to write notify credential: {error}"));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let settings_report = match verify_onboarding_settings_artifacts(
        &root,
        &body.token,
        provider,
        body.command_token_2.as_deref(),
        body.command_provider_2.as_deref(),
        &body.guild_id,
        body.owner_id.as_deref(),
        body.announce_token.as_deref(),
        body.notify_token.as_deref(),
        &resolved_channels,
    ) {
        Ok(report) => report,
        Err(error) => {
            completion_state.last_error =
                Some(format!("onboarding settings verification failed: {error}"));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };

    let pipeline_report = match verify_onboarding_pipeline_artifact(&root) {
        Ok(report) => report,
        Err(error) => {
            completion_state.last_error =
                Some(format!("onboarding pipeline verification failed: {error}"));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };

    completion_state = build_onboarding_completion_state(
        &request_fingerprint,
        &body.guild_id,
        provider,
        rerun_policy,
        OnboardingCompletionStage::ArtifactsPersisted,
        true,
        true,
        None,
        &resolved_channels,
    );
    if let Err(error) = save_onboarding_completion_state(&root, &completion_state) {
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if options.fail_after_stage == Some(OnboardingCompletionStage::ArtifactsPersisted) {
        let error = format!(
            "test failpoint triggered after stage {}",
            OnboardingCompletionStage::ArtifactsPersisted.as_str()
        );
        completion_state.last_error = Some(error.clone());
        completion_state.retry_recommended = true;
        if let Err(save_error) = save_onboarding_completion_state(&root, &completion_state) {
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                Some(format!(
                    "{error}; additionally failed to persist completion state: {save_error}"
                )),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Some(pool) = state.pg_pool_ref() {
        if let Err(error) = persist_onboarding_pg(pool, body, provider, &resolved_channels).await {
            completion_state.last_error = Some(error);
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    } else {
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        {
            completion_state.last_error =
                Some("Postgres pool is required to persist onboarding state".to_string());
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            let mut conn = match legacy_db(state).lock() {
                Ok(conn) => conn,
                Err(error) => {
                    completion_state.last_error = Some(format!("{error}"));
                    let _ = save_onboarding_completion_state(&root, &completion_state);
                    return completion_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        false,
                        provider,
                        rerun_policy,
                        explicit_rerun_policy,
                        Some(&completion_state),
                        Some(format!("{error}")),
                        Vec::new(),
                        serde_json::Map::new(),
                    );
                }
            };

            let tx = match conn.transaction() {
                Ok(tx) => tx,
                Err(error) => {
                    completion_state.last_error =
                        Some(format!("failed to start onboarding transaction: {error}"));
                    let _ = save_onboarding_completion_state(&root, &completion_state);
                    return completion_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        false,
                        provider,
                        rerun_policy,
                        explicit_rerun_policy,
                        Some(&completion_state),
                        completion_state.last_error.clone(),
                        Vec::new(),
                        serde_json::Map::new(),
                    );
                }
            };

            for (key, value) in [
                ("onboarding_bot_token", Some(body.token.trim())),
                ("onboarding_guild_id", Some(body.guild_id.trim())),
                ("onboarding_provider", Some(provider)),
                (
                    "onboarding_owner_id",
                    body.owner_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty()),
                ),
                (
                    "onboarding_announce_token",
                    body.announce_token
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty()),
                ),
                (
                    "onboarding_notify_token",
                    body.notify_token
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty()),
                ),
                (
                    "onboarding_command_token_2",
                    body.command_token_2
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty()),
                ),
                (
                    "onboarding_command_provider_2",
                    body.command_provider_2
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty()),
                ),
                ("onboarding_complete", Some("true")),
            ] {
                match value {
                    Some(value) => {
                        if let Err(error) = tx.execute(
                            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                            sqlite_test::params![key, value],
                        ) {
                            completion_state.last_error =
                                Some(format!("failed to persist kv_meta {}: {error}", key));
                            let _ = save_onboarding_completion_state(&root, &completion_state);
                            return completion_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                false,
                                provider,
                                rerun_policy,
                                explicit_rerun_policy,
                                Some(&completion_state),
                                completion_state.last_error.clone(),
                                Vec::new(),
                                serde_json::Map::new(),
                            );
                        }
                    }
                    None => {
                        if let Err(error) = tx.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
                        {
                            completion_state.last_error =
                                Some(format!("failed to clear kv_meta {}: {error}", key));
                            let _ = save_onboarding_completion_state(&root, &completion_state);
                            return completion_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                false,
                                provider,
                                rerun_policy,
                                explicit_rerun_policy,
                                Some(&completion_state),
                                completion_state.last_error.clone(),
                                Vec::new(),
                                serde_json::Map::new(),
                            );
                        }
                    }
                }
            }

            for mapping in &resolved_channels {
                if let Err(error) = tx.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, description, system_prompt, status, xp) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 0) \
             ON CONFLICT(id) DO UPDATE SET \
               name = COALESCE(excluded.name, agents.name), \
               provider = COALESCE(excluded.provider, agents.provider), \
               discord_channel_id = excluded.discord_channel_id, \
               description = COALESCE(excluded.description, agents.description), \
               system_prompt = COALESCE(excluded.system_prompt, agents.system_prompt)",
            sqlite_test::params![
                mapping.role_id,
                mapping.role_id,
                provider,
                mapping.channel_id,
                mapping.description,
                mapping.system_prompt
            ],
        ) {
            completion_state.last_error =
                Some(format!("failed to upsert agent {}: {error}", mapping.role_id));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
            }

            if !resolved_channels.is_empty() {
                let (template_name, template_name_ko, template_icon, template_color) =
                    match body.template.as_deref() {
                        Some("delivery") => ("Delivery Squad", "전달 스쿼드", "🚀", "#8b5cf6"),
                        Some("operations") => ("Operations Cell", "운영 셀", "🛠️", "#10b981"),
                        Some("insight") => ("Insight Desk", "인사이트 데스크", "📚", "#3b82f6"),
                        _ => ("General", "일반", "📁", "#6b7280"),
                    };

                let office_id = "hq";
                if let Err(error) = tx.execute(
                "INSERT OR IGNORE INTO offices (id, name, name_ko, icon) VALUES (?1, ?2, ?3, ?4)",
                sqlite_test::params![office_id, "Headquarters", "본사", "🏛️"],
            ) {
                completion_state.last_error =
                    Some(format!("failed to upsert default office: {error}"));
                let _ = save_onboarding_completion_state(&root, &completion_state);
                return completion_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    false,
                    provider,
                    rerun_policy,
                    explicit_rerun_policy,
                    Some(&completion_state),
                    completion_state.last_error.clone(),
                    Vec::new(),
                    serde_json::Map::new(),
                );
            }

                let dept_id = body.template.as_deref().unwrap_or("general").to_string();
                if let Err(error) = tx.execute(
            "INSERT OR IGNORE INTO departments (id, name, name_ko, icon, color, office_id, sort_order) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            sqlite_test::params![
                dept_id,
                template_name,
                template_name_ko,
                template_icon,
                template_color,
                office_id,
            ],
        ) {
            completion_state.last_error =
                Some(format!("failed to upsert onboarding department: {error}"));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }

                for mapping in &resolved_channels {
                    if let Err(error) = tx.execute(
                        "INSERT OR REPLACE INTO office_agents (office_id, agent_id, department_id) \
                 VALUES (?1, ?2, ?3)",
                        sqlite_test::params![office_id, mapping.role_id, dept_id],
                    ) {
                        completion_state.last_error = Some(format!(
                            "failed to assign office agent {}: {error}",
                            mapping.role_id
                        ));
                        let _ = save_onboarding_completion_state(&root, &completion_state);
                        return completion_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            false,
                            provider,
                            rerun_policy,
                            explicit_rerun_policy,
                            Some(&completion_state),
                            completion_state.last_error.clone(),
                            Vec::new(),
                            serde_json::Map::new(),
                        );
                    }
                    if let Err(error) = tx.execute(
                        "UPDATE agents SET department = ?1 WHERE id = ?2",
                        sqlite_test::params![dept_id, mapping.role_id],
                    ) {
                        completion_state.last_error = Some(format!(
                            "failed to set agent department {}: {error}",
                            mapping.role_id
                        ));
                        let _ = save_onboarding_completion_state(&root, &completion_state);
                        return completion_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            false,
                            provider,
                            rerun_policy,
                            explicit_rerun_policy,
                            Some(&completion_state),
                            completion_state.last_error.clone(),
                            Vec::new(),
                            serde_json::Map::new(),
                        );
                    }
                }
            }

            if let Err(error) = tx.commit() {
                completion_state.last_error =
                    Some(format!("failed to commit onboarding transaction: {error}"));
                let _ = save_onboarding_completion_state(&root, &completion_state);
                return completion_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    false,
                    provider,
                    rerun_policy,
                    explicit_rerun_policy,
                    Some(&completion_state),
                    completion_state.last_error.clone(),
                    Vec::new(),
                    serde_json::Map::new(),
                );
            }
        }
    }

    completion_state = build_onboarding_completion_state(
        &request_fingerprint,
        &body.guild_id,
        provider,
        rerun_policy,
        OnboardingCompletionStage::Completed,
        false,
        false,
        None,
        &resolved_channels,
    );
    if let Err(error) = save_onboarding_completion_state(&root, &completion_state) {
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }
    if let Err(error) = clear_onboarding_draft(&root) {
        tracing::warn!("failed to clear onboarding draft after completion: {error}");
    }

    let checklist = vec![
        json!({
            "key": "channels",
            "ok": true,
            "label": "Discord channels ready",
            "detail": format!(
                "{} channel mappings resolved ({} created, {} reused, {} checkpointed)",
                resolved_channels.len(),
                channels_created,
                resolved_channels.len().saturating_sub(channels_created + checkpoint_reused),
                checkpoint_reused,
            ),
        }),
        json!({
            "key": "settings",
            "ok": true,
            "label": "Settings persisted",
            "detail": format!(
                "agentdesk config, credentials, role-map, and {} workspaces verified",
                resolved_channels.len()
            ),
        }),
        json!({
            "key": "pipeline",
            "ok": true,
            "label": "Pipeline ready",
            "detail": format!(
                "default pipeline verified at {}",
                pipeline_report["path"].as_str().unwrap_or("(unknown)")
            ),
        }),
    ];

    let mut extra = serde_json::Map::new();
    extra.insert("agents_created".to_string(), json!(resolved_channels.len()));
    extra.insert("channels_created".to_string(), json!(channels_created));
    extra.insert("checklist".to_string(), json!(checklist));
    extra.insert(
        "artifacts".to_string(),
        json!({
            "settings": settings_report,
            "pipeline": pipeline_report,
            "channel_mappings": resolved_channels
                .iter()
                .map(|mapping| {
                    json!({
                        "role_id": mapping.role_id,
                        "channel_id": mapping.channel_id,
                        "channel_name": mapping.channel_name,
                        "requested_channel_name": mapping.requested_channel_name,
                        "created": mapping.created,
                        "resolution": mapping.resolution,
                    })
                })
                .collect::<Vec<_>>(),
        }),
    );

    completion_response(
        StatusCode::OK,
        true,
        provider,
        rerun_policy,
        explicit_rerun_policy,
        Some(&completion_state),
        None,
        Vec::new(),
        extra,
    )
}

