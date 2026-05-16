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
/// The directory is keyed by the **expected session name** for each
/// `(provider, channel_id)` pair — i.e. the *exact* string that
/// [`ProviderKind::build_tmux_session_name`] emits. Long channel ids that get
/// sanitized + prefix-truncated to 44 bytes therefore round-trip through
/// matching without the matcher needing the original (untruncated)
/// `channel_id` to be reconstructible from the tmux session name. Each
/// [`ChannelBinding`] still carries the original `channel_id` for downstream
/// consumers.
///
/// Building the directory fails (returns an [`Err`]) if two distinct bindings
/// hash to the same expected session name; that protects E2 discovery from
/// silently adopting the wrong session when long channel ids collide after
/// truncation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelDirectory {
    by_session_name: BTreeMap<String, ChannelBinding>,
}

/// Errors returned when assembling a [`ChannelDirectory`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryBuildError {
    /// Two bindings produced the same expected session name. The second
    /// channel id is shadowed by the first; callers must dedupe upstream
    /// (e.g. by adding a per-channel nonce when collisions are unavoidable).
    SessionNameCollision {
        expected_session_name: String,
        existing_channel_id: String,
        conflicting_channel_id: String,
    },
}

impl std::fmt::Display for DirectoryBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionNameCollision {
                expected_session_name,
                existing_channel_id,
                conflicting_channel_id,
            } => write!(
                f,
                "session-name collision on {expected_session_name}: \
                 channel ids '{existing_channel_id}' and '{conflicting_channel_id}' \
                 sanitize+truncate to the same tmux session name"
            ),
        }
    }
}

impl std::error::Error for DirectoryBuildError {}

impl ChannelDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a directory, rejecting expected-session-name collisions. Use this
    /// whenever you ingest live bindings (PG agents table, yaml config, etc.).
    pub fn try_from_bindings<I>(bindings: I) -> Result<Self, DirectoryBuildError>
    where
        I: IntoIterator<Item = ChannelBinding>,
    {
        let mut directory = Self::new();
        for binding in bindings {
            directory.insert(binding)?;
        }
        Ok(directory)
    }

    /// Lenient builder for tests / non-critical call sites. Later bindings
    /// **overwrite** earlier ones on collision. Prefer [`Self::try_from_bindings`]
    /// in production paths.
    pub fn from_bindings<I>(bindings: I) -> Self
    where
        I: IntoIterator<Item = ChannelBinding>,
    {
        let mut directory = Self::new();
        for binding in bindings {
            let key = expected_session_name_for(None, &binding.provider, &binding.channel_id);
            directory.by_session_name.insert(key, binding);
        }
        directory
    }

    pub fn insert(&mut self, binding: ChannelBinding) -> Result<(), DirectoryBuildError> {
        let key = expected_session_name_for(None, &binding.provider, &binding.channel_id);
        if let Some(existing) = self.by_session_name.get(&key) {
            if existing != &binding {
                // Same expected session name but the binding differs in *any*
                // field (channel_id post-truncation collision, provider, or
                // agent_id). Fail closed — silently overwriting would create
                // cross-agent ownership / dispatch / observability corruption.
                return Err(DirectoryBuildError::SessionNameCollision {
                    expected_session_name: key,
                    existing_channel_id: existing.channel_id.clone(),
                    conflicting_channel_id: binding.channel_id.clone(),
                });
            }
        }
        self.by_session_name.insert(key, binding);
        Ok(())
    }

    /// Look up a binding by the **exact** expected tmux session name. This is
    /// what the matcher uses internally.
    pub fn binding_for_session_name(&self, session_name: &str) -> Option<&ChannelBinding> {
        self.by_session_name.get(session_name)
    }

    pub fn is_empty(&self) -> bool {
        self.by_session_name.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_session_name.len()
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
    /// Provider parsed but no binding exists for that session name.
    NoChannelBinding {
        session_name: String,
        provider: ProviderKind,
    },
    /// Pane current command is unavailable (None / empty / whitespace).
    /// Retryable — the supervisor must re-probe the pane before adopting.
    /// Never produced by [`match_session_offline`]; [`match_session`] requires
    /// a positive provider fingerprint.
    PaneProviderUnknown {
        session_name: String,
        expected: ProviderKind,
    },
    /// The tmux pane is running a different provider than the session name
    /// (and binding) declares — operator-created sessions with the wrong
    /// binary, or stale sessions where the provider has crashed back to a
    /// shell.
    PaneProviderMismatch {
        session_name: String,
        expected: ProviderKind,
        actual_pane_command: String,
        detected: Option<ProviderKind>,
    },
}

