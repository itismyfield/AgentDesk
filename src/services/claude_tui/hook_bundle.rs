use sha2::{Digest, Sha256};
use std::path::Path;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookBundleConfig {
    pub endpoint: String,
    pub provider: String,
    pub session_id: String,
    pub agentdesk_exe: String,
}

const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "SubagentStop",
];

const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
];

pub fn render_claude_hook_settings(config: &HookBundleConfig) -> Value {
    let mut hooks = serde_json::Map::new();
    for event in CLAUDE_HOOK_EVENTS {
        let hook = json!({
            "type": "command",
            "command": hook_relay_command(config, event),
            "timeout": 5
        });
        let matcher = if matches!(*event, "PreToolUse" | "PostToolUse") {
            json!({
                "matcher": "*",
                "hooks": [hook]
            })
        } else {
            json!({
                "hooks": [hook]
            })
        };
        hooks.insert((*event).to_string(), json!([matcher]));
    }

    json!({
        "hooks": hooks
    })
}

pub fn render_codex_hook_config_override(config: &HookBundleConfig) -> String {
    let mut rendered = String::from("hooks={");
    let mut first_event = true;
    for event in CODEX_HOOK_EVENTS {
        if !first_event {
            rendered.push(',');
        }
        first_event = false;
        rendered.push_str(event);
        rendered.push_str("=[");
        let matchers = codex_event_matchers(event);
        let mut first_group = true;
        for matcher in &matchers {
            if !first_group {
                rendered.push(',');
            }
            first_group = false;
            rendered.push('{');
            if let Some(matcher_value) = matcher {
                rendered.push_str("matcher = ");
                rendered.push_str(&toml_string(matcher_value));
                rendered.push(',');
            }
            rendered.push_str("hooks=[{type=\"command\",command=");
            rendered.push_str(&toml_string(&codex_hook_relay_command(config, event)));
            rendered.push_str(",timeout=5,statusMessage=");
            rendered.push_str(&toml_string(&format!("AgentDesk {event} hook relay")));
            rendered.push_str(",async=false}]}");
        }
        rendered.push(']');
    }
    rendered.push_str(",state={");
    // Codex CLI 0.130 does not expose a usable hook-trust bypass flag. Keep the
    // relay non-persistent by installing it as a session-flag hook override and
    // pairing it with the matching session-flag trust hashes.
    let mut first_state = true;
    for entry in codex_hook_state_entries(config) {
        if !first_state {
            rendered.push(',');
        }
        first_state = false;
        rendered.push_str(&toml_string(&entry.state_key));
        rendered.push_str("={trusted_hash=");
        rendered.push_str(&toml_string(&entry.trusted_hash));
        rendered.push('}');
    }
    rendered.push_str("}}");
    rendered
}

pub fn codex_hook_config_overrides(config: &HookBundleConfig) -> Vec<String> {
    vec![
        "features.hooks=true".to_string(),
        render_codex_hook_config_override(config),
    ]
}

pub fn write_claude_hook_settings(path: &Path, config: &HookBundleConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create hook settings dir {}: {error}", parent.display()))?;
    }
    let rendered = serde_json::to_string_pretty(&render_claude_hook_settings(config))
        .map_err(|error| format!("render hook settings: {error}"))?;
    std::fs::write(path, rendered)
        .map_err(|error| format!("write hook settings {}: {error}", path.display()))
}

fn hook_relay_command(config: &HookBundleConfig, event: &str) -> String {
    [
        shell_quote(&config.agentdesk_exe),
        "claude-hook-relay".to_string(),
        "--endpoint".to_string(),
        shell_quote(&config.endpoint),
        "--provider".to_string(),
        shell_quote(&config.provider),
        "--event".to_string(),
        shell_quote(event),
        "--session-id".to_string(),
        shell_quote(&config.session_id),
    ]
    .join(" ")
}

