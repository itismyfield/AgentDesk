//! Canonical provider registry rows, aliases, and counterpart derivation.

use crate::services::provider_auth::ProviderAuthSpec;
use serde::{Deserialize, Serialize};

use super::{
    CODEX_FALLBACK_CONTEXT_WINDOW, ProviderCapabilities, ProviderDefaultBehavior, ProviderKind,
};

/// Wire-format dialect used by providers whose CLI emits line-delimited JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamJsonDialectId {
    Grok,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderExecutionAdapter {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    Qwen,
    StreamJsonCli(StreamJsonDialectId),
}

impl ProviderExecutionAdapter {
    pub const fn execution_surface(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode_http",
            Self::Qwen => "managed_tmux_wrapper",
            Self::StreamJsonCli(_) => "stream_json_cli",
        }
    }

    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Qwen => "qwen",
            Self::StreamJsonCli(StreamJsonDialectId::Grok) => "grok",
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
            Self::StreamJsonCli(StreamJsonDialectId::Grok) => ProviderCapabilities {
                binary_name: "grok",
                supports_structured_output: false,
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
    StreamJsonDisabled,
}

impl ProviderCompactionAdapter {
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::ClaudeEnvironment => "claude",
            Self::CodexCli => "codex",
            Self::GeminiDisabled => "gemini",
            Self::OpenCodeDisabled => "opencode",
            Self::QwenDisabled => "qwen",
            Self::StreamJsonDisabled => "grok",
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
    GenericBanner,
}

impl ProviderReadinessAdapter {
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Qwen => "qwen",
            Self::GenericBanner => "grok",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderRegistryEntry {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    pub display_name: &'static str,
    pub cli_init_label: &'static str,
    pub channel_suffix: Option<&'static str>,
    pub default_channel_provider: bool,
    pub capabilities: ProviderCapabilities,
    pub execution_adapter: ProviderExecutionAdapter,
    pub compaction_adapter: ProviderCompactionAdapter,
    pub readiness_adapter: ProviderReadinessAdapter,
    pub default_behavior: ProviderDefaultBehavior,
    pub default_context_window: u64,
    pub context_window_known: bool,
    pub supports_restricted_tool_policy: bool,
    pub supports_tui_hosting: bool,
    pub system_prompt_transport: &'static str,
    pub managed_tmux_backend: bool,
    pub managed_tmux_wrapper_subcommand: Option<&'static str>,
    pub auth: ProviderAuthSpec,
}

/// Preserve the historical first counterpart while deriving every remaining
/// counterpart from the registry. Adding a provider then requires one row, not
/// edits to every existing provider's counterpart list.
const FROZEN_FIRST_COUNTERPART: &[(&str, &str)] = &[
    ("claude", "codex"),
    ("codex", "claude"),
    ("gemini", "codex"),
    ("opencode", "codex"),
    ("qwen", "codex"),
    ("grok", "codex"),
];

pub fn frozen_first_counterpart_id(provider_id: &str) -> Option<&'static str> {
    FROZEN_FIRST_COUNTERPART
        .iter()
        .find(|(id, _)| *id == provider_id)
        .map(|(_, first)| *first)
}

pub fn derived_counterpart_ids(provider_id: &str) -> Vec<&'static str> {
    let first = frozen_first_counterpart_id(provider_id);
    let mut rest: Vec<&'static str> = provider_registry()
        .iter()
        .map(|entry| entry.id)
        .filter(|id| *id != provider_id && Some(*id) != first)
        .collect();
    rest.sort_unstable();
    match first {
        Some(first) => std::iter::once(first).chain(rest).collect(),
        None => rest,
    }
}

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
const GROK_AUTH_PATHS: &[&str] = &["~/.grok/auth.json"];
const GROK_AUTH_ENV: &[&str] = &["XAI_API_KEY"];

