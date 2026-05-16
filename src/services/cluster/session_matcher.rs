//! SessionMatcher — pure function `(tmux_session) → Option<MatchedChannel>`.
//!
//! Epic #2285 / E1 (issue #2343). Foundational layer for the unified
//! session-bound watcher refactor. This module is intentionally side-effect
//! free: callers pass in the channel directory and optional filesystem-probe
//! callbacks. No I/O, no global state — everything is reproducible from inputs
//! and trivially unit-testable.
//!
//! ## Public naming contract
//!
//! AgentDesk's deterministic tmux session naming convention is:
//!
//! ```text
//!     AgentDesk-{provider_id}-{sanitized_channel}
//! ```
//!
//! - `provider_id` is the lowercase provider registry id (`claude`, `codex`,
//!   `gemini`, `opencode`, `qwen`).
//! - `sanitized_channel` is the Discord channel name (or stable channel
//!   identifier) with non-alphanumeric / non-`-_` characters replaced by `-`,
//!   then prefix-truncated to 44 bytes. A trailing `-t{thread_id}` suffix is
//!   preserved across truncation so unified-thread guards keep working.
//! - There is currently **no nonce**. Two channels that sanitize+truncate to
//!   the same string would collide — by design, because the channel directory
//!   guarantees uniqueness at the source.
//!
//! Operators can pre-create matching sessions with:
//! `tmux new -s "$(agentdesk show session-name --channel <id>)"` and AgentDesk
//! will adopt them naturally via the upcoming `SessionDiscovery` loop (E2).
//!
//! ## Provider fingerprint
//!
//! Beyond the session name, a matched session must run the *expected provider*
//! inside its tmux pane. `detect_provider_from_pane_command` is the pure
//! helper that maps a pane current-command string (as reported by tmux's
//! `#{pane_current_command}`) to a `ProviderKind`. It uses substring / prefix
//! matching against the provider registry's `binary_name` so Codex CLI version
//! drift (e.g. `codex`, `codex-cli`, `codex_bin_v2`) still maps cleanly.
//!
//! ## What this module does NOT do (deferred to later E-issues)
//!
//! - E2: enumerate tmux sessions / discovery loop.
//! - E3: registry + watcher supervisor.
//! - E4: relay refactor.

use std::collections::BTreeMap;

use crate::services::provider::{
    ProviderKind, TMUX_SESSION_PREFIX, parse_provider_and_channel_from_tmux_name,
    provider_registry, tmux_env_suffix,
};

/// A single channel → (agent_id, provider) binding entry. The matcher only
/// needs this minimal projection from the live AgentChannelBindings table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelBinding {
    pub channel_id: String,
    pub agent_id: String,
    pub provider: ProviderKind,
}

/// In-memory directory of channel bindings. Callers (E2 discovery, the CLI
/// subcommand) build this from the live PG agents table or from yaml config
/// and pass it in. The matcher itself never touches a database.
///
/// `channel_id` here is the **same identifier** that gets fed into
/// `ProviderKind::build_tmux_session_name`, i.e. the Discord channel name or
/// stable channel identifier used at tmux-session-creation time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelDirectory {
    by_channel: BTreeMap<String, Vec<ChannelBinding>>,
}