fn codex_hook_relay_command(config: &HookBundleConfig, event: &str) -> String {
    [
        shell_quote(&config.agentdesk_exe),
        "codex-hook-relay".to_string(),
        "--endpoint".to_string(),
        shell_quote(&config.endpoint),
        "--provider".to_string(),
        shell_quote(&config.provider),
        "--event".to_string(),
        shell_quote(event),
        "--session-id".to_string(),
        shell_quote(&config.session_id),
    ]
    .join(" ")
}

/// Returns the matcher group list for a given Codex hook event.
///
/// Codex CLI 0.130 deserializes the matcher field as a regex (the binary's
/// internally-tagged enum `HookHandlerConfig` declares the matcher as `regex`).
/// That means `"startup|resume|clear"` would match the three SessionStart
/// triggers via regex alternation. To future-proof against any silent
/// transition to literal matching (which would silently disable SessionStart
/// hooks on Codex CLI upgrade — see issue #2210), AgentDesk emits one
/// matcher group per literal trigger for SessionStart. Each literal value is
/// also a valid regex matching only itself, so the contract works under
/// either interpretation.
fn codex_event_matchers(event: &str) -> Vec<Option<&'static str>> {
    match event {
        "SessionStart" => vec![Some("startup"), Some("resume"), Some("clear")],
        "PreToolUse" | "PermissionRequest" | "PostToolUse" => vec![Some("*")],
        _ => vec![None],
    }
}

fn codex_event_key_label(event: &str) -> &'static str {
    match event {
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "Stop" => "stop",
        _ => "unknown",
    }
}

fn codex_session_flag_hook_state_key(event: &str, matcher_index: usize) -> String {
    format!(
        "/config.toml:{}:{matcher_index}:0",
        codex_event_key_label(event)
    )
}

/// One row of the AgentDesk-computed Codex hook trust state.
///
/// Used by `codex_hook_self_check_failures` to verify the AgentDesk-side
/// canonicalization is internally consistent and never produces an empty or
/// placeholder hash on startup (issue #2210 item 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHookStateEntry {
    pub event: &'static str,
    pub matcher: Option<&'static str>,
    pub matcher_index: usize,
    pub state_key: String,
    pub trusted_hash: String,
}

/// Iterates every Codex hook state entry AgentDesk advertises as trusted.
///
/// One entry per (event × matcher group). For SessionStart this expands to
/// three rows — `startup`, `resume`, `clear` — so each trigger has its own
/// trust hash, immune to a hypothetical Codex switch from regex to literal
/// matcher semantics.
pub fn codex_hook_state_entries(config: &HookBundleConfig) -> Vec<CodexHookStateEntry> {
    let mut entries = Vec::new();
    for event in CODEX_HOOK_EVENTS {
        for (matcher_index, matcher) in codex_event_matchers(event).into_iter().enumerate() {
            let state_key = codex_session_flag_hook_state_key(event, matcher_index);
            let trusted_hash = codex_hook_trust_hash_with_matcher(config, event, matcher);
            entries.push(CodexHookStateEntry {
                event,
                matcher,
                matcher_index,
                state_key,
                trusted_hash,
            });
        }
    }
    entries
}

fn codex_hook_trust_hash_with_matcher(
    config: &HookBundleConfig,
    event: &str,
    matcher: Option<&str>,
) -> String {
    let mut handler = serde_json::Map::new();
    handler.insert("async".to_string(), Value::Bool(false));
    handler.insert(
        "command".to_string(),
        Value::String(codex_hook_relay_command(config, event)),
    );
    handler.insert(
        "statusMessage".to_string(),
        Value::String(format!("AgentDesk {event} hook relay")),
    );
    handler.insert("timeout".to_string(), Value::Number(5.into()));
    handler.insert("type".to_string(), Value::String("command".to_string()));

    let mut identity = serde_json::Map::new();
    identity.insert(
        "event_name".to_string(),
        Value::String(codex_event_key_label(event).to_string()),
    );
    if let Some(matcher_value) = matcher {
        identity.insert(
            "matcher".to_string(),
            Value::String(matcher_value.to_string()),
        );
    }
    identity.insert(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(handler)]),
    );

    let canonical = canonical_json(&Value::Object(identity));
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    let hash = hasher.finalize();
    let hex = hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

