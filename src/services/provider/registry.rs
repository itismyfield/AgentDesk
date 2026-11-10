//! Canonical provider registry rows, aliases, and counterpart derivation.

use crate::services::provider_auth::ProviderAuthSpec;

use super::{ProviderCapabilities, ProviderDefaultBehavior, ProviderKind};
pub use crate::domain::provider::{
    ProviderCatalogEntry, ProviderCompactionAdapter, ProviderExecutionAdapter,
    ProviderReadinessAdapter, StreamJsonDialectId, derived_counterpart_ids,
    public_provider_catalog,
};
use crate::domain::provider::{ProviderDescriptor, provider_catalog};

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

impl ProviderRegistryEntry {
    const fn from_descriptor(value: ProviderDescriptor, auth: ProviderAuthSpec) -> Self {
        Self {
            id: value.id,
            aliases: value.aliases,
            display_name: value.display_name,
            cli_init_label: value.cli_init_label,
            channel_suffix: value.channel_suffix,
            default_channel_provider: value.default_channel_provider,
            capabilities: value.capabilities,
            execution_adapter: value.execution_adapter,
            compaction_adapter: value.compaction_adapter,
            readiness_adapter: value.readiness_adapter,
            default_behavior: value.default_behavior,
            default_context_window: value.default_context_window,
            context_window_known: value.context_window_known,
            supports_restricted_tool_policy: value.supports_restricted_tool_policy,
            supports_tui_hosting: value.supports_tui_hosting,
            system_prompt_transport: value.system_prompt_transport,
            managed_tmux_backend: value.managed_tmux_backend,
            managed_tmux_wrapper_subcommand: value.managed_tmux_wrapper_subcommand,
            auth,
        }
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
    ProviderRegistryEntry::from_descriptor(
        provider_catalog()[0],
        ProviderAuthSpec {
            credential_paths: CLAUDE_AUTH_PATHS,
            env_keys: CLAUDE_AUTH_ENV,
            auth_check_argv: Some(CLAUDE_AUTH_CHECK),
        },
    ),
    ProviderRegistryEntry::from_descriptor(
        provider_catalog()[1],
        ProviderAuthSpec {
            credential_paths: CODEX_AUTH_PATHS,
            env_keys: CODEX_AUTH_ENV,
            auth_check_argv: Some(CODEX_AUTH_CHECK),
        },
    ),
    ProviderRegistryEntry::from_descriptor(
        provider_catalog()[2],
        ProviderAuthSpec {
            credential_paths: GEMINI_AUTH_PATHS,
            env_keys: GEMINI_AUTH_ENV,
            auth_check_argv: Some(GEMINI_AUTH_CHECK),
        },
    ),
    ProviderRegistryEntry::from_descriptor(
        provider_catalog()[3],
        ProviderAuthSpec {
            credential_paths: OPENCODE_AUTH_PATHS,
            env_keys: OPENCODE_AUTH_ENV,
            auth_check_argv: Some(OPENCODE_AUTH_CHECK),
        },
    ),
    ProviderRegistryEntry::from_descriptor(
        provider_catalog()[4],
        ProviderAuthSpec {
            credential_paths: QWEN_AUTH_PATHS,
            env_keys: QWEN_AUTH_ENV,
            auth_check_argv: None,
        },
    ),
    ProviderRegistryEntry::from_descriptor(
        provider_catalog()[5],
        ProviderAuthSpec {
            credential_paths: GROK_AUTH_PATHS,
            env_keys: GROK_AUTH_ENV,
            auth_check_argv: None,
        },
    ),
    ProviderRegistryEntry::from_descriptor(
        provider_catalog()[6],
        ProviderAuthSpec {
            credential_paths: &[],
            env_keys: &[],
            auth_check_argv: None,
        },
    ),
];

pub fn provider_registry() -> &'static [ProviderRegistryEntry] {
    PROVIDER_REGISTRY
}

pub fn supported_provider_ids() -> Vec<&'static str> {
    provider_registry().iter().map(|entry| entry.id).collect()
}

pub fn intern_provider_id(raw: &str) -> Option<&'static str> {
    let normalized = raw.trim().to_ascii_lowercase();
    provider_registry()
        .iter()
        .find(|entry| entry.matches_id_or_alias(&normalized))
        .map(|entry| entry.id)
}

pub(super) fn resolve_kind_runtime_path(kind: &ProviderKind) -> Option<String> {
    match kind {
        ProviderKind::Claude => {
            crate::services::platform::resolve_provider_binary("claude").resolved_path
        }
        ProviderKind::Codex => crate::services::codex::resolve_codex_path(),
        ProviderKind::Gemini => crate::services::gemini::resolve_gemini_path(),
        ProviderKind::OpenCode => crate::services::opencode::resolve_opencode_path(),
        ProviderKind::Qwen => crate::services::qwen::resolve_qwen_path(),
        ProviderKind::Grok => crate::services::stream_json_cli::dialects::grok::resolve_grok_path(),
        ProviderKind::Antigravity => {
            crate::services::stream_json_cli::dialects::agy::resolve_agy_path()
        }
        ProviderKind::Unsupported(_) => None,
    }
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
            "antigravity" => ProviderKind::Antigravity,
            _ => return None,
        })
    }
}
