use crate::services::platform::BinaryResolution;
use crate::utils::format::safe_prefix;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Tmux session name prefix — always "AgentDesk".
pub const TMUX_SESSION_PREFIX: &str = "AgentDesk";

/// Tmux session name suffix (reserved for future isolation use; currently empty).
pub fn tmux_env_suffix() -> &'static str {
    ""
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    Qwen,
    Unsupported(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub binary_name: &'static str,
    pub supports_structured_output: bool,
    pub supports_resume: bool,
    pub supports_tool_stream: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRuntimeProbe {
    pub provider: ProviderKind,
    pub capabilities: ProviderCapabilities,
    pub resolution: BinaryResolution,
    pub version: Option<String>,
    pub probe_failure_kind: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderDefaultBehavior {
    pub resume_without_reset: bool,
    pub runtime_model: Option<&'static str>,
    pub source_label: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderRegistryEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    pub cli_init_label: &'static str,
    pub channel_suffix: Option<&'static str>,
    pub default_channel_provider: bool,
    pub counterpart_provider_ids: &'static [&'static str],
    pub capabilities: ProviderCapabilities,
    pub default_behavior: ProviderDefaultBehavior,
    pub default_context_window: u64,
    pub managed_tmux_backend: bool,
    pub managed_tmux_wrapper_subcommand: Option<&'static str>,
}

const CLAUDE_COUNTERPARTS: &[&str] = &["codex", "gemini", "opencode", "qwen"];
const CODEX_COUNTERPARTS: &[&str] = &["claude", "gemini", "opencode", "qwen"];
const GEMINI_COUNTERPARTS: &[&str] = &["codex", "claude", "opencode", "qwen"];
const OPENCODE_COUNTERPARTS: &[&str] = &["codex", "claude", "gemini", "qwen"];
const QWEN_COUNTERPARTS: &[&str] = &["codex", "claude", "gemini", "opencode"];

const PROVIDER_REGISTRY: &[ProviderRegistryEntry] = &[
    ProviderRegistryEntry {
        id: "claude",
        display_name: "Claude",
        cli_init_label: "claude (Anthropic)",
        channel_suffix: Some("-cc"),
        default_channel_provider: true,
        counterpart_provider_ids: CLAUDE_COUNTERPARTS,
        capabilities: ProviderCapabilities {
            binary_name: "claude",
            supports_structured_output: true,
            supports_resume: true,
            supports_tool_stream: true,
        },
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "Claude provider default",
        },
        default_context_window: 1_000_000,
        managed_tmux_backend: true,
        managed_tmux_wrapper_subcommand: Some("tmux-wrapper"),
    },
    ProviderRegistryEntry {
        id: "codex",
        display_name: "Codex",
        cli_init_label: "codex (OpenAI)",
        channel_suffix: Some("-cdx"),
        default_channel_provider: false,
        counterpart_provider_ids: CODEX_COUNTERPARTS,
        capabilities: ProviderCapabilities {
            binary_name: "codex",
            supports_structured_output: true,
            supports_resume: true,
            supports_tool_stream: true,
        },
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "provider default",
        },
        default_context_window: 200_000,
        managed_tmux_backend: true,
        managed_tmux_wrapper_subcommand: Some("codex-tmux-wrapper"),
    },
    ProviderRegistryEntry {
        id: "gemini",
        display_name: "Gemini",
        cli_init_label: "gemini (Google)",
        channel_suffix: Some("-gm"),
        default_channel_provider: false,
        counterpart_provider_ids: GEMINI_COUNTERPARTS,
        capabilities: ProviderCapabilities {
            binary_name: "gemini",
            supports_structured_output: true,
            supports_resume: true,
            supports_tool_stream: true,
        },
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "provider default",
        },
        default_context_window: 1_000_000,
        managed_tmux_backend: false,
        managed_tmux_wrapper_subcommand: None,
    },
    ProviderRegistryEntry {
        id: "opencode",
        display_name: "OpenCode",
        cli_init_label: "opencode (OpenCode)",
        channel_suffix: Some("-oc"),
        default_channel_provider: false,
        counterpart_provider_ids: OPENCODE_COUNTERPARTS,
        capabilities: ProviderCapabilities {
            binary_name: "opencode",
            supports_structured_output: true,
            supports_resume: false,
            supports_tool_stream: true,
        },
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: false,
            runtime_model: None,
            source_label: "provider default",
        },
        default_context_window: 128_000,
        managed_tmux_backend: false,
        managed_tmux_wrapper_subcommand: None,
    },
    ProviderRegistryEntry {
        id: "qwen",
        display_name: "Qwen Code",
        cli_init_label: "qwen (Alibaba)",
        channel_suffix: Some("-qw"),
        default_channel_provider: false,
        counterpart_provider_ids: QWEN_COUNTERPARTS,
        capabilities: ProviderCapabilities {
            binary_name: "qwen",
            supports_structured_output: true,
            supports_resume: true,
            supports_tool_stream: true,
        },
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "provider default",
        },
        default_context_window: 128_000,
        managed_tmux_backend: true,
        managed_tmux_wrapper_subcommand: Some("qwen-tmux-wrapper"),
    },
];

pub fn provider_registry() -> &'static [ProviderRegistryEntry] {
    PROVIDER_REGISTRY
}

pub fn supported_provider_ids() -> Vec<&'static str> {
    provider_registry().iter().map(|entry| entry.id).collect()
}

impl ProviderKind {
    pub fn registry_entry(&self) -> Option<&'static ProviderRegistryEntry> {
        provider_registry()
            .iter()
            .find(|entry| entry.id == self.as_str() && !matches!(self, Self::Unsupported(_)))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Qwen => "qwen",
            Self::Unsupported(s) => s.as_str(),
        }
    }

    pub fn display_name(&self) -> &str {
        self.registry_entry()
            .map(|entry| entry.display_name)
            .unwrap_or_else(|| match self {
                Self::Unsupported(s) => s.as_str(),
                _ => self.as_str(),
            })
    }

    pub fn preferred_counterparts(&self) -> Vec<Self> {
        self.registry_entry()
            .map(|entry| {
                entry
                    .counterpart_provider_ids
                    .iter()
                    .filter_map(|provider_id| Self::from_str(provider_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn select_counterpart_from<I>(&self, available: I) -> Option<Self>
    where
        I: IntoIterator<Item = Self>,
    {
        let available: Vec<Self> = available.into_iter().collect();
        self.preferred_counterparts().into_iter().find(|candidate| {
            available
                .iter()
                .any(|available_provider| available_provider == candidate)
        })
    }

    pub fn default_channel_provider() -> Option<Self> {
        provider_registry()
            .iter()
            .find(|entry| entry.default_channel_provider)
            .and_then(|entry| Self::from_str(entry.id))
    }

    pub fn from_channel_suffix(channel_name: &str) -> Option<Self> {
        provider_registry()
            .iter()
            .filter_map(|entry| {
                entry
                    .channel_suffix
                    .filter(|suffix| channel_name.ends_with(suffix))
                    .and_then(|_| Self::from_str(entry.id))
            })
            .next()
    }

    pub fn counterpart(&self) -> Self {
        self.preferred_counterparts()
            .into_iter()
            .next()
            .unwrap_or_else(|| self.clone())
    }

    pub fn capabilities(&self) -> Option<ProviderCapabilities> {
        self.registry_entry().map(|entry| entry.capabilities)
    }

    /// Provider-specific behavior when AgentDesk clears its explicit model
    /// override and falls through to the provider-managed default path.
    pub fn default_model_behavior(&self) -> ProviderDefaultBehavior {
        self.registry_entry()
            .map(|entry| entry.default_behavior)
            .unwrap_or(ProviderDefaultBehavior {
                resume_without_reset: true,
                runtime_model: None,
                source_label: "provider default",
            })
    }

    #[allow(dead_code)]
    pub fn resolve_runtime_path(&self) -> Option<String> {
        match self {
            Self::Claude => crate::services::claude::resolve_claude_path(),
            Self::Codex => crate::services::codex::resolve_codex_path(),
            Self::Gemini => crate::services::gemini::resolve_gemini_path(),
            Self::OpenCode => crate::services::opencode::resolve_opencode_path(),
            Self::Qwen => crate::services::qwen::resolve_qwen_path(),
            Self::Unsupported(_) => None,
        }
    }

    pub fn probe_runtime(&self) -> Option<ProviderRuntimeProbe> {
        let capabilities = self.capabilities()?;
        let resolution = crate::services::platform::resolve_provider_binary(self.as_str());
        let (version, probe_failure_kind) = resolution
            .resolved_path
            .as_ref()
            .map(|path| crate::services::platform::probe_resolved_binary_version(path, &resolution))
            .unwrap_or((None, None));
        Some(ProviderRuntimeProbe {
            provider: self.clone(),
            capabilities,
            resolution,
            version,
            probe_failure_kind,
        })
    }

    /// Parse a known provider string. Returns None for unknown providers.
    pub fn from_str(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        provider_registry()
            .iter()
            .find(|entry| entry.id == normalized)
            .and_then(|entry| match entry.id {
                "claude" => Some(Self::Claude),
                "codex" => Some(Self::Codex),
                "gemini" => Some(Self::Gemini),
                "opencode" => Some(Self::OpenCode),
                "qwen" => Some(Self::Qwen),
                _ => None,
            })
    }

    pub fn cli_init_labels() -> Vec<&'static str> {
        provider_registry()
            .iter()
            .map(|entry| entry.cli_init_label)
            .collect()
    }

    pub fn provider_for_cli_init_index(index: usize) -> Option<Self> {
        provider_registry()
            .get(index)
            .and_then(|entry| Self::from_str(entry.id))
    }

    pub fn resolve_channel_provider(
        channel_name: Option<&str>,
        explicit_provider: Option<&ProviderKind>,
    ) -> Option<Self> {
        explicit_provider
            .cloned()
            .filter(ProviderKind::is_supported)
            .or_else(|| channel_name.and_then(Self::from_channel_suffix))
            .or_else(Self::default_channel_provider)
    }

    /// Returns true if this is a known, supported provider.
    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }

    pub fn is_channel_supported(
        &self,
        channel_name: Option<&str>,
        is_dm: bool,
        explicit_provider: Option<&ProviderKind>,
    ) -> bool {
        if is_dm {
            return self.is_supported();
        }
        Self::resolve_channel_provider(channel_name, explicit_provider)
            .is_some_and(|provider| provider == *self)
    }

    /// Parse a provider string, returning Unsupported for unknown providers.
    pub fn from_str_or_unsupported(raw: &str) -> Self {
        Self::from_str(raw).unwrap_or_else(|| Self::Unsupported(raw.trim().to_string()))
    }

    /// Returns provider-specific environment variables for auto-compact configuration.
    /// - Claude: CLAUDE_AUTOCOMPACT_PCT_OVERRIDE = percent
    /// - Codex: uses CLI args instead (see compact_cli_config)
    #[allow(dead_code)]
    pub fn compact_env_vars(&self, percent: u64) -> Vec<(String, String)> {
        match self {
            Self::Claude => vec![(
                "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".to_string(),
                percent.to_string(),
            )],
            // Codex uses -c CLI arg, not env vars
            _ => vec![],
        }
    }

    /// Default context window size in tokens for this provider.
    pub fn default_context_window(&self) -> u64 {
        self.registry_entry()
            .map(|entry| entry.default_context_window)
            .unwrap_or(200_000)
    }

    /// Resolve the context window for a specific model, falling back to
    /// the provider default if the model-specific value is unavailable.
    pub fn resolve_context_window(&self, model: Option<&str>) -> u64 {
        if let (Self::Codex, Some(m)) = (self, model) {
            if let Some(window) = codex_model_context_window(m) {
                return window;
            }
        }
        self.default_context_window()
    }

    /// Returns Codex-specific CLI config overrides for auto-compact.
    /// Codex uses model_auto_compact_token_limit (absolute token count).
    pub fn compact_cli_config(&self, percent: u64, context_window: u64) -> Vec<(String, String)> {
        match self {
            Self::Codex => {
                let token_limit = context_window * percent / 100;
                vec![(
                    "model_auto_compact_token_limit".to_string(),
                    token_limit.to_string(),
                )]
            }
            _ => vec![],
        }
    }

    /// Returns true when this provider can own a reusable local tmux/process
    /// session that AgentDesk may need to clear or pre-seed in inflight state.
    pub fn uses_managed_tmux_backend(&self) -> bool {
        self.registry_entry()
            .map(|entry| entry.managed_tmux_backend)
            .unwrap_or(false)
    }

    pub fn managed_tmux_wrapper_subcommand(&self) -> Option<&'static str> {
        self.registry_entry()
            .and_then(|entry| entry.managed_tmux_wrapper_subcommand)
    }

    pub fn build_tmux_session_name(&self, channel_name: &str) -> String {
        let sanitized: String = channel_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        // #145: Preserve -t{thread_id} suffix when truncating, so unified-thread
        // guards (is_unified_thread_channel_name_active) can extract the thread ID.
        let trimmed: String = if let Some(pos) = sanitized.rfind("-t") {
            let suffix = &sanitized[pos..];
            let is_thread_suffix =
                suffix.len() > 2 && suffix[2..].chars().all(|c| c.is_ascii_digit());
            if is_thread_suffix && sanitized.len() > 44 {
                let prefix_budget = 44_usize.saturating_sub(suffix.len());
                let prefix = safe_prefix(&sanitized[..pos], prefix_budget);
                format!("{}{}", prefix, suffix)
            } else {
                safe_prefix(&sanitized, 44).to_string()
            }
        } else {
            safe_prefix(&sanitized, 44).to_string()
        };
        format!(
            "{}-{}-{}{}",
            TMUX_SESSION_PREFIX,
            self.as_str(),
            trimmed,
            tmux_env_suffix()
        )
    }
}

pub fn parse_provider_and_channel_from_tmux_name(
    session_name: &str,
) -> Option<(ProviderKind, String)> {
    let prefix = format!("{}-", TMUX_SESSION_PREFIX);
    let stripped = session_name.strip_prefix(&prefix)?;
    let suffix = tmux_env_suffix();
    let without_suffix = if !suffix.is_empty() {
        stripped.strip_suffix(suffix).unwrap_or(stripped)
    } else {
        stripped
    };
    for entry in provider_registry() {
        let prefix = format!("{}-", entry.id);
        if let Some(rest) = without_suffix.strip_prefix(&prefix) {
            if let Some(provider) = ProviderKind::from_str(entry.id) {
                return Some((provider, rest.to_string()));
            }
        }
    }
    ProviderKind::default_channel_provider().map(|provider| (provider, without_suffix.to_string()))
}

pub fn compose_structured_turn_prompt(
    prompt: &str,
    system_prompt: Option<&str>,
    _allowed_tools: Option<&[String]>,
) -> String {
    let mut sections = Vec::new();

    if let Some(system_prompt) = system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!(
            "[Authoritative Instructions]\n{}\n\nThese instructions are authoritative for this turn. Follow them over any generic assistant persona unless the user explicitly asks to inspect or compare them.",
            system_prompt
        ));
    }

    if sections.is_empty() {
        return prompt.to_string();
    }

    sections.push(format!("[User Request]\n{}", prompt));
    sections.join("\n\n")
}