#[cfg(test)]
fn codex_hook_trust_hash(config: &HookBundleConfig, event: &str) -> String {
    // Test-only helper that uses the first matcher group for the event.
    let matcher = codex_event_matchers(event)
        .into_iter()
        .next()
        .unwrap_or(None);
    codex_hook_trust_hash_with_matcher(config, event, matcher)
}

/// Reasons the AgentDesk-side Codex hook trust hash self-check can fail.
///
/// Emitted by `codex_hook_self_check_failures` at startup so the operator sees
/// a clear breadcrumb if AgentDesk's canonicalization drifts away from a
/// healthy baseline. None of these block startup — Codex CLI is the final
/// arbiter at runtime — but they make the silent-feature-off failure mode in
/// issue #2210 visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexHookSelfCheckFailure {
    EmptyHash {
        event: &'static str,
        matcher: Option<&'static str>,
    },
    DuplicateStateKey {
        state_key: String,
    },
    MissingExpectedEvent {
        event: &'static str,
    },
    UnexpectedMatcherCount {
        event: &'static str,
        expected: usize,
        actual: usize,
    },
}

/// AgentDesk-side ground-truth for the matcher contract per Codex hook event.
///
/// If this disagrees with the rendered bundle, AgentDesk has silently regressed
/// the contract internally — surface a warning at startup.
fn expected_matcher_counts() -> &'static [(&'static str, usize)] {
    &[
        ("SessionStart", 3),
        ("UserPromptSubmit", 1),
        ("Stop", 1),
        ("PreToolUse", 1),
        ("PermissionRequest", 1),
        ("PostToolUse", 1),
        ("PreCompact", 1),
        ("PostCompact", 1),
    ]
}

/// Runs an in-process self-check on the AgentDesk-computed Codex hook trust
/// hashes for a synthetic config. Returns the list of detected failures so
/// callers can decide how loudly to log.
///
/// This does NOT call the Codex CLI — it only verifies that AgentDesk's own
/// canonicalization is structurally sane and matches the matcher contract
/// AgentDesk advertises. A real Codex-CLI cross-check (item 1 of #2210) is
/// tracked separately as #2259 and requires a Codex binary in CI.
// TODO(#2259): add an integration test that exercises a real Codex CLI to
// assert this AgentDesk-computed hash matches Codex's own hash for at least
// one event. Deferred from #2210 because it requires a Codex CLI binary in CI.
pub fn codex_hook_self_check_failures(config: &HookBundleConfig) -> Vec<CodexHookSelfCheckFailure> {
    let mut failures = Vec::new();
    let entries = codex_hook_state_entries(config);

    // 1. No empty / placeholder hashes leak into the trust state.
    for entry in &entries {
        if entry.trusted_hash.trim() == "sha256:" || !entry.trusted_hash.starts_with("sha256:") {
            failures.push(CodexHookSelfCheckFailure::EmptyHash {
                event: entry.event,
                matcher: entry.matcher,
            });
        }
    }

    // 2. State keys are unique across the entire bundle (Codex collapses
    //    duplicates silently, which would silently disable a hook).
    let mut seen = std::collections::HashSet::new();
    for entry in &entries {
        if !seen.insert(entry.state_key.clone()) {
            failures.push(CodexHookSelfCheckFailure::DuplicateStateKey {
                state_key: entry.state_key.clone(),
            });
        }
    }

    // 3. Matcher count per event matches the AgentDesk-side ground truth.
    for (event, expected) in expected_matcher_counts() {
        let actual = entries.iter().filter(|entry| entry.event == *event).count();
        if actual == 0 {
            failures.push(CodexHookSelfCheckFailure::MissingExpectedEvent { event });
        } else if actual != *expected {
            failures.push(CodexHookSelfCheckFailure::UnexpectedMatcherCount {
                event,
                expected: *expected,
                actual,
            });
        }
    }

    failures
}