impl ChannelDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_bindings<I>(bindings: I) -> Self
    where
        I: IntoIterator<Item = ChannelBinding>,
    {
        let mut directory = Self::new();
        for binding in bindings {
            directory.insert(binding);
        }
        directory
    }

    pub fn insert(&mut self, binding: ChannelBinding) {
        self.by_channel
            .entry(binding.channel_id.clone())
            .or_default()
            .push(binding);
    }

    /// All bindings for the given channel id. There can be multiple when one
    /// channel is bound to several providers (claude + codex sibling channels
    /// on the same agent map to *different* channel ids, but a directory may
    /// legitimately hold both lookup keys).
    pub fn bindings_for_channel(&self, channel_id: &str) -> &[ChannelBinding] {
        self.by_channel
            .get(channel_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Pick the binding for a specific provider on a channel, if present.
    pub fn binding_for_channel_provider(
        &self,
        channel_id: &str,
        provider: &ProviderKind,
    ) -> Option<&ChannelBinding> {
        self.bindings_for_channel(channel_id)
            .iter()
            .find(|binding| binding.provider == *provider)
    }

    pub fn is_empty(&self) -> bool {
        self.by_channel.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_channel.values().map(Vec::len).sum()
    }
}

/// Output of a successful match. `expected_session_name` is exactly the input
/// session name when [`match_session`] returns `Some`; we still echo it back so
/// downstream supervisor code can rebuild a `MatchedChannel` from a binding
/// alone (via [`expected_session_name_for`]) without re-deriving anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchedChannel {
    pub channel_id: String,
    pub agent_id: String,
    pub provider: ProviderKind,
    pub expected_session_name: String,
    pub expected_rollout_path: String,
}

/// Reasons a candidate session was rejected. Returned by
/// [`match_session_detailed`] so the upcoming discovery loop / CLI can emit
/// actionable diagnostics rather than a bare `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchRejection {
    /// Session name doesn't start with the AgentDesk- prefix at all.
    NotAgentDeskNamed,
    /// Provider segment present but unknown to the registry.
    UnknownProvider(String),
    /// Provider parsed but no binding exists for that channel id.
    NoChannelBinding {
        channel_id: String,
        provider: ProviderKind,
    },
    /// A binding exists for the channel but for a *different* provider — i.e.
    /// the pane was started against the wrong provider.
    ProviderMismatch {
        channel_id: String,
        expected: ProviderKind,
        actual: ProviderKind,
    },
}

/// Result of a single match attempt — either a successful binding or a
/// machine-readable rejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchOutcome {
    Matched(MatchedChannel),
    Rejected(MatchRejection),
}

/// Pure function: map a tmux session name to a channel binding, if any.
///
/// `None` is returned for any rejection — call [`match_session_detailed`] when
/// you want to know *why*.
pub fn match_session(
    session_name: &str,
    channels: &ChannelDirectory,
) -> Option<MatchedChannel> {
    match match_session_detailed(session_name, channels) {
        MatchOutcome::Matched(matched) => Some(matched),
        MatchOutcome::Rejected(_) => None,
    }
}

/// Pure function with diagnostic detail. See [`match_session`] for the
/// option-shaped convenience wrapper.
pub fn match_session_detailed(
    session_name: &str,
    channels: &ChannelDirectory,
) -> MatchOutcome {
    let Some((provider, channel_id)) = parse_provider_and_channel_from_tmux_name(session_name)
    else {
        // Either no AgentDesk- prefix, or unknown provider segment.
        let prefix = format!("{}-", TMUX_SESSION_PREFIX);
        if let Some(stripped) = session_name.strip_prefix(&prefix) {
            // Stripped successfully but parse failed — provider segment is
            // unknown. Extract it for the rejection variant.
            let provider_segment = stripped.split('-').next().unwrap_or("").to_string();
            return MatchOutcome::Rejected(MatchRejection::UnknownProvider(provider_segment));
        }
        return MatchOutcome::Rejected(MatchRejection::NotAgentDeskNamed);
    };

    let bindings = channels.bindings_for_channel(&channel_id);
    if bindings.is_empty() {
        return MatchOutcome::Rejected(MatchRejection::NoChannelBinding {
            channel_id,
            provider,
        });
    }

    match channels.binding_for_channel_provider(&channel_id, &provider) {
        Some(binding) => MatchOutcome::Matched(MatchedChannel {
            channel_id: binding.channel_id.clone(),
            agent_id: binding.agent_id.clone(),
            provider: binding.provider.clone(),
            expected_session_name: session_name.to_string(),
            expected_rollout_path: expected_rollout_path_for(session_name),
        }),
        None => {
            let actual = bindings[0].provider.clone();
            MatchOutcome::Rejected(MatchRejection::ProviderMismatch {
                channel_id,
                expected: provider,
                actual,
            })
        }
    }
}

