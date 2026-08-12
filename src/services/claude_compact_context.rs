//! Launch-bound Claude context-window resolution for auto compaction.
//!
//! AgentDesk records a launch marker for every Claude pane it starts. The
//! auto-compact trigger uses that marker to tell a managed pane (whose launch
//! this process controlled) apart from an unmanaged one, and resolves windows
//! from Claude's own native model table. Completion reads are synchronous and
//! purely local, so the watcher path never performs I/O.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS: u64 = 300_000;
const COMPACT_SAFETY_RESERVE_TOKENS: u64 = 64_000;
const NATIVE_STANDARD_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
const ONE_MILLION_CONTEXT_WINDOW_TOKENS: u64 = 1_000_000;
const CLAUDE_AUTO_COMPACT_MIN_TOKENS: u64 = 100_000;
pub(crate) const CLAUDE_AUTO_COMPACT_MAX_TOKENS: u64 = 1_000_000;
const LAUNCH_PROVENANCE_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_LAUNCH_PROVENANCE: usize = 512;
pub(crate) const CLAUDE_AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

/// tmux session name -> the instant this process launched that Claude pane.
static LAUNCH_PROVENANCE: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompactThreshold {
    pub actual_window_tokens: u64,
    pub effective_tokens: u64,
    pub rearm_floor_tokens: u64,
}

/// Mark this tmux session as an AgentDesk-launched Claude pane before it can
/// receive its first prompt. A same-name relaunch overwrites the old entry.
pub(crate) fn register_launch_provenance(tmux_session_name: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return;
    }
    let mut entries = LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_launch_provenance(&mut entries);
    entries.insert(tmux_session_name.to_string(), Instant::now());
    trim_oldest_launch_provenance(&mut entries);
}

pub(crate) fn clear_launch_provenance_for_tmux(tmux_session_name: &str) {
    LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(tmux_session_name.trim());
}

/// Resolve the trigger window for a live interactive TUI turn.
///
/// A TUI pane can change model mid-session (`/model`), so its *current* model
/// is never authoritative evidence for an exact window: a completion may have
/// canonicalized away an explicit `[1m]` selector, and compacting a 1M session
/// against a 200K threshold would fire far too early. Known launch provenance
/// plus a non-empty model therefore permits only the conservative
/// maximum-window trigger bound. `None` remains reserved for panes without
/// managed launch provenance or usable model evidence.
pub(crate) fn context_window_for_turn(
    tmux_session_name: &str,
    current_model: Option<&str>,
) -> Option<u64> {
    has_launch_provenance(tmux_session_name).then_some(())?;
    current_model.and_then(preserve_model_selector)?;
    Some(CLAUDE_AUTO_COMPACT_MAX_TOKENS)
}

/// Calculate AgentDesk's authoritative absolute trigger. The multiplication is
/// deliberately widened: a malformed large context window or percentage must
/// still clamp safely rather than overflowing before the safety ceiling applies.
pub(crate) fn compact_threshold(
    actual_window_tokens: u64,
    compact_percent: u64,
    lower_bound_tokens: u64,
) -> Option<CompactThreshold> {
    // Zero is the explicit per-provider disable setting. Check it before the
    // lower-bound max so a configured floor cannot accidentally re-enable
    // automatic compaction.
    if compact_percent == 0 {
        return None;
    }
    let ceiling = actual_window_tokens.saturating_sub(COMPACT_SAFETY_RESERVE_TOKENS);
    if ceiling == 0 {
        return None;
    }
    let ratio_tokens = ((u128::from(actual_window_tokens) * u128::from(compact_percent)) / 100)
        .min(u128::from(u64::MAX)) as u64;
    let effective_tokens = ratio_tokens.max(lower_bound_tokens).min(ceiling);
    if effective_tokens == 0 {
        return None;
    }
    let five_percent_tokens =
        ((u128::from(actual_window_tokens) * 5) / 100).min(u128::from(u64::MAX)) as u64;
    Some(CompactThreshold {
        actual_window_tokens,
        effective_tokens,
        rearm_floor_tokens: effective_tokens.saturating_sub(five_percent_tokens),
    })
}

