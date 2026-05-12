#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct OnboardingDraftBotInfo {
    valid: bool,
    bot_id: Option<String>,
    bot_name: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct OnboardingDraftCommandBot {
    provider: String,
    token: String,
    bot_info: Option<OnboardingDraftBotInfo>,
}

impl Default for OnboardingDraftCommandBot {
    fn default() -> Self {
        Self {
            provider: "claude".to_string(),
            token: String::new(),
            bot_info: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct OnboardingDraftProviderStatus {
    installed: bool,
    logged_in: bool,
    version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct OnboardingDraftAgent {
    id: String,
    name: String,
    name_en: Option<String>,
    description: String,
    description_en: Option<String>,
    prompt: String,
    custom: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct OnboardingDraftChannelAssignment {
    agent_id: String,
    agent_name: String,
    recommended_name: String,
    channel_id: String,
    channel_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct OnboardingDraft {
    version: u8,
    updated_at_ms: i64,
    step: u8,
    command_bots: Vec<OnboardingDraftCommandBot>,
    announce_token: String,
    notify_token: String,
    announce_bot_info: Option<OnboardingDraftBotInfo>,
    notify_bot_info: Option<OnboardingDraftBotInfo>,
    provider_statuses: BTreeMap<String, OnboardingDraftProviderStatus>,
    selected_template: Option<String>,
    agents: Vec<OnboardingDraftAgent>,
    custom_name: String,
    custom_desc: String,
    custom_name_en: String,
    custom_desc_en: String,
    expanded_agent: Option<String>,
    selected_guild: String,
    channel_assignments: Vec<OnboardingDraftChannelAssignment>,
    owner_id: String,
    has_existing_setup: bool,
    confirm_rerun_overwrite: bool,
}

impl OnboardingDraft {
    fn normalize(mut self) -> Result<Self, String> {
        if self.version != ONBOARDING_DRAFT_VERSION {
            return Err(format!(
                "unsupported onboarding draft version '{}'",
                self.version
            ));
        }
        self.step = self.step.clamp(1, 5);
        let now = now_unix_ms();
        self.updated_at_ms = if self.updated_at_ms > 0 {
            self.updated_at_ms
                .min(now.saturating_add(MAX_ONBOARDING_DRAFT_FUTURE_SKEW_MS))
        } else {
            now
        };
        if self.command_bots.is_empty() {
            self.command_bots.push(OnboardingDraftCommandBot::default());
        }
        if self.command_bots.len() > MAX_ONBOARDING_DRAFT_COMMAND_BOTS {
            return Err(format!(
                "onboarding draft exceeds max command bot entries ({MAX_ONBOARDING_DRAFT_COMMAND_BOTS})"
            ));
        }
        if self.agents.len() > MAX_ONBOARDING_DRAFT_AGENTS {
            return Err(format!(
                "onboarding draft exceeds max agents ({MAX_ONBOARDING_DRAFT_AGENTS})"
            ));
        }
        if self.channel_assignments.len() > MAX_ONBOARDING_DRAFT_CHANNEL_ASSIGNMENTS {
            return Err(format!(
                "onboarding draft exceeds max channel assignments ({MAX_ONBOARDING_DRAFT_CHANNEL_ASSIGNMENTS})"
            ));
        }
        if self.provider_statuses.len() > MAX_ONBOARDING_DRAFT_PROVIDER_STATUSES {
            return Err(format!(
                "onboarding draft exceeds max provider statuses ({MAX_ONBOARDING_DRAFT_PROVIDER_STATUSES})"
            ));
        }
        self.owner_id = self.owner_id.trim().to_string();
        parse_owner_id(Some(self.owner_id.as_str()))?;
        let payload_size = serde_json::to_vec(&self)
            .map_err(|error| {
                format!("failed to serialize onboarding draft for validation: {error}")
            })?
            .len();
        if payload_size > MAX_ONBOARDING_DRAFT_BYTES {
            return Err(format!(
                "onboarding draft exceeds max payload size ({} bytes)",
                MAX_ONBOARDING_DRAFT_BYTES
            ));
        }
        Ok(self)
    }

    fn redact_secrets(mut self) -> Self {
        for bot in &mut self.command_bots {
            bot.token.clear();
        }
        self.announce_token.clear();
        self.notify_token.clear();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingSetupMode {
    Fresh,
    Rerun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingResumeState {
    None,
    DraftAvailable,
    PartialApply,
}

fn onboarding_setup_mode(completed: bool) -> OnboardingSetupMode {
    if completed {
        OnboardingSetupMode::Rerun
    } else {
        OnboardingSetupMode::Fresh
    }
}

fn onboarding_resume_state(
    draft_available: bool,
    completion_state: Option<&OnboardingCompletionState>,
) -> OnboardingResumeState {
    if completion_state
        .map(|state| state.partial_apply)
        .unwrap_or(false)
    {
        OnboardingResumeState::PartialApply
    } else if draft_available {
        OnboardingResumeState::DraftAvailable
    } else {
        OnboardingResumeState::None
    }
}

fn sanitize_legacy_owner_id(owner_id: Option<String>) -> Option<String> {
    let value = owner_id?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    parse_owner_id(Some(trimmed)).ok().flatten()?;
    Some(trimmed.to_string())
}

fn sanitize_draft_owner_id(owner_id: &str) -> String {
    let trimmed = owner_id.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if parse_owner_id(Some(trimmed)).ok().flatten().is_some() {
        trimmed.to_string()
    } else {
        String::new()
    }
}

fn onboarding_draft_secret_policy_value() -> serde_json::Value {
    json!({
        "stores_raw_tokens": false,
        "returns_raw_tokens_in_draft": false,
        "masked_in_status_after_completion": true,
        "cleared_on_complete": true,
        "cleared_on_delete": true,
    })
}