/// Synthetic config used by the startup self-check. Independent from any real
/// session so the computed hashes are deterministic and reproducible.
fn synthetic_self_check_config() -> HookBundleConfig {
    HookBundleConfig {
        endpoint: "http://127.0.0.1:0".to_string(),
        provider: "codex".to_string(),
        session_id: "self-check-synthetic-session".to_string(),
        agentdesk_exe: "agentdesk".to_string(),
    }
}

/// Codex CLI versions whose hook trust-hash canonicalization AgentDesk has
/// been audited against. When the detected CLI version is in this list, the
/// startup self-check can confidently report the in-process invariants align
/// with the real CLI's contract. For any other version we emit a warning even
/// when the in-process invariants pass, because AgentDesk has no way to know
/// (until #2259 lands) whether Codex changed its canonicalization upstream.
const VERIFIED_CODEX_CLI_VERSIONS: &[&str] = &["codex-cli 0.130.0"];

/// One-shot startup self-check (issue #2210 item 2).
///
/// If the Codex CLI is present on `PATH`, recompute the AgentDesk trust hash
/// bundle for a synthetic event and warn the operator if AgentDesk's own
/// invariants don't hold. The warning includes the offending hash and an
/// actionable hint so an operator can investigate before SessionStart silently
/// stops firing on a Codex CLI bump.
///
/// The self-check is intentionally in-process: it verifies AgentDesk's own
/// canonicalization is structurally sane and pins the matcher contract.
/// It does NOT call the Codex CLI to recompute and compare hashes — that
/// cross-CLI verification is tracked in #2259 (and requires a Codex CLI
/// binary in CI). To surface the cross-CLI drift risk anyway, the check also
/// warns when the detected Codex CLI version isn't in `VERIFIED_CODEX_CLI_VERSIONS`.
///
/// Returns `true` when the in-process check passed AND the Codex CLI version
/// is on the verified allowlist, `false` otherwise (a warning is logged).
pub fn run_codex_hook_startup_self_check(
    codex_cli_present: bool,
    codex_cli_version: Option<&str>,
    codex_cli_path: Option<&str>,
) -> bool {
    if !codex_cli_present {
        tracing::debug!("codex_tui hook self-check skipped: codex CLI not detected on PATH");
        return true;
    }

    let config = synthetic_self_check_config();
    let failures = codex_hook_self_check_failures(&config);
    let entries = codex_hook_state_entries(&config);
    let session_start_hashes: Vec<String> = entries
        .iter()
        .filter(|entry| entry.event == "SessionStart")
        .map(|entry| {
            format!(
                "{}={}",
                entry.matcher.unwrap_or("(none)"),
                entry.trusted_hash
            )
        })
        .collect();
    let version_display = codex_cli_version.unwrap_or("unknown");
    let path_display = codex_cli_path.unwrap_or("unknown");

    if failures.is_empty() {
        let version_verified = codex_cli_version
            .map(|version| VERIFIED_CODEX_CLI_VERSIONS.iter().any(|v| *v == version))
            .unwrap_or(false);
        if version_verified {
            tracing::info!(
                codex_cli_version = version_display,
                codex_cli_path = path_display,
                session_start_trust_hashes = session_start_hashes.join(","),
                "codex_tui hook trust hash self-check passed (in-process invariants ok, \
                 Codex CLI version is on the verified allowlist)"
            );
            return true;
        } else {
            tracing::warn!(
                codex_cli_version = version_display,
                codex_cli_path = path_display,
                verified_versions = VERIFIED_CODEX_CLI_VERSIONS.join(","),
                session_start_trust_hashes = session_start_hashes.join(","),
                "codex_tui hook trust hash self-check PARTIAL: in-process invariants hold, \
                 but the detected Codex CLI version is NOT on the AgentDesk-verified \
                 allowlist. AgentDesk cannot cross-check its computed trust hashes \
                 against this Codex CLI in-process (cross-CLI verification is tracked in \
                 #2259). The feature will silently break on Codex CLI upgrade if Codex \
                 changes its canonicalization. Update VERIFIED_CODEX_CLI_VERSIONS in \
                 src/services/claude_tui/hook_bundle.rs after auditing this version."
            );
            return false;
        }
    }

    for failure in &failures {
        match failure {
            CodexHookSelfCheckFailure::EmptyHash { event, matcher } => {
                tracing::warn!(
                    codex_cli_version = version_display,
                    codex_cli_path = path_display,
                    event = *event,
                    matcher = matcher.unwrap_or("(none)"),
                    "codex_tui hook trust hash self-check FAILED: empty or malformed hash. \
                     Codex CLI is on PATH but AgentDesk computed an unusable trust hash for \
                     this event. The SessionStart / Stop / etc. relay will silently fail \
                     on Codex CLI; the feature will silently break on Codex CLI upgrade. \
                     Investigate src/services/claude_tui/hook_bundle.rs canonicalization."
                );
            }
            CodexHookSelfCheckFailure::DuplicateStateKey { state_key } => {
                tracing::warn!(
                    codex_cli_version = version_display,
                    codex_cli_path = path_display,
                    state_key = state_key.as_str(),
                    "codex_tui hook trust hash self-check FAILED: duplicate state key. \
                     Codex CLI is on PATH but AgentDesk emits two hook entries that collide \
                     on the same trust-state slot; only one will be honored and the rest \
                     will silently fail on Codex CLI. The feature will silently break on \
                     Codex CLI upgrade. Investigate the state-key derivation in \
                     src/services/claude_tui/hook_bundle.rs."
                );
            }
            CodexHookSelfCheckFailure::MissingExpectedEvent { event } => {
                tracing::warn!(
                    codex_cli_version = version_display,
                    codex_cli_path = path_display,
                    event = *event,
                    "codex_tui hook trust hash self-check FAILED: expected event not advertised. \
                     Codex CLI is on PATH but AgentDesk no longer emits this hook event; \
                     the feature will silently break on Codex CLI upgrade. Re-check \
                     CODEX_HOOK_EVENTS in src/services/claude_tui/hook_bundle.rs."
                );
            }
            CodexHookSelfCheckFailure::UnexpectedMatcherCount {
                event,
                expected,
                actual,
            } => {
                tracing::warn!(
                    codex_cli_version = version_display,
                    codex_cli_path = path_display,
                    event = *event,
                    expected = *expected,
                    actual = *actual,
                    "codex_tui hook trust hash self-check FAILED: matcher count drift. \
                     Codex CLI is on PATH but AgentDesk advertises a different number of \
                     matcher groups for this event than the pinned ground truth; \
                     the feature will silently break on Codex CLI upgrade. \
                     Re-check codex_event_matchers in src/services/claude_tui/hook_bundle.rs."
                );
            }
        }
    }

    false
}