/// Absolute Claude Code launch knob for immutable headless/process launches.
/// Unlike an interactive TUI completion, the launch argv is authoritative for
/// this process and may retain an explicit `[1m]` selector.
pub(crate) fn launch_auto_compact_window(
    launch_model: Option<&str>,
    compact_percent: u64,
    lower_bound_tokens: u64,
) -> Option<u64> {
    if compact_percent == 0 {
        return None;
    }
    let launch_model = launch_model.and_then(preserve_model_selector)?;
    let window = immutable_launch_context_window(&launch_model)?;
    let threshold = compact_threshold(window, compact_percent, lower_bound_tokens)?;
    (CLAUDE_AUTO_COMPACT_MIN_TOKENS..=CLAUDE_AUTO_COMPACT_MAX_TOKENS)
        .contains(&threshold.effective_tokens)
        .then_some(threshold.effective_tokens)
}

/// Extract the effective Claude model selector from a launch argv.
pub(crate) fn claude_model_from_args(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|pair| pair[0] == "--model")
        .map(|pair| pair[1].as_str())
}

/// Record the launch marker before deriving Claude Code's optional absolute
/// auto-compact setting for this process launch.
pub(crate) fn launch_auto_compact_window_for_session(
    launch_key: &str,
    model: Option<&str>,
    compact_percent: Option<u64>,
    compact_lower_bound_tokens: u64,
) -> Option<u64> {
    register_launch_provenance(launch_key);
    compact_percent
        .and_then(|percent| launch_auto_compact_window(model, percent, compact_lower_bound_tokens))
}

/// Render an isolation fence for shell-based launches. An inherited absolute
/// Claude window is never valid unless this launch resolved a fresh value.
pub(crate) fn append_auto_compact_window_shell_env(output: &mut String, window: Option<u64>) {
    output.push_str("unset ");
    output.push_str(CLAUDE_AUTO_COMPACT_WINDOW_ENV);
    output.push('\n');
    if let Some(window) = window {
        output.push_str("export ");
        output.push_str(CLAUDE_AUTO_COMPACT_WINDOW_ENV);
        output.push('=');
        output.push_str(&window.to_string());
        output.push('\n');
    }
}

/// Apply the same isolation fence to direct process launches.
pub(crate) fn apply_auto_compact_window_to_command(command: &mut Command, window: Option<u64>) {
    command.env_remove(CLAUDE_AUTO_COMPACT_WINDOW_ENV);
    if let Some(window) = window {
        command.env(CLAUDE_AUTO_COMPACT_WINDOW_ENV, window.to_string());
    }
}

pub(crate) fn normalize_model_selector(model: &str) -> Option<String> {
    let model = model.trim();
    let model = model.strip_suffix("[1m]").unwrap_or(model).trim_end();
    (!model.is_empty()).then(|| model.to_string())
}

fn preserve_model_selector(model: &str) -> Option<String> {
    let model = model.trim();
    (!model.is_empty()).then(|| model.to_string())
}

fn is_one_m_model_selector(model: &str) -> bool {
    model
        .trim()
        .strip_suffix("[1m]")
        .is_some_and(|base| !base.trim().is_empty())
}

/// Resolve a launch-time model whose argv cannot change underneath this process.
/// A launch resolves against Claude's exact native selector table; unknown
/// selectors remain ambiguous and therefore disable the absolute launch knob.
fn immutable_launch_context_window(launch_model: &str) -> Option<u64> {
    native_context_window(Some(launch_model))
}

/// Classify only exact native selectors known to Claude Code. This deliberately
/// has no prefix/family fallback: an unrecognized future id must take the
/// conservative unknown policy instead of inheriting a stale mapping.
fn native_model_family(model: &str) -> Option<&'static str> {
    let model = normalize_model_selector(model)?;
    match model.as_str() {
        "sonnet"
        | "claude-sonnet-5"
        | "claude-sonnet-4-6"
        | "claude-sonnet-4-5"
        | "claude-sonnet-4-5-20250929"
        | "claude-sonnet-4"
        | "claude-sonnet-4-20250514"
        | "claude-3-7-sonnet"
        | "claude-3-7-sonnet-20250219"
        | "claude-3-5-sonnet"
        | "claude-3-5-sonnet-20241022" => Some("sonnet"),
        "opus"
        | "claude-opus-4-8"
        | "claude-opus-4-7"
        | "claude-opus-4-6"
        | "claude-opus-4-5"
        | "claude-opus-4-5-20251101"
        | "claude-opus-4-1"
        | "claude-opus-4-1-20250805"
        | "claude-opus-4"
        | "claude-opus-4-20250514" => Some("opus"),
        "haiku"
        | "claude-haiku-4-5"
        | "claude-haiku-4-5-20251001"
        | "claude-3-5-haiku"
        | "claude-3-5-haiku-20241022" => Some("haiku"),
        // Opus Plan launches an Opus planning shell but its execution model is
        // Sonnet; classify it accordingly for transcript model reconciliation.
        "opusplan" => Some("sonnet"),
        _ => None,
    }
}

