use crate::services::platform::BinaryResolution;
use crate::services::provider::cancel_token_cleanup::target::CapturedProcess;
use crate::services::provider_auth::ProviderAuthSpec;
use crate::utils::format::safe_prefix;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};

pub(crate) mod cancel_token_claude_interrupt;
pub(crate) mod cancel_token_cleanup;
mod cancel_watchdog;
pub use cancel_watchdog::{CancelWatchdog, spawn_cancel_watchdog};
use cancel_watchdog::{current_unix_millis, enforce_watchdog_deadline};

/// Tmux session name prefix — always "AgentDesk".
pub const TMUX_SESSION_PREFIX: &str = "AgentDesk";

/// #3263: Codex context-window LAST-RESORT fallback (tokens).
///
/// Codex is the only provider with a dynamic local context-window source
/// (`~/.codex/models_cache.json`, keyed by model slug). Resolution order is:
///   1. exact slug match in the cache (`context_window`),
///   2. max `context_window` across cached models (cache present, slug drift),
///   3. this constant (cache absent / empty / unparseable).
/// It is set to a conservative current-generation Codex window (matching the
/// current gpt-5.x cache value) so the fallback is not stale; the cache and the
/// max-of-cache value above are authoritative whenever the cache exists.
const CODEX_FALLBACK_CONTEXT_WINDOW: u64 = 272_000;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderExecutionAdapter {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    Qwen,
}