pub fn should_omit_repeated_system_prompt(
    provider: &ProviderKind,
    session_id: Option<&str>,
) -> bool {
    matches!(provider, ProviderKind::Codex)
        && session_id
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
}

/// Env flag that, when set to `1`/`true`/`yes`/`all`, extends envelope
/// dedup to every provider — not just Codex+resumed. We default to OFF so
/// the existing Codex-only behavior is preserved bit-for-bit; operators
/// flip the flag once they've verified each provider correctly retains the
/// system prompt across resumed turns.
///
/// See #2662 for the audit measurement (~200KB/session of repeated envelope).
const ENVELOPE_DEDUP_FEATURE_ENV: &str = "AGENTDESK_ENVELOPE_DEDUP";

fn envelope_dedup_globally_enabled() -> bool {
    std::env::var(ENVELOPE_DEDUP_FEATURE_ENV)
        .ok()
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "all")
        })
        .unwrap_or(false)
}

/// #2668: process-local registry of (provider, role_id, system_prompt_hash)
/// tuples that have already received their full dev-role instructions in this
/// dcserver lifetime. The first time we see a (role, hash) combo we include
/// the full system_prompt; on subsequent fresh forks of the same role with
/// the same hash we emit the same compact marker the resume path uses, since
/// the prior session is already past the instruction step in the same
/// provider process anyway (Codex CLI is sticky-launched per role on this
/// host).
///
/// The hash is byte-hash of the rendered system_prompt — if anything in the
/// prompt drift (role binding, SAK, peer agents) the hash changes and we go
/// back to a full re-inject, so correctness never regresses.
fn dev_instruction_registry()
-> &'static std::sync::Mutex<std::collections::HashSet<(ProviderKind, String, u64)>> {
    static CELL: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<(ProviderKind, String, u64)>>,
    > = std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

fn hash_system_prompt(system_prompt: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    system_prompt.hash(&mut hasher);
    hasher.finish()
}

/// #2668: returns `true` if the provider+role_id combo has already received
/// the rendered system_prompt at least once in this process. The very first
/// call registers the (provider, role_id, hash) and returns `false`, so the
/// caller knows it must still inject the full instructions on this turn.
///
/// Used by the Codex launch path to drop the ~1KB `<permissions instructions>`
/// + dev-role text on every fresh fork after the first one inside the same
/// dcserver lifetime, while still re-injecting on actual prompt drift.
pub fn note_dev_role_instructions_sent(
    provider: &ProviderKind,
    role_id: Option<&str>,
    system_prompt: &str,
) -> bool {
    let Some(role_id) = role_id.map(str::trim).filter(|value| !value.is_empty()) else {
        // No role binding → we cannot dedupe safely.
        return false;
    };
    if !matches!(provider, ProviderKind::Codex) {
        // Only Codex pays the dev-role re-injection cost flagged by #2668.
        return false;
    }
    let hash = hash_system_prompt(system_prompt);
    let key = (provider.clone(), role_id.to_string(), hash);
    let mut guard = dev_instruction_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // `insert` returns true when the value was *newly* inserted → that's our
    // signal that the dev instructions had not been sent before.
    let newly_inserted = guard.insert(key);
    // We "already-sent" iff the entry was NOT newly inserted.
    !newly_inserted
}

/// #2668: test-only helper to drain the dev-role registry between cases.
#[cfg(test)]
pub fn reset_dev_role_instruction_registry_for_tests() {
    let mut guard = dev_instruction_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.clear();
}

/// Returns `Some(prompt)` if the caller should include the system prompt
/// in this turn, or `None` if it can be safely omitted. The omission
/// decision considers, in order:
///
/// 1. Empty system prompts (always omitted — nothing to send).
/// 2. The legacy Codex+resumed-session rule
///    ([`should_omit_repeated_system_prompt`]).
/// 3. (Opt-in via `AGENTDESK_ENVELOPE_DEDUP=all`) a content-hash dedup
///    against [`crate::services::envelope_dedup`]: if a session has
///    already received this exact envelope, omit it.
///
/// Callers are responsible for **recording** the envelope after they've
/// successfully sent the turn (via
/// [`record_envelope_after_send`]). We separate lookup and
/// record so a transient send failure does not poison the dedup state.
pub fn system_prompt_for_provider_turn<'a>(
    provider: &ProviderKind,
    session_id: Option<&str>,
    system_prompt: &'a str,
) -> Option<&'a str> {
    let trimmed = system_prompt.trim();
    if trimmed.is_empty() {
        return None;
    }
    if should_omit_repeated_system_prompt(provider, session_id) {
        return None;
    }
    if envelope_dedup_globally_enabled() {
        if let Some(key) = dedup_session_key(provider, session_id) {
            if crate::services::envelope_dedup::envelope_already_sent(&key, trimmed) {
                return None;
            }
        }
    }
    Some(system_prompt)
}

/// Caller-side hook: after a turn has actually been dispatched, record
/// that the envelope was sent so the next turn can skip it. No-op when the
/// global dedup feature flag is off, when the session_id is missing, or
/// when `system_prompt` is empty.
pub fn record_envelope_after_send(
    provider: &ProviderKind,
    session_id: Option<&str>,
    system_prompt: &str,
) {
    if !envelope_dedup_globally_enabled() {
        return;
    }
    let trimmed = system_prompt.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(key) = dedup_session_key(provider, session_id) {
        crate::services::envelope_dedup::record_envelope_sent(&key, trimmed);
    }
}

/// Build a stable dedup-store key for `(provider, session_id)`. Sessions
/// without an `id` cannot be deduped — there is no continuity for the
/// dedup store to attach to — so we return `None` and the caller falls
/// back to the legacy behavior. Provider is mixed in so a Claude session
/// and a Codex session that happen to share an id do not collide.
fn dedup_session_key(provider: &ProviderKind, session_id: Option<&str>) -> Option<String> {
    let sid = session_id.map(str::trim).filter(|s| !s.is_empty())?;
    let kind = match provider {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Qwen => "qwen",
        ProviderKind::Unsupported(_) => "unsupported",
    };
    Some(format!("{kind}::{sid}"))
}

/// #2668: same contract as [`system_prompt_for_provider_turn`] but additionally
/// short-circuits when the rendered system_prompt has already been delivered to
/// the same `(provider, role_id)` fork inside this dcserver lifetime. The
/// caller is the Codex spawn path, which can then emit the same short
/// `[Provider Session Reuse]` marker the resume path uses, instead of the
/// full ~1KB dev-role text.
///
/// `force_full_inject` is reserved for callers that must defeat the dedupe
/// (e.g. operator forced a fresh codex fork because the previous one wedged);
/// when set, the function behaves exactly like the original helper.
pub fn system_prompt_for_provider_turn_with_dev_role_dedup<'a>(
    provider: &ProviderKind,
    session_id: Option<&str>,
    role_id: Option<&str>,
    system_prompt: &'a str,
    force_full_inject: bool,
) -> Option<&'a str> {
    let base = system_prompt_for_provider_turn(provider, session_id, system_prompt);
    if force_full_inject {
        return base;
    }
    let Some(prompt) = base else {
        return None;
    };
    // The dedupe only fires for genuinely fresh codex forks (no session_id).
    // Resumes are already handled by `should_omit_repeated_system_prompt`.
    if session_id
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return Some(prompt);
    }
    if note_dev_role_instructions_sent(provider, role_id, prompt) {
        None
    } else {
        Some(prompt)
    }
}

pub fn compact_resumed_provider_turn_prompt(
    provider: &ProviderKind,
    session_id: Option<&str>,
    prompt: String,
) -> String {
    if !should_omit_repeated_system_prompt(provider, session_id) {
        return prompt;
    }

    format!(
        "[Provider Session Reuse]\n\
         The prior authoritative Discord, role, and tool instructions already present in this \
         Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
         files, and memory recall below as new actionable input.\n\n{prompt}"
    )
}

/// #2668: companion to [`compact_resumed_provider_turn_prompt`] for fresh
/// forks. Emits the same compact reuse marker when the dev-role dedup
/// registry has seen this `(provider, role_id, system_prompt_hash)`
/// combination before in this dcserver lifetime; otherwise returns the
/// prompt unchanged so the caller still inlines the full instructions on
/// the first fork.
pub fn compact_fresh_codex_fork_prompt_when_dev_role_sent(
    provider: &ProviderKind,
    role_id: Option<&str>,
    system_prompt: &str,
    prompt: String,
) -> String {
    if !matches!(provider, ProviderKind::Codex) {
        return prompt;
    }
    if !note_dev_role_instructions_sent(provider, role_id, system_prompt) {
        return prompt;
    }
    format!(
        "[Provider Session Reuse]\n\
         The prior authoritative Discord, role, and tool instructions already issued to this \
         role in the current dcserver lifetime still apply. Treat only this turn's user request, \
         reply context, uploaded files, and memory recall below as new actionable input.\n\n{prompt}"
    )
}

pub fn is_readonly_tool_policy(allowed_tools: Option<&[String]>) -> bool {
    let Some(allowed_tools) = allowed_tools.filter(|tools| !tools.is_empty()) else {
        return false;
    };

    allowed_tools.iter().all(|tool| {
        matches!(
            tool.trim().to_ascii_lowercase().as_str(),
            "read" | "grep" | "glob"
        )
    })
}

#[cfg(test)]
mod dev_role_dedup_tests {
    use super::*;

    /// All four tests share global state through `dev_instruction_registry`,
    /// so serialise them under one mutex to keep them deterministic across
    /// `cargo test --test-threads=N` runs. The registry's reset helper is
    /// called at the start of each test under this guard.
    fn lock_test_state() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn first_fresh_fork_inlines_full_dev_instructions() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp = "[Authoritative Instructions]\nRole=ch-td\nrules…";
        let result = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-td"),
            sp,
            false,
        );
        assert_eq!(result, Some(sp));
    }

    #[test]
    fn second_fresh_fork_with_same_hash_omits_dev_instructions() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp = "[Authoritative Instructions]\nRole=ch-td\nrules…";
        // First call registers the (provider, role, hash) tuple.
        let _ = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-td"),
            sp,
            false,
        );
        // Second fresh fork should now omit.
        let result = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-td"),
            sp,
            false,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn force_full_inject_defeats_dedup() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp = "[Authoritative Instructions]\nRole=ch-td\nrules…";
        let _ = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-td"),
            sp,
            false,
        );
        let result = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-td"),
            sp,
            true,
        );
        assert_eq!(result, Some(sp));
    }

    #[test]
    fn prompt_drift_reinjects_full_instructions() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp1 = "[Authoritative Instructions]\nRole=ch-td\nrules v1";
        let sp2 = "[Authoritative Instructions]\nRole=ch-td\nrules v2";
        let _ = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-td"),
            sp1,
            false,
        );
        // Same role, different prompt content → different hash → full inject.
        let result = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-td"),
            sp2,
            false,
        );
        assert_eq!(result, Some(sp2));
    }

    #[test]
    fn role_id_isolation_between_agents() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp = "[Authoritative Instructions]\nshared body";
        let _ = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-td"),
            sp,
            false,
        );
        let result_pd = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            Some("ch-pd"),
            sp,
            false,
        );
        // Different role_id with identical prompt body still gets full inject —
        // a different agent has its own dev role on the Codex side.
        assert_eq!(result_pd, Some(sp));
    }

    #[test]
    fn non_codex_providers_are_not_affected() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp = "[Authoritative Instructions]\nbody";
        // Two calls in a row for Claude → must always return the prompt.
        let _ = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Claude,
            None,
            Some("ch-td"),
            sp,
            false,
        );
        let result = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Claude,
            None,
            Some("ch-td"),
            sp,
            false,
        );
        assert_eq!(result, Some(sp));
    }

    #[test]
    fn missing_role_id_disables_dedup() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp = "[Authoritative Instructions]\nbody";
        let _ = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            None,
            sp,
            false,
        );
        let result = system_prompt_for_provider_turn_with_dev_role_dedup(
            &ProviderKind::Codex,
            None,
            None,
            sp,
            false,
        );
        // No role_id → we cannot dedupe safely, full inject every time.
        assert_eq!(result, Some(sp));
    }

    #[test]
    fn compact_fresh_codex_fork_emits_marker_after_first_fork() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp = "[Authoritative Instructions]\nbody";
        let user_prompt = "do the thing".to_string();
        let first = compact_fresh_codex_fork_prompt_when_dev_role_sent(
            &ProviderKind::Codex,
            Some("ch-td"),
            sp,
            user_prompt.clone(),
        );
        assert_eq!(first, user_prompt);
        let second = compact_fresh_codex_fork_prompt_when_dev_role_sent(
            &ProviderKind::Codex,
            Some("ch-td"),
            sp,
            user_prompt.clone(),
        );
        assert!(second.starts_with("[Provider Session Reuse]"));
        assert!(second.contains(&user_prompt));
    }

    #[test]
    fn compact_fresh_codex_fork_does_not_alter_non_codex_prompts() {
        let _guard = lock_test_state();
        reset_dev_role_instruction_registry_for_tests();
        let sp = "[Authoritative Instructions]\nbody";
        let user_prompt = "do the thing".to_string();
        let result = compact_fresh_codex_fork_prompt_when_dev_role_sent(
            &ProviderKind::Claude,
            Some("ch-td"),
            sp,
            user_prompt.clone(),
        );
        assert_eq!(result, user_prompt);
    }
}

