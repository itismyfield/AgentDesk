use super::ProviderCapabilities;

/// Wire-format dialect used by providers whose CLI emits line-delimited JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamJsonDialectId {
    Grok,
    Agy,
}

impl StreamJsonDialectId {
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::Grok => "grok",
            Self::Agy => "antigravity",
        }
    }
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
            Self::StreamJsonCli(dialect) => dialect.provider_id(),
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
            Self::StreamJsonCli(StreamJsonDialectId::Agy) => ProviderCapabilities {
                binary_name: "agy",
                supports_structured_output: false,
                supports_resume: true,
                supports_tool_stream: false,
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
    StreamJsonDisabled(StreamJsonDialectId),
}

impl ProviderCompactionAdapter {
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::ClaudeEnvironment => "claude",
            Self::CodexCli => "codex",
            Self::GeminiDisabled => "gemini",
            Self::OpenCodeDisabled => "opencode",
            Self::QwenDisabled => "qwen",
            Self::StreamJsonDisabled(dialect) => dialect.provider_id(),
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
    GenericBanner(StreamJsonDialectId),
}

impl ProviderReadinessAdapter {
    pub const fn provider_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Qwen => "qwen",
            Self::GenericBanner(dialect) => dialect.provider_id(),
        }
    }
}
