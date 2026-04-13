use crate::services::platform::BinaryResolution;
use crate::utils::format::safe_prefix;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

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
}

const CLAUDE_COUNTERPARTS: &[&str] = &["codex", "gemini", "qwen"];
const CODEX_COUNTERPARTS: &[&str] = &["claude", "gemini", "qwen"];
const GEMINI_COUNTERPARTS: &[&str] = &["codex", "claude", "qwen"];
const QWEN_COUNTERPARTS: &[&str] = &["codex", "claude", "gemini"];

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
            runtime_model: Some("default"),
            source_label: "Claude default alias",
        },
        default_context_window: 1_000_000,
        managed_tmux_backend: true,
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
    },
];

pub fn provider_registry() -> &'static [ProviderRegistryEntry] {
    PROVIDER_REGISTRY
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
    allowed_tools: Option<&[String]>,
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

    if let Some(allowed_tools) = allowed_tools.filter(|tools| !tools.is_empty()) {
        sections.push(format!(
            "[Tool Policy]\nIf tools are needed, stay within this allowlist unless the user explicitly asks to change it: {}",
            allowed_tools.join(", ")
        ));
    }

    if sections.is_empty() {
        return prompt.to_string();
    }

    sections.push(format!("[User Request]\n{}", prompt));
    sections.join("\n\n")
}

/// Cooperative cancellation token shared by provider runtimes and Discord orchestration.
pub struct CancelToken {
    pub cancelled: AtomicBool,
    pub child_pid: Mutex<Option<u32>>,
    /// SSH cancel flag — set to true to signal remote execution to close the channel
    #[allow(dead_code)]
    pub ssh_cancel: Mutex<Option<std::sync::Arc<AtomicBool>>>,
    /// tmux session name for cleanup on cancel
    pub tmux_session: Mutex<Option<String>>,
    /// Watchdog deadline as Unix timestamp in milliseconds.
    /// The watchdog fires when `now_ms >= deadline_ms`. Extend by setting a future value.
    /// Maximum absolute cap: initial deadline + MAX_EXTENSION (3 hours).
    pub watchdog_deadline_ms: AtomicI64,
    /// The hard ceiling for watchdog_deadline_ms (initial + 3h). Extensions cannot exceed this.
    pub watchdog_max_deadline_ms: AtomicI64,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            child_pid: Mutex::new(None),
            ssh_cancel: Mutex::new(None),
            tmux_session: Mutex::new(None),
            watchdog_deadline_ms: AtomicI64::new(0),
            watchdog_max_deadline_ms: AtomicI64::new(0),
        }
    }

    /// Cancel and clean up any associated tmux session.
    pub fn cancel_with_tmux_cleanup(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(name) = self.tmux_session.lock().unwrap().take() {
            #[cfg(unix)]
            {
                crate::services::tmux_diagnostics::record_tmux_exit_reason(
                    &name,
                    "explicit cleanup via cancel_with_tmux_cleanup",
                );
                crate::services::platform::tmux::kill_session(&name);
            }
            #[cfg(not(unix))]
            {
                let _ = &name;
            }
        }
    }
}

pub fn cancel_requested(token: Option<&CancelToken>) -> bool {
    token.is_some_and(|token| token.cancelled.load(Ordering::Relaxed))
}

