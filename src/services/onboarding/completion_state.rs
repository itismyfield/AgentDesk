
#[derive(Debug, Deserialize, Clone)]
pub struct CompleteBody {
    pub token: String,
    pub announce_token: Option<String>,
    pub notify_token: Option<String>,
    pub command_token_2: Option<String>,
    pub command_provider_2: Option<String>,
    pub guild_id: String,
    pub owner_id: Option<String>,
    pub provider: Option<String>,
    pub channels: Vec<ChannelMapping>,
    pub template: Option<String>,
    pub rerun_policy: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ChannelMapping {
    pub channel_id: String,
    pub channel_name: String,
    pub role_id: String,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChannelResolutionKind {
    ProvidedId,
    ExistingChannel,
    CreatedChannel,
    Checkpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedChannelMapping {
    channel_id: String,
    channel_name: String,
    requested_channel_name: String,
    role_id: String,
    description: Option<String>,
    system_prompt: Option<String>,
    created: bool,
    resolution: ChannelResolutionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingRerunPolicy {
    ReuseExisting,
    ReplaceExisting,
}

impl OnboardingRerunPolicy {
    fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("reuse_existing") => Ok(Self::ReuseExisting),
            Some("replace_existing") => Ok(Self::ReplaceExisting),
            Some(other) => Err(format!("unsupported rerun_policy '{other}'")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ReuseExisting => "reuse_existing",
            Self::ReplaceExisting => "replace_existing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingCompletionStage {
    ChannelsResolved,
    ArtifactsPersisted,
    Completed,
}

impl OnboardingCompletionStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::ChannelsResolved => "channels_resolved",
            Self::ArtifactsPersisted => "artifacts_persisted",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OnboardingCompletionChannelState {
    role_id: String,
    requested_channel_name: String,
    channel_id: String,
    channel_name: String,
    created: bool,
    resolution: ChannelResolutionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OnboardingCompletionState {
    request_fingerprint: String,
    guild_id: String,
    provider: String,
    rerun_policy: String,
    stage: OnboardingCompletionStage,
    partial_apply: bool,
    retry_recommended: bool,
    updated_at_ms: i64,
    last_error: Option<String>,
    channels: Vec<OnboardingCompletionChannelState>,
}

#[derive(Debug, Clone)]
struct CompleteExecutionOptions {
    discord_api_base: String,
    fail_after_stage: Option<OnboardingCompletionStage>,
}

impl Default for CompleteExecutionOptions {
    fn default() -> Self {
        Self {
            discord_api_base: DISCORD_API_BASE.to_string(),
            fail_after_stage: None,
        }
    }
}

fn is_discord_channel_id(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit())
}

fn normalized_channel_name(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_start_matches('#').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn desired_channel_name(mapping: &ChannelMapping) -> Result<String, String> {
    normalized_channel_name(&mapping.channel_name)
        .or_else(|| normalized_channel_name(&mapping.channel_id))
        .ok_or_else(|| format!("agent '{}' is missing a channel name", mapping.role_id))
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn onboarding_draft_path(runtime_root: &Path) -> PathBuf {
    crate::runtime_layout::config_dir(runtime_root).join("onboarding_draft.json")
}

fn load_onboarding_draft(runtime_root: &Path) -> Result<Option<OnboardingDraft>, String> {
    let path = onboarding_draft_path(runtime_root);
    if !path.is_file() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read onboarding draft {}: {e}", path.display()))?;
    let draft = match serde_json::from_str::<OnboardingDraft>(&content) {
        Ok(draft) => draft,
        Err(error) => {
            let corrupt_path = path.with_file_name(format!(
                "{}.corrupt-{}",
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("draft"),
                now_unix_ms()
            ));
            match std::fs::rename(&path, &corrupt_path) {
                Ok(()) => tracing::warn!(
                    "ignored corrupt onboarding draft {}; moved to {}: {}",
                    path.display(),
                    corrupt_path.display(),
                    error
                ),
                Err(rename_error) => tracing::warn!(
                    "ignored corrupt onboarding draft {}; failed to move aside: {}; parse error: {}",
                    path.display(),
                    rename_error,
                    error
                ),
            }
            return Ok(None);
        }
    };
    let mut draft = draft;
    draft.owner_id = sanitize_draft_owner_id(&draft.owner_id);
    Ok(Some(draft))
}

fn save_onboarding_draft(runtime_root: &Path, draft: &OnboardingDraft) -> Result<(), String> {
    let path = onboarding_draft_path(runtime_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create draft dir {}: {e}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(draft)
        .map_err(|e| format!("failed to serialize onboarding draft: {e}"))?;
    crate::services::discord::runtime_store::atomic_write(&path, &content)
        .map_err(|e| format!("failed to write onboarding draft {}: {e}", path.display()))
}

fn clear_onboarding_draft(runtime_root: &Path) -> Result<(), String> {
    let path = onboarding_draft_path(runtime_root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to remove onboarding draft {}: {error}",
            path.display()
        )),
    }
}

fn onboarding_completion_state_path(runtime_root: &Path) -> PathBuf {
    crate::runtime_layout::config_dir(runtime_root).join("onboarding_completion_state.json")
}

fn load_onboarding_completion_state(
    runtime_root: &Path,
) -> Result<Option<OnboardingCompletionState>, String> {
    let path = onboarding_completion_state_path(runtime_root);
    if !path.is_file() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "failed to read onboarding completion state {}: {e}",
            path.display()
        )
    })?;
    let state = match serde_json::from_str::<OnboardingCompletionState>(&content) {
        Ok(state) => state,
        Err(error) => {
            let corrupt_path = path.with_file_name(format!(
                "{}.corrupt-{}",
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("state"),
                now_unix_ms()
            ));
            match std::fs::rename(&path, &corrupt_path) {
                Ok(()) => tracing::warn!(
                    "ignored corrupt onboarding completion state {}; moved to {}: {}",
                    path.display(),
                    corrupt_path.display(),
                    error
                ),
                Err(rename_error) => tracing::warn!(
                    "ignored corrupt onboarding completion state {}; failed to move aside: {}; parse error: {}",
                    path.display(),
                    rename_error,
                    error
                ),
            }
            return Ok(None);
        }
    };
    Ok(Some(state))
}

fn save_onboarding_completion_state(
    runtime_root: &Path,
    state: &OnboardingCompletionState,
) -> Result<(), String> {
    let path = onboarding_completion_state_path(runtime_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create completion state dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let content = serde_json::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize onboarding completion state: {e}"))?;
    crate::services::discord::runtime_store::atomic_write(&path, &content).map_err(|e| {
        format!(
            "failed to write onboarding completion state {}: {e}",
            path.display()
        )
    })
}

fn build_onboarding_completion_state(
    request_fingerprint: &str,
    guild_id: &str,
    provider: &str,
    rerun_policy: OnboardingRerunPolicy,
    stage: OnboardingCompletionStage,
    partial_apply: bool,
    retry_recommended: bool,
    last_error: Option<String>,
    resolved_channels: &[ResolvedChannelMapping],
) -> OnboardingCompletionState {
    OnboardingCompletionState {
        request_fingerprint: request_fingerprint.to_string(),
        guild_id: guild_id.trim().to_string(),
        provider: provider.trim().to_string(),
        rerun_policy: rerun_policy.as_str().to_string(),
        stage,
        partial_apply,
        retry_recommended,
        updated_at_ms: now_unix_ms(),
        last_error,
        channels: resolved_channels
            .iter()
            .map(|mapping| OnboardingCompletionChannelState {
                role_id: mapping.role_id.clone(),
                requested_channel_name: mapping.requested_channel_name.clone(),
                channel_id: mapping.channel_id.clone(),
                channel_name: mapping.channel_name.clone(),
                created: mapping.created,
                resolution: mapping.resolution,
            })
            .collect(),
    }
}

fn onboarding_completion_state_value(
    completion_state: Option<&OnboardingCompletionState>,
) -> serde_json::Value {
    completion_state
        .and_then(|state| serde_json::to_value(state).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn onboarding_rerun_policy_value(
    rerun_policy: OnboardingRerunPolicy,
    explicit: bool,
) -> serde_json::Value {
    json!({
        "applied": rerun_policy.as_str(),
        "explicit": explicit,
        "supported": ["reuse_existing", "replace_existing"],
    })
}

fn completion_response(
    status: StatusCode,
    ok: bool,
    provider: &str,
    rerun_policy: OnboardingRerunPolicy,
    explicit_rerun_policy: bool,
    completion_state: Option<&OnboardingCompletionState>,
    error: Option<String>,
    conflicts: Vec<String>,
    mut extra: serde_json::Map<String, serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    extra.insert("ok".to_string(), json!(ok));
    extra.insert("provider".to_string(), json!(provider));
    extra.insert(
        "partial_apply".to_string(),
        json!(
            completion_state
                .map(|state| state.partial_apply)
                .unwrap_or(false)
        ),
    );
    extra.insert(
        "retry_recommended".to_string(),
        json!(
            completion_state
                .map(|state| state.retry_recommended)
                .unwrap_or(false)
        ),
    );
    extra.insert(
        "completion_state".to_string(),
        onboarding_completion_state_value(completion_state),
    );
    extra.insert(
        "rerun_policy".to_string(),
        onboarding_rerun_policy_value(rerun_policy, explicit_rerun_policy),
    );
    if let Some(error) = error {
        extra.insert("error".to_string(), json!(error));
    }
    if !conflicts.is_empty() {
        extra.insert("conflicts".to_string(), json!(conflicts));
    }
    (status, serde_json::Value::Object(extra))
}

fn normalized_optional_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

fn requested_channel_fingerprint(body: &CompleteBody, provider: &str) -> Result<String, String> {
    let mut channels = body
        .channels
        .iter()
        .map(|mapping| {
            Ok(json!({
                "role_id": mapping.role_id.trim(),
                "channel_id": normalized_channel_name(&mapping.channel_id)
                    .unwrap_or_else(|| mapping.channel_id.trim().to_string()),
                "channel_name": desired_channel_name(mapping)?,
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;

    channels.sort_by(|left, right| left.to_string().cmp(&right.to_string()));

    let payload = json!({
        "guild_id": body.guild_id.trim(),
        "provider": provider.trim(),
        "channels": channels,
    });
    let mut hasher = Sha256::new();
    hasher.update(payload.to_string().as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

fn role_map_entry_role_id(value: &serde_json::Value) -> Option<&str> {
    value.get("roleId").and_then(|value| value.as_str())
}

fn role_map_entry_channel_id(value: &serde_json::Value) -> Option<&str> {
    value.get("channelId").and_then(|value| value.as_str())
}

async fn discord_list_guild_channels(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let url = format!(
        "{}/guilds/{}/channels",
        api_base.trim_end_matches('/'),
        guild_id
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|e| format!("failed to fetch guild channels: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Discord API {status} while listing channels: {body}"
        ));
    }

    resp.json::<Vec<serde_json::Value>>()
        .await
        .map_err(|e| format!("failed to parse guild channels: {e}"))
}

async fn discord_create_text_channel(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
    channel_name: &str,
    topic: Option<&str>,
) -> Result<serde_json::Value, String> {
    let url = format!(
        "{}/guilds/{}/channels",
        api_base.trim_end_matches('/'),
        guild_id
    );

    let mut payload = json!({
        "name": channel_name,
        "type": 0,
    });

    if let Some(topic) = topic.map(str::trim).filter(|value| !value.is_empty()) {
        let truncated: String = topic.chars().take(1024).collect();
        payload["topic"] = json!(truncated);
    }

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bot {}", token))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("failed to create channel '{channel_name}': {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Discord API {status} while creating channel '{channel_name}': {body}"
        ));
    }

    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("failed to parse created channel '{channel_name}': {e}"))
}

async fn resolve_channel_mapping(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
    mapping: &ChannelMapping,
    checkpoint: Option<&OnboardingCompletionChannelState>,
) -> Result<ResolvedChannelMapping, String> {
    let requested_name = desired_channel_name(mapping)?;

    if let Some(checkpoint) = checkpoint {
        return Ok(ResolvedChannelMapping {
            channel_id: checkpoint.channel_id.clone(),
            channel_name: checkpoint.channel_name.clone(),
            requested_channel_name: requested_name.clone(),
            role_id: mapping.role_id.clone(),
            description: mapping.description.clone(),
            system_prompt: mapping.system_prompt.clone(),
            created: checkpoint.created,
            resolution: ChannelResolutionKind::Checkpoint,
        });
    }

    if is_discord_channel_id(&mapping.channel_id) {
        return Ok(ResolvedChannelMapping {
            channel_id: mapping.channel_id.trim().to_string(),
            channel_name: requested_name.clone(),
            requested_channel_name: requested_name.clone(),
            role_id: mapping.role_id.clone(),
            description: mapping.description.clone(),
            system_prompt: mapping.system_prompt.clone(),
            created: false,
            resolution: ChannelResolutionKind::ProvidedId,
        });
    }

    let guild_id = guild_id.trim();
    if guild_id.is_empty() {
        return Err(format!(
            "cannot create channel '{}' without selecting a Discord server",
            requested_name
        ));
    }

    let existing = discord_list_guild_channels(client, token, api_base, guild_id)
        .await?
        .into_iter()
        .find(|channel| {
            channel.get("type").and_then(|value| value.as_i64()) == Some(0)
                && channel
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(|name| name == requested_name)
                    .unwrap_or(false)
        });

    if let Some(channel) = existing {
        let channel_id = channel
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("existing channel '{}' is missing an id", requested_name))?;
        let channel_name = channel
            .get("name")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&requested_name)
            .to_string();

        return Ok(ResolvedChannelMapping {
            channel_id: channel_id.to_string(),
            channel_name,
            requested_channel_name: requested_name.clone(),
            role_id: mapping.role_id.clone(),
            description: mapping.description.clone(),
            system_prompt: mapping.system_prompt.clone(),
            created: false,
            resolution: ChannelResolutionKind::ExistingChannel,
        });
    }

    let created = discord_create_text_channel(
        client,
        token,
        api_base,
        guild_id,
        &requested_name,
        mapping.description.as_deref(),
    )
    .await?;

    let channel_id = created
        .get("id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("created channel '{}' is missing an id", requested_name))?;
    let channel_name = created
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&requested_name)
        .to_string();

    Ok(ResolvedChannelMapping {
        channel_id: channel_id.to_string(),
        channel_name,
        requested_channel_name: requested_name,
        role_id: mapping.role_id.clone(),
        description: mapping.description.clone(),
        system_prompt: mapping.system_prompt.clone(),
        created: true,
        resolution: ChannelResolutionKind::CreatedChannel,
    })
}

