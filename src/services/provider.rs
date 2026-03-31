use crate::utils::format::safe_prefix;
use std::process::Command;

/// Tmux session name prefix — always "AgentDesk".
pub const TMUX_SESSION_PREFIX: &str = "AgentDesk";

/// Tmux session name suffix for dev/release isolation.
/// Dev environment (`~/.adk/dev`) appends "-dev"; release has no suffix.
pub fn tmux_env_suffix() -> &'static str {
    use std::sync::OnceLock;
    static SUFFIX: OnceLock<String> = OnceLock::new();
    SUFFIX.get_or_init(|| match std::env::var("AGENTDESK_ROOT_DIR").ok() {
        Some(root) if root.contains(".adk/dev") => "-dev".to_string(),
        _ => String::new(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    Claude,
    Codex,
    Gemini,
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    pub binary_path: Option<String>,
    pub version: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderDefaultBehavior {
    pub resume_without_reset: bool,
    pub runtime_model: Option<&'static str>,
    pub source_label: &'static str,
}

impl ProviderKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Unsupported(s) => s.as_str(),
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini",
            Self::Unsupported(s) => s.as_str(),
        }
    }

    pub fn counterpart(&self) -> Self {
        match self {
            Self::Claude => Self::Codex,
            Self::Codex => Self::Claude,
            Self::Gemini => Self::Codex,
            Self::Unsupported(_) => self.clone(),
        }
    }

    pub fn capabilities(&self) -> Option<ProviderCapabilities> {
        match self {
            Self::Claude => Some(ProviderCapabilities {
                binary_name: "claude",
                supports_structured_output: true,
                supports_resume: true,
                supports_tool_stream: true,
            }),
            Self::Codex => Some(ProviderCapabilities {
                binary_name: "codex",
                supports_structured_output: true,
                supports_resume: true,
                supports_tool_stream: true,
            }),
            Self::Gemini => Some(ProviderCapabilities {
                binary_name: "gemini",
                supports_structured_output: true,
                supports_resume: true,
                supports_tool_stream: true,
            }),
            Self::Unsupported(_) => None,
        }
    }

    /// Provider-specific behavior when AgentDesk clears its explicit model
    /// override and falls through to the provider-managed default path.
    pub fn default_model_behavior(&self) -> ProviderDefaultBehavior {
        match self {
            Self::Claude => ProviderDefaultBehavior {
                resume_without_reset: true,
                runtime_model: Some("default"),
                source_label: "Claude default alias",
            },
            Self::Codex | Self::Gemini | Self::Unsupported(_) => ProviderDefaultBehavior {
                resume_without_reset: true,
                runtime_model: None,
                source_label: "provider default",
            },
        }
    }

    pub fn resolve_runtime_path(&self) -> Option<String> {
        match self {
            Self::Claude => crate::services::claude::resolve_claude_path(),
            Self::Codex => crate::services::codex::resolve_codex_path(),
            Self::Gemini => crate::services::gemini::resolve_gemini_path(),
            Self::Unsupported(_) => None,
        }
    }

    pub fn probe_runtime(&self) -> Option<ProviderRuntimeProbe> {
        let capabilities = self.capabilities()?;
        let binary_path = self.resolve_runtime_path();
        let version = binary_path.as_ref().and_then(|path| {
            let mut command = Command::new(path);
            crate::services::platform::apply_runtime_path(&mut command);
            command
                .arg("--version")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| {
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if stdout.is_empty() {
                        None
                    } else {
                        Some(stdout)
                    }
                })
        });
        Some(ProviderRuntimeProbe {
            provider: self.clone(),
            capabilities,
            binary_path,
            version,
        })
    }

    /// Parse a known provider string. Returns None for unknown providers.
    pub fn from_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    /// Parse a provider string, returning Unsupported for unknown providers.
    pub fn from_str_or_unsupported(raw: &str) -> Self {
        Self::from_str(raw).unwrap_or_else(|| Self::Unsupported(raw.trim().to_string()))
    }

    /// Returns true if this is a known, supported provider.
    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Unsupported(_))
    }

    pub fn is_channel_supported(&self, channel_name: Option<&str>, is_dm: bool) -> bool {
        if is_dm {
            return self.is_supported();
        }

        let Some(channel_name) = channel_name else {
            return matches!(self, Self::Claude);
        };

        if channel_name.ends_with("-cdx") {
            return matches!(self, Self::Codex);
        }

        if channel_name.ends_with("-cc") {
            return matches!(self, Self::Claude);
        }

        if channel_name.ends_with("-gm") {
            return matches!(self, Self::Gemini);
        }

        matches!(self, Self::Claude)
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
    if let Some(rest) = without_suffix.strip_prefix("claude-") {
        return Some((ProviderKind::Claude, rest.to_string()));
    }
    if let Some(rest) = without_suffix.strip_prefix("codex-") {
        return Some((ProviderKind::Codex, rest.to_string()));
    }
    if let Some(rest) = without_suffix.strip_prefix("gemini-") {
        return Some((ProviderKind::Gemini, rest.to_string()));
    }
    Some((ProviderKind::Claude, without_suffix.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{ProviderKind, parse_provider_and_channel_from_tmux_name};
    use crate::dispatch::extract_thread_channel_id;

    #[test]
    fn test_provider_channel_support() {
        assert!(ProviderKind::Claude.is_channel_supported(Some("mac-mini"), false));
        assert!(ProviderKind::Claude.is_channel_supported(Some("cookingheart-dev-cc"), false));
        assert!(!ProviderKind::Claude.is_channel_supported(Some("cookingheart-dev-cdx"), false));
        assert!(ProviderKind::Codex.is_channel_supported(Some("cookingheart-dev-cdx"), false));
        assert!(!ProviderKind::Codex.is_channel_supported(Some("cookingheart-dev-cc"), false));
        assert!(ProviderKind::Codex.is_channel_supported(None, true));
        assert!(ProviderKind::Gemini.is_channel_supported(Some("research-gm"), false));
        assert!(!ProviderKind::Gemini.is_channel_supported(Some("research-cc"), false));
        assert!(ProviderKind::Gemini.is_channel_supported(None, true));
    }

    #[test]
    fn test_unsupported_provider() {
        let p = ProviderKind::from_str_or_unsupported("gpt");
        assert!(!p.is_supported());
        assert_eq!(p.as_str(), "gpt");
        assert_eq!(p.display_name(), "gpt");
        assert!(!p.is_channel_supported(Some("test-cc"), false));
        assert!(!p.is_channel_supported(Some("test"), false));
        assert!(!p.is_channel_supported(None, true));
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
    fn test_provider_from_str_case_insensitive() {
        assert_eq!(ProviderKind::from_str("Claude"), Some(ProviderKind::Claude));
        assert_eq!(ProviderKind::from_str("CLAUDE"), Some(ProviderKind::Claude));
        assert_eq!(ProviderKind::from_str("CODEX"), Some(ProviderKind::Codex));
        assert_eq!(ProviderKind::from_str("Codex"), Some(ProviderKind::Codex));
        assert_eq!(ProviderKind::from_str("Gemini"), Some(ProviderKind::Gemini));
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
    }

    #[test]
    fn test_is_channel_supported_cc_suffix() {
        assert!(ProviderKind::Claude.is_channel_supported(Some("dev-cc"), false));
        assert!(!ProviderKind::Codex.is_channel_supported(Some("dev-cc"), false));
    }

    #[test]
    fn test_is_channel_supported_cdx_suffix() {
        assert!(ProviderKind::Codex.is_channel_supported(Some("dev-cdx"), false));
        assert!(!ProviderKind::Claude.is_channel_supported(Some("dev-cdx"), false));
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
        ] {
            let capabilities = provider.capabilities().expect("supported provider");
            assert!(capabilities.supports_structured_output);
            assert!(capabilities.supports_resume);
            assert!(capabilities.supports_tool_stream);
            assert!(!capabilities.binary_name.is_empty());
        }
    }
}