#[cfg(test)]
mod prompt_reuse_tests {
    use super::{
        ProviderKind, compact_resumed_provider_turn_prompt, should_omit_repeated_system_prompt,
        system_prompt_for_provider_turn,
    };

    #[test]
    fn provider_registry_exposes_managed_tmux_wrapper_subcommands() {
        assert_eq!(
            ProviderKind::Claude.managed_tmux_wrapper_subcommand(),
            Some("tmux-wrapper")
        );
        assert_eq!(
            ProviderKind::Codex.managed_tmux_wrapper_subcommand(),
            Some("codex-tmux-wrapper")
        );
        assert_eq!(
            ProviderKind::Qwen.managed_tmux_wrapper_subcommand(),
            Some("qwen-tmux-wrapper")
        );
        assert_eq!(ProviderKind::Gemini.managed_tmux_wrapper_subcommand(), None);
        assert_eq!(
            ProviderKind::OpenCode.managed_tmux_wrapper_subcommand(),
            None
        );
    }

    #[test]
    fn codex_resume_omits_repeated_system_prompt() {
        assert!(should_omit_repeated_system_prompt(
            &ProviderKind::Codex,
            Some("thread-1")
        ));
        assert_eq!(
            system_prompt_for_provider_turn(
                &ProviderKind::Codex,
                Some("thread-1"),
                "full Discord prompt"
            ),
            None
        );
        assert_eq!(
            system_prompt_for_provider_turn(&ProviderKind::Codex, None, "full Discord prompt"),
            Some("full Discord prompt")
        );
        assert_eq!(
            system_prompt_for_provider_turn(
                &ProviderKind::Claude,
                Some("session-1"),
                "full Discord prompt"
            ),
            Some("full Discord prompt")
        );
    }

    // #2662: feature-flagged envelope dedup must not affect legacy callers
    // when the env flag is off, and must dedup correctly when it is on.
    // We use a per-test mutex because env mutation is process-global.
    use super::{
        ENVELOPE_DEDUP_FEATURE_ENV, envelope_dedup_globally_enabled, record_envelope_after_send,
    };
    use std::sync::Mutex;
    static ENV_DEDUP_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct EnvelopeDedupEnvGuard {
        previous: Option<String>,
    }
    impl EnvelopeDedupEnvGuard {
        fn set(value: Option<&str>) -> Self {
            let previous = std::env::var(ENVELOPE_DEDUP_FEATURE_ENV).ok();
            match value {
                Some(v) => unsafe { std::env::set_var(ENVELOPE_DEDUP_FEATURE_ENV, v) },
                None => unsafe { std::env::remove_var(ENVELOPE_DEDUP_FEATURE_ENV) },
            }
            Self { previous }
        }
    }
    impl Drop for EnvelopeDedupEnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var(ENVELOPE_DEDUP_FEATURE_ENV, v) },
                None => unsafe { std::env::remove_var(ENVELOPE_DEDUP_FEATURE_ENV) },
            }
        }
    }

    #[test]
    fn envelope_dedup_disabled_by_default() {
        let _guard = ENV_DEDUP_TEST_LOCK.lock().unwrap();
        let _env = EnvelopeDedupEnvGuard::set(None);
        assert!(!envelope_dedup_globally_enabled());
    }

    #[test]
    fn envelope_dedup_env_flag_truthy_values() {
        let _guard = ENV_DEDUP_TEST_LOCK.lock().unwrap();
        for v in ["1", "true", "TRUE", "yes", "Yes", "all", "ALL", "  all  "] {
            let _env = EnvelopeDedupEnvGuard::set(Some(v));
            assert!(
                envelope_dedup_globally_enabled(),
                "expected truthy for {v:?}"
            );
        }
        for v in ["0", "false", "off", "", " ", "no", "maybe"] {
            let _env = EnvelopeDedupEnvGuard::set(Some(v));
            assert!(
                !envelope_dedup_globally_enabled(),
                "expected falsy for {v:?}"
            );
        }
    }

    #[test]
    fn envelope_dedup_no_effect_when_flag_off() {
        let _guard = ENV_DEDUP_TEST_LOCK.lock().unwrap();
        let _env = EnvelopeDedupEnvGuard::set(None);
        // Two consecutive lookups for Claude+session must both yield the
        // system prompt — feature is off so dedup must not kick in.
        let prompt = "full Discord prompt";
        let first = system_prompt_for_provider_turn(
            &ProviderKind::Claude,
            Some("session-flagoff-1"),
            prompt,
        );
        record_envelope_after_send(&ProviderKind::Claude, Some("session-flagoff-1"), prompt);
        let second = system_prompt_for_provider_turn(
            &ProviderKind::Claude,
            Some("session-flagoff-1"),
            prompt,
        );
        assert_eq!(first, Some(prompt));
        assert_eq!(second, Some(prompt));
    }

    #[test]
    fn envelope_dedup_suppresses_after_record_when_flag_on() {
        let _guard = ENV_DEDUP_TEST_LOCK.lock().unwrap();
        let _env = EnvelopeDedupEnvGuard::set(Some("all"));
        let session = "session-flagon-suppress-xyz";
        let prompt = "[Authoritative Instructions]\nrole: PMD";
        // First turn — envelope must be returned.
        let first = system_prompt_for_provider_turn(&ProviderKind::Claude, Some(session), prompt);
        assert_eq!(first, Some(prompt));
        record_envelope_after_send(&ProviderKind::Claude, Some(session), prompt);
        // Second turn — dedup kicks in.
        let second = system_prompt_for_provider_turn(&ProviderKind::Claude, Some(session), prompt);
        assert_eq!(second, None);
        // Cleanup so we don't pollute other tests.
        crate::services::envelope_dedup::shared().forget_session(&format!("claude::{session}"));
    }

    #[test]
    fn envelope_dedup_isolates_distinct_envelopes() {
        let _guard = ENV_DEDUP_TEST_LOCK.lock().unwrap();
        let _env = EnvelopeDedupEnvGuard::set(Some("all"));
        let session = "session-flagon-distinct-envs";
        let env_a = "[Authoritative Instructions]\nA";
        let env_b = "[Authoritative Instructions]\nB";
        let _ = system_prompt_for_provider_turn(&ProviderKind::Claude, Some(session), env_a);
        record_envelope_after_send(&ProviderKind::Claude, Some(session), env_a);
        // A is now deduped; B must still pass through unchanged.
        assert_eq!(
            system_prompt_for_provider_turn(&ProviderKind::Claude, Some(session), env_a),
            None
        );
        assert_eq!(
            system_prompt_for_provider_turn(&ProviderKind::Claude, Some(session), env_b),
            Some(env_b)
        );
        crate::services::envelope_dedup::shared().forget_session(&format!("claude::{session}"));
    }

    #[test]
    fn envelope_dedup_provider_isolation() {
        let _guard = ENV_DEDUP_TEST_LOCK.lock().unwrap();
        let _env = EnvelopeDedupEnvGuard::set(Some("all"));
        // Same session-id text on different providers must NOT collide —
        // the key includes the provider kind.
        let id = "collision-test-12345";
        let prompt = "[Authoritative Instructions]\nshared content";
        record_envelope_after_send(&ProviderKind::Claude, Some(id), prompt);
        // Codex with the same id text + resumed session still hits the
        // legacy Codex omission path (returns None), so use Gemini for
        // the cross-provider check.
        assert_eq!(
            system_prompt_for_provider_turn(&ProviderKind::Claude, Some(id), prompt),
            None
        );
        assert_eq!(
            system_prompt_for_provider_turn(&ProviderKind::Gemini, Some(id), prompt),
            Some(prompt)
        );
        crate::services::envelope_dedup::shared().forget_session(&format!("claude::{id}"));
    }

    #[test]
    fn envelope_dedup_no_session_id_falls_back_to_legacy() {
        let _guard = ENV_DEDUP_TEST_LOCK.lock().unwrap();
        let _env = EnvelopeDedupEnvGuard::set(Some("all"));
        // Without a session id there is no continuity to dedup against —
        // every turn must return the full envelope.
        let prompt = "[Authoritative Instructions]\nno session";
        assert_eq!(
            system_prompt_for_provider_turn(&ProviderKind::Claude, None, prompt),
            Some(prompt)
        );
        // Recording with no session id is a no-op.
        record_envelope_after_send(&ProviderKind::Claude, None, prompt);
        assert_eq!(
            system_prompt_for_provider_turn(&ProviderKind::Claude, None, prompt),
            Some(prompt)
        );
    }

    #[test]
    fn codex_resume_context_gets_compact_reuse_note() {
        let prompt = compact_resumed_provider_turn_prompt(
            &ProviderKind::Codex,
            Some("thread-1"),
            "[User Request]\nhello".to_string(),
        );

        assert!(prompt.starts_with("[Provider Session Reuse]"));
        assert!(prompt.contains("prior authoritative Discord"));
        assert!(prompt.contains("[User Request]\nhello"));
        assert!(!prompt.contains("[Authoritative Instructions]"));

        let fresh = compact_resumed_provider_turn_prompt(
            &ProviderKind::Codex,
            None,
            "[User Request]\nhello".to_string(),
        );
        assert_eq!(fresh, "[User Request]\nhello");
    }
}

/// Coarse-grained classification of who triggered a cancellation.
///
/// Issue #2335 (a): downstream branches (e.g. should we still speak the
/// partial summary?) need to distinguish "user explicitly told us to stop"
/// from "the watchdog/timeout expired". The free-form `cancel_source` label
/// remains for tracing, but consumers should branch on this enum to avoid
/// brittle string matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CancelSource {
    /// User talked over an in-flight playback / turn ("live PCM cut" or the
    /// explicit-stop barge-in path).
    UserBargeIn,
    /// User explicitly asked to stop via a non-voice control surface.
    ExplicitStop,
    /// Foreground/background summary or ack generation timed out.
    SummaryTimeout,
    /// Surrounding session is being torn down (guild leave, restart, etc.).
    SessionTeardown,
    /// Long-running watchdog deadline elapsed.
    WatchdogTimeout,
    /// Cancel came in via a path that did not classify itself.
    Other,
}

impl CancelSource {
    /// Canonical string label used by tracing fields and downstream
    /// systems (e.g. dispatch row `cancel_reason`).
    pub fn as_label(self) -> &'static str {
        match self {
            CancelSource::UserBargeIn => "user_barge_in",
            CancelSource::ExplicitStop => "explicit_stop",
            CancelSource::SummaryTimeout => "summary_timeout",
            CancelSource::SessionTeardown => "session_teardown",
            CancelSource::WatchdogTimeout => "watchdog_timeout",
            CancelSource::Other => "other",
        }
    }

    /// Best-effort classification of a free-form label set via
    /// [`CancelToken::set_cancel_source`]. Unknown labels fall back to
    /// [`CancelSource::Other`] so consumers can still branch safely.
    pub fn classify(label: &str) -> Self {
        // Order matters: more specific substrings first.
        let lower = label.to_ascii_lowercase();
        if lower.contains("watchdog") || lower.contains("ack_timeout") {
            return CancelSource::WatchdogTimeout;
        }
        if lower.contains("summary_timeout")
            || lower.contains("text_reply_timeout")
            || lower.contains("background_summary")
        {
            return CancelSource::SummaryTimeout;
        }
        if lower.contains("live_cut") || lower.contains("barge_in") {
            return CancelSource::UserBargeIn;
        }
        if lower.contains("teardown")
            || lower.contains("guild_teardown")
            || lower.contains("shutdown")
            || lower.contains("restart")
        {
            return CancelSource::SessionTeardown;
        }
        if lower.contains("explicit_stop") || lower.contains("user_cancel") {
            return CancelSource::ExplicitStop;
        }
        CancelSource::Other
    }
}