/// Result of a single match attempt — either a successful binding or a
/// machine-readable rejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchOutcome {
    Matched(MatchedChannel),
    Rejected(MatchRejection),
}

/// Strict, adoption-grade match. Requires a positive provider fingerprint
/// from the live tmux pane.
///
/// `pane_current_command` is the string AgentDesk reads from
/// `tmux display-message -p '#{pane_current_command}'`. The supervisor layer
/// must have just probed the pane; we treat an absent / blank command as a
/// **retryable rejection** ([`MatchRejection::PaneProviderUnknown`]) rather
/// than silently adopting the session.
///
/// `None` is returned for any rejection — call [`match_session_detailed`] when
/// you want to know *why*.
pub fn match_session(
    session_name: &str,
    pane_current_command: &str,
    channels: &ChannelDirectory,
) -> Option<MatchedChannel> {
    match match_session_detailed(session_name, Some(pane_current_command), channels) {
        MatchOutcome::Matched(matched) => Some(matched),
        MatchOutcome::Rejected(_) => None,
    }
}

/// Offline / audit-only variant. Skips pane-provider verification — useful for
/// operator dashboards or unit-test fixtures that just want to know "would
/// this binding be present, ignoring provider liveness?". The output is a
/// distinct type so it can never be mistaken for the adoption-grade result of
/// [`match_session`].
pub fn match_session_offline(
    session_name: &str,
    channels: &ChannelDirectory,
) -> Option<MatchedChannelAudit> {
    // Same name/binding gating as the strict path, but skip the pane probe.
    let _ = parse_provider_and_channel_from_tmux_name(session_name)?;
    let binding = channels.binding_for_session_name(session_name)?;
    Some(MatchedChannelAudit(MatchedChannel {
        channel_id: binding.channel_id.clone(),
        agent_id: binding.agent_id.clone(),
        provider: binding.provider.clone(),
        expected_session_name: session_name.to_string(),
        expected_rollout_path: expected_rollout_path_for(session_name),
    }))
}

/// Audit-only wrapper around [`MatchedChannel`]. The type system enforces that
/// callers wanting to actually attach a watcher go through [`match_session`]
/// (which verifies the pane provider) instead of the offline path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchedChannelAudit(MatchedChannel);

impl MatchedChannelAudit {
    pub fn binding(&self) -> &MatchedChannel {
        &self.0
    }
}