/// Per-launch deduplication cache for the Codex hook self-check. Keyed by
/// `(codex_bin_path, codex_cli_version)` so an operator only sees one warning
/// per distinct binary across the lifetime of the dcserver process.
static LAUNCH_SELF_CHECK_SEEN: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(String, String)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Per-launch self-check runner. Called each time the Codex TUI launch path
/// resolves a Codex binary so the operator gets a warning if a session-specific
/// binary (canary/candidate channel, env override, etc.) doesn't match the
/// version AgentDesk has been audited against. Dedupes across the dcserver
/// lifetime so it never spams logs on repeated launches of the same binary.
///
/// `exec_path` is the optional PATH augmentation the TUI launch path injects
/// (e.g. for npm-shim Codex installs that need `node` on PATH to print
/// `--version`). When provided, the version probe runs with that PATH so the
/// probe sees what the launch will see.
///
/// The dedupe cache key uses (canonical_path, version, mtime_nanos) so an
/// in-place upgrade at the same path emits a fresh warning. When version
/// probing fails the mtime still distinguishes binaries, avoiding the
/// "all-failures-collapse-to-unknown" hole flagged in #2210 review.
///
/// Returns `true` if the launch binary passed the check (or was already
/// reported in this process); `false` if a warning was logged.
pub fn run_codex_hook_launch_self_check(codex_bin_path: &str) -> bool {
    run_codex_hook_launch_self_check_with_exec_path(codex_bin_path, None)
}