pub fn register_child_pid(token: Option<&CancelToken>, child_pid: u32) {
    if let Some(token) = token {
        *token.child_pid.lock().unwrap() = Some(child_pid);
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
    pub fn tmux(session_name: String) -> Self {
        let name_alive = session_name.clone();
        let name_ready = session_name;
        Self::new(
            move || tmux_session_alive(&name_alive),
            move || tmux_session_ready_for_input(&name_ready),
        )
    }

    #[cfg(not(unix))]
    pub fn tmux(_session_name: String) -> Self {
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

#[cfg(unix)]
pub(crate) fn tmux_capture_indicates_ready_for_input(capture: &str) -> bool {
    capture
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .any(|l| l.contains("Ready for input (type message + Enter)"))
}

#[cfg(unix)]
pub(crate) fn tmux_session_ready_for_input(tmux_session_name: &str) -> bool {
    crate::services::platform::tmux::capture_pane(tmux_session_name, -80)
        .map(|stdout| tmux_capture_indicates_ready_for_input(&stdout))
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(crate) fn tmux_session_ready_for_input(_tmux_session_name: &str) -> bool {
    false
}

const READY_FOR_INPUT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const READY_FOR_INPUT_IDLE_MIN_PROBES: u32 = 3;

#[derive(Debug, Default, Clone)]
pub(crate) struct ReadyForInputIdleTracker {
    first_ready_at: Option<std::time::Instant>,
    consecutive_ready_probes: u32,
}

impl ReadyForInputIdleTracker {
    pub(crate) fn record_output(&mut self) {
        self.reset();
    }

    pub(crate) fn observe_idle(
        &mut self,
        output_ever_grew: bool,
        ready_for_input: bool,
        now: std::time::Instant,
    ) -> bool {
        if !output_ever_grew || !ready_for_input {
            self.reset();
            return false;
        }

        if self.first_ready_at.is_none() {
            self.first_ready_at = Some(now);
        }
        self.consecutive_ready_probes += 1;

        now.duration_since(
            self.first_ready_at
                .expect("first_ready_at set above before elapsed check"),
        ) >= READY_FOR_INPUT_IDLE_TIMEOUT
            && self.consecutive_ready_probes >= READY_FOR_INPUT_IDLE_MIN_PROBES
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
                        && ready_for_input_tracker.observe_idle(
                            output_ever_grew,
                            is_ready_for_input(),
                            Instant::now(),
                        )
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
mod tests {
    use super::{
        CancelToken, FollowupResult, ProviderKind, ReadOutputResult, StreamAttemptFailure,
        StreamAttemptResult, StreamFinalState, cancel_requested, compose_structured_turn_prompt,
        fold_read_output_result, followup_result_from_read_output_result,
        parse_provider_and_channel_from_tmux_name, poll_output_file_until_result,
        provider_registry, register_child_pid, run_retrying_stream_attempts,
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
    fn test_provider_from_str_case_insensitive() {
        assert_eq!(ProviderKind::from_str("Claude"), Some(ProviderKind::Claude));
        assert_eq!(ProviderKind::from_str("CLAUDE"), Some(ProviderKind::Claude));
        assert_eq!(ProviderKind::from_str("CODEX"), Some(ProviderKind::Codex));
        assert_eq!(ProviderKind::from_str("Codex"), Some(ProviderKind::Codex));
        assert_eq!(ProviderKind::from_str("Gemini"), Some(ProviderKind::Gemini));
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

        let name4 = ProviderKind::Qwen.build_tmux_session_name("sandbox-qw");
        assert!(name4.starts_with("AgentDesk-qwen-"));
        assert!(name4.contains("sandbox-qw"));
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

        let session4 = ProviderKind::Qwen.build_tmux_session_name("sandbox-qw");
        let (provider4, parsed_channel4) =
            parse_provider_and_channel_from_tmux_name(&session4).unwrap();
        assert_eq!(provider4, ProviderKind::Qwen);
        assert_eq!(parsed_channel4, "sandbox-qw");
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
                "qwen (Alibaba)"
            ]
        );
        assert_eq!(
            ProviderKind::provider_for_cli_init_index(3),
            Some(ProviderKind::Qwen)
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

        register_child_pid(Some(&token), 4242);
        assert_eq!(*token.child_pid.lock().unwrap(), Some(4242));

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
        assert!(prompt.contains("[Tool Policy]"));
        assert!(prompt.contains("Bash, Read"));
        assert!(prompt.contains("[User Request]\nrole과 mission만 답해줘."));
    }

    #[test]
    fn test_compose_structured_turn_prompt_returns_plain_prompt_without_overrides() {
        let prompt = compose_structured_turn_prompt("just answer", None, None);
        assert_eq!(prompt, "just answer");
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

        assert!(!tracker.observe_idle(false, true, start));

        tracker.record_output();
        assert!(!tracker.observe_idle(true, true, start));
        assert!(!tracker.observe_idle(true, true, start + std::time::Duration::from_secs(10)));
        assert!(tracker.observe_idle(true, true, start + std::time::Duration::from_secs(16)));
    }

    #[test]
    fn test_ready_for_input_idle_tracker_resets_when_output_resumes_or_prompt_disappears() {
        let mut tracker = super::ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        tracker.record_output();
        assert!(!tracker.observe_idle(true, true, start));
        assert!(!tracker.observe_idle(true, false, start + std::time::Duration::from_secs(8)));
        assert!(!tracker.observe_idle(true, true, start + std::time::Duration::from_secs(16)));

        tracker.record_output();
        assert!(!tracker.observe_idle(true, true, start + std::time::Duration::from_secs(17)));
        assert!(!tracker.observe_idle(true, true, start + std::time::Duration::from_secs(25)));
        assert!(tracker.observe_idle(true, true, start + std::time::Duration::from_secs(33)));
    }

    #[cfg(unix)]
    #[test]
    fn test_tmux_capture_indicates_ready_for_input_detects_recent_ready_banner() {
        let capture = "\
build logs\n\
Ready for input (type message + Enter)\n\
> ";
        assert!(super::tmux_capture_indicates_ready_for_input(capture));
    }

    #[cfg(unix)]
    #[test]
    fn test_tmux_capture_indicates_ready_for_input_rejects_non_ready_capture() {
        let capture = "\
build logs\n\
waiting for tool output\n\
still running";
        assert!(!super::tmux_capture_indicates_ready_for_input(capture));
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