impl ProviderExecutionAdapter {
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Qwen => "qwen",
        }
    }

    pub const fn supported_capabilities(self) -> ProviderCapabilities {
        match self {
            Self::Claude => ProviderCapabilities {
                binary_name: "claude",
                supports_structured_output: true,
                supports_resume: true,
                supports_tool_stream: true,
            },
            Self::Codex => ProviderCapabilities {
                binary_name: "codex",
                supports_structured_output: true,
                supports_resume: true,
                supports_tool_stream: true,
            },
            Self::Gemini => ProviderCapabilities {
                binary_name: "gemini",
                supports_structured_output: true,
                supports_resume: true,
                supports_tool_stream: true,
            },
            Self::OpenCode => ProviderCapabilities {
                binary_name: "opencode",
                supports_structured_output: true,
                supports_resume: false,
                supports_tool_stream: true,
            },
            Self::Qwen => ProviderCapabilities {
                binary_name: "qwen",
                supports_structured_output: true,
                supports_resume: true,
                supports_tool_stream: true,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderCompactionAdapter {
    ClaudeEnvironment,
    CodexCli,
    GeminiDisabled,
    OpenCodeDisabled,
    QwenDisabled,
}

impl ProviderCompactionAdapter {
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::ClaudeEnvironment => "claude",
            Self::CodexCli => "codex",
            Self::GeminiDisabled => "gemini",
            Self::OpenCodeDisabled => "opencode",
            Self::QwenDisabled => "qwen",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderReadinessAdapter {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    Qwen,
}

impl ProviderReadinessAdapter {
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Qwen => "qwen",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRuntimeProbe {
    pub provider: ProviderKind,
    pub capabilities: ProviderCapabilities,
    pub resolution: BinaryResolution,
    pub version: Option<String>,
    pub probe_failure_kind: Option<String>,
    pub skipped_candidate_failures: Vec<String>,
    pub credential_present: bool,
    pub credential_source: Option<String>,
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
    pub execution_adapter: ProviderExecutionAdapter,
    pub compaction_adapter: ProviderCompactionAdapter,
    pub readiness_adapter: ProviderReadinessAdapter,
    pub default_behavior: ProviderDefaultBehavior,
    pub default_context_window: u64,
    pub managed_tmux_backend: bool,
    pub managed_tmux_wrapper_subcommand: Option<&'static str>,
    pub auth: ProviderAuthSpec,
}

const CLAUDE_COUNTERPARTS: &[&str] = &["codex", "gemini", "opencode", "qwen"];
const CODEX_COUNTERPARTS: &[&str] = &["claude", "gemini", "opencode", "qwen"];
const GEMINI_COUNTERPARTS: &[&str] = &["codex", "claude", "opencode", "qwen"];
const OPENCODE_COUNTERPARTS: &[&str] = &["codex", "claude", "gemini", "qwen"];
const QWEN_COUNTERPARTS: &[&str] = &["codex", "claude", "gemini", "opencode"];

const CLAUDE_AUTH_PATHS: &[&str] = &["~/.claude/.credentials.json"];
const CLAUDE_AUTH_ENV: &[&str] = &["ANTHROPIC_API_KEY"];
const CLAUDE_AUTH_CHECK: &[&str] = &["claude", "auth", "status"];
const CODEX_AUTH_PATHS: &[&str] = &["~/.codex/auth.json"];
const CODEX_AUTH_ENV: &[&str] = &["OPENAI_API_KEY"];
const CODEX_AUTH_CHECK: &[&str] = &["codex", "auth", "status"];
const GEMINI_AUTH_PATHS: &[&str] = &["~/.gemini/oauth_creds.json"];
const GEMINI_AUTH_ENV: &[&str] = &["GEMINI_API_KEY", "GOOGLE_API_KEY"];
const GEMINI_AUTH_CHECK: &[&str] = &["gemini", "auth", "status"];
// opencode stores `opencode auth login` credentials in the XDG data dir and
// accepts per-provider apiKey entries in opencode.json; both are observable
// credential sources (XDG_DATA_HOME/XDG_CONFIG_HOME overrides handled in
// provider_auth::detect_opencode_file_auth).
const OPENCODE_AUTH_PATHS: &[&str] = &[
    "~/.local/share/opencode/auth.json",
    "~/.config/opencode/opencode.json",
];
const OPENCODE_AUTH_ENV: &[&str] = &[
    "OPENCODE_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
];
const OPENCODE_AUTH_CHECK: &[&str] = &["opencode", "auth", "list"];
// qwen-code resolves credentials from OAuth (oauth_creds.json), the
// settings.json `env`/`modelProviders` blocks, and .env files
// (~/.qwen/.env plus project-relative fallbacks).
const QWEN_AUTH_PATHS: &[&str] = &[
    "~/.qwen/oauth_creds.json",
    "~/.qwen/settings.json",
    "~/.qwen/.env",
    "./.qwen/.env",
    "./.env",
];
const QWEN_AUTH_ENV: &[&str] = &[
    "DASHSCOPE_API_KEY",
    "QWEN_API_KEY",
    "OPENAI_API_KEY",
    "BAILIAN_CODING_PLAN_API_KEY",
];

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
        execution_adapter: ProviderExecutionAdapter::Claude,
        compaction_adapter: ProviderCompactionAdapter::ClaudeEnvironment,
        readiness_adapter: ProviderReadinessAdapter::Claude,
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "Claude provider default",
        },
        // #3263: Claude exposes no local/CLI context-window source, but AgentDesk
        // launches it in 1M-context mode, so this hardcoded 1M is accurate.
        default_context_window: 1_000_000,
        managed_tmux_backend: true,
        managed_tmux_wrapper_subcommand: Some("tmux-wrapper"),
        auth: ProviderAuthSpec {
            credential_paths: CLAUDE_AUTH_PATHS,
            env_keys: CLAUDE_AUTH_ENV,
            auth_check_argv: Some(CLAUDE_AUTH_CHECK),
        },
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
        execution_adapter: ProviderExecutionAdapter::Codex,
        compaction_adapter: ProviderCompactionAdapter::CodexCli,
        readiness_adapter: ProviderReadinessAdapter::Codex,
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "provider default",
        },
        // #3263: Codex resolves its context window cache-first from
        // ~/.codex/models_cache.json (see resolve_context_window /
        // codex_model_context_window). This registry value is only the
        // last-resort fallback when that cache is absent/unusable.
        default_context_window: CODEX_FALLBACK_CONTEXT_WINDOW,
        managed_tmux_backend: true,
        managed_tmux_wrapper_subcommand: Some("codex-tmux-wrapper"),
        auth: ProviderAuthSpec {
            credential_paths: CODEX_AUTH_PATHS,
            env_keys: CODEX_AUTH_ENV,
            auth_check_argv: Some(CODEX_AUTH_CHECK),
        },
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
        execution_adapter: ProviderExecutionAdapter::Gemini,
        compaction_adapter: ProviderCompactionAdapter::GeminiDisabled,
        readiness_adapter: ProviderReadinessAdapter::Gemini,
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "provider default",
        },
        // #3263: Gemini exposes no local/CLI context-window source, but AgentDesk
        // launches it in 1M-context mode, so this hardcoded 1M is accurate.
        default_context_window: 1_000_000,
        managed_tmux_backend: false,
        managed_tmux_wrapper_subcommand: None,
        auth: ProviderAuthSpec {
            credential_paths: GEMINI_AUTH_PATHS,
            env_keys: GEMINI_AUTH_ENV,
            auth_check_argv: Some(GEMINI_AUTH_CHECK),
        },
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
        execution_adapter: ProviderExecutionAdapter::OpenCode,
        compaction_adapter: ProviderCompactionAdapter::OpenCodeDisabled,
        readiness_adapter: ProviderReadinessAdapter::OpenCode,
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: false,
            runtime_model: None,
            source_label: "provider default",
        },
        // #3263: OpenCode exposes no local/CLI context-window source; this
        // conservative 128k is a hardcoded default (no dynamic source exists).
        default_context_window: 128_000,
        managed_tmux_backend: false,
        managed_tmux_wrapper_subcommand: None,
        auth: ProviderAuthSpec {
            credential_paths: OPENCODE_AUTH_PATHS,
            env_keys: OPENCODE_AUTH_ENV,
            auth_check_argv: Some(OPENCODE_AUTH_CHECK),
        },
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
        execution_adapter: ProviderExecutionAdapter::Qwen,
        compaction_adapter: ProviderCompactionAdapter::QwenDisabled,
        readiness_adapter: ProviderReadinessAdapter::Qwen,
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "provider default",
        },
        // #3263: Qwen exposes no local/CLI context-window source; this
        // conservative 128k is a hardcoded default (no dynamic source exists).
        default_context_window: 128_000,
        managed_tmux_backend: true,
        managed_tmux_wrapper_subcommand: Some("qwen-tmux-wrapper"),
        auth: ProviderAuthSpec {
            credential_paths: QWEN_AUTH_PATHS,
            env_keys: QWEN_AUTH_ENV,
            // qwen-code 0.15+ removed the `qwen auth` subcommand; credentials
            // are configured via the interactive /auth flow or env keys.
            auth_check_argv: None,
        },
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

    pub fn execution_adapter(&self) -> Option<ProviderExecutionAdapter> {
        self.registry_entry().map(|entry| entry.execution_adapter)
    }

    pub fn compaction_adapter(&self) -> Option<ProviderCompactionAdapter> {
        self.registry_entry().map(|entry| entry.compaction_adapter)
    }

    pub fn readiness_adapter(&self) -> Option<ProviderReadinessAdapter> {
        self.registry_entry().map(|entry| entry.readiness_adapter)
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
    pub(crate) fn resolve_runtime_path(&self) -> Option<String> {
        match self {
            Self::Claude => {
                crate::services::platform::resolve_provider_binary("claude").resolved_path
            }
            Self::Codex => crate::services::codex::resolve_codex_path(),
            Self::Gemini => crate::services::gemini::resolve_gemini_path(),
            Self::OpenCode => crate::services::opencode::resolve_opencode_path(),
            Self::Qwen => crate::services::qwen::resolve_qwen_path(),
            Self::Unsupported(_) => None,
        }
    }

    pub fn probe_runtime(&self) -> Option<ProviderRuntimeProbe> {
        let entry = self.registry_entry()?;
        let capabilities = entry.capabilities;
        let binary_probe = crate::services::platform::probe_provider_binary_version(self.as_str());
        let credentials =
            crate::services::provider_auth::detect_provider_credentials(entry.id, &entry.auth);
        Some(ProviderRuntimeProbe {
            provider: self.clone(),
            capabilities,
            resolution: binary_probe.resolution,
            version: binary_probe.version_output,
            probe_failure_kind: binary_probe.probe_failure_kind,
            skipped_candidate_failures: binary_probe.skipped_candidate_failures,
            credential_present: credentials.credential_present,
            credential_source: credentials.source,
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

    /// Returns generic provider environment variables for auto-compact.
    ///
    /// Claude intentionally has no generic percent environment variable: its
    /// launch path resolves a model-aware absolute
    /// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` from launch provenance instead.
    #[allow(dead_code)]
    pub fn compact_env_vars(&self, percent: u64) -> Vec<(String, String)> {
        let Some(adapter) = self.compaction_adapter() else {
            return Vec::new();
        };
        match adapter {
            ProviderCompactionAdapter::ClaudeEnvironment => {
                let _ = percent;
                Vec::new()
            }
            ProviderCompactionAdapter::CodexCli
            | ProviderCompactionAdapter::GeminiDisabled
            | ProviderCompactionAdapter::OpenCodeDisabled
            | ProviderCompactionAdapter::QwenDisabled => Vec::new(),
        }
    }

    /// Default context window size in tokens for this provider.
    pub fn default_context_window(&self) -> u64 {
        self.registry_entry()
            .map(|entry| entry.default_context_window)
            .unwrap_or(200_000)
    }

    /// Resolve the context window for a specific model.
    ///
    /// #3263: Codex is cache-first REGARDLESS of whether `model` is `Some`/`None`
    /// — `codex_model_context_window` reads `~/.codex/models_cache.json` (exact
    /// slug when supplied, else max-of-cache). A provider-default Codex turn
    /// (`None`) therefore still prefers the cache's max-of-cache window over the
    /// stale constant. Only when that cache is absent/empty/unparseable do we
    /// fall back to the provider default (`CODEX_FALLBACK_CONTEXT_WINDOW`). Other
    /// providers have no local source and always use their registry default.
    pub fn resolve_context_window(&self, model: Option<&str>) -> u64 {
        // `None` (provider-default turn) maps to an empty slug, which never
        // matches an entry, so `codex_context_window_from_cache` skips the
        // exact-match branch and yields the cache's max-of-cache window.
        let cached = if let Self::Codex = self {
            codex_model_context_window(model.unwrap_or(""))
        } else {
            None
        };
        self.resolve_context_window_with(cached)
    }

    /// Pure composition of [`resolve_context_window`]: apply the cache-first →
    /// provider-default fallback to a pre-resolved cache window. Splitting this
    /// out keeps the absent-cache → `default_context_window()` fallback testable
    /// without touching the real `~/.codex/models_cache.json` on disk.
    fn resolve_context_window_with(&self, cached: Option<u64>) -> u64 {
        cached.unwrap_or_else(|| self.default_context_window())
    }

    /// Returns Codex-specific CLI config overrides for auto-compact.
    /// Codex uses model_auto_compact_token_limit (absolute token count).
    pub fn compact_cli_config(&self, percent: u64, context_window: u64) -> Vec<(String, String)> {
        let Some(adapter) = self.compaction_adapter() else {
            return Vec::new();
        };
        match adapter {
            ProviderCompactionAdapter::CodexCli => {
                let token_limit = context_window * percent / 100;
                vec![(
                    "model_auto_compact_token_limit".to_string(),
                    token_limit.to_string(),
                )]
            }
            ProviderCompactionAdapter::ClaudeEnvironment
            | ProviderCompactionAdapter::GeminiDisabled
            | ProviderCompactionAdapter::OpenCodeDisabled
            | ProviderCompactionAdapter::QwenDisabled => Vec::new(),
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

/// Returns `Some(prompt)` if the caller should include the system prompt
/// in this turn, or `None` if it can be safely omitted. The omission
/// decision is deliberately narrow:
///
/// 1. Empty system prompts (always omitted — nothing to send).
/// 2. The legacy Codex+resumed-session rule
///    ([`should_omit_repeated_system_prompt`]).
///
/// Issue #3744 retired the unused generalized envelope/dev-role dedup
/// infrastructure rather than wiring it unsafely across provider resets.
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
    Some(system_prompt)
}

pub fn compact_resumed_provider_turn_prompt(
    _provider: &ProviderKind,
    _session_id: Option<&str>,
    prompt: String,
) -> String {
    prompt
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

    #[test]
    fn codex_resume_context_preserves_payload_without_reuse_prologue() {
        let prompt = compact_resumed_provider_turn_prompt(
            &ProviderKind::Codex,
            Some("thread-1"),
            "[User Request]\nhello".to_string(),
        );

        assert_eq!(prompt, "[User Request]\nhello");
        assert!(!prompt.contains("[Provider Session Reuse]"));
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
    child_pid: Mutex<Option<CapturedProcess>>,
    cancel_source: Mutex<Option<String>>,
    cancel_source_kind: Mutex<Option<CancelSource>>,
    /// Serializes cancellation attribution, timeout, and completion publication.
    cancellation_publication: Mutex<()>,
    /// SSH cancel flag — set to true to signal remote execution to close the channel
    #[allow(dead_code)]
    pub ssh_cancel: Mutex<Option<std::sync::Arc<AtomicBool>>>,
    /// Tmux binding for cleanup on cancel.
    pub(crate) tmux_binding: Mutex<Option<cancel_token_cleanup::authority::TmuxBinding>>,
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
    /// Claude turn-interrupt fence. Each `CancelToken` is a turn generation.
    /// `0 -> 1` reserves one delivery attempt; a skipped or failed attempt
    /// rolls it back to 0, while a successful provider write commits `1 -> 2`.
    claude_interrupt_claim: AtomicU8,
    /// Monotonic Claude turn identity used for diagnostics and fence observability.
    claude_interrupt_generation: u64,
    /// Durable episode identity shared with the Discord mailbox actor and inflight row.
    /// `None` is reserved for explicitly restored legacy rows.
    turn_nonce: Option<String>,
    /// Wrapper prompt handoff completed before its JSONL user envelope appeared.
    /// The stop path must treat this window as submitted, not as prior-turn idle.
    claude_interrupt_submit_pending: AtomicBool,
    /// Lifecycle-aware restart/handoff mode for inflight preservation.
    pub restart_mode: AtomicU8,
    /// Independent destructive claims prevent a PID-only cleanup from suppressing tmux cleanup.
    pub(crate) pid_kill_claim: AtomicU8,
    pub(crate) name_kill_claim: AtomicU8,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::with_turn_nonce(Some(uuid::Uuid::new_v4().to_string()))
    }

    /// Restore a durable turn episode. `None` preserves the legacy row's
    /// identity instead of silently upgrading it to an unrelated episode.
    pub fn from_persisted_turn_nonce(turn_nonce: Option<String>) -> Self {
        Self::with_turn_nonce(turn_nonce.filter(|nonce| !nonce.is_empty()))
    }

    fn with_turn_nonce(turn_nonce: Option<String>) -> Self {
        static NEXT_CLAUDE_INTERRUPT_GENERATION: AtomicU64 = AtomicU64::new(1);

        Self {
            cancelled: AtomicBool::new(false),
            child_pid: Mutex::new(None),
            cancel_source: Mutex::new(None),
            cancel_source_kind: Mutex::new(None),
            cancellation_publication: Mutex::new(()),
            ssh_cancel: Mutex::new(None),
            tmux_binding: Mutex::new(None),
            watchdog_deadline_ms: AtomicI64::new(0),
            watchdog_max_deadline_ms: AtomicI64::new(0),
            async_managed: AtomicBool::new(false),
            completion_cleanup: AtomicBool::new(false),
            claude_interrupt_claim: AtomicU8::new(0),
            claude_interrupt_generation: NEXT_CLAUDE_INTERRUPT_GENERATION
                .fetch_add(1, Ordering::Relaxed),
            turn_nonce,
            claude_interrupt_submit_pending: AtomicBool::new(false),
            restart_mode: AtomicU8::new(0),
            pid_kill_claim: AtomicU8::new(0),
            name_kill_claim: AtomicU8::new(0),
        }
    }

    pub fn turn_nonce(&self) -> Option<&str> {
        self.turn_nonce.as_deref()
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
        let _publication = self
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.completion_cleanup.store(true, Ordering::Release);
    }

    pub fn is_completion_cleanup(&self) -> bool {
        self.completion_cleanup.load(Ordering::Acquire)
    }

    pub(crate) fn store_child_pid(&self, pid: u32) {
        *self.child_pid.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(CapturedProcess::capture(pid));
    }

    pub(crate) fn child_pid_value(&self) -> Option<u32> {
        self.child_pid
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|process| process.pid))
    }

    pub(crate) fn captured_child_process(&self) -> Option<CapturedProcess> {
        self.child_pid.lock().ok().and_then(|guard| guard.clone())
    }

    #[cfg(test)]
    pub(crate) fn store_child_pid_without_identity_for_test(&self, pid: u32) {
        *self.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(CapturedProcess {
            pid,
            identity: None,
        });
    }

    pub(crate) fn clear_child_pid(&self) {
        *self.child_pid.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(crate) fn store_child_pid_if_empty(&self, pid: u32) {
        let captured = CapturedProcess::capture(pid);
        let mut child_pid = self.child_pid.lock().unwrap_or_else(|e| e.into_inner());
        if child_pid.is_none() {
            *child_pid = Some(captured);
        }
    }

    /// Compatibility adapter for legacy callers; cleanup authority lives in request_cleanup.
    pub fn cancel_with_tmux_cleanup(&self) {
        let _ = self.request_cleanup(cancel_token_cleanup::executor::CleanupRequest {
            cancel_source: "tmux_cleanup".to_string(),
            intent: cancel_token_cleanup::executor::TmuxCleanupIntent::CleanupSession,
            termination_reason: None,
            hard_stop_target: None,
        });
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

    fn set_cancel_source_if_absent_locked(&self, source: impl Into<String>) {
        let label = source.into();
        let classified = CancelSource::classify(&label);
        // All source writers take kind before label. Cleanup is provisional and
        // may never replace an already recorded cancellation cause.
        let mut kind = self
            .cancel_source_kind
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut current_label = self.cancel_source.lock().unwrap_or_else(|e| e.into_inner());
        if current_label.is_none() {
            *current_label = Some(label);
            if kind.is_none() {
                *kind = Some(classified);
            }
        }
    }

    pub(crate) fn set_cancel_source_if_absent(&self, source: impl Into<String>) {
        let _publication = self
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.set_cancel_source_if_absent_locked(source);
    }

    fn set_cancel_source_locked(&self, source: impl Into<String>) {
        let label = source.into();
        let classified = CancelSource::classify(&label);
        // Keep kind and label transactional. Specific kinds retain #3908's
        // first-wins behavior, except that cleanup's Other classification is
        // deliberately provisional and upgrades to a later specific source.
        let mut kind = self
            .cancel_source_kind
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut current_label = self.cancel_source.lock().unwrap_or_else(|e| e.into_inner());
        if kind.is_none()
            || (*kind == Some(CancelSource::Other) && classified != CancelSource::Other)
        {
            *kind = Some(classified);
        }
        *current_label = Some(label);
    }

    pub fn set_cancel_source(&self, source: impl Into<String>) {
        let _publication = self
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.set_cancel_source_locked(source);
    }

    /// Explicitly set the structured cancel source. Also updates the
    /// free-form label (used for tracing / dispatch reason) to the canonical
    /// string for the variant when no label was previously recorded.
    pub fn set_cancel_source_kind(&self, kind: CancelSource) {
        let _publication = self
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.set_cancel_source_kind_transactional(kind, |_| {});
    }

    pub(crate) fn try_mark_watchdog_timeout(&self) -> bool {
        let _publication = self
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.publish_watchdog_timeout_locked()
    }

    fn publish_watchdog_timeout_locked(&self) -> bool {
        if self.completion_cleanup.load(Ordering::Acquire) {
            return false;
        }

        let mut kind = self
            .cancel_source_kind
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut label = self
            .cancel_source
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self
            .cancelled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        *kind = Some(CancelSource::WatchdogTimeout);
        if label.is_none() {
            *label = Some(CancelSource::WatchdogTimeout.as_label().to_string());
        }
        true
    }

    pub(crate) fn publish_cancel(&self, source: impl Into<String>) {
        let _publication = self
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.set_cancel_source_locked(source);
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn publish_cancel_if_source_absent(&self, source: impl Into<String>) {
        let _publication = self
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.set_cancel_source_if_absent_locked(source);
        self.cancelled.store(true, Ordering::Release);
    }

    fn set_cancel_source_kind_transactional(
        &self,
        kind: CancelSource,
        after_kind_write: impl FnOnce(&Self),
    ) {
        // Hold both locks across the pair update so cleanup cannot leave a
        // canonical kind paired with a cleanup-only label.
        let mut current_kind = self
            .cancel_source_kind
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut label = self.cancel_source.lock().unwrap_or_else(|e| e.into_inner());
        let replace_provisional_cleanup_label =
            *current_kind == Some(CancelSource::Other) && label.as_deref() == Some("tmux_cleanup");
        *current_kind = Some(kind);
        after_kind_write(self);
        if label.is_none() || replace_provisional_cleanup_label {
            *label = Some(kind.as_label().to_string());
        }
    }

    #[cfg(test)]
    fn set_cancel_source_kind_with_interleaving(
        &self,
        kind: CancelSource,
        after_kind_write: impl FnOnce(&Self),
    ) {
        self.set_cancel_source_kind_transactional(kind, after_kind_write);
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

pub fn register_child_pid(token: Option<&CancelToken>, child_pid: u32) {
    if let Some(token) = token {
        token.store_child_pid(child_pid);
    }
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
        let runtime_kind =
            crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&session_name);
        Self::tmux_with_runtime(session_name, provider, runtime_kind)
    }

    #[cfg(unix)]
    pub fn tmux_with_runtime(
        session_name: String,
        provider: ProviderKind,
        runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    ) -> Self {
        let name_alive = session_name.clone();
        let name_ready = session_name;
        let provider_ready = provider;
        Self::new(
            move || tmux_session_alive(&name_alive),
            move || {
                tmux_session_fallback_ready_for_input(&name_ready, &provider_ready, runtime_kind)
                    .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
            },
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
                .or_else(|| {
                    tmux_session_fallback_ready_for_input(
                        &name_ready,
                        &provider_ready,
                        runtime_kind,
                    )
                    .map(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
                })
                .unwrap_or(false)
            },
        )
    }

    #[cfg(not(unix))]
    pub fn tmux(_session_name: String, _provider: ProviderKind) -> Self {
        Self::new(|| false, || false)
    }

    #[cfg(not(unix))]
    pub fn tmux_with_runtime(
        _session_name: String,
        _provider: ProviderKind,
        _runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    ) -> Self {
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
    if let ProviderKind::Unsupported(_) = provider {
        return crate::services::tmux_common::tmux_capture_indicates_generic_ready_banner(capture)
            || tmux_capture_contains_wrapper_ready_marker(capture, provider);
    }
    let Some(adapter) = provider.readiness_adapter() else {
        return false;
    };
    match adapter {
        ProviderReadinessAdapter::Claude => {
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(capture)
        }
        ProviderReadinessAdapter::Codex => {
            crate::services::codex_tui::input::pane_looks_ready_for_codex_prompt(capture)
                || crate::services::tmux_common::tmux_capture_indicates_generic_ready_banner(
                    capture,
                )
                || tmux_capture_contains_wrapper_ready_marker(capture, provider)
        }
        ProviderReadinessAdapter::Qwen => {
            crate::services::tmux_common::tmux_capture_indicates_generic_ready_banner(capture)
                || tmux_capture_contains_wrapper_ready_marker(capture, provider)
        }
        ProviderReadinessAdapter::Gemini => {
            crate::services::tmux_common::tmux_capture_indicates_generic_ready_banner(capture)
                || tmux_capture_contains_wrapper_ready_marker(capture, provider)
        }
        ProviderReadinessAdapter::OpenCode => {
            crate::services::tmux_common::tmux_capture_indicates_generic_ready_banner(capture)
                || tmux_capture_contains_wrapper_ready_marker(capture, provider)
        }
    }
}

#[cfg(test)]
#[path = "provider/provider_conformance_invariant_tests.rs"]
mod provider_conformance_invariant_tests;

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
pub(crate) fn tmux_session_fallback_ready_for_input(
    tmux_session_name: &str,
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
) -> Option<crate::services::pane_readiness::FallbackPaneReadiness> {
    crate::services::pane_readiness::FallbackPaneReadiness::from_pane_scrape(
        provider,
        runtime_kind,
        || {
            crate::services::platform::tmux::capture_pane(tmux_session_name, -80)
                .map(|stdout| tmux_capture_indicates_ready_for_input(&stdout, provider))
                .unwrap_or(false)
        },
    )
}

#[cfg(not(unix))]
pub(crate) fn tmux_session_fallback_ready_for_input(
    _tmux_session_name: &str,
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
) -> Option<crate::services::pane_readiness::FallbackPaneReadiness> {
    crate::services::pane_readiness::FallbackPaneReadiness::from_pane_scrape(
        provider,
        runtime_kind,
        || false,
    )
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
    let mut committed_offset = start_offset;
    let mut partial_line = Vec::new();
    let mut buf = [0u8; 8192];
    let mut no_data_count: u32 = 0;
    let mut ready_for_input_tracker = ReadyForInputIdleTracker::default();

    loop {
        if cancel_requested(cancel_token.as_deref()) {
            return Ok(ReadOutputResult::Cancelled {
                offset: committed_offset,
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
                                offset: committed_offset,
                            });
                        }
                        return Ok(ReadOutputResult::Completed {
                            offset: committed_offset,
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
                partial_line.extend_from_slice(&buf[..n]);
                if let Some(pos) = partial_line.iter().rposition(|byte| *byte == b'\n') {
                    emit_output_offset(committed_offset.saturating_add((pos + 1) as u64));
                }

                while let Some(pos) = partial_line.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = partial_line.drain(..=pos).collect();
                    committed_offset = committed_offset.saturating_add(line.len() as u64);
                    let line = String::from_utf8_lossy(&line);
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    if !process_line(trimmed, state) {
                        return Ok(ReadOutputResult::Cancelled {
                            offset: committed_offset,
                        });
                    }

                    if has_final(state) {
                        return Ok(ReadOutputResult::Completed {
                            offset: committed_offset,
                        });
                    }
                }
            }
            Err(_) => break,
        }
    }

    emit_deferred_error(state);
    Ok(ReadOutputResult::SessionDied {
        offset: committed_offset,
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
/// Returns None only when the cache is missing/unreadable; resolution within a
/// present-but-non-matching cache is handled by `codex_context_window_from_cache`.
fn codex_model_context_window(model: &str) -> Option<u64> {
    let cache_path = dirs::home_dir()?.join(".codex/models_cache.json");
    let data = std::fs::read_to_string(cache_path).ok()?;
    codex_context_window_from_cache(&data, model)
}

/// #3263: Resolve a Codex context window from the raw `models_cache.json` body.
///
/// 1. Exact slug match → that entry's `context_window`.
/// 2. Cache present with models but no slug match (slug drift) → the MAX
///    `context_window` across cached entries. The cache is authoritative and
///    far fresher than the hardcoded last-resort, so we prefer it.
/// 3. Cache empty / unparseable / no usable `context_window` → None, letting the
///    caller fall back to `CODEX_FALLBACK_CONTEXT_WINDOW`.
///
/// Defensive: malformed JSON or unexpected shapes yield None, never a panic.
fn codex_context_window_from_cache(data: &str, model: &str) -> Option<u64> {
    let json: serde_json::Value = serde_json::from_str(data).ok()?;
    let models = json.get("models")?.as_array()?;

    let mut max_window: Option<u64> = None;
    for entry in models {
        let Some(window) = entry.get("context_window").and_then(|v| v.as_u64()) else {
            continue;
        };
        // Only attempt an exact slug match when a real slug was supplied. An
        // empty `model` (the `None` provider-default path) must NEVER exact-match
        // — not even a blank-slug (`"slug": ""`) cache entry — and must always
        // resolve via max-of-cache (cache present) or None (cache absent).
        if !model.is_empty() && entry.get("slug").and_then(|s| s.as_str()) == Some(model) {
            return Some(window);
        }
        max_window = Some(max_window.map_or(window, |m| m.max(window)));
    }
    max_window
}

#[cfg(test)]
mod codex_context_window_tests {
    use super::{CODEX_FALLBACK_CONTEXT_WINDOW, ProviderKind, codex_context_window_from_cache};

    const CACHE: &str = r#"{
        "models": [
            { "slug": "gpt-5.1-codex", "context_window": 200000 },
            { "slug": "gpt-5.5-codex", "context_window": 272000 }
        ]
    }"#;

    // ---- Pure helper: present cache × {exact slug, unknown slug, None-as-""} ----

    #[test]
    fn exact_slug_match_returns_that_window() {
        assert_eq!(
            codex_context_window_from_cache(CACHE, "gpt-5.5-codex"),
            Some(272_000)
        );
        // A non-max exact match still wins over max-of-cache.
        assert_eq!(
            codex_context_window_from_cache(CACHE, "gpt-5.1-codex"),
            Some(200_000)
        );
    }

    #[test]
    fn cache_present_with_unknown_slug_returns_max_of_cache() {
        // #3263: slug drift — cache exists, requested slug absent → use the MAX
        // cached context_window, not the stale hardcoded last-resort.
        assert_eq!(
            codex_context_window_from_cache(CACHE, "gpt-6-codex-future"),
            Some(272_000)
        );
    }

    #[test]
    fn cache_present_with_empty_slug_returns_max_of_cache() {
        // #3263 Issue-1: the `None` (provider-default) path maps to "" and must
        // never exact-match, so the helper yields the cache's max-of-cache.
        assert_eq!(codex_context_window_from_cache(CACHE, ""), Some(272_000));
    }

    #[test]
    fn blank_slug_entry_never_exact_matches_empty_model() {
        // #3263 Fix-1: a cache containing a blank-slug (`"slug": ""`) entry must
        // NOT let an empty `model` (`None` path) exact-match the blank entry's
        // (smaller) window. The `!model.is_empty()` guard forces max-of-cache.
        const CACHE_WITH_BLANK_SLUG: &str = r#"{
            "models": [
                { "slug": "", "context_window": 8000 },
                { "slug": "gpt-5.5-codex", "context_window": 272000 }
            ]
        }"#;
        // Empty model → MAX of cache (272000), NOT the blank-slug 8000.
        assert_eq!(
            codex_context_window_from_cache(CACHE_WITH_BLANK_SLUG, ""),
            Some(272_000)
        );
        // Unknown slug → also MAX of cache, never the blank-slug small value.
        assert_eq!(
            codex_context_window_from_cache(CACHE_WITH_BLANK_SLUG, "gpt-x"),
            Some(272_000)
        );
        // A real slug still exact-matches its own (non-max) window.
        assert_eq!(
            codex_context_window_from_cache(CACHE_WITH_BLANK_SLUG, "gpt-5.5-codex"),
            Some(272_000)
        );
    }

    // ---- Pure helper: absent / empty / unparseable cache → None (constant) ----

    #[test]
    fn empty_or_unparseable_cache_returns_none() {
        // Absent-equivalent shapes for every lookup form (slug AND None-as-"").
        assert_eq!(
            codex_context_window_from_cache(r#"{"models": []}"#, "anything"),
            None
        );
        assert_eq!(
            codex_context_window_from_cache(r#"{"models": []}"#, ""),
            None
        );
        assert_eq!(codex_context_window_from_cache("not json", "x"), None);
        assert_eq!(codex_context_window_from_cache("not json", ""), None);
        assert_eq!(codex_context_window_from_cache("{}", "x"), None);
        assert_eq!(codex_context_window_from_cache("{}", ""), None);
    }

    // ---- Resolve-level: constant is the last resort only when cache unusable ---

    #[test]
    fn resolve_falls_back_to_documented_constant_when_cache_unusable() {
        // #3263 Issue-2: cache-absent must resolve to the documented last-resort
        // for BOTH the no-model (`None`) path and a `Some(model)` path. We drive
        // the *resolve-level* composition (`resolve_context_window_with`) with the
        // None the cache reader yields on a missing/unparseable file, proving the
        // constant is reached at the resolve level — not only via the pure helper.

        // Sanity: the cache reader yields None for absent-equivalent inputs
        // across both lookup forms (empty slug AND a real slug).
        assert_eq!(codex_context_window_from_cache("not json", ""), None);
        assert_eq!(
            codex_context_window_from_cache("not json", "gpt-5.5-codex"),
            None
        );

        // Resolve-level: cache-absent (None) → the documented constant, for both
        // the `None` (empty-slug) and `Some(model)` paths.
        assert_eq!(
            ProviderKind::Codex.resolve_context_window_with(None),
            CODEX_FALLBACK_CONTEXT_WINDOW
        );

        // And the constant the resolve-level fallback yields IS the provider
        // default, so a present cache window would supersede it when non-None.
        assert_eq!(
            ProviderKind::Codex.default_context_window(),
            CODEX_FALLBACK_CONTEXT_WINDOW
        );
        // A present cache window supersedes the constant at the resolve level.
        assert_eq!(
            ProviderKind::Codex.resolve_context_window_with(Some(123_456)),
            123_456
        );
    }

    #[test]
    fn resolve_is_cache_first_for_none_and_some_consistently() {
        // #3263 Issue-1/Issue-2: whatever the real `~/.codex` cache yields for an
        // empty slug (max-of-cache when present, else the constant), the `None`
        // and "absent-model" `Some("")` resolve paths must agree — proving the
        // None path is cache-first, not a hardcoded shortcut to the constant.
        let none_resolved = ProviderKind::Codex.resolve_context_window(None);
        let empty_resolved = ProviderKind::Codex.resolve_context_window(Some(""));
        assert_eq!(none_resolved, empty_resolved);

        // Whichever branch wins, the resolved window is a usable positive size
        // (cache max-of-cache when present, else the constant) — never zero.
        assert!(none_resolved > 0);
    }
}

#[cfg(test)]
mod cancel_token_tests {
    use super::{
        CancelSource, CancelToken, cancel_requested, current_unix_millis,
        enforce_watchdog_deadline, register_child_pid,
    };
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn cancel_token_helpers_register_source_and_state() {
        let token = CancelToken::new();
        assert!(!cancel_requested(Some(&token)));
        assert!(!cancel_requested(None));
        assert_eq!(token.cancel_source(), None);

        register_child_pid(Some(&token), 4242);
        assert_eq!(token.child_pid_value(), Some(4242));
        assert_eq!(
            token.captured_child_process().map(|process| process.pid),
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
    fn cleanup_adapter_preserves_existing_voice_cancel_source_and_kind() {
        let token = CancelToken::new();
        token.set_cancel_source("voice_barge_in_explicit_stop");
        token.set_cancel_source_kind(CancelSource::UserBargeIn);

        token.cancel_with_tmux_cleanup();

        assert_eq!(
            token.cancel_source().as_deref(),
            Some("voice_barge_in_explicit_stop")
        );
        assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
    }

    #[test]
    fn cleanup_adapter_populates_source_for_fresh_token() {
        let token = CancelToken::new();

        token.cancel_with_tmux_cleanup();

        assert_eq!(token.cancel_source().as_deref(), Some("tmux_cleanup"));
        assert_eq!(token.cancel_source_kind(), Some(CancelSource::Other));
    }

    #[test]
    fn concurrent_cleanup_cannot_downgrade_primary_cancel_source() {
        let token = Arc::new(CancelToken::new());
        token.set_cancel_source("voice_barge_in_live_cut");
        let barrier = Arc::new(Barrier::new(3));
        let mut cleaners = Vec::new();
        for _ in 0..2 {
            let token = Arc::clone(&token);
            let barrier = Arc::clone(&barrier);
            cleaners.push(thread::spawn(move || {
                barrier.wait();
                token.cancel_with_tmux_cleanup();
            }));
        }
        barrier.wait();
        for cleaner in cleaners {
            cleaner.join().unwrap();
        }

        assert_eq!(
            token.cancel_source().as_deref(),
            Some("voice_barge_in_live_cut")
        );
        assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
    }

    #[test]
    fn cleanup_first_upgrades_to_specific_voice_source() {
        let token = CancelToken::new();
        token.cancel_with_tmux_cleanup();
        token.set_cancel_source("voice_barge_in_live_cut");

        assert_eq!(
            token.cancel_source().as_deref(),
            Some("voice_barge_in_live_cut")
        );
        assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
    }

    #[test]
    fn concurrent_cleanup_and_voice_source_never_leave_provisional_kind() {
        for cleanup_first in [true, false] {
            let token = Arc::new(CancelToken::new());
            let cleanup_ready = Arc::new(Barrier::new(2));
            let voice_ready = Arc::new(Barrier::new(2));
            let cleanup_token = Arc::clone(&token);
            let cleanup_ready_for_thread = Arc::clone(&cleanup_ready);
            let voice_ready_for_thread = Arc::clone(&voice_ready);
            let cleanup = thread::spawn(move || {
                if cleanup_first {
                    cleanup_token.cancel_with_tmux_cleanup();
                    cleanup_ready_for_thread.wait();
                    voice_ready_for_thread.wait();
                } else {
                    cleanup_ready_for_thread.wait();
                    voice_ready_for_thread.wait();
                    cleanup_token.cancel_with_tmux_cleanup();
                }
            });
            let voice_token = Arc::clone(&token);
            if cleanup_first {
                cleanup_ready.wait();
                voice_token.set_cancel_source("voice_barge_in_explicit_stop");
                voice_ready.wait();
            } else {
                voice_token.set_cancel_source("voice_barge_in_explicit_stop");
                cleanup_ready.wait();
                voice_ready.wait();
            }
            cleanup.join().unwrap();

            assert_eq!(
                token.cancel_source().as_deref(),
                Some("voice_barge_in_explicit_stop")
            );
            assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
        }
    }

    #[test]
    fn explicit_kind_pair_update_excludes_cleanup_interleaving() {
        let token = CancelToken::new();

        token.set_cancel_source_kind_with_interleaving(CancelSource::UserBargeIn, |token| {
            assert!(
                token.cancel_source.try_lock().is_err(),
                "the label lock must remain held after the kind write"
            );
        });

        assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
        assert_eq!(token.cancel_source().as_deref(), Some("user_barge_in"));
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

    #[test]
    fn watchdog_poll_path_respects_completion_cleanup_before_timeout_commit() {
        let token = CancelToken::new();
        let now = current_unix_millis();
        token
            .watchdog_deadline_ms
            .store(now + 1_000, Ordering::Relaxed);
        token.mark_completion_cleanup();

        assert!(!enforce_watchdog_deadline(&token, now + 1_000));
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert!(!cancel_requested(Some(&token)));
        assert_eq!(token.cancel_source_kind(), None);
        assert_eq!(token.cancel_source(), None);
    }

    #[test]
    fn watchdog_poll_path_commits_timeout_through_publication_boundary() {
        let token = CancelToken::new();
        let now = current_unix_millis();
        token
            .watchdog_deadline_ms
            .store(now + 1_000, Ordering::Relaxed);

        assert!(enforce_watchdog_deadline(&token, now + 1_000));
        assert!(token.cancelled.load(Ordering::Acquire));
        assert!(cancel_requested(Some(&token)));
        assert_eq!(
            token.cancel_source_kind(),
            Some(CancelSource::WatchdogTimeout)
        );
        assert_eq!(token.cancel_source().as_deref(), Some("watchdog_timeout"));
    }

    #[test]
    fn completion_cleanup_can_win_when_publication_is_held_before_poll() {
        let token = Arc::new(CancelToken::new());
        let now = current_unix_millis();
        token
            .watchdog_deadline_ms
            .store(now + 1_000, Ordering::Relaxed);

        let publication = token
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let token_for_poll = Arc::clone(&token);
        let poll =
            std::thread::spawn(move || enforce_watchdog_deadline(&token_for_poll, now + 1_000));

        token.completion_cleanup.store(true, Ordering::Release);
        drop(publication);

        assert!(!poll.join().expect("poll thread should finish"));
        assert!(!token.cancelled.load(Ordering::Acquire));
        assert_eq!(token.cancel_source_kind(), None);
        assert_eq!(token.cancel_source(), None);
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
    use super::{CancelToken, ReadOutputResult, poll_output_file_until_result};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

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

    #[test]
    fn unterminated_tail_bytes_do_not_advance_committed_offset() {
        #[derive(Default)]
        struct TestState {
            lines: Vec<String>,
        }

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stream.jsonl");
        let complete = "complete\n";
        std::fs::write(&output_path, format!("{complete}unterminated")).unwrap();
        let safe_offset = complete.len() as u64;
        let cancel_token = Arc::new(CancelToken::new());
        let cancel_from_line = cancel_token.clone();

        let mut state = TestState::default();
        let mut offsets = Vec::new();
        let result = poll_output_file_until_result(
            output_path.to_str().unwrap(),
            0,
            Some(cancel_token),
            &mut state,
            || true,
            || false,
            |offset| offsets.push(offset),
            |line: &str, state| {
                state.lines.push(line.to_string());
                cancel_from_line.cancelled.store(true, Ordering::Relaxed);
                true
            },
            |_| false,
            |_| true,
            |_| {},
        )
        .unwrap();

        assert_eq!(
            result,
            ReadOutputResult::Cancelled {
                offset: safe_offset
            }
        );
        assert_eq!(state.lines, vec!["complete".to_string()]);
        assert_eq!(offsets, vec![safe_offset]);
    }
}