/// Pure function with diagnostic detail. `pane_current_command = None` is the
/// audit-only path and never produces [`MatchRejection::PaneProviderMismatch`]
/// — it returns [`MatchRejection::PaneProviderUnknown`] instead so the calling
/// API can decide whether unknown is fatal (live discovery) or acceptable
/// (offline audit).
pub fn match_session_detailed(
    session_name: &str,
    pane_current_command: Option<&str>,
    channels: &ChannelDirectory,
) -> MatchOutcome {
    let Some((provider, _channel_segment)) =
        parse_provider_and_channel_from_tmux_name(session_name)
    else {
        let prefix = format!("{}-", TMUX_SESSION_PREFIX);
        if let Some(stripped) = session_name.strip_prefix(&prefix) {
            let provider_segment = stripped.split('-').next().unwrap_or("").to_string();
            return MatchOutcome::Rejected(MatchRejection::UnknownProvider(provider_segment));
        }
        return MatchOutcome::Rejected(MatchRejection::NotAgentDeskNamed);
    };

    let Some(binding) = channels.binding_for_session_name(session_name) else {
        return MatchOutcome::Rejected(MatchRejection::NoChannelBinding {
            session_name: session_name.to_string(),
            provider,
        });
    };

    let pane_cmd_trimmed = pane_current_command.map(str::trim).unwrap_or("");
    if pane_cmd_trimmed.is_empty() {
        return MatchOutcome::Rejected(MatchRejection::PaneProviderUnknown {
            session_name: session_name.to_string(),
            expected: binding.provider.clone(),
        });
    }

    // Managed-wrapper case: pane shows `agentdesk` because we foregrounded a
    // tmux-wrapper subcommand. The provider lives as a child process. Trust
    // the session-name-encoded provider (which by construction equals the
    // binding provider — we already looked the binding up by exact name).
    if !is_agentdesk_managed_wrapper_command(pane_cmd_trimmed) {
        let detected = detect_provider_from_pane_command(pane_cmd_trimmed);
        if detected.as_ref() != Some(&binding.provider) {
            return MatchOutcome::Rejected(MatchRejection::PaneProviderMismatch {
                session_name: session_name.to_string(),
                expected: binding.provider.clone(),
                actual_pane_command: pane_cmd_trimmed.to_string(),
                detected,
            });
        }
    }

    MatchOutcome::Matched(MatchedChannel {
        channel_id: binding.channel_id.clone(),
        agent_id: binding.agent_id.clone(),
        provider: binding.provider.clone(),
        expected_session_name: session_name.to_string(),
        expected_rollout_path: expected_rollout_path_for(session_name),
    })
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
/// - absolute-path basename matching (`/path/to/codex` → Codex).
///
/// This is deliberately permissive so that *future* Codex / Claude CLI version
/// drift (renamed shims, vendored binary names) keeps matching without code
/// changes — the registry stays the single source of truth.
///
/// **Important**: AgentDesk-managed panes foreground the `agentdesk` tmux
/// wrapper (a tmux-wrapper / codex-tmux-wrapper / qwen-tmux-wrapper
/// subcommand), so `#{pane_current_command}` reports `agentdesk` rather than
/// the provider binary even though the provider is alive as a child process.
/// Use [`is_agentdesk_managed_wrapper_command`] to detect that case before
/// rejecting on provider mismatch.
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

/// Returns true when the pane current command looks like the AgentDesk binary
/// itself — i.e. the pane is running one of the managed tmux-wrapper
/// subcommands (`tmux-wrapper`, `codex-tmux-wrapper`, `qwen-tmux-wrapper`,
/// etc.). In that case the pane's *foreground* process is a wrapper and the
/// provider lives as a child; the matcher trusts the session-name-encoded
/// provider for these.
pub fn is_agentdesk_managed_wrapper_command(pane_cmd: &str) -> bool {
    let cmd = pane_cmd.trim();
    if cmd.is_empty() {
        return false;
    }
    let lower = cmd.to_ascii_lowercase();
    let leaf = lower.rsplit('/').next().unwrap_or(lower.as_str());
    leaf == "agentdesk" || leaf.starts_with("agentdesk-") || leaf.starts_with("agentdesk.")
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
        let matched = match_session(&session, "claude", &directory).expect("should match");
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
        let matched = match_session(&session, "codex", &directory).expect("should match");
        assert_eq!(matched.provider, ProviderKind::Codex);
        assert_eq!(matched.agent_id, "td");
    }

    #[test]
    fn match_session_unknown_pane_command_is_retryable_rejection() {
        // Empty / whitespace pane_cmd is a *retryable* rejection (the
        // supervisor must re-probe). It does NOT silently adopt.
        let channel = "agent-warmup";
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        let directory = dir_with(vec![binding(channel, "td", ProviderKind::Claude)]);
        assert!(match_session(&session, "", &directory).is_none());
        assert!(match_session(&session, "   ", &directory).is_none());
        match match_session_detailed(&session, Some("   "), &directory) {
            MatchOutcome::Rejected(MatchRejection::PaneProviderUnknown {
                session_name,
                expected,
            }) => {
                assert_eq!(session_name, session);
                assert_eq!(expected, ProviderKind::Claude);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn match_session_offline_skips_provider_check() {
        // The offline audit API returns a distinct type that the type system
        // prevents callers from mistakenly using for live adoption.
        let channel = "audit-chan";
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        let directory = dir_with(vec![binding(channel, "td", ProviderKind::Claude)]);
        let audit = match_session_offline(&session, &directory).expect("audit match");
        assert_eq!(audit.binding().agent_id, "td");
        assert_eq!(audit.binding().provider, ProviderKind::Claude);
    }

    #[test]
    fn match_session_no_channel_binding() {
        let channel = "ghost-channel";
        let session = ProviderKind::Codex.build_tmux_session_name(channel);
        let directory = ChannelDirectory::new();
        assert!(match_session(&session, "codex", &directory).is_none());
        match match_session_detailed(&session, Some("codex"), &directory) {
            MatchOutcome::Rejected(MatchRejection::NoChannelBinding {
                session_name,
                provider,
            }) => {
                assert_eq!(session_name, session);
                assert_eq!(provider, ProviderKind::Codex);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn match_session_pane_provider_mismatch() {
        // Binding says Claude, but the pane is running bash (operator started
        // tmux with the right name but never launched claude).
        let channel = "agent-pane-mismatch";
        let session = ProviderKind::Claude.build_tmux_session_name(channel);
        let directory =
            dir_with(vec![binding(channel, "td", ProviderKind::Claude)]);
        match match_session_detailed(&session, Some("bash"), &directory) {
            MatchOutcome::Rejected(MatchRejection::PaneProviderMismatch {
                session_name,
                expected,
                detected,
                actual_pane_command,
            }) => {
                assert_eq!(session_name, session);
                assert_eq!(expected, ProviderKind::Claude);
                assert_eq!(detected, None);
                assert_eq!(actual_pane_command, "bash");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        // Wrong provider running in the pane (claude name, codex binary).
        match match_session_detailed(&session, Some("codex"), &directory) {
            MatchOutcome::Rejected(MatchRejection::PaneProviderMismatch {
                detected,
                expected,
                ..
            }) => {
                assert_eq!(expected, ProviderKind::Claude);
                assert_eq!(detected, Some(ProviderKind::Codex));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn match_session_managed_wrapper_pane_is_trusted() {
        // AgentDesk-launched sessions foreground the `agentdesk` wrapper
        // subcommand; `#{pane_current_command}` reports `agentdesk` even
        // though claude/codex is alive as a child process. Matcher must
        // accept this.
        let channel = "agent-managed";
        let session = ProviderKind::Codex.build_tmux_session_name(channel);
        let directory = dir_with(vec![binding(channel, "td", ProviderKind::Codex)]);
        for pane in [
            "agentdesk",
            "/usr/local/bin/agentdesk",
            "agentdesk-helper",
            "agentdesk.real",
        ] {
            let m = match_session(&session, pane, &directory)
                .unwrap_or_else(|| panic!("managed wrapper '{pane}' should match"));
            assert_eq!(m.agent_id, "td");
            assert_eq!(m.provider, ProviderKind::Codex);
        }
        assert!(is_agentdesk_managed_wrapper_command("agentdesk"));
        assert!(!is_agentdesk_managed_wrapper_command("agentdeskish"));
        assert!(!is_agentdesk_managed_wrapper_command(""));
    }

    #[test]
    fn try_from_bindings_rejects_duplicate_agent_id() {
        // Same (channel_id, provider) but different agent_id must fail closed.
        let chan = "agent-dupkey";
        let result = ChannelDirectory::try_from_bindings(vec![
            binding(chan, "agent-a", ProviderKind::Claude),
            binding(chan, "agent-b", ProviderKind::Claude),
        ]);
        match result {
            Err(DirectoryBuildError::SessionNameCollision { .. }) => {}
            other => panic!("expected collision for differing agent_id, got: {other:?}"),
        }
    }

    #[test]
    fn match_session_not_agentdesk_named() {
        let directory = ChannelDirectory::new();
        match match_session_detailed("zellij-foo", None, &directory) {
            MatchOutcome::Rejected(MatchRejection::NotAgentDeskNamed) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn match_session_with_long_truncated_channel_id() {
        // Long channel id that gets truncated; the original (untruncated)
        // channel id must NOT need to be reconstructible from the session
        // name. Directory keys by expected_session_name, not by raw channel id.
        let long_channel = "project-skillmanager-extremely-verbose-channel-cdx";
        let session = ProviderKind::Codex.build_tmux_session_name(long_channel);
        // build truncates; parse can't recover the original.
        let (_, parsed_channel) =
            parse_provider_and_channel_from_tmux_name(&session).expect("parse");
        assert!(
            parsed_channel != long_channel,
            "long channel should be truncated"
        );

        let directory = dir_with(vec![binding(long_channel, "td", ProviderKind::Codex)]);
        let matched = match_session(&session, "codex", &directory).expect("matched");
        // Critically: agent_id and original (untruncated) channel_id survive.
        assert_eq!(matched.agent_id, "td");
        assert_eq!(matched.channel_id, long_channel);
    }

    #[test]
    fn try_from_bindings_rejects_session_name_collision() {
        // Two distinct long channel ids whose sanitize+truncate yields the
        // same tmux session name. The strict builder must reject this. The
        // truncate budget is 44 bytes; both fixtures share the same 44-byte
        // prefix and differ only past the cut.
        let prefix = "agent-foo-extremely-verbose-channel-suffix-A"; // exactly 44 bytes
        assert_eq!(prefix.len(), 44, "fixture sanity: prefix must be 44 bytes");
        let a = format!("{prefix}AAAAA-extra");
        let b = format!("{prefix}BBBBB-different");
        let session_a = ProviderKind::Codex.build_tmux_session_name(&a);
        let session_b = ProviderKind::Codex.build_tmux_session_name(&b);
        assert_eq!(session_a, session_b, "test fixture should collide");
        let a = a.as_str();
        let b = b.as_str();

        let result = ChannelDirectory::try_from_bindings(vec![
            binding(a, "agent-a", ProviderKind::Codex),
            binding(b, "agent-b", ProviderKind::Codex),
        ]);
        match result {
            Err(DirectoryBuildError::SessionNameCollision {
                expected_session_name,
                existing_channel_id,
                conflicting_channel_id,
            }) => {
                assert_eq!(expected_session_name, session_a);
                assert_eq!(existing_channel_id, a);
                assert_eq!(conflicting_channel_id, b);
            }
            other => panic!("expected collision error, got: {other:?}"),
        }
    }

    #[test]
    fn try_from_bindings_accepts_same_channel_idempotent_insert() {
        // Re-inserting the *same* binding is not a collision.
        let chan = "agent-dup";
        let directory = ChannelDirectory::try_from_bindings(vec![
            binding(chan, "td", ProviderKind::Claude),
            binding(chan, "td", ProviderKind::Claude),
        ])
        .expect("idempotent insert");
        assert_eq!(directory.len(), 1);
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
        let matched = match_session(&session, "claude", &directory).expect("matches");
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

        let m_claude = match_session(&claude_session, "claude", &directory).unwrap();
        assert_eq!(m_claude.agent_id, "agent-a");
        assert_eq!(m_claude.provider, ProviderKind::Claude);

        let m_codex = match_session(&codex_session, "codex", &directory).unwrap();
        assert_eq!(m_codex.agent_id, "agent-b");
        assert_eq!(m_codex.provider, ProviderKind::Codex);
    }
}