/// Cooperative cancellation token shared by provider runtimes and Discord orchestration.
pub struct CancelToken {
    pub cancelled: AtomicBool,
    pub child_pid: Mutex<Option<u32>>,
    cancel_source: Mutex<Option<String>>,
    cancel_source_kind: Mutex<Option<CancelSource>>,
    /// SSH cancel flag — set to true to signal remote execution to close the channel
    #[allow(dead_code)]
    pub ssh_cancel: Mutex<Option<std::sync::Arc<AtomicBool>>>,
    /// tmux session name for cleanup on cancel
    pub tmux_session: Mutex<Option<String>>,
    /// Watchdog deadline as Unix timestamp in milliseconds.
    /// The watchdog fires when `now_ms >= deadline_ms`. Extend by setting a future value.
    /// Operator extensions may move this and the max cap together within configured limits.
    pub watchdog_deadline_ms: AtomicI64,
    /// The current ceiling for watchdog_deadline_ms. Operator extensions may move this forward.
    pub watchdog_max_deadline_ms: AtomicI64,
    /// claude-e rollout Phase 1 (counter-review round 3 with Codex). When
    /// `true`, the synchronous `enforce_watchdog_deadline` poll inside
    /// `spawn_cancel_watchdog` becomes a no-op for this token; the async
    /// Discord watchdog at 30s cadence is the only deadline enforcer.
    /// Set by the headless / text turn watchdog setup paths before they
    /// store the deadline. Direct callers that need the legacy sub-30s
    /// enforcement leave this `false`.
    pub async_managed: AtomicBool,
    /// Normal turn-completion cleanup marker. The Discord bridge may flip
    /// `cancelled` after a terminal frame only to release lingering token
    /// observers; provider cancel watchdogs must not treat that as a live
    /// mid-stream cancel that should kill the child process.
    completion_cleanup: AtomicBool,
    /// Lifecycle-aware restart/handoff mode for inflight preservation.
    pub restart_mode: AtomicU8,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            child_pid: Mutex::new(None),
            cancel_source: Mutex::new(None),
            cancel_source_kind: Mutex::new(None),
            ssh_cancel: Mutex::new(None),
            tmux_session: Mutex::new(None),
            watchdog_deadline_ms: AtomicI64::new(0),
            watchdog_max_deadline_ms: AtomicI64::new(0),
            async_managed: AtomicBool::new(false),
            completion_cleanup: AtomicBool::new(false),
            restart_mode: AtomicU8::new(0),
        }
    }

    /// claude-e rollout Phase 1: opt this token out of synchronous
    /// `enforce_watchdog_deadline` enforcement. The async Discord
    /// watchdog still polls `watchdog_deadline_ms` at 30s and cancels
    /// when the deadline expires; the per-provider sync poll inside
    /// `spawn_cancel_watchdog` stops short-circuiting on it.
    ///
    /// Call this from the Discord turn-watchdog setup paths immediately
    /// before storing `watchdog_deadline_ms`. Non-Discord callers leave
    /// this flag at its `false` default and keep the historical
    /// behaviour.
    pub fn mark_async_managed(&self) {
        self.async_managed.store(true, Ordering::Relaxed);
    }

    /// claude-e rollout Phase 1: read counterpart to
    /// `mark_async_managed`. `enforce_watchdog_deadline` calls this to
    /// decide whether to honour the sync deadline poll.
    pub fn is_async_managed(&self) -> bool {
        self.async_managed.load(Ordering::Relaxed)
    }

    pub fn mark_completion_cleanup(&self) {
        self.completion_cleanup.store(true, Ordering::Relaxed);
    }

    pub fn is_completion_cleanup(&self) -> bool {
        self.completion_cleanup.load(Ordering::Relaxed)
    }

    /// Cancel and clean up any associated tmux session.
    pub fn cancel_with_tmux_cleanup(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(name) = self
            .tmux_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            #[cfg(unix)]
            {
                crate::services::tmux_diagnostics::record_tmux_exit_reason(
                    &name,
                    "턴 취소에 의한 tmux 세션 정리",
                );
                crate::services::platform::tmux::kill_session(
                    &name,
                    "턴 취소에 의한 tmux 세션 정리",
                );
            }
            #[cfg(not(unix))]
            {
                let _ = &name;
            }
        }
    }

    pub fn set_restart_mode(&self, mode: Option<crate::services::discord::InflightRestartMode>) {
        self.restart_mode.store(
            mode.map(crate::services::discord::InflightRestartMode::as_u8)
                .unwrap_or(0),
            Ordering::Relaxed,
        );
    }

    pub fn restart_mode(&self) -> Option<crate::services::discord::InflightRestartMode> {
        crate::services::discord::InflightRestartMode::from_u8(
            self.restart_mode.load(Ordering::Relaxed),
        )
    }

    pub fn set_cancel_source(&self, source: impl Into<String>) {
        let label = source.into();
        // Issue #2335 (a): keep the enum classification in sync with the
        // free-form label so downstream consumers can branch on the variant
        // without re-parsing the string. We only auto-classify when no
        // explicit kind has been set yet so that callers using
        // `set_cancel_source_kind` keep precedence.
        let classified = CancelSource::classify(&label);
        {
            let mut kind = self
                .cancel_source_kind
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if kind.is_none() {
                *kind = Some(classified);
            }
        }
        *self.cancel_source.lock().unwrap_or_else(|e| e.into_inner()) = Some(label);
    }

    /// Explicitly set the structured cancel source. Also updates the
    /// free-form label (used for tracing / dispatch reason) to the canonical
    /// string for the variant when no label was previously recorded.
    pub fn set_cancel_source_kind(&self, kind: CancelSource) {
        *self
            .cancel_source_kind
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(kind);
        let mut label = self.cancel_source.lock().unwrap_or_else(|e| e.into_inner());
        if label.is_none() {
            *label = Some(kind.as_label().to_string());
        }
    }

    pub fn cancel_source(&self) -> Option<String> {
        self.cancel_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Issue #2335 (a): structured classification of the cancellation
    /// trigger. Returns `None` only if neither
    /// [`CancelToken::set_cancel_source`] nor
    /// [`CancelToken::set_cancel_source_kind`] has been called yet.
    pub fn cancel_source_kind(&self) -> Option<CancelSource> {
        *self
            .cancel_source_kind
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

pub fn cancel_requested(token: Option<&CancelToken>) -> bool {
    token.is_some_and(|token| {
        enforce_watchdog_deadline(token, current_unix_millis());
        token.cancelled.load(Ordering::Relaxed)
    })
}

fn current_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn enforce_watchdog_deadline(token: &CancelToken, now_ms: i64) -> bool {
    let deadline_ms = token.watchdog_deadline_ms.load(Ordering::Relaxed);
    // claude-e rollout Phase 1 (counter-review round 3 with Codex):
    // Discord-managed tokens are watched by the async 30s reconcile
    // loop. Skipping the synchronous fire here avoids a class of
    // mid-stream cancellations seen during claude-e e2e (provider
    // sync watchdog killed the per-turn child before the async path
    // had a chance to extend the deadline). Non-Discord callers leave
    // `async_managed=false` and keep the historical sub-30s
    // enforcement.
    if deadline_ms > 0 && now_ms >= deadline_ms && !token.is_async_managed() {
        token.set_cancel_source_kind(CancelSource::WatchdogTimeout);
        token.cancelled.store(true, Ordering::Relaxed);
        return true;
    }
    false
}

pub fn register_child_pid(token: Option<&CancelToken>, child_pid: u32) {
    if let Some(token) = token {
        *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(child_pid);
    }
}

pub struct CancelWatchdog {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CancelWatchdog {
    fn new(done: Arc<AtomicBool>, handle: JoinHandle<()>) -> Self {
        Self {
            done,
            handle: Some(handle),
        }
    }
}

impl Drop for CancelWatchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn cancel_watchdog_should_kill(token: &CancelToken) -> bool {
    token.cancelled.load(Ordering::Relaxed) && !token.is_completion_cleanup()
}

pub fn spawn_cancel_watchdog(
    token: Option<Arc<CancelToken>>,
    child_pid: u32,
    label: &'static str,
) -> Option<CancelWatchdog> {
    let token = token?;
    let done = Arc::new(AtomicBool::new(false));
    let done_for_thread = done.clone();
    let handle = std::thread::spawn(move || {
        while !done_for_thread.load(Ordering::Relaxed) {
            enforce_watchdog_deadline(&token, current_unix_millis());
            if token.cancelled.load(Ordering::Relaxed) {
                if !cancel_watchdog_should_kill(&token) {
                    tracing::debug!(
                        provider_cancel_watchdog = label,
                        child_pid,
                        "cancel watchdog exiting after normal completion cleanup"
                    );
                    return;
                }
                tracing::warn!(
                    provider_cancel_watchdog = label,
                    child_pid,
                    "cancel watchdog killing provider process tree"
                );
                crate::services::process::kill_pid_tree(child_pid);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    Some(CancelWatchdog::new(done, handle))
}

/// Result from reading a provider session output stream until completion or session death.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutputResult {
    /// Normal completion (terminal result observed)
    Completed { offset: u64 },
    /// Session died without producing a terminal result
    SessionDied { offset: u64 },
    /// User cancelled the operation
    Cancelled { offset: u64 },
}

/// Result from sending a follow-up message to an existing provider session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowupResult {
    /// Message delivered and output successfully read to completion.
    Delivered,
    /// Session needs to be killed and recreated.
    RecreateSession { error: String },
}

/// Best-effort tmux watcher handoff after follow-up output polling fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxFollowupFallback {
    /// Offset the watcher should resume from.
    pub last_offset: u64,
    /// Whether the provider path should synthesize an empty Done event first
    /// because the pane is already idle and no unread bytes remain.
    pub emit_synthetic_done: bool,
}

/// Decide whether a managed tmux provider can recover from a follow-up read
/// failure by attaching a watcher instead of silently leaving the dispatch
/// without a watcher on a reused session.
pub fn tmux_followup_fallback_after_read_error(
    start_offset: u64,
    last_observed_offset: u64,
    current_file_len: Option<u64>,
    session_alive: bool,
    ready_for_input: bool,
    output_path_exists: bool,
    input_path_exists: bool,
) -> Option<TmuxFollowupFallback> {
    if !session_alive || !output_path_exists || !input_path_exists {
        return None;
    }

    let file_len = current_file_len.unwrap_or(last_observed_offset);
    let last_offset = std::cmp::min(last_observed_offset, file_len);
    let emit_synthetic_done =
        ready_for_input && last_offset == file_len && last_offset > start_offset;

    Some(TmuxFollowupFallback {
        last_offset,
        emit_synthetic_done,
    })
}

/// Callbacks for session status checks during output file polling.
pub(crate) struct SessionProbe {
    /// Returns true if the session process is still running.
    pub is_alive: Box<dyn Fn() -> bool + Send>,
    /// Returns true if the session is idle and ready for new input.
    pub is_ready_for_input: Box<dyn Fn() -> bool + Send>,
}

impl SessionProbe {
    pub fn new(
        is_alive: impl Fn() -> bool + Send + 'static,
        is_ready_for_input: impl Fn() -> bool + Send + 'static,
    ) -> Self {
        Self {
            is_alive: Box::new(is_alive),
            is_ready_for_input: Box::new(is_ready_for_input),
        }
    }

    #[cfg(unix)]
    pub fn tmux(session_name: String, provider: ProviderKind) -> Self {
        let name_alive = session_name.clone();
        let name_ready = session_name;
        let provider_ready = provider;
        Self::new(
            move || tmux_session_alive(&name_alive),
            move || tmux_session_ready_for_input(&name_ready, &provider_ready),
        )
    }

    #[cfg(unix)]
    pub fn tmux_with_structured_output(
        session_name: String,
        provider: ProviderKind,
        runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
        output_path: String,
    ) -> Self {
        let name_alive = session_name.clone();
        let name_ready = session_name;
        let provider_ready = provider;
        Self::new(
            move || tmux_session_alive(&name_alive),
            move || {
                crate::services::tui_turn_state::jsonl_ready_for_input(
                    &provider_ready,
                    runtime_kind,
                    std::path::Path::new(&output_path),
                    None,
                )
                .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
                .unwrap_or_else(|| tmux_session_ready_for_input(&name_ready, &provider_ready))
            },
        )
    }

    #[cfg(not(unix))]
    pub fn tmux(_session_name: String, _provider: ProviderKind) -> Self {
        Self::new(|| false, || false)
    }

    #[cfg(not(unix))]
    pub fn tmux_with_structured_output(
        _session_name: String,
        _provider: ProviderKind,
        _runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
        _output_path: String,
    ) -> Self {
        Self::new(|| false, || false)
    }

    pub fn process(is_alive: impl Fn() -> bool + Send + 'static) -> Self {
        Self::new(is_alive, || false)
    }
}

#[cfg(unix)]
fn tmux_session_alive(tmux_session_name: &str) -> bool {
    crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
}

pub(crate) fn tmux_capture_indicates_ready_for_input(
    capture: &str,
    provider: &ProviderKind,
) -> bool {
    match provider {
        ProviderKind::Claude => {
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(capture)
        }
        ProviderKind::Codex => {
            crate::services::codex_tui::input::pane_looks_ready_for_codex_prompt(capture)
                || crate::services::tmux_common::tmux_capture_indicates_generic_ready_banner(
                    capture,
                )
                || tmux_capture_contains_wrapper_ready_marker(capture, provider)
        }
        ProviderKind::Qwen => {
            crate::services::tmux_common::tmux_capture_indicates_generic_ready_banner(capture)
                || tmux_capture_contains_wrapper_ready_marker(capture, provider)
        }
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => {
            crate::services::tmux_common::tmux_capture_indicates_generic_ready_banner(capture)
                || tmux_capture_contains_wrapper_ready_marker(capture, provider)
        }
    }
}

fn tmux_capture_contains_wrapper_ready_marker(capture: &str, provider: &ProviderKind) -> bool {
    capture
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(12)
        .any(|line| wrapper_ready_marker_matches_provider(line, provider))
}

fn wrapper_ready_marker_matches_provider(line: &str, provider: &ProviderKind) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return false;
    };
    value.get("type").and_then(|field| field.as_str())
        == Some(crate::services::tmux_common::WRAPPER_READY_FOR_INPUT_EVENT)
        && value.get("provider").and_then(|field| field.as_str()) == Some(provider.as_str())
}

#[cfg(all(test, unix))]
mod ready_input_prompt_tests {
    use super::ProviderKind;

    #[test]
    fn detects_ready_banner() {
        let capture = "\
build logs\n\
Ready for input (type message + Enter)\n\
> ";
        assert!(super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Claude
        ));
        assert!(super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Qwen
        ));
    }

    #[test]
    fn detects_claude_tui_prompt_above_footer() {
        let capture = "\
output recap\n\
─────────────────────────────────────────────────────────────────────────────\n\
❯\u{00a0}\n\
─────────────────────────────────────────────────────────────────────────────\n\
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%\n\
  📁 agentdesk (main*) │ Todos: -\n\
  ⏵⏵ bypass permissions on";
        assert!(super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Claude
        ));
        assert!(!super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Codex
        ));
        assert!(!super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Qwen
        ));
    }

    #[test]
    fn detects_codex_tui_prompt_for_codex_only() {
        let capture = "\
some earlier output\n\
╭──────────────────────────────────────────────────────────────╮\n\
│ ▌                                                            │\n\
╰──────────────────────────────────────────────────────────────╯\n\
  Esc to interrupt   Ctrl+J newline   ⏎ send";
        assert!(super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Codex
        ));
        assert!(!super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Claude
        ));
        assert!(!super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Qwen
        ));
    }

    #[test]
    fn detects_provider_scoped_wrapper_ready_marker() {
        let qwen_capture =
            r#"{"type":"ready_for_input","provider":"qwen","ts":"2026-05-18T00:00:00Z"}"#;
        assert!(super::tmux_capture_indicates_ready_for_input(
            qwen_capture,
            &ProviderKind::Qwen
        ));
        assert!(!super::tmux_capture_indicates_ready_for_input(
            qwen_capture,
            &ProviderKind::Codex
        ));
    }

    #[test]
    fn rejects_non_ready_capture() {
        let capture = "\
build logs\n\
waiting for tool output\n\
still running";
        assert!(!super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Claude
        ));
    }
}