fn native_context_window(model: Option<&str>) -> Option<u64> {
    let model = model?.trim();
    // Validate the stripped base against the exact native table first. The
    // `[1m]` picker suffix changes a known family window; it cannot turn an
    // arbitrary future/typo selector into a supported native model.
    native_model_family(model)?;
    // This suffix is emitted by Claude Code's model picker and means the
    // selected model has explicitly opted into the 1M context beta. It must
    // be checked before `normalize_model_selector` erases the suffix.
    if is_one_m_model_selector(model) {
        return Some(ONE_MILLION_CONTEXT_WINDOW_TOKENS);
    }
    Some(NATIVE_STANDARD_CONTEXT_WINDOW_TOKENS)
}

fn has_launch_provenance(tmux_session_name: &str) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return false;
    }
    let mut entries = LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_launch_provenance(&mut entries);
    entries.contains_key(tmux_session_name)
}

fn purge_launch_provenance(entries: &mut HashMap<String, Instant>) {
    entries.retain(|_, recorded_at| recorded_at.elapsed() <= LAUNCH_PROVENANCE_TTL);
}

fn trim_oldest_launch_provenance(entries: &mut HashMap<String, Instant>) {
    while entries.len() > MAX_LAUNCH_PROVENANCE {
        let Some(key) = entries
            .iter()
            .min_by_key(|(_, recorded_at)| **recorded_at)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        entries.remove(&key);
    }
}