pub fn run_codex_hook_launch_self_check_with_exec_path(
    codex_bin_path: &str,
    exec_path: Option<&str>,
) -> bool {
    let version = probe_codex_cli_version_with_path(codex_bin_path, exec_path);
    let canonical_path = std::fs::canonicalize(codex_bin_path)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| codex_bin_path.to_string());
    let mtime_nanos = std::fs::metadata(codex_bin_path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|| "<no-mtime>".to_string());
    let version_or_mtime = version
        .clone()
        .unwrap_or_else(|| format!("<unknown>@mtime={mtime_nanos}"));
    let cache_key = (canonical_path.clone(), version_or_mtime);
    {
        let mut seen = LAUNCH_SELF_CHECK_SEEN
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !seen.insert(cache_key.clone()) {
            return true; // already reported in this process for this binary identity
        }
    }
    run_codex_hook_startup_self_check(true, version.as_deref(), Some(&canonical_path))
}

/// Test-only seam to drain the launch self-check dedupe cache so tests
/// exercising the launch path don't fight each other.
#[cfg(test)]
pub fn reset_launch_self_check_cache_for_tests() {
    LAUNCH_SELF_CHECK_SEEN
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// Probes `codex --version` to obtain the CLI version string for the
/// startup self-check. Returns `None` if Codex CLI is absent, unreachable,
/// or doesn't print a parseable version on stdout. The probe is best-effort
/// and never blocks startup for long: a 2-second hard timeout guards against
/// a hanging CLI subprocess.
pub fn probe_codex_cli_version(codex_path: &str) -> Option<String> {
    probe_codex_cli_version_with_path(codex_path, None)
}

/// Same as [`probe_codex_cli_version`] but with an optional PATH override.
/// The launch self-check passes the `BinaryResolution::exec_path` here so
/// npm-shim Codex installs (which need `node` on PATH to print `--version`)
/// produce the same version string the launch will observe.
pub fn probe_codex_cli_version_with_path(
    codex_path: &str,
    exec_path: Option<&str>,
) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut command = Command::new(codex_path);
    command
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    if let Some(exec_path_value) = exec_path {
        let trimmed = exec_path_value.trim();
        if !trimmed.is_empty() {
            command.env("PATH", trimmed);
        }
    }
    let mut child = command.spawn().ok()?;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait().ok()? {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }

    let mut buf = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut buf);
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.lines().next().unwrap_or(trimmed).to_string())
    }
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    sorted.insert(key, canonical_json(value));
                }
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