#[cfg(unix)]
pub(crate) fn tmux_session_ready_for_input(
    tmux_session_name: &str,
    provider: &ProviderKind,
) -> bool {
    crate::services::platform::tmux::capture_pane(tmux_session_name, -80)
        .map(|stdout| tmux_capture_indicates_ready_for_input(&stdout, provider))
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(crate) fn tmux_session_ready_for_input(
    _tmux_session_name: &str,
    _provider: &ProviderKind,
) -> bool {
    false
}

const READY_FOR_INPUT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const READY_FOR_INPUT_IDLE_MIN_PROBES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadyForInputIdleState {
    None,
    FreshIdle,
    PostWorkIdleTimeout,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ReadyForInputIdleTracker {
    first_ready_at: Option<std::time::Instant>,
    consecutive_ready_probes: u32,
    recovery_primed: bool,
}

impl ReadyForInputIdleTracker {
    pub(crate) fn primed_for_recovery() -> Self {
        Self {
            recovery_primed: true,
            ..Self::default()
        }
    }

    pub(crate) fn record_output(&mut self) {
        self.recovery_primed = false;
        self.reset();
    }

    pub(crate) fn observe_idle_state(
        &mut self,
        output_ever_grew: bool,
        ready_for_input: bool,
        post_work_observed: bool,
        now: std::time::Instant,
    ) -> ReadyForInputIdleState {
        let output_ready = output_ever_grew || self.recovery_primed;
        if !output_ready || !ready_for_input {
            self.reset();
            return ReadyForInputIdleState::None;
        }

        if self.first_ready_at.is_none() {
            self.first_ready_at = Some(now);
        }
        self.consecutive_ready_probes += 1;

        let timed_out = now.duration_since(
            self.first_ready_at
                .expect("first_ready_at set above before elapsed check"),
        ) >= READY_FOR_INPUT_IDLE_TIMEOUT
            && self.consecutive_ready_probes >= READY_FOR_INPUT_IDLE_MIN_PROBES;

        if !timed_out {
            return ReadyForInputIdleState::None;
        }

        if self.recovery_primed || post_work_observed {
            ReadyForInputIdleState::PostWorkIdleTimeout
        } else {
            ReadyForInputIdleState::FreshIdle
        }
    }

    fn reset(&mut self) {
        self.first_ready_at = None;
        self.consecutive_ready_probes = 0;
    }
}

pub fn fold_read_output_result<T>(
    read_result: ReadOutputResult,
    on_ready: impl FnOnce(u64) -> T,
    on_session_died: impl FnOnce(u64) -> T,
) -> T {
    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            on_ready(offset)
        }
        ReadOutputResult::SessionDied { offset } => on_session_died(offset),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn followup_result_from_read_output_result(
    read_result: ReadOutputResult,
    session_died_error: impl Into<String>,
) -> FollowupResult {
    let session_died_error = session_died_error.into();
    fold_read_output_result(
        read_result,
        |_| FollowupResult::Delivered,
        |_| FollowupResult::RecreateSession {
            error: session_died_error,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn poll_output_file_until_result<
    State,
    IsAlive,
    IsReady,
    EmitOffset,
    ProcessLine,
    HasFinal,
    EmitSyntheticDone,
    EmitDeferredError,
>(
    output_path: &str,
    start_offset: u64,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    state: &mut State,
    mut is_alive: IsAlive,
    mut is_ready_for_input: IsReady,
    mut emit_output_offset: EmitOffset,
    mut process_line: ProcessLine,
    has_final: HasFinal,
    mut emit_synthetic_done: EmitSyntheticDone,
    mut emit_deferred_error: EmitDeferredError,
) -> Result<ReadOutputResult, String>
where
    IsAlive: FnMut() -> bool,
    IsReady: FnMut() -> bool,
    EmitOffset: FnMut(u64),
    ProcessLine: FnMut(&str, &mut State) -> bool,
    HasFinal: Fn(&State) -> bool,
    EmitSyntheticDone: FnMut(&State) -> bool,
    EmitDeferredError: FnMut(&State),
{
    use std::io::{Read, Seek, SeekFrom};
    use std::time::{Duration, Instant};

    let wait_start = Instant::now();
    let mut wait_interval = Duration::from_millis(10);
    let max_wait_interval = Duration::from_millis(500);
    loop {
        if std::fs::metadata(output_path).is_ok() {
            break;
        }
        if !is_alive() {
            return Ok(ReadOutputResult::SessionDied {
                offset: start_offset,
            });
        }
        if wait_start.elapsed() > Duration::from_secs(30) {
            return Err("Timeout waiting for output file".to_string());
        }
        if cancel_requested(cancel_token.as_deref()) {
            return Ok(ReadOutputResult::Cancelled {
                offset: start_offset,
            });
        }
        std::thread::sleep(wait_interval);
        wait_interval = std::cmp::min(
            Duration::from_millis((wait_interval.as_millis() as f64 * 1.5) as u64),
            max_wait_interval,
        );
    }

    if start_offset > 0 {
        emit_output_offset(start_offset);
    }

    let mut file = std::fs::File::open(output_path)
        .map_err(|e| format!("Failed to open output file: {}", e))?;
    file.seek(SeekFrom::Start(start_offset))
        .map_err(|e| format!("Failed to seek output file: {}", e))?;

    let mut current_offset = start_offset;
    let mut partial_line = String::new();
    let mut buf = [0u8; 8192];
    let mut no_data_count: u32 = 0;
    let mut ready_for_input_tracker = ReadyForInputIdleTracker::default();

    loop {
        if cancel_requested(cancel_token.as_deref()) {
            return Ok(ReadOutputResult::Cancelled {
                offset: current_offset,
            });
        }

        match file.read(&mut buf) {
            Ok(0) => {
                no_data_count += 1;
                if no_data_count % 25 == 0 {
                    if !is_alive() {
                        let file_len = std::fs::metadata(output_path)
                            .map(|meta| meta.len())
                            .unwrap_or(current_offset);
                        if file_len > current_offset {
                            continue;
                        }
                        break;
                    }

                    let file_len = std::fs::metadata(output_path)
                        .map(|meta| meta.len())
                        .unwrap_or(current_offset);
                    let has_new_bytes = file_len > current_offset;
                    let output_ever_grew = current_offset > start_offset;
                    if !has_new_bytes
                        && ready_for_input_tracker.observe_idle_state(
                            output_ever_grew,
                            is_ready_for_input(),
                            true,
                            Instant::now(),
                        ) == ReadyForInputIdleState::PostWorkIdleTimeout
                    {
                        if !emit_synthetic_done(state) {
                            return Ok(ReadOutputResult::Cancelled {
                                offset: current_offset,
                            });
                        }
                        return Ok(ReadOutputResult::Completed {
                            offset: current_offset,
                        });
                    } else if has_new_bytes {
                        ready_for_input_tracker.record_output();
                    }
                }

                let read_interval = if no_data_count < 5 {
                    Duration::from_millis(10)
                } else if no_data_count < 20 {
                    Duration::from_millis(50)
                } else {
                    Duration::from_millis(200)
                };
                std::thread::sleep(read_interval);
            }
            Ok(n) => {
                no_data_count = 0;
                ready_for_input_tracker.record_output();
                current_offset += n as u64;
                emit_output_offset(current_offset);
                partial_line.push_str(&String::from_utf8_lossy(&buf[..n]));

                while let Some(pos) = partial_line.find('\n') {
                    let line: String = partial_line.drain(..=pos).collect();
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    if !process_line(trimmed, state) {
                        return Ok(ReadOutputResult::Cancelled {
                            offset: current_offset,
                        });
                    }

                    if has_final(state) {
                        return Ok(ReadOutputResult::Completed {
                            offset: current_offset,
                        });
                    }
                }
            }
            Err(_) => break,
        }
    }

    emit_deferred_error(state);
    Ok(ReadOutputResult::SessionDied {
        offset: current_offset,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAttemptFailure {
    pub message: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

impl StreamAttemptFailure {
    pub fn with_message(mut self, message: String) -> Self {
        self.message = message;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamAttemptResult {
    Completed,
    RetrySession(StreamAttemptFailure),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamFinalState {
    Done {
        result: String,
        session_id: Option<String>,
    },
    Error(StreamAttemptFailure),
    RetrySession(StreamAttemptFailure),
}

pub fn run_retrying_stream_attempts<F, G>(
    provider_name: &str,
    mut resume_selector: Option<String>,
    max_session_retries: usize,
    mut execute_attempt: F,
    mut on_retry_exhausted: G,
) -> Result<(), String>
where
    F: FnMut(Option<String>) -> Result<StreamAttemptResult, String>,
    G: FnMut(StreamAttemptFailure),
{
    for attempt in 0..=max_session_retries {
        match execute_attempt(resume_selector.clone())? {
            StreamAttemptResult::Completed | StreamAttemptResult::Cancelled => return Ok(()),
            StreamAttemptResult::RetrySession(failure) => {
                if attempt < max_session_retries {
                    resume_selector = None;
                    continue;
                }
                let exhausted_message = format!(
                    "{} session could not be recovered after retry: {}",
                    provider_name, failure.message
                );
                on_retry_exhausted(failure.with_message(exhausted_message));
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Read Codex model context_window from the local CLI cache file.
/// Returns None if the cache is missing, unreadable, or the model isn't found.
fn codex_model_context_window(model: &str) -> Option<u64> {
    let cache_path = dirs::home_dir()?.join(".codex/models_cache.json");
    let data = std::fs::read_to_string(cache_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&data).ok()?;
    let models = json.get("models")?.as_array()?;
    models
        .iter()
        .find(|m| m.get("slug").and_then(|s| s.as_str()) == Some(model))
        .and_then(|m| m.get("context_window"))
        .and_then(|v| v.as_u64())
}

#[cfg(test)]
mod cancel_token_tests {
    use super::{
        CancelSource, CancelToken, cancel_requested, cancel_watchdog_should_kill,
        current_unix_millis, enforce_watchdog_deadline, register_child_pid,
    };
    use std::sync::atomic::Ordering;

    #[test]
    fn cancel_token_helpers_register_source_and_state() {
        let token = CancelToken::new();
        assert!(!cancel_requested(Some(&token)));
        assert!(!cancel_requested(None));
        assert_eq!(token.cancel_source(), None);

        register_child_pid(Some(&token), 4242);
        assert_eq!(
            *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()),
            Some(4242)
        );

        token.set_cancel_source("watchdog_timeout");
        assert_eq!(token.cancel_source().as_deref(), Some("watchdog_timeout"));

        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(cancel_requested(Some(&token)));
    }

    /// Issue #2335 (a): free-form labels used today by the voice paths must
    /// classify into the new [`CancelSource`] enum so downstream branches
    /// can distinguish timeout-vs-user-cancel without string parsing.
    #[test]
    fn cancel_source_classifies_known_voice_labels() {
        assert_eq!(
            CancelSource::classify("voice_barge_in_explicit_stop"),
            CancelSource::UserBargeIn
        );
        assert_eq!(
            CancelSource::classify("voice_barge_in_live_cut"),
            CancelSource::UserBargeIn
        );
        assert_eq!(
            CancelSource::classify("voice_background_summary_timeout"),
            CancelSource::SummaryTimeout
        );
        assert_eq!(
            CancelSource::classify("voice_channel_text_reply_timeout"),
            CancelSource::SummaryTimeout
        );
        assert_eq!(
            CancelSource::classify("voice_foreground_ack_timeout"),
            CancelSource::WatchdogTimeout
        );
        assert_eq!(
            CancelSource::classify("voice_guild_teardown"),
            CancelSource::SessionTeardown
        );
        assert_eq!(
            CancelSource::classify("watchdog_timeout"),
            CancelSource::WatchdogTimeout
        );
        assert_eq!(
            CancelSource::classify("unknown_label_xyz"),
            CancelSource::Other
        );
    }

    /// `set_cancel_source` must keep the structured kind in sync so that a
    /// caller writing only the legacy free-form label still produces a
    /// branchable enum value (#2335 a).
    #[test]
    fn set_cancel_source_populates_kind() {
        let token = CancelToken::new();
        assert_eq!(token.cancel_source_kind(), None);
        token.set_cancel_source("voice_barge_in_explicit_stop");
        assert_eq!(
            token.cancel_source_kind(),
            Some(CancelSource::UserBargeIn),
            "set_cancel_source should auto-classify into the enum"
        );

        // Explicit kind setter overrides classification when called first.
        let token = CancelToken::new();
        token.set_cancel_source_kind(CancelSource::SessionTeardown);
        token.set_cancel_source("voice_barge_in_live_cut");
        assert_eq!(
            token.cancel_source_kind(),
            Some(CancelSource::SessionTeardown),
            "explicit set_cancel_source_kind must win over later free-form auto-classification"
        );
        assert_eq!(
            token.cancel_source().as_deref(),
            Some("voice_barge_in_live_cut")
        );
    }

    #[test]
    fn watchdog_deadline_enforcement_marks_cancelled_timeout() {
        let token = CancelToken::new();
        let now = current_unix_millis();
        token
            .watchdog_deadline_ms
            .store(now + 1_000, Ordering::Relaxed);

        assert!(!enforce_watchdog_deadline(&token, now + 999));
        assert!(!token.cancelled.load(Ordering::Relaxed));

        assert!(enforce_watchdog_deadline(&token, now + 1_000));
        assert!(cancel_requested(Some(&token)));
        assert_eq!(
            token.cancel_source_kind(),
            Some(CancelSource::WatchdogTimeout)
        );
        assert_eq!(token.cancel_source().as_deref(), Some("watchdog_timeout"));
    }

    /// claude-e rollout Phase 1 (counter-review round 3 with Codex): when
    /// a Discord turn watchdog marks the token as async-managed, the
    /// per-provider sync poll inside `spawn_cancel_watchdog` must NOT fire
    /// on an expired deadline — the async 30s reconcile loop owns it.
    /// Non-Discord callers (legacy default) keep the original behaviour.
    #[test]
    fn watchdog_deadline_enforcement_skips_async_managed_token() {
        let token = CancelToken::new();
        let now = current_unix_millis();
        token.mark_async_managed();
        token
            .watchdog_deadline_ms
            .store(now + 1_000, Ordering::Relaxed);

        // Deadline has expired, but `async_managed` suppresses the sync
        // fire: enforce returns false, cancelled stays false, source kind
        // stays None, and the public `cancel_requested` reports no cancel.
        assert!(!enforce_watchdog_deadline(&token, now + 5_000));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(token.cancel_source_kind(), None);
        assert!(!cancel_requested(Some(&token)));

        // Explicit cancel still works — the gate is deadline-only.
        token.cancelled.store(true, Ordering::Relaxed);
        assert!(cancel_requested(Some(&token)));
    }

    #[test]
    fn cancel_watchdog_ignores_normal_completion_cleanup_cancel() {
        let token = CancelToken::new();
        token.mark_completion_cleanup();
        token.cancelled.store(true, Ordering::Relaxed);

        assert!(!cancel_watchdog_should_kill(&token));
        assert!(
            cancel_requested(Some(&token)),
            "cleanup marker only suppresses provider watchdog killing"
        );
    }

    #[test]
    fn cancel_watchdog_still_kills_explicit_cancel() {
        let token = CancelToken::new();
        token.cancelled.store(true, Ordering::Relaxed);

        assert!(cancel_watchdog_should_kill(&token));
    }

    #[test]
    fn watchdog_deadline_enforcement_default_token_still_fires() {
        // Companion to the async-managed test above: ensure the default
        // (non-Discord) token path is unchanged. This guards against an
        // accidental flip of the default in `CancelToken::new`.
        let token = CancelToken::new();
        let now = current_unix_millis();
        assert!(!token.is_async_managed());
        token
            .watchdog_deadline_ms
            .store(now + 1_000, Ordering::Relaxed);

        assert!(enforce_watchdog_deadline(&token, now + 5_000));
        assert!(cancel_requested(Some(&token)));
        assert_eq!(
            token.cancel_source_kind(),
            Some(CancelSource::WatchdogTimeout)
        );
    }
}

#[cfg(test)]
mod poll_output_file_tests {
    use super::{ReadOutputResult, poll_output_file_until_result};

    #[test]
    fn missing_output_file_reports_session_died_when_process_already_exited() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing.jsonl");
        let mut state = ();

        let result = poll_output_file_until_result(
            missing_path.to_str().unwrap(),
            12,
            None,
            &mut state,
            || false,
            || false,
            |_| {},
            |_, _| true,
            |_| false,
            |_| true,
            |_| {},
        )
        .unwrap();

        assert_eq!(result, ReadOutputResult::SessionDied { offset: 12 });
    }

    #[test]
    fn existing_output_file_emits_start_offset_before_new_bytes() {
        #[derive(Default)]
        struct TestState {
            saw_done: bool,
            lines: Vec<String>,
        }

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stream.jsonl");
        let previous = "old\n";
        std::fs::write(&output_path, format!("{previous}DONE\n")).unwrap();
        let start_offset = previous.len() as u64;

        let mut state = TestState::default();
        let mut offsets = Vec::new();
        let result = poll_output_file_until_result(
            output_path.to_str().unwrap(),
            start_offset,
            None,
            &mut state,
            || true,
            || false,
            |offset| offsets.push(offset),
            |line: &str, state| {
                state.lines.push(line.to_string());
                state.saw_done = line == "DONE";
                true
            },
            |state| state.saw_done,
            |_| true,
            |_| {},
        )
        .unwrap();

        let file_len = std::fs::metadata(&output_path).unwrap().len();
        assert_eq!(result, ReadOutputResult::Completed { offset: file_len });
        assert_eq!(state.lines, vec!["DONE".to_string()]);
        assert_eq!(offsets, vec![start_offset, file_len]);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        CancelToken, FollowupResult, ProviderKind, ReadOutputResult, StreamAttemptFailure,
        StreamAttemptResult, StreamFinalState, cancel_requested, compose_structured_turn_prompt,
        fold_read_output_result, followup_result_from_read_output_result, is_readonly_tool_policy,
        parse_provider_and_channel_from_tmux_name, poll_output_file_until_result,
        provider_registry, register_child_pid, run_retrying_stream_attempts,
        supported_provider_ids,
    };
    use crate::dispatch::extract_thread_channel_id;

    #[test]
    fn test_compact_cli_config_uses_context_window() {
        let codex = ProviderKind::Codex;
        // 60% of 272000 = 163200
        let config = codex.compact_cli_config(60, 272_000);
        assert_eq!(config.len(), 1);
        assert_eq!(config[0].0, "model_auto_compact_token_limit");
        assert_eq!(config[0].1, "163200");

        // 60% of 128000 = 76800
        let config = codex.compact_cli_config(60, 128_000);
        assert_eq!(config[0].1, "76800");

        // Claude returns no CLI config
        let config = ProviderKind::Claude.compact_cli_config(60, 1_000_000);
        assert!(config.is_empty());
    }

    #[test]
    fn test_resolve_context_window_fallback() {
        // Without a matching model, falls back to provider default
        assert_eq!(
            ProviderKind::Codex.resolve_context_window(Some("nonexistent-model")),
            200_000
        );
        assert_eq!(ProviderKind::Codex.resolve_context_window(None), 200_000);
        assert_eq!(
            ProviderKind::Claude.resolve_context_window(Some("opus")),
            1_000_000
        );
    }

    #[test]
    fn test_provider_channel_support() {
        assert!(ProviderKind::Claude.is_channel_supported(Some("mac-mini"), false, None));
        assert!(ProviderKind::Claude.is_channel_supported(
            Some("cookingheart-dev-cc"),
            false,
            None
        ));
        assert!(!ProviderKind::Claude.is_channel_supported(
            Some("cookingheart-dev-cdx"),
            false,
            None
        ));
        assert!(ProviderKind::Codex.is_channel_supported(
            Some("cookingheart-dev-cdx"),
            false,
            None
        ));
        assert!(!ProviderKind::Codex.is_channel_supported(
            Some("cookingheart-dev-cc"),
            false,
            None
        ));
        assert!(ProviderKind::Codex.is_channel_supported(None, true, None));
        assert!(ProviderKind::Gemini.is_channel_supported(Some("research-gm"), false, None));
        assert!(!ProviderKind::Gemini.is_channel_supported(Some("research-cc"), false, None));
        assert!(ProviderKind::Gemini.is_channel_supported(None, true, None));
        assert!(ProviderKind::Qwen.is_channel_supported(Some("sandbox-qw"), false, None));
        assert!(!ProviderKind::Qwen.is_channel_supported(Some("sandbox-cc"), false, None));
        assert!(ProviderKind::Qwen.is_channel_supported(None, true, None));
    }

    #[test]
    fn test_unsupported_provider() {
        let p = ProviderKind::from_str_or_unsupported("gpt");
        assert!(!p.is_supported());
        assert_eq!(p.as_str(), "gpt");
        assert_eq!(p.display_name(), "gpt");
        assert!(!p.is_channel_supported(Some("test-cc"), false, None));
        assert!(!p.is_channel_supported(Some("test"), false, None));
        assert!(!p.is_channel_supported(None, true, None));
    }

    #[test]
    fn test_from_str_or_unsupported_known() {
        assert_eq!(
            ProviderKind::from_str_or_unsupported("claude"),
            ProviderKind::Claude
        );
        assert_eq!(
            ProviderKind::from_str_or_unsupported("Codex"),
            ProviderKind::Codex
        );
        assert_eq!(
            ProviderKind::from_str_or_unsupported("Gemini"),
            ProviderKind::Gemini
        );
        assert_eq!(
            ProviderKind::from_str_or_unsupported("OpenCode"),
            ProviderKind::OpenCode
        );
        assert_eq!(
            ProviderKind::from_str_or_unsupported("Qwen"),
            ProviderKind::Qwen
        );
    }

    #[test]
    fn test_tmux_name_parse_supports_provider_aware_names() {
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("AgentDesk-claude-cookingheart-dev-cc"),
            Some((ProviderKind::Claude, "cookingheart-dev-cc".to_string()))
        );
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("AgentDesk-codex-cookingheart-dev-cdx"),
            Some((ProviderKind::Codex, "cookingheart-dev-cdx".to_string()))
        );
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("AgentDesk-gemini-research-gm"),
            Some((ProviderKind::Gemini, "research-gm".to_string()))
        );
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("AgentDesk-qwen-sandbox-qw"),
            Some((ProviderKind::Qwen, "sandbox-qw".to_string()))
        );
        assert_eq!(
            parse_provider_and_channel_from_tmux_name("AgentDesk-mac-mini"),
            Some((ProviderKind::Claude, "mac-mini".to_string()))
        );
    }

    #[test]
    fn test_provider_from_str_claude() {
        assert_eq!(ProviderKind::from_str("claude"), Some(ProviderKind::Claude));
    }

    #[test]
    fn test_provider_from_str_codex() {
        assert_eq!(ProviderKind::from_str("codex"), Some(ProviderKind::Codex));
    }

    #[test]
    fn test_provider_from_str_gemini() {
        assert_eq!(ProviderKind::from_str("gemini"), Some(ProviderKind::Gemini));
    }

    #[test]
    fn test_provider_from_str_qwen() {
        assert_eq!(ProviderKind::from_str("qwen"), Some(ProviderKind::Qwen));
    }

    #[test]
    fn test_provider_from_str_opencode() {
        assert_eq!(
            ProviderKind::from_str("opencode"),
            Some(ProviderKind::OpenCode)
        );
    }

    #[test]
    fn test_provider_from_str_case_insensitive() {
        assert_eq!(ProviderKind::from_str("Claude"), Some(ProviderKind::Claude));
        assert_eq!(ProviderKind::from_str("CLAUDE"), Some(ProviderKind::Claude));
        assert_eq!(ProviderKind::from_str("CODEX"), Some(ProviderKind::Codex));
        assert_eq!(ProviderKind::from_str("Codex"), Some(ProviderKind::Codex));
        assert_eq!(ProviderKind::from_str("Gemini"), Some(ProviderKind::Gemini));
        assert_eq!(
            ProviderKind::from_str("OpenCode"),
            Some(ProviderKind::OpenCode)
        );
        assert_eq!(ProviderKind::from_str("Qwen"), Some(ProviderKind::Qwen));
    }

    #[test]
    fn test_provider_from_str_unknown() {
        assert_eq!(ProviderKind::from_str("gpt"), None);
        assert_eq!(ProviderKind::from_str(""), None);
    }

    #[test]
    fn test_build_tmux_session_name() {
        let name = ProviderKind::Claude.build_tmux_session_name("my-channel");
        assert!(name.starts_with("AgentDesk-claude-"));
        assert!(name.contains("my-channel"));

        let name2 = ProviderKind::Codex.build_tmux_session_name("dev-cdx");
        assert!(name2.starts_with("AgentDesk-codex-"));
        assert!(name2.contains("dev-cdx"));

        let name3 = ProviderKind::Gemini.build_tmux_session_name("research-gm");
        assert!(name3.starts_with("AgentDesk-gemini-"));
        assert!(name3.contains("research-gm"));

        let name4 = ProviderKind::OpenCode.build_tmux_session_name("sandbox-oc");
        assert!(name4.starts_with("AgentDesk-opencode-"));
        assert!(name4.contains("sandbox-oc"));

        let name5 = ProviderKind::Qwen.build_tmux_session_name("sandbox-qw");
        assert!(name5.starts_with("AgentDesk-qwen-"));
        assert!(name5.contains("sandbox-qw"));
    }

    #[test]
    fn test_parse_provider_and_channel_from_tmux_name() {
        let channel = "my-test-channel";
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        let (provider, parsed_channel) =
            parse_provider_and_channel_from_tmux_name(&session).unwrap();
        assert_eq!(provider, ProviderKind::Claude);
        assert_eq!(parsed_channel, channel);

        let session2 = ProviderKind::Codex.build_tmux_session_name(channel);
        let (provider2, parsed_channel2) =
            parse_provider_and_channel_from_tmux_name(&session2).unwrap();
        assert_eq!(provider2, ProviderKind::Codex);
        assert_eq!(parsed_channel2, channel);

        let session3 = ProviderKind::Gemini.build_tmux_session_name("research-gm");
        let (provider3, parsed_channel3) =
            parse_provider_and_channel_from_tmux_name(&session3).unwrap();
        assert_eq!(provider3, ProviderKind::Gemini);
        assert_eq!(parsed_channel3, "research-gm");

        let session4 = ProviderKind::OpenCode.build_tmux_session_name("sandbox-oc");
        let (provider4, parsed_channel4) =
            parse_provider_and_channel_from_tmux_name(&session4).unwrap();
        assert_eq!(provider4, ProviderKind::OpenCode);
        assert_eq!(parsed_channel4, "sandbox-oc");

        let session5 = ProviderKind::Qwen.build_tmux_session_name("sandbox-qw");
        let (provider5, parsed_channel5) =
            parse_provider_and_channel_from_tmux_name(&session5).unwrap();
        assert_eq!(provider5, ProviderKind::Qwen);
        assert_eq!(parsed_channel5, "sandbox-qw");
    }

    #[test]
    fn test_is_channel_supported_cc_suffix() {
        assert!(ProviderKind::Claude.is_channel_supported(Some("dev-cc"), false, None));
        assert!(!ProviderKind::Codex.is_channel_supported(Some("dev-cc"), false, None));
    }

    #[test]
    fn test_is_channel_supported_cdx_suffix() {
        assert!(ProviderKind::Codex.is_channel_supported(Some("dev-cdx"), false, None));
        assert!(!ProviderKind::Claude.is_channel_supported(Some("dev-cdx"), false, None));
    }

    #[test]
    fn test_registry_exposes_all_supported_cli_init_providers() {
        let labels: Vec<&str> = provider_registry()
            .iter()
            .map(|entry| entry.cli_init_label)
            .collect();
        assert_eq!(
            labels,
            vec![
                "claude (Anthropic)",
                "codex (OpenAI)",
                "gemini (Google)",
                "opencode (OpenCode)",
                "qwen (Alibaba)"
            ]
        );
        assert_eq!(
            ProviderKind::provider_for_cli_init_index(3),
            Some(ProviderKind::OpenCode)
        );
        assert_eq!(
            ProviderKind::provider_for_cli_init_index(4),
            Some(ProviderKind::Qwen)
        );
    }

    #[test]
    fn test_supported_provider_ids_follow_registry_order() {
        assert_eq!(
            supported_provider_ids(),
            vec!["claude", "codex", "gemini", "opencode", "qwen"]
        );
    }

    #[test]
    fn test_resolve_channel_provider_prefers_explicit_metadata_before_suffix() {
        assert_eq!(
            ProviderKind::resolve_channel_provider(Some("mixed-cdx"), Some(&ProviderKind::Gemini)),
            Some(ProviderKind::Gemini)
        );
    }

    #[test]
    fn test_from_channel_suffix_supports_qwen() {
        assert_eq!(
            ProviderKind::from_channel_suffix("sandbox-qw"),
            Some(ProviderKind::Qwen)
        );
        assert_eq!(ProviderKind::from_channel_suffix("plain"), None);
    }

    // ── #157 suffix preservation tests ─────────────────────────────────
    // All tests use `crate::dispatch::extract_thread_channel_id` — the same
    // pure parsing function that production `is_unified_thread_channel_name_active` calls.

    #[test]
    fn test_suffix_preserved_long_ascii_parent() {
        // Parent 30 chars + "-t" + 19-digit thread ID = 51 chars (exceeds 44)
        let parent = "very-long-channel-name-for-test"; // 30 chars
        let thread_id = "1487044675541012490"; // 19 digits
        let channel = format!("{}-t{}", parent, thread_id);
        assert!(channel.len() > 44);

        let session = ProviderKind::Claude.build_tmux_session_name(&channel);
        let (provider, parsed) = parse_provider_and_channel_from_tmux_name(&session).unwrap();
        assert_eq!(provider, ProviderKind::Claude);

        // Suffix must be extractable
        let extracted = extract_thread_channel_id(&parsed);
        assert_eq!(
            extracted,
            Some(1487044675541012490u64),
            "thread ID must survive truncation, got channel_name='{}'",
            parsed
        );
    }

    #[test]
    fn test_suffix_preserved_very_long_parent() {
        // Parent 40 chars → total with suffix well over 44
        let parent = "a]b]c]d]e]f]g]h]i]j]k]l]m]n]o]p]q]r]s]t"; // sanitized to 40+ chars
        let thread_id = "1234567890123456789";
        let channel = format!("{}-t{}", parent, thread_id);

        let session = ProviderKind::Claude.build_tmux_session_name(&channel);
        let (_, parsed) = parse_provider_and_channel_from_tmux_name(&session).unwrap();

        let extracted = extract_thread_channel_id(&parsed);
        assert_eq!(
            extracted,
            Some(1234567890123456789u64),
            "thread ID must survive even extreme parent length, got channel_name='{}'",
            parsed
        );
    }

    #[test]
    fn test_suffix_preserved_cjk_parent() {
        // CJK characters: each 3 bytes in UTF-8, but still alphanumeric
        let parent = "한글채널테스트용이름"; // 9 CJK chars = 27 bytes
        let thread_id = "1487044675541012490";
        let channel = format!("{}-t{}", parent, thread_id);
        // 27 + 2 + 19 = 48 bytes, exceeds 44

        let session = ProviderKind::Claude.build_tmux_session_name(&channel);
        let (_, parsed) = parse_provider_and_channel_from_tmux_name(&session).unwrap();

        let extracted = extract_thread_channel_id(&parsed);
        assert_eq!(
            extracted,
            Some(1487044675541012490u64),
            "thread ID must survive CJK parent truncation, got channel_name='{}'",
            parsed
        );
        // Verify truncation happened at a CJK char boundary (not mid-byte).
        // The suffix starts at "-t"; everything before it is the truncated prefix.
        // Each CJK char is 3 bytes, so prefix byte length must be divisible by 3
        // (all chars in the prefix are CJK after sanitization).
        let suffix_pos = parsed.rfind("-t").unwrap();
        let prefix = &parsed[..suffix_pos];
        assert!(
            prefix.len() % 3 == 0 && prefix.chars().all(|c| c.len_utf8() == 3),
            "CJK prefix must be cut at char boundary, got prefix='{}' ({}B)",
            prefix,
            prefix.len()
        );
    }

    #[test]
    fn test_suffix_preserved_prefix_budget_near_zero() {
        // Construct a case where prefix_budget is very small (but >0 with real IDs)
        // 44 - 21 (suffix len) = 23 prefix budget
        // Use a parent that's exactly long enough to trigger truncation
        let parent = "abcdefghijklmnopqrstuvwxyz0123"; // 30 chars
        let thread_id = "1487044675541012490"; // 19 digits → suffix = 21 chars
        let channel = format!("{}-t{}", parent, thread_id);
        // 30 + 21 = 51 > 44

        let session = ProviderKind::Claude.build_tmux_session_name(&channel);
        let (_, parsed) = parse_provider_and_channel_from_tmux_name(&session).unwrap();

        let extracted = extract_thread_channel_id(&parsed);
        assert_eq!(extracted, Some(1487044675541012490u64));
        // Trimmed total should be <= 44
        assert!(
            parsed.len() <= 44,
            "trimmed channel must be <= 44 bytes, got {}",
            parsed.len()
        );
    }

    #[test]
    fn test_no_thread_suffix_still_truncates_normally() {
        // Non-thread channel names should still be truncated to 44 chars
        let long_channel =
            "a]very]long]channel]name]that]exceeds]the]maximum]allowed]length]for]tmux";
        let session = ProviderKind::Claude.build_tmux_session_name(long_channel);
        let (_, parsed) = parse_provider_and_channel_from_tmux_name(&session).unwrap();
        assert!(
            parsed.len() <= 44,
            "non-thread channel must be <= 44 bytes, got {}",
            parsed.len()
        );
    }

    #[test]
    fn test_short_channel_with_thread_no_truncation() {
        // Short parent + thread suffix that fits within 44 → no truncation needed
        let channel = "adk-cc-t1487044675541012490"; // 27 chars, fits in 44
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        let (_, parsed) = parse_provider_and_channel_from_tmux_name(&session).unwrap();
        assert_eq!(parsed, channel);
        let extracted = extract_thread_channel_id(&parsed);
        assert_eq!(extracted, Some(1487044675541012490u64));
    }

    #[test]
    fn test_roundtrip_all_providers_long_thread() {
        let parent = "cookingheart-very-long-channel";
        let thread_id = "1487044675541012490";
        let channel = format!("{}-t{}", parent, thread_id);

        for provider in [ProviderKind::Claude, ProviderKind::Codex] {
            let session = provider.build_tmux_session_name(&channel);
            let (parsed_provider, parsed_channel) =
                parse_provider_and_channel_from_tmux_name(&session).unwrap();
            assert_eq!(parsed_provider, provider);
            let extracted = extract_thread_channel_id(&parsed_channel);
            assert_eq!(
                extracted,
                Some(1487044675541012490u64),
                "roundtrip failed for {:?}, got channel_name='{}'",
                provider,
                parsed_channel
            );
        }
    }

    #[test]
    fn test_suffix_preserved_prefix_budget_zero_no_panic() {
        // prefix_budget=0 is unreachable with valid Discord IDs (max 20 digits →
        // suffix max 22 chars → budget min 22). This test proves the code handles
        // the theoretical boundary safely (no panic, suffix marker preserved).
        let digits = "1".repeat(43); // 43 digits → suffix = 45 chars > 44
        let channel = format!("parent-t{}", digits);

        // Must not panic
        let session = ProviderKind::Claude.build_tmux_session_name(&channel);
        let (_, parsed) = parse_provider_and_channel_from_tmux_name(&session).unwrap();

        // u64 overflow means extract_thread_channel_id returns None — expected.
        // The invariant we prove: code survives gracefully, suffix marker preserved.
        assert!(
            parsed.contains("-t"),
            "suffix marker must survive at budget=0, got channel_name='{}'",
            parsed
        );
        // extract_thread_channel_id returns None due to u64 overflow
        assert_eq!(extract_thread_channel_id(&parsed), None);
    }

    #[test]
    fn test_suffix_preserved_min_realistic_budget() {
        // Minimum realistic prefix_budget: u64::MAX (20 digits) → suffix 22 chars
        // → prefix_budget = 44 - 22 = 22. Even with max-length ID + long parent,
        // the production parsing function must extract the correct thread ID.
        let parent = "abcdefghijklmnopqrstuvwxyz-very-long-name-x"; // 43 chars
        let thread_id = "18446744073709551615"; // u64::MAX, 20 digits
        let channel = format!("{}-t{}", parent, thread_id);
        assert!(channel.len() > 44); // 43 + 22 = 65

        let session = ProviderKind::Claude.build_tmux_session_name(&channel);
        let (_, parsed) = parse_provider_and_channel_from_tmux_name(&session).unwrap();

        // DoD 2: even at minimum realistic budget, production parser succeeds
        let extracted = extract_thread_channel_id(&parsed);
        assert_eq!(
            extracted,
            Some(u64::MAX),
            "max u64 thread ID must be parseable at min budget, got channel_name='{}'",
            parsed
        );
        assert!(parsed.len() <= 44);
    }

    #[test]
    fn test_counterpart_provider() {
        assert_eq!(ProviderKind::Claude.counterpart(), ProviderKind::Codex);
        assert_eq!(ProviderKind::Codex.counterpart(), ProviderKind::Claude);
        assert_eq!(ProviderKind::Gemini.counterpart(), ProviderKind::Codex);
        assert_eq!(ProviderKind::Qwen.counterpart(), ProviderKind::Codex);

        let unsupported = ProviderKind::Unsupported("gpt".to_string());
        assert_eq!(
            unsupported.counterpart(),
            ProviderKind::Unsupported("gpt".to_string())
        );
    }

    #[test]
    fn test_provider_capabilities_known_providers_support_agent_contract() {
        for provider in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Gemini,
            ProviderKind::Qwen,
        ] {
            let capabilities = provider.capabilities().expect("supported provider");
            assert!(capabilities.supports_structured_output);
            assert!(capabilities.supports_resume);
            assert!(capabilities.supports_tool_stream);
            assert!(!capabilities.binary_name.is_empty());
        }
        let capabilities = ProviderKind::OpenCode
            .capabilities()
            .expect("supported provider");
        assert!(capabilities.supports_structured_output);
        assert!(!capabilities.supports_resume);
        assert!(capabilities.supports_tool_stream);
        assert!(!capabilities.binary_name.is_empty());
    }

    #[test]
    fn test_uses_managed_tmux_backend_for_claude_codex_and_qwen() {
        assert!(ProviderKind::Claude.uses_managed_tmux_backend());
        assert!(ProviderKind::Codex.uses_managed_tmux_backend());
        assert!(ProviderKind::Qwen.uses_managed_tmux_backend());
        assert!(!ProviderKind::Gemini.uses_managed_tmux_backend());
        assert!(!ProviderKind::Unsupported("gpt".to_string()).uses_managed_tmux_backend());
    }

    #[test]
    fn test_cancel_token_helpers_register_and_report_state() {
        let token = CancelToken::new();
        assert!(!cancel_requested(Some(&token)));
        assert!(!cancel_requested(None));
        assert_eq!(token.cancel_source(), None);

        register_child_pid(Some(&token), 4242);
        assert_eq!(
            *token.child_pid.lock().unwrap_or_else(|e| e.into_inner()),
            Some(4242)
        );

        token.set_cancel_source("watchdog_timeout");
        assert_eq!(token.cancel_source().as_deref(), Some("watchdog_timeout"));

        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(cancel_requested(Some(&token)));
    }

    #[test]
    fn test_run_retrying_stream_attempts_resets_resume_selector_after_retry() {
        let mut selectors = Vec::new();

        let result = run_retrying_stream_attempts(
            "Gemini",
            Some("latest".to_string()),
            1,
            |selector| {
                selectors.push(selector.clone());
                if selectors.len() == 1 {
                    Ok(StreamAttemptResult::RetrySession(StreamAttemptFailure {
                        message: "dead session".to_string(),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                    }))
                } else {
                    Ok(StreamAttemptResult::Completed)
                }
            },
            |_| panic!("retry should have recovered"),
        );

        assert!(result.is_ok());
        assert_eq!(selectors, vec![Some("latest".to_string()), None]);
    }

    #[test]
    fn test_run_retrying_stream_attempts_reports_exhausted_failure() {
        let mut exhausted: Option<StreamAttemptFailure> = None;

        let result = run_retrying_stream_attempts(
            "Gemini",
            Some("latest".to_string()),
            1,
            |_| {
                Ok(StreamAttemptResult::RetrySession(StreamAttemptFailure {
                    message: "dead session".to_string(),
                    stdout: "partial".to_string(),
                    stderr: String::new(),
                    exit_code: None,
                }))
            },
            |failure| exhausted = Some(failure),
        );

        assert!(result.is_ok());
        assert_eq!(
            exhausted,
            Some(StreamAttemptFailure {
                message: "Gemini session could not be recovered after retry: dead session"
                    .to_string(),
                stdout: "partial".to_string(),
                stderr: String::new(),
                exit_code: None,
            })
        );
    }

    #[test]
    fn test_stream_final_state_done_preserves_result_and_session_id() {
        let final_state = StreamFinalState::Done {
            result: "hello".to_string(),
            session_id: Some("latest".to_string()),
        };

        assert_eq!(
            final_state,
            StreamFinalState::Done {
                result: "hello".to_string(),
                session_id: Some("latest".to_string()),
            }
        );
    }

    #[test]
    fn test_compose_structured_turn_prompt_includes_authoritative_sections() {
        let prompt = compose_structured_turn_prompt(
            "role과 mission만 답해줘.",
            Some("role: PMD\nmission: 백로그 관리"),
            Some(&["Bash".to_string(), "Read".to_string()]),
        );

        assert!(prompt.contains("[Authoritative Instructions]"));
        assert!(prompt.contains("role: PMD"));
        assert!(!prompt.contains("[Tool Policy]"));
        assert!(!prompt.contains("Bash, Read"));
        assert!(prompt.contains("[User Request]\nrole과 mission만 답해줘."));
    }

    #[test]
    fn test_compose_structured_turn_prompt_returns_plain_prompt_without_overrides() {
        let prompt = compose_structured_turn_prompt("just answer", None, None);
        assert_eq!(prompt, "just answer");
    }

    #[test]
    fn test_is_readonly_tool_policy_accepts_read_and_search_tools() {
        assert!(is_readonly_tool_policy(Some(&[
            "Read".to_string(),
            "Grep".to_string(),
            "Glob".to_string(),
        ])));
        assert!(is_readonly_tool_policy(Some(&[" read ".to_string()])));
    }

    #[test]
    fn test_is_readonly_tool_policy_rejects_empty_or_write_tools() {
        assert!(!is_readonly_tool_policy(None));
        assert!(!is_readonly_tool_policy(Some(&[])));
        assert!(!is_readonly_tool_policy(Some(&[
            "Read".to_string(),
            "Write".to_string(),
        ])));
        assert!(!is_readonly_tool_policy(Some(&[
            "Read".to_string(),
            "WebSearch".to_string(),
        ])));
    }

    #[test]
    fn test_fold_read_output_result_maps_completed_to_ready_offset() {
        let outcome = fold_read_output_result(
            ReadOutputResult::Completed { offset: 42 },
            |offset| format!("ready:{offset}"),
            |offset| format!("dead:{offset}"),
        );
        assert_eq!(outcome, "ready:42");
    }

    #[test]
    fn test_fold_read_output_result_maps_session_died_to_dead_branch() {
        let outcome = fold_read_output_result(
            ReadOutputResult::SessionDied { offset: 7 },
            |offset| format!("ready:{offset}"),
            |offset| format!("dead:{offset}"),
        );
        assert_eq!(outcome, "dead:7");
    }

    #[test]
    fn test_followup_result_from_read_output_result_maps_completed_to_delivered() {
        let outcome = followup_result_from_read_output_result(
            ReadOutputResult::Completed { offset: 99 },
            "session died during follow-up output reading",
        );
        assert_eq!(outcome, FollowupResult::Delivered);
    }

    #[test]
    fn test_followup_result_from_read_output_result_maps_session_died_to_recreate() {
        let outcome = followup_result_from_read_output_result(
            ReadOutputResult::SessionDied { offset: 99 },
            "session died during follow-up output reading",
        );
        assert_eq!(
            outcome,
            FollowupResult::RecreateSession {
                error: "session died during follow-up output reading".to_string(),
            }
        );
    }

    #[test]
    fn test_fold_read_output_result_maps_cancelled_to_ready_offset() {
        let outcome = fold_read_output_result(
            ReadOutputResult::Cancelled { offset: 15 },
            |offset| format!("ready:{offset}"),
            |offset| format!("dead:{offset}"),
        );
        assert_eq!(outcome, "ready:15");
    }

    #[test]
    fn test_followup_result_from_read_output_result_maps_cancelled_to_delivered() {
        let outcome = followup_result_from_read_output_result(
            ReadOutputResult::Cancelled { offset: 99 },
            "session died during follow-up output reading",
        );
        assert_eq!(outcome, FollowupResult::Delivered);
    }

    #[test]
    fn test_run_retrying_stream_attempts_returns_early_on_cancelled() {
        let mut exhausted: Option<StreamAttemptFailure> = None;
        let mut calls = 0usize;

        let result = run_retrying_stream_attempts(
            "Gemini",
            Some("latest".to_string()),
            1,
            |_| {
                calls += 1;
                Ok(StreamAttemptResult::Cancelled)
            },
            |failure| exhausted = Some(failure),
        );

        assert!(result.is_ok());
        assert_eq!(calls, 1);
        assert!(exhausted.is_none());
    }

    #[test]
    fn test_poll_output_file_until_result_completes_after_terminal_line() {
        #[derive(Default)]
        struct TestState {
            saw_done: bool,
            lines: Vec<String>,
        }

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stream.jsonl");
        std::fs::write(&output_path, "hello\nDONE\n").unwrap();

        let mut state = TestState::default();
        let mut offsets = Vec::new();
        let result = poll_output_file_until_result(
            output_path.to_str().unwrap(),
            0,
            None,
            &mut state,
            || true,
            || false,
            |offset| offsets.push(offset),
            |line: &str, state| {
                state.lines.push(line.to_string());
                if line == "DONE" {
                    state.saw_done = true;
                }
                true
            },
            |state| state.saw_done,
            |_| true,
            |_| {},
        )
        .unwrap();

        assert_eq!(
            result,
            ReadOutputResult::Completed {
                offset: std::fs::metadata(&output_path).unwrap().len(),
            }
        );
        assert_eq!(state.lines, vec!["hello".to_string(), "DONE".to_string()]);
        assert_eq!(
            offsets,
            vec![std::fs::metadata(&output_path).unwrap().len()],
        );
    }

    #[test]
    fn test_poll_output_file_until_result_honors_preexisting_cancel_before_file_exists() {
        let token = std::sync::Arc::new(CancelToken::new());
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing.jsonl");

        let mut state = ();
        let result = poll_output_file_until_result(
            missing_path.to_str().unwrap(),
            17,
            Some(token),
            &mut state,
            || true,
            || false,
            |_| {},
            |_, _| true,
            |_| false,
            |_| true,
            |_| {},
        )
        .unwrap();

        assert_eq!(result, ReadOutputResult::Cancelled { offset: 17 });
    }

    #[test]
    fn test_poll_output_file_until_result_reports_session_died_without_terminal_result() {
        #[derive(Default)]
        struct TestState {
            lines: Vec<String>,
        }

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stream.jsonl");
        std::fs::write(&output_path, "partial\n").unwrap();

        let mut state = TestState::default();
        let mut alive_checks = 0usize;
        let result = poll_output_file_until_result(
            output_path.to_str().unwrap(),
            0,
            None,
            &mut state,
            || {
                alive_checks += 1;
                alive_checks < 1
            },
            || false,
            |_| {},
            |line: &str, state| {
                state.lines.push(line.to_string());
                true
            },
            |_| false,
            |_| true,
            |_| {},
        )
        .unwrap();

        assert_eq!(
            result,
            ReadOutputResult::SessionDied {
                offset: std::fs::metadata(&output_path).unwrap().len(),
            }
        );
        assert_eq!(state.lines, vec!["partial".to_string()]);
    }

    #[test]
    fn test_ready_for_input_idle_tracker_requires_stable_prompt_after_output() {
        let mut tracker = super::ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        assert_eq!(
            tracker.observe_idle_state(false, true, false, start),
            super::ReadyForInputIdleState::None
        );

        tracker.record_output();
        assert_eq!(
            tracker.observe_idle_state(true, true, true, start),
            super::ReadyForInputIdleState::None
        );
        assert_eq!(
            tracker.observe_idle_state(
                true,
                true,
                true,
                start + std::time::Duration::from_secs(10)
            ),
            super::ReadyForInputIdleState::None
        );
        assert_eq!(
            tracker.observe_idle_state(
                true,
                true,
                true,
                start + std::time::Duration::from_secs(16)
            ),
            super::ReadyForInputIdleState::PostWorkIdleTimeout
        );
    }

    #[test]
    fn test_ready_for_input_idle_tracker_resets_when_output_resumes_or_prompt_disappears() {
        let mut tracker = super::ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        tracker.record_output();
        assert_eq!(
            tracker.observe_idle_state(true, true, true, start),
            super::ReadyForInputIdleState::None
        );
        assert_eq!(
            tracker.observe_idle_state(
                true,
                false,
                true,
                start + std::time::Duration::from_secs(8)
            ),
            super::ReadyForInputIdleState::None
        );
        assert_eq!(
            tracker.observe_idle_state(
                true,
                true,
                true,
                start + std::time::Duration::from_secs(16)
            ),
            super::ReadyForInputIdleState::None
        );

        tracker.record_output();
        assert_eq!(
            tracker.observe_idle_state(
                true,
                true,
                true,
                start + std::time::Duration::from_secs(17)
            ),
            super::ReadyForInputIdleState::None
        );
        assert_eq!(
            tracker.observe_idle_state(
                true,
                true,
                true,
                start + std::time::Duration::from_secs(25)
            ),
            super::ReadyForInputIdleState::None
        );
        assert_eq!(
            tracker.observe_idle_state(
                true,
                true,
                true,
                start + std::time::Duration::from_secs(33)
            ),
            super::ReadyForInputIdleState::PostWorkIdleTimeout
        );
    }

    #[test]
    fn test_ready_for_input_idle_tracker_distinguishes_fresh_idle_from_post_work_idle() {
        let mut tracker = super::ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        tracker.record_output();
        assert_eq!(
            tracker.observe_idle_state(true, true, false, start),
            super::ReadyForInputIdleState::None
        );
        assert_eq!(
            tracker.observe_idle_state(
                true,
                true,
                false,
                start + std::time::Duration::from_secs(10)
            ),
            super::ReadyForInputIdleState::None
        );
        assert_eq!(
            tracker.observe_idle_state(
                true,
                true,
                false,
                start + std::time::Duration::from_secs(16)
            ),
            super::ReadyForInputIdleState::FreshIdle
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_tmux_capture_indicates_ready_for_input_detects_recent_ready_banner() {
        let capture = "\
build logs\n\
Ready for input (type message + Enter)\n\
> ";
        assert!(super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Claude
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_tmux_capture_indicates_ready_for_input_detects_claude_tui_prompt() {
        let capture = "\
output recap\n\
─────────────────────────────────────────────────────────────────────────────\n\
❯\u{00a0}\n\
─────────────────────────────────────────────────────────────────────────────\n\
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%\n\
  📁 agentdesk (main*) │ Todos: -\n\
  ⏵⏵ bypass permissions on";
        assert!(super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Claude
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_tmux_capture_indicates_ready_for_input_rejects_non_ready_capture() {
        let capture = "\
build logs\n\
waiting for tool output\n\
still running";
        assert!(!super::tmux_capture_indicates_ready_for_input(
            capture,
            &ProviderKind::Claude
        ));
    }

    #[test]
    fn test_tmux_followup_fallback_attaches_watcher_for_unread_bytes() {
        let fallback = super::tmux_followup_fallback_after_read_error(
            100,
            140,
            Some(180),
            true,
            false,
            true,
            true,
        )
        .expect("fallback decision");

        assert_eq!(
            fallback,
            super::TmuxFollowupFallback {
                last_offset: 140,
                emit_synthetic_done: false,
            }
        );
    }

    #[test]
    fn test_tmux_followup_fallback_synthesizes_done_when_turn_is_idle() {
        let fallback = super::tmux_followup_fallback_after_read_error(
            100,
            180,
            Some(180),
            true,
            true,
            true,
            true,
        )
        .expect("fallback decision");

        assert_eq!(
            fallback,
            super::TmuxFollowupFallback {
                last_offset: 180,
                emit_synthetic_done: true,
            }
        );
    }

    #[test]
    fn test_tmux_followup_fallback_requires_live_session_and_paths() {
        assert!(
            super::tmux_followup_fallback_after_read_error(
                100,
                180,
                Some(180),
                false,
                true,
                true,
                true,
            )
            .is_none()
        );
        assert!(
            super::tmux_followup_fallback_after_read_error(
                100,
                180,
                Some(180),
                true,
                true,
                false,
                true,
            )
            .is_none()
        );
        assert!(
            super::tmux_followup_fallback_after_read_error(
                100,
                180,
                Some(180),
                true,
                true,
                true,
                false,
            )
            .is_none()
        );
    }
}