const PROVIDER_REGISTRY: &[ProviderRegistryEntry] = &[
    ProviderRegistryEntry {
        id: "claude",
        aliases: &[],
        display_name: "Claude",
        cli_init_label: "claude (Anthropic)",
        channel_suffix: Some("-cc"),
        default_channel_provider: true,
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
        default_context_window: 1_000_000,
        context_window_known: true,
        supports_restricted_tool_policy: true,
        supports_tui_hosting: true,
        system_prompt_transport: "native",
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
        aliases: &[],
        display_name: "Codex",
        cli_init_label: "codex (OpenAI)",
        channel_suffix: Some("-cdx"),
        default_channel_provider: false,
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
        default_context_window: CODEX_FALLBACK_CONTEXT_WINDOW,
        context_window_known: true,
        supports_restricted_tool_policy: true,
        supports_tui_hosting: true,
        system_prompt_transport: "native",
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
        aliases: &[],
        display_name: "Gemini",
        cli_init_label: "gemini (Google)",
        channel_suffix: Some("-gm"),
        default_channel_provider: false,
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
        default_context_window: 1_000_000,
        context_window_known: true,
        supports_restricted_tool_policy: true,
        supports_tui_hosting: false,
        system_prompt_transport: "native",
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
        aliases: &[],
        display_name: "OpenCode",
        cli_init_label: "opencode (OpenCode)",
        channel_suffix: Some("-oc"),
        default_channel_provider: false,
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
        default_context_window: 128_000,
        context_window_known: true,
        supports_restricted_tool_policy: true,
        supports_tui_hosting: false,
        system_prompt_transport: "native",
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
        aliases: &[],
        display_name: "Qwen Code",
        cli_init_label: "qwen (Alibaba)",
        channel_suffix: Some("-qw"),
        default_channel_provider: false,
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
        default_context_window: 128_000,
        context_window_known: true,
        supports_restricted_tool_policy: true,
        supports_tui_hosting: false,
        system_prompt_transport: "native",
        managed_tmux_backend: true,
        managed_tmux_wrapper_subcommand: Some("qwen-tmux-wrapper"),
        auth: ProviderAuthSpec {
            credential_paths: QWEN_AUTH_PATHS,
            env_keys: QWEN_AUTH_ENV,
            auth_check_argv: None,
        },
    },
    ProviderRegistryEntry {
        id: "grok",
        aliases: &[],
        display_name: "Grok",
        cli_init_label: "grok (xAI)",
        channel_suffix: Some("-gx"),
        default_channel_provider: false,
        capabilities: ProviderCapabilities {
            binary_name: "grok",
            supports_structured_output: false,
            supports_resume: true,
            supports_tool_stream: true,
        },
        execution_adapter: ProviderExecutionAdapter::StreamJsonCli(StreamJsonDialectId::Grok),
        compaction_adapter: ProviderCompactionAdapter::StreamJsonDisabled,
        readiness_adapter: ProviderReadinessAdapter::GenericBanner,
        default_behavior: ProviderDefaultBehavior {
            resume_without_reset: true,
            runtime_model: None,
            source_label: "provider default",
        },
        default_context_window: 0,
        context_window_known: false,
        supports_restricted_tool_policy: true,
        supports_tui_hosting: false,
        system_prompt_transport: "prompt",
        managed_tmux_backend: false,
        managed_tmux_wrapper_subcommand: None,
        auth: ProviderAuthSpec {
            credential_paths: GROK_AUTH_PATHS,
            env_keys: GROK_AUTH_ENV,
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

/// Public, non-secret projection of the canonical provider registry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderCatalogEntry {
    pub id: String,
    pub display_name: String,
    pub channel_suffix: Option<String>,
    pub binary_name: String,
    pub execution_surface: String,
    pub supports_resume: bool,
    pub supports_structured_output: bool,
    pub supports_tool_stream: bool,
    pub supports_restricted_tool_policy: bool,
    pub supports_tui_hosting: bool,
    pub system_prompt_transport: String,
    pub context_window_tokens: Option<u64>,
}

impl ProviderCatalogEntry {
    fn from_registry(entry: &ProviderRegistryEntry) -> Self {
        Self {
            id: entry.id.to_string(),
            display_name: entry.display_name.to_string(),
            channel_suffix: entry.channel_suffix.map(str::to_string),
            binary_name: entry.capabilities.binary_name.to_string(),
            execution_surface: entry.execution_adapter.execution_surface().to_string(),
            supports_resume: entry.capabilities.supports_resume,
            supports_structured_output: entry.capabilities.supports_structured_output,
            supports_tool_stream: entry.capabilities.supports_tool_stream,
            supports_restricted_tool_policy: entry.supports_restricted_tool_policy,
            supports_tui_hosting: entry.supports_tui_hosting,
            system_prompt_transport: entry.system_prompt_transport.to_string(),
            context_window_tokens: entry
                .context_window_known
                .then_some(entry.default_context_window),
        }
    }
}

pub fn public_provider_catalog() -> Vec<ProviderCatalogEntry> {
    provider_registry()
        .iter()
        .map(ProviderCatalogEntry::from_registry)
        .collect()
}

pub fn intern_provider_id(raw: &str) -> Option<&'static str> {
    let normalized = raw.trim().to_ascii_lowercase();
    provider_registry()
        .iter()
        .find(|entry| entry.matches_id_or_alias(&normalized))
        .map(|entry| entry.id)
}

impl ProviderRegistryEntry {
    pub fn matches_id_or_alias(&self, normalized: &str) -> bool {
        self.id == normalized || self.aliases.iter().any(|alias| *alias == normalized)
    }

    pub fn kind(&self) -> Option<ProviderKind> {
        Some(match self.id {
            "claude" => ProviderKind::Claude,
            "codex" => ProviderKind::Codex,
            "gemini" => ProviderKind::Gemini,
            "opencode" => ProviderKind::OpenCode,
            "qwen" => ProviderKind::Qwen,
            "grok" => ProviderKind::Grok,
            _ => return None,
        })
    }
}