/// Reverse function: given (channel_id, provider), produce the expected tmux
/// session name. This is the canonical operator-facing helper backing the
/// `agentdesk show session-name` CLI subcommand.
///
/// `agent_id` is not used by the current naming convention — sessions are
/// identified by `(provider, channel_id)`. It is accepted as a parameter for
/// forward-compatibility (and for symmetry with [`MatchedChannel`]); callers
/// may pass `None` when only the session name is needed.
pub fn expected_session_name_for(
    _agent_id: Option<&str>,
    provider: &ProviderKind,
    channel_id: &str,
) -> String {
    provider.build_tmux_session_name(channel_id)
}

/// The expected rollout / jsonl file path that AgentDesk's session wrapper
/// writes for the given session. Today both Claude and Codex wrappers route
/// their structured stream through the same `session_temp_path(session, "jsonl")`
/// location, so this is provider-independent.
pub fn expected_rollout_path_for(session_name: &str) -> String {
    #[cfg(unix)]
    {
        crate::services::tmux_common::session_temp_path(session_name, "jsonl")
    }
    #[cfg(not(unix))]
    {
        format!(
            "{}/agentdesk-{}.jsonl",
            std::env::temp_dir().display(),
            session_name
        )
    }
}

/// Detect a provider from a tmux pane's current-command string (as reported by
/// `tmux display-message -p '#{pane_current_command}'`).
///
/// Matching is case-insensitive and uses the provider registry's `binary_name`
/// as the seed. We accept:
///
/// - exact match against the binary name (`codex` → Codex),
/// - prefix match with a `-` / `_` / `.` separator (`codex-cli`, `codex_v2`),
/// - substring match anchored at a word boundary (`/path/to/codex` → Codex).
///
/// This is deliberately permissive so that *future* Codex / Claude CLI version
/// drift (renamed shims, vendored binary names) keeps matching without code
/// changes — the registry stays the single source of truth.
pub fn detect_provider_from_pane_command(pane_cmd: &str) -> Option<ProviderKind> {
    let cmd = pane_cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    let lower = cmd.to_ascii_lowercase();

    // Use the leaf basename (after the last '/') to ignore absolute paths.
    let leaf = lower.rsplit('/').next().unwrap_or(lower.as_str());

    for entry in provider_registry() {
        let bin = entry.capabilities.binary_name;
        if leaf == bin {
            return ProviderKind::from_str(entry.id);
        }
        // bin- / bin_ / bin. prefix matches: `codex-cli`, `codex_v2`, `codex.sh`.
        if leaf.starts_with(bin) {
            let next = leaf.as_bytes().get(bin.len()).copied();
            match next {
                None => return ProviderKind::from_str(entry.id),
                Some(b) if b == b'-' || b == b'_' || b == b'.' => {
                    return ProviderKind::from_str(entry.id);
                }
                _ => {}
            }
        }
    }
    None
}