fn toml_string(value: &str) -> String {
    let escaped = value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect::<String>();
    format!("\"{escaped}\"")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> HookBundleConfig {
        HookBundleConfig {
            endpoint: "http://127.0.0.1:49152".to_string(),
            provider: "claude".to_string(),
            session_id: "01234567-89ab-cdef-0123-456789abcdef".to_string(),
            agentdesk_exe: "/tmp/Agent Desk/agentdesk".to_string(),
        }
    }

    #[test]
    fn hook_settings_render_all_required_claude_events() {
        let settings = render_claude_hook_settings(&sample_config());
        let hooks = settings["hooks"].as_object().unwrap();

        for event in CLAUDE_HOOK_EVENTS {
            assert!(hooks.contains_key(*event), "missing {event}");
        }
        assert_eq!(hooks["PreToolUse"][0]["matcher"], "*");
        assert_eq!(hooks["PostToolUse"][0]["matcher"], "*");
        assert!(hooks["Stop"][0]["matcher"].is_null());
    }

    #[test]
    fn codex_hook_config_override_renders_all_current_events() {
        let mut config = sample_config();
        config.provider = "codex".to_string();
        let settings = render_codex_hook_config_override(&config);

        for event in CODEX_HOOK_EVENTS {
            assert!(settings.contains(&format!("{event}=[")), "missing {event}");
        }
        assert!(settings.starts_with("hooks={"));
        // Issue #2210 item 3: SessionStart MUST emit one matcher group per
        // literal trigger so the contract works whether Codex CLI matches as
        // regex or literal. The legacy regex alternation form is removed.
        assert!(
            settings.contains("matcher = \"startup\""),
            "expected literal startup matcher: {settings}"
        );
        assert!(
            settings.contains("matcher = \"resume\""),
            "expected literal resume matcher: {settings}"
        );
        assert!(
            settings.contains("matcher = \"clear\""),
            "expected literal clear matcher: {settings}"
        );
        assert!(
            !settings.contains("matcher = \"startup|resume|clear\""),
            "regex-alternation matcher must be removed: {settings}"
        );
        assert!(settings.contains("matcher = \"*\""));
        assert!(settings.contains("codex-hook-relay"));
        assert!(settings.contains("--provider codex"));
        assert!(settings.contains("\"/config.toml:stop:0:0\"={trusted_hash=\"sha256:"));
    }

    #[test]
    fn codex_session_start_emits_three_separate_matcher_groups() {
        // Issue #2210 item 3: pin the matcher contract.
        // SessionStart must expose three distinct hook entries (one per literal
        // trigger). Each gets its own trusted_hash slot keyed by matcher index.
        let mut config = sample_config();
        config.provider = "codex".to_string();
        let settings = render_codex_hook_config_override(&config);

        let session_start_block = settings
            .split("SessionStart=[")
            .nth(1)
            .expect("SessionStart block present")
            .split("],")
            .next()
            .expect("SessionStart block delimited");
        let matcher_groups = session_start_block.matches("matcher = ").count();
        assert_eq!(
            matcher_groups, 3,
            "SessionStart must have three matcher groups, got {matcher_groups} in: \
             {session_start_block}"
        );

        // Each matcher group has its own state slot, indexed 0..=2.
        for matcher_index in 0..3 {
            let needle = format!(
                "\"/config.toml:session_start:{matcher_index}:0\"={{trusted_hash=\"sha256:"
            );
            assert!(
                settings.contains(&needle),
                "missing state slot for matcher_index={matcher_index}: {settings}"
            );
        }
    }

    #[test]
    fn codex_hook_state_entries_are_unique() {
        let mut config = sample_config();
        config.provider = "codex".to_string();
        let entries = codex_hook_state_entries(&config);

        // SessionStart contributes 3 entries; the other 7 events contribute 1 each.
        assert_eq!(entries.len(), CODEX_HOOK_EVENTS.len() + 2);

        let mut state_keys = std::collections::HashSet::new();
        for entry in &entries {
            assert!(
                state_keys.insert(entry.state_key.clone()),
                "duplicate state key: {}",
                entry.state_key
            );
            assert!(entry.trusted_hash.starts_with("sha256:"));
            assert!(entry.trusted_hash.len() > "sha256:".len());
        }
    }

    #[test]
    fn codex_hook_self_check_passes_for_synthetic_config() {
        // Item 2: in-process invariants hold for the synthetic startup config.
        let failures = codex_hook_self_check_failures(&synthetic_self_check_config());
        assert!(
            failures.is_empty(),
            "self-check unexpectedly failed: {failures:?}"
        );
    }

    #[test]
    fn run_codex_hook_startup_self_check_skips_when_codex_absent() {
        // No Codex CLI on PATH → no warning, returns true (no-op).
        assert!(run_codex_hook_startup_self_check(false, None, None));
    }

    #[test]
    fn run_codex_hook_startup_self_check_passes_on_verified_version() {
        // In-process invariants hold + verified Codex CLI version → returns true.
        assert!(run_codex_hook_startup_self_check(
            true,
            Some("codex-cli 0.130.0"),
            Some("/opt/homebrew/bin/codex"),
        ));
    }

    #[test]
    fn run_codex_hook_startup_self_check_warns_on_unverified_version() {
        // In-process invariants hold but Codex CLI version is unknown to
        // AgentDesk → still emits a PARTIAL warning (cross-CLI drift risk)
        // and returns false so callers can surface it elsewhere.
        let pass =
            run_codex_hook_startup_self_check(true, Some("codex-cli 9.99.99"), Some("/tmp/codex"));
        assert!(!pass);
    }

    #[test]
    fn run_codex_hook_launch_self_check_dedupes_repeated_calls() {
        // Per-launch check warns once per (canonical_path, version-or-mtime)
        // combo and returns true on subsequent invocations for the same binary
        // identity.
        reset_launch_self_check_cache_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let fake_path = dir.path().join("codex-stub-fake-for-test");
        std::fs::write(&fake_path, b"#!/bin/sh\nexit 0\n").unwrap();
        let path_str = fake_path.to_string_lossy().into_owned();
        // First call: probe fails (binary not executable for --version), so the
        // cache key uses the mtime fallback. Second call for the same file
        // hits the cache.
        let _first = run_codex_hook_launch_self_check(&path_str);
        let second = run_codex_hook_launch_self_check(&path_str);
        assert!(second, "second call for same binary must be deduped");
        reset_launch_self_check_cache_for_tests();
    }

    #[test]
    fn run_codex_hook_launch_self_check_redetects_after_in_place_upgrade() {
        // Same path, different mtime → different cache key → fresh warning.
        reset_launch_self_check_cache_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let fake_path = dir.path().join("codex-stub-upgradable");
        std::fs::write(&fake_path, b"#!/bin/sh\nexit 0\n").unwrap();
        let path_str = fake_path.to_string_lossy().into_owned();
        let _first = run_codex_hook_launch_self_check(&path_str);
        // Force the mtime forward by rewriting + sleeping briefly.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&fake_path, b"#!/bin/sh\nexit 1\n").unwrap();
        let cache_before = LAUNCH_SELF_CHECK_SEEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        let _second = run_codex_hook_launch_self_check(&path_str);
        let cache_after = LAUNCH_SELF_CHECK_SEEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        assert!(
            cache_after > cache_before,
            "in-place upgrade should produce a distinct cache entry: before={cache_before}, after={cache_after}"
        );
        reset_launch_self_check_cache_for_tests();
    }

    #[test]
    fn run_codex_hook_startup_self_check_warns_when_version_unknown() {
        // Codex CLI on PATH but version probe failed → still warn (don't claim
        // success on a totally unknown CLI).
        let pass = run_codex_hook_startup_self_check(true, None, Some("/tmp/codex"));
        assert!(!pass);
    }

    #[test]
    fn codex_hook_config_overrides_enable_and_trust_hooks_for_session() {
        let mut config = sample_config();
        config.provider = "codex".to_string();
        let overrides = codex_hook_config_overrides(&config);

        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides[0], "features.hooks=true");
        assert!(overrides[1].starts_with("hooks={"));
        // SessionStart now has three matcher slots; each must be advertised.
        assert!(overrides[1].contains("\"/config.toml:session_start:0:0\"={trusted_hash="));
        assert!(overrides[1].contains("\"/config.toml:session_start:1:0\"={trusted_hash="));
        assert!(overrides[1].contains("\"/config.toml:session_start:2:0\"={trusted_hash="));
    }

    #[test]
    fn hook_command_shell_quotes_executable_with_spaces() {
        let settings = render_claude_hook_settings(&sample_config());
        let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();

        assert!(command.starts_with("'/tmp/Agent Desk/agentdesk' claude-hook-relay"));
        assert!(command.contains("--event Stop"));
        assert!(command.contains("--session-id 01234567-89ab-cdef-0123-456789abcdef"));
    }

    #[test]
    fn write_hook_settings_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");

        write_claude_hook_settings(&path, &sample_config()).unwrap();

        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains("claude-hook-relay"));
        assert!(raw.contains("SessionStart"));
    }

    #[test]
    fn codex_hook_trust_hash_changes_when_command_identity_changes() {
        let mut config = sample_config();
        config.provider = "codex".to_string();
        let first = codex_hook_trust_hash(&config, "Stop");

        config.session_id.push_str("-new");
        let second = codex_hook_trust_hash(&config, "Stop");

        assert_ne!(first, second);
        assert!(first.starts_with("sha256:"));
        assert!(second.starts_with("sha256:"));
    }
}