#[cfg(test)]
fn reset_for_test() {
    LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// Launch-provenance fixtures are process-global. Keep every test that touches
/// that map behind this single guard under normal parallel test execution.
#[cfg(test)]
pub(crate) static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn state_test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    reset_for_test();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_table_preserves_safety_ceiling_and_token_hysteresis() {
        let cases = [
            (100_000, 60, 300_000, 36_000),
            (200_000, 60, 300_000, 136_000),
            (372_000, 50, 300_000, 300_000),
            (1_000_000, 50, 300_000, 500_000),
            (200_000, 200, 1, 136_000),
        ];
        for (window, percent, lower, expected) in cases {
            let threshold = compact_threshold(window, percent, lower).unwrap();
            assert_eq!(threshold.effective_tokens, expected);
            assert_eq!(
                threshold.rearm_floor_tokens,
                expected.saturating_sub(window * 5 / 100)
            );
        }
    }

    /// Mutation guard: replacing the unproven trigger bound with any sampled
    /// smaller window breaks at least one comparison below. The maximum-window
    /// threshold must never be earlier than the threshold for a supported window.
    #[test]
    fn compact_threshold_is_monotonic_through_the_maximum_supported_window() {
        for percent in [1, 5, 25, 50, 60, 80, 100, 200] {
            for lower in [1, 100_000, 300_000, 900_000, u64::MAX] {
                let max = compact_threshold(CLAUDE_AUTO_COMPACT_MAX_TOKENS, percent, lower)
                    .expect("maximum-window threshold");
                for window in [64_001, 100_000, 128_000, 200_000, 372_000, 500_000, 999_999] {
                    let current = compact_threshold(window, percent, lower)
                        .expect("sampled supported-window threshold");
                    assert!(
                        current.effective_tokens <= max.effective_tokens,
                        "window={window}, percent={percent}, lower={lower}"
                    );
                }
            }
        }
    }

    #[test]
    fn zero_compact_percent_disables_before_the_lower_bound_can_reenable_it() {
        assert_eq!(compact_threshold(1_000_000, 0, 300_000), None);
        assert_eq!(compact_threshold(100_000, 0, u64::MAX), None);
    }

    #[test]
    fn native_table_handles_exact_aliases_versions_and_one_million_suffixes() {
        for model in [
            "sonnet",
            "opus",
            "haiku",
            "opusplan",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-8",
            "claude-opus-4-5-20251101",
            "claude-haiku-4-5-20251001",
        ] {
            assert_eq!(
                native_context_window(Some(model)),
                Some(NATIVE_STANDARD_CONTEXT_WINDOW_TOKENS),
                "model {model}"
            );
        }
        for model in ["sonnet[1m]", "opus[1m]", "claude-sonnet-4-6[1m]"] {
            assert_eq!(
                native_context_window(Some(model)),
                Some(ONE_MILLION_CONTEXT_WINDOW_TOKENS),
                "model {model}"
            );
        }
        for model in ["future-model", "future-model[1m]", "claude-sonnet-typo[1m]"] {
            assert_eq!(
                native_context_window(Some(model)),
                None,
                "an unknown base must not gain native 1M support through its suffix"
            );
        }
    }

    #[test]
    fn live_scrub_tui_uses_launch_bound_fallback_only_with_a_model() {
        let _guard = state_test_guard();
        register_launch_provenance("tmux-native");
        assert_eq!(context_window_for_turn("tmux-native", None), None);
        assert_eq!(
            context_window_for_turn("tmux-native", Some("sonnet")),
            Some(CLAUDE_AUTO_COMPACT_MAX_TOKENS),
            "a canonicalized base selector must not be falsely proven as a 200K window"
        );
        assert_eq!(
            context_window_for_turn("tmux-native", Some("claude-sonnet-4-6")),
            Some(CLAUDE_AUTO_COMPACT_MAX_TOKENS),
            "launch provenance plus a model permits only the maximum-window trigger bound"
        );
    }

    #[test]
    fn immutable_launch_uses_its_exact_selector_and_disables_unknown_native_models() {
        assert_eq!(
            immutable_launch_context_window("sonnet"),
            Some(NATIVE_STANDARD_CONTEXT_WINDOW_TOKENS)
        );
        assert_eq!(
            immutable_launch_context_window("sonnet[1m]"),
            Some(1_000_000),
            "the immutable argv preserves an explicit [1m] selector"
        );
        assert_eq!(
            immutable_launch_context_window("future-model"),
            None,
            "an unknown native selector must not invent a conservative launch window"
        );
        assert_eq!(
            immutable_launch_context_window("future-model[1m]"),
            None,
            "an unknown native selector must not gain a 1M launch window through its suffix"
        );
    }

    #[test]
    fn launch_zero_percent_disables_before_any_context_resolution() {
        assert_eq!(
            launch_auto_compact_window(
                Some("sonnet"),
                0,
                DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS
            ),
            None
        );
    }

    #[test]
    fn auto_compact_environment_helpers_always_scrub_before_optionally_exporting() {
        use std::ffi::OsStr;

        let mut disabled_shell = String::new();
        append_auto_compact_window_shell_env(&mut disabled_shell, None);
        assert_eq!(disabled_shell, "unset CLAUDE_CODE_AUTO_COMPACT_WINDOW\n");

        let mut enabled_shell = String::new();
        append_auto_compact_window_shell_env(&mut enabled_shell, Some(700_000));
        assert_eq!(
            enabled_shell,
            "unset CLAUDE_CODE_AUTO_COMPACT_WINDOW\nexport CLAUDE_CODE_AUTO_COMPACT_WINDOW=700000\n"
        );

        let mut disabled_command = Command::new("claude");
        disabled_command.env(CLAUDE_AUTO_COMPACT_WINDOW_ENV, "stale");
        apply_auto_compact_window_to_command(&mut disabled_command, None);
        assert!(disabled_command.get_envs().any(|(key, value)| {
            key == OsStr::new(CLAUDE_AUTO_COMPACT_WINDOW_ENV) && value.is_none()
        }));

        let mut enabled_command = Command::new("claude");
        enabled_command.env(CLAUDE_AUTO_COMPACT_WINDOW_ENV, "stale");
        apply_auto_compact_window_to_command(&mut enabled_command, Some(700_000));
        assert!(enabled_command.get_envs().any(|(key, value)| {
            key == OsStr::new(CLAUDE_AUTO_COMPACT_WINDOW_ENV) && value == Some(OsStr::new("700000"))
        }));
    }
}