/// Sanity-check helper exposed for the upcoming session discovery loop (E2):
/// returns `true` when `session_name` looks plausibly like an AgentDesk session
/// regardless of whether the directory has a binding for it.
pub fn looks_like_agentdesk_session(session_name: &str) -> bool {
    let prefix = format!("{}-", TMUX_SESSION_PREFIX);
    let suffix = tmux_env_suffix();
    if !session_name.starts_with(&prefix) {
        return false;
    }
    if !suffix.is_empty() && !session_name.ends_with(suffix) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(channel_id: &str, agent_id: &str, provider: ProviderKind) -> ChannelBinding {
        ChannelBinding {
            channel_id: channel_id.to_string(),
            agent_id: agent_id.to_string(),
            provider,
        }
    }

    fn dir_with(bindings: Vec<ChannelBinding>) -> ChannelDirectory {
        ChannelDirectory::from_bindings(bindings)
    }

    #[test]
    fn match_session_happy_claude() {
        let channel = "agent-channel-cc";
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        let directory =
            dir_with(vec![binding(channel, "agent-a3061", ProviderKind::Claude)]);
        let matched = match_session(&session, &directory).expect("should match");
        assert_eq!(matched.channel_id, channel);
        assert_eq!(matched.agent_id, "agent-a3061");
        assert_eq!(matched.provider, ProviderKind::Claude);
        assert_eq!(matched.expected_session_name, session);
        assert!(!matched.expected_rollout_path.is_empty());
        assert!(matched.expected_rollout_path.ends_with(".jsonl"));
    }

    #[test]
    fn match_session_happy_codex() {
        let channel = "dev-cdx";
        let session = ProviderKind::Codex.build_tmux_session_name(channel);
        let directory = dir_with(vec![binding(channel, "td", ProviderKind::Codex)]);
        let matched = match_session(&session, &directory).expect("should match");
        assert_eq!(matched.provider, ProviderKind::Codex);
        assert_eq!(matched.agent_id, "td");
    }

    #[test]
    fn match_session_no_channel_binding() {
        let channel = "ghost-channel";
        let session = ProviderKind::Codex.build_tmux_session_name(channel);
        let directory = ChannelDirectory::new();
        assert!(match_session(&session, &directory).is_none());
        match match_session_detailed(&session, &directory) {
            MatchOutcome::Rejected(MatchRejection::NoChannelBinding {
                channel_id,
                provider,
            }) => {
                assert_eq!(channel_id, channel);
                assert_eq!(provider, ProviderKind::Codex);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn match_session_provider_mismatch() {
        // Channel is bound to Claude but the running session name encodes Codex.
        let channel = "agent-mismatch";
        let session = ProviderKind::Codex.build_tmux_session_name(channel);
        let directory =
            dir_with(vec![binding(channel, "td-alt", ProviderKind::Claude)]);
        assert!(match_session(&session, &directory).is_none());
        match match_session_detailed(&session, &directory) {
            MatchOutcome::Rejected(MatchRejection::ProviderMismatch {
                channel_id,
                expected,
                actual,
            }) => {
                assert_eq!(channel_id, channel);
                assert_eq!(expected, ProviderKind::Codex);
                assert_eq!(actual, ProviderKind::Claude);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn match_session_not_agentdesk_named() {
        let directory = ChannelDirectory::new();
        match match_session_detailed("zellij-foo", &directory) {
            MatchOutcome::Rejected(MatchRejection::NotAgentDeskNamed) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn expected_session_name_reverse_function_is_lossless() {
        // Channel ids that survive sanitize+truncate unchanged must round-trip
        // verbatim through (build → parse).
        for (provider, channel) in [
            (ProviderKind::Claude, "agent-cc"),
            (ProviderKind::Codex, "dev-cdx"),
            (ProviderKind::Gemini, "research-gm"),
            (ProviderKind::OpenCode, "sandbox-oc"),
            (ProviderKind::Qwen, "sandbox-qw"),
        ] {
            let session = expected_session_name_for(None, &provider, channel);
            let (parsed_provider, parsed_channel) =
                parse_provider_and_channel_from_tmux_name(&session).expect("parse");
            assert_eq!(parsed_provider, provider);
            assert_eq!(parsed_channel, channel, "round-trip lost for {channel}");
        }
    }

    #[test]
    fn expected_rollout_path_is_session_scoped() {
        let session_a = ProviderKind::Claude.build_tmux_session_name("chan-a");
        let session_b = ProviderKind::Claude.build_tmux_session_name("chan-b");
        let path_a = expected_rollout_path_for(&session_a);
        let path_b = expected_rollout_path_for(&session_b);
        assert_ne!(path_a, path_b);
        assert!(path_a.ends_with(".jsonl"));
    }

    #[test]
    fn missing_rollout_does_not_break_match() {
        // The matcher reports an *expected* rollout path; it never probes the
        // filesystem. A matched binding with a non-existent rollout still
        // returns `Some(matched)`; the supervisor layer is what decides whether
        // to wait for the file to appear or kill the session.
        let channel = "chan-no-rollout";
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        let directory =
            dir_with(vec![binding(channel, "agent", ProviderKind::Claude)]);
        let matched = match_session(&session, &directory).expect("matches");
        // Expected path is reported even though no jsonl exists on disk.
        assert!(matched.expected_rollout_path.contains(&session));
    }

    #[test]
    fn detect_provider_exact_binary_name() {
        assert_eq!(
            detect_provider_from_pane_command("claude"),
            Some(ProviderKind::Claude)
        );
        assert_eq!(
            detect_provider_from_pane_command("codex"),
            Some(ProviderKind::Codex)
        );
        assert_eq!(
            detect_provider_from_pane_command("gemini"),
            Some(ProviderKind::Gemini)
        );
    }

    #[test]
    fn detect_provider_with_path_prefix() {
        assert_eq!(
            detect_provider_from_pane_command("/usr/local/bin/codex"),
            Some(ProviderKind::Codex)
        );
        assert_eq!(
            detect_provider_from_pane_command("/Users/x/.local/bin/claude"),
            Some(ProviderKind::Claude)
        );
    }

    #[test]
    fn detect_provider_with_version_drift_suffix() {
        // Future Codex CLI shims that AgentDesk doesn't know about yet.
        assert_eq!(
            detect_provider_from_pane_command("codex-cli"),
            Some(ProviderKind::Codex)
        );
        assert_eq!(
            detect_provider_from_pane_command("codex_v2"),
            Some(ProviderKind::Codex)
        );
        assert_eq!(
            detect_provider_from_pane_command("codex.sh"),
            Some(ProviderKind::Codex)
        );
        assert_eq!(
            detect_provider_from_pane_command("claude-1.x"),
            Some(ProviderKind::Claude)
        );
    }

    #[test]
    fn detect_provider_rejects_unknown() {
        assert_eq!(detect_provider_from_pane_command(""), None);
        assert_eq!(detect_provider_from_pane_command("bash"), None);
        assert_eq!(detect_provider_from_pane_command("zsh"), None);
        // Substring matches that aren't word-boundary-anchored must not match.
        assert_eq!(detect_provider_from_pane_command("claudio"), None);
        assert_eq!(detect_provider_from_pane_command("codexterm"), None);
    }

    #[test]
    fn looks_like_agentdesk_session_basic() {
        let s = ProviderKind::Claude.build_tmux_session_name("chan");
        assert!(looks_like_agentdesk_session(&s));
        assert!(!looks_like_agentdesk_session("vim"));
        assert!(!looks_like_agentdesk_session("other-AgentDesk-thing"));
    }

    #[test]
    fn channel_directory_separates_providers() {
        let channel = "shared-channel";
        let directory = dir_with(vec![
            binding(channel, "agent-a", ProviderKind::Claude),
            binding(channel, "agent-b", ProviderKind::Codex),
        ]);
        let claude_session = ProviderKind::Claude.build_tmux_session_name(channel);
        let codex_session = ProviderKind::Codex.build_tmux_session_name(channel);

        let m_claude = match_session(&claude_session, &directory).unwrap();
        assert_eq!(m_claude.agent_id, "agent-a");
        assert_eq!(m_claude.provider, ProviderKind::Claude);

        let m_codex = match_session(&codex_session, &directory).unwrap();
        assert_eq!(m_codex.agent_id, "agent-b");
        assert_eq!(m_codex.provider, ProviderKind::Codex);
    }
}
