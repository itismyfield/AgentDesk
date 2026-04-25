//! Shell command guard for the AgentDesk `!shell` surface (issue #1128).
//!
//! Background: on 2026-04-25 KST a Discord-driven `!shell` invocation ran
//! `grep -rln -i ... /Users/itismyfield/.adk/release/workspaces/agentdesk/`
//! against an entire workspace root including `target/`, `.git/`, and other
//! large build artifacts. The command did not stream incremental output, so
//! the bot's shell tool blocked for 19+ minutes with no visible progress in
//! Discord. Prompt-level "search safety" guidance alone did not prevent the
//! caller from issuing the command.
//!
//! This module enforces a runtime-level guard on every `!shell` command before
//! it is handed off to the platform shell. The guard is intentionally narrow:
//! it detects the specific anti-patterns that produced the 2026-04-25 stall
//! (recursive `grep -r/-R`, broad workspace-root scans, `find /Users` without
//! a name filter) and offers a concrete `rg`-based alternative that respects
//! `.gitignore` and excludes `target/`, `node_modules/`, and `.git/` by
//! default.
//!
//! The guard never executes the command itself; it returns a [`GuardDecision`]
//! describing whether the caller should proceed and, when blocked, the reason
//! plus suggested replacement. Callers are responsible for delivering the
//! reason to the user surface (Discord) and for applying the no-output
//! timeout independently — see [`wait_with_no_output_timeout`] below.

#![allow(dead_code)]

use std::io::Read;
use std::process::Child;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Default no-output timeout applied to long-running shell commands when the
/// caller does not specify a value. Five minutes matches the longest expected
/// "produces no incremental output" command we still consider acceptable
/// (e.g. a slow `cargo check` warm-up). Anything longer is almost certainly
/// stuck and should be killed so Discord regains responsiveness.
pub const DEFAULT_NO_OUTPUT_TIMEOUT: Duration = Duration::from_secs(300);

/// Hard ceiling on total wall-clock runtime, regardless of streaming output.
/// Prevents a slowly-streaming runaway from monopolizing the shell tool.
pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(900);

/// Outcome of inspecting a shell command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Command is safe to execute as-is.
    Allow,
    /// Command matches a known dangerous pattern. The caller must refuse to
    /// execute it and surface `reason` plus `suggestion` to the user.
    Block { reason: String, suggestion: String },
}

impl GuardDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, GuardDecision::Allow)
    }

    pub fn is_block(&self) -> bool {
        matches!(self, GuardDecision::Block { .. })
    }
}

/// Inspect a single shell command string and decide whether to allow it.
///
/// The detector is intentionally lexical — it does not invoke a real shell
/// parser. We only need to catch the high-impact anti-patterns; false
/// negatives are tolerated because the no-output timeout below provides a
/// second line of defense.
pub fn inspect_command(cmd: &str) -> GuardDecision {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return GuardDecision::Allow;
    }

    // Tokenize on whitespace. We do not split on shell metacharacters because
    // we want to scan every subcommand (`a && b`, `a | b`, `a; b`).
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();

    // Scan for `grep -r`, `grep -R`, `grep -rn`, `grep -rln`, etc. We only
    // flag when the recursive flag is present *and* there is no exclude
    // (`--exclude-dir`, `--include`) — agents that already filter are fine.
    if let Some(reason) = detect_recursive_grep(&tokens) {
        return GuardDecision::Block {
            reason,
            suggestion: suggest_rg_replacement(&tokens),
        };
    }

    // `find /Users` or `find /Users/...` without a `-name`/`-iname`/`-path`
    // filter scans the entire home tree.
    if let Some(reason) = detect_find_user_root(&tokens) {
        return GuardDecision::Block {
            reason,
            suggestion: suggest_find_replacement(&tokens),
        };
    }

    // Workspace-root recursive scan: any tool reading the AgentDesk workspace
    // root path with no scoping flag.
    if let Some(reason) = detect_workspace_root_scan(trimmed) {
        return GuardDecision::Block {
            reason,
            suggestion: suggest_rg_replacement(&tokens),
        };
    }

    GuardDecision::Allow
}

/// Detect recursive `grep -r/-R` invocations that lack any exclusion flag.
fn detect_recursive_grep(tokens: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if !is_grep_command(tok) {
            i += 1;
            continue;
        }

        // Walk forward over flags until the next non-flag token or end.
        let mut has_recursive = false;
        let mut has_exclude = false;
        let mut j = i + 1;
        while j < tokens.len() {
            let arg = tokens[j];
            if arg == "--" {
                break;
            }
            if !arg.starts_with('-') {
                break;
            }
            if arg == "--recursive" || arg == "--dereference-recursive" {
                has_recursive = true;
            }
            if arg.starts_with("--exclude") || arg.starts_with("--include") {
                has_exclude = true;
            }
            // Short cluster like `-rln`, `-Rni`, `-r`, `-R`. Single leading
            // `-` only — long options are handled above.
            if let Some(short) = arg.strip_prefix('-') {
                if !short.starts_with('-')
                    && !short.is_empty()
                    && (short.contains('r') || short.contains('R'))
                {
                    has_recursive = true;
                }
            }
            j += 1;
        }
        if has_recursive && !has_exclude {
            return Some(
                "Detected `grep -r/-R` without `--exclude-dir`/`--include`. Recursive grep on \
                 a workspace root traverses `target/`, `.git/`, `node_modules/`, and other \
                 large directories that can take 10+ minutes and block the shell tool."
                    .to_string(),
            );
        }
        i = j.max(i + 1);
    }
    None
}

fn is_grep_command(tok: &str) -> bool {
    let basename = tok.rsplit('/').next().unwrap_or(tok);
    matches!(basename, "grep" | "egrep" | "fgrep" | "rgrep")
}

/// Detect `find /Users[/...]` or `find /` without a name filter.
fn detect_find_user_root(tokens: &[&str]) -> Option<String> {
    for (i, tok) in tokens.iter().enumerate() {
        if !is_find_command(tok) {
            continue;
        }
        let next = tokens.get(i + 1).copied().unwrap_or("");
        let is_dangerous_root = next == "/"
            || next == "/Users"
            || next.starts_with("/Users/")
            || next.starts_with("~/")
            || next == "~";
        if !is_dangerous_root {
            continue;
        }
        // Look ahead for a name-like predicate that scopes the scan.
        let rest = &tokens[i + 1..];
        let has_filter = rest
            .iter()
            .any(|t| matches!(*t, "-name" | "-iname" | "-path" | "-ipath" | "-regex"));
        if !has_filter {
            return Some(format!(
                "Detected `find {next}` without `-name`/`-iname`/`-path` filter. This walks the \
                 entire user home tree (Library, ObsidianVault, .adk, etc.) and can take many \
                 minutes with no streaming output."
            ));
        }
    }
    None
}

fn is_find_command(tok: &str) -> bool {
    let basename = tok.rsplit('/').next().unwrap_or(tok);
    basename == "find"
}

/// Detect commands that explicitly point at the AgentDesk workspace root or
/// runtime root combined with a recursive scan tool.
fn detect_workspace_root_scan(cmd: &str) -> Option<String> {
    let dangerous_roots = [
        "/Users/itismyfield/.adk/release/workspaces/agentdesk/",
        "/Users/itismyfield/.adk/release/workspaces/agentdesk ",
        "~/.adk/release/workspaces/agentdesk/",
        "~/.adk/release/workspaces/agentdesk ",
    ];
    for root in dangerous_roots {
        if cmd.contains(root) {
            let looks_recursive = cmd.contains("grep -r")
                || cmd.contains("grep -R")
                || cmd.contains("grep -rln")
                || cmd.contains("grep -rn")
                || cmd.contains("ls -R")
                || cmd.contains("du -h")
                || cmd.contains("wc -l");
            if looks_recursive {
                return Some(format!(
                    "Detected recursive scan of workspace root `{}`. The workspace contains \
                     `target/` (multi-GB), `.git/`, and worktrees that should not be traversed \
                     by ad-hoc grep/find.",
                    root.trim_end()
                ));
            }
        }
    }
    None
}

/// Suggest a safer `rg`-based replacement that respects `.gitignore` and
/// adds explicit excludes for build artifacts.
pub fn suggest_rg_replacement(tokens: &[&str]) -> String {
    // Heuristic: first non-flag token after `grep` is the pattern, second is
    // the path.
    let mut pattern: Option<String> = None;
    let mut path: Option<String> = None;
    let mut after_grep = false;
    for tok in tokens.iter() {
        if is_grep_command(tok) {
            after_grep = true;
            continue;
        }
        if !after_grep {
            continue;
        }
        if tok.starts_with('-') {
            continue;
        }
        if pattern.is_none() {
            pattern = Some((*tok).to_string());
        } else if path.is_none() {
            path = Some((*tok).to_string());
        }
    }
    let pat = pattern.unwrap_or_else(|| "<pattern>".to_string());
    let p = path.unwrap_or_else(|| ".".to_string());
    format!(
        "Use `rg --hidden -n -S {pat} {p} -g '!target/**' -g '!node_modules/**' -g '!.git/**'` \
         (rg respects .gitignore; pipe through `| head -200` if you only need a sample)."
    )
}

/// Suggest a safer alternative for `find` calls.
pub fn suggest_find_replacement(tokens: &[&str]) -> String {
    let mut path = "<scoped-path>".to_string();
    for (i, tok) in tokens.iter().enumerate() {
        if is_find_command(tok) {
            if let Some(next) = tokens.get(i + 1) {
                path = (*next).to_string();
            }
        }
    }
    format!(
        "Scope the search and add a name filter, e.g. `find {path} -maxdepth 4 -type f -name \
         '<glob>'`, or prefer `rg --files {path} -g '<glob>'` which respects .gitignore."
    )
}

/// Outcome of [`wait_with_no_output_timeout`].
pub struct WaitOutcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub timed_out: Option<TimeoutCause>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutCause {
    NoOutput,
    Total,
}

impl TimeoutCause {
    pub fn as_str(self) -> &'static str {
        match self {
            TimeoutCause::NoOutput => "no-output idle timeout",
            TimeoutCause::Total => "total wall-clock timeout",
        }
    }
}

/// Wait on a child process, killing it if it produces no output for
/// `no_output` or runs longer than `total`. Returns the captured stdout,
/// stderr, exit code, and whether the wait was terminated by a timeout.
///
/// Callers are expected to have configured the child with piped stdout and
/// stderr and to have called
/// [`crate::services::process::configure_child_process_group`] so that the
/// kill on timeout cleans up descendants.
pub fn wait_with_no_output_timeout(
    mut child: Child,
    no_output: Duration,
    total: Duration,
) -> Result<WaitOutcome, String> {
    let pid = child.id();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "child has no stdout pipe".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "child has no stderr pipe".to_string())?;

    let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>();
    let (tx_err, rx_err) = mpsc::channel::<Vec<u8>>();

    let out_handle = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut acc = Vec::with_capacity(4096);
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    let _ = tx_out.send(buf[..n].to_vec());
                }
                Err(_) => break,
            }
        }
        acc
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut acc = Vec::with_capacity(4096);
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    let _ = tx_err.send(buf[..n].to_vec());
                }
                Err(_) => break,
            }
        }
        acc
    });

    let started = Instant::now();
    let mut last_output = Instant::now();
    let mut timed_out: Option<TimeoutCause> = None;

    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }

        if started.elapsed() >= total {
            timed_out = Some(TimeoutCause::Total);
            break;
        }
        if last_output.elapsed() >= no_output {
            timed_out = Some(TimeoutCause::NoOutput);
            break;
        }

        let mut got_any = false;
        while let Ok(_chunk) = rx_out.try_recv() {
            got_any = true;
        }
        while let Ok(_chunk) = rx_err.try_recv() {
            got_any = true;
        }
        if got_any {
            last_output = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if timed_out.is_some() {
        crate::services::process::kill_pid_tree(pid);
    }

    let _ = child.wait();
    let stdout_buf = out_handle.join().unwrap_or_default();
    let stderr_buf = err_handle.join().unwrap_or_default();

    let exit_code = child
        .try_wait()
        .ok()
        .flatten()
        .and_then(|s| s.code())
        .unwrap_or(if timed_out.is_some() { -1 } else { 0 });

    Ok(WaitOutcome {
        stdout: stdout_buf,
        stderr: stderr_buf,
        exit_code,
        timed_out,
    })
}

/// Format a user-visible message for a guarded command that was blocked.
/// Used by the Discord surface so the agent receives an actionable response.
pub fn format_block_message(decision: &GuardDecision) -> Option<String> {
    if let GuardDecision::Block { reason, suggestion } = decision {
        Some(format!(
            "Shell guard blocked this command (issue #1128).\n\n**Reason**\n{reason}\n\n\
             **Safer alternative**\n`{suggestion}`"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_command_is_allowed() {
        assert_eq!(inspect_command(""), GuardDecision::Allow);
        assert_eq!(inspect_command("   "), GuardDecision::Allow);
    }

    #[test]
    fn safe_command_is_allowed() {
        assert!(inspect_command("ls -la").is_allow());
        assert!(inspect_command("git status").is_allow());
        assert!(inspect_command("rg pattern src/").is_allow());
        assert!(inspect_command("echo hello").is_allow());
        assert!(inspect_command("cargo check").is_allow());
    }

    #[test]
    fn grep_recursive_short_flag_is_blocked() {
        let d = inspect_command("grep -r foo /Users/itismyfield");
        assert!(d.is_block(), "expected block, got {d:?}");
        if let GuardDecision::Block { reason, suggestion } = &d {
            assert!(reason.contains("grep -r"));
            assert!(suggestion.contains("rg"));
            assert!(suggestion.contains("!target/**"));
        }
    }

    #[test]
    fn grep_recursive_capital_short_flag_is_blocked() {
        let d = inspect_command("grep -R foo .");
        assert!(d.is_block());
    }

    #[test]
    fn grep_recursive_clustered_flags_is_blocked() {
        let d = inspect_command("grep -rln -i pattern /workspace/");
        assert!(d.is_block());
        let d2 = inspect_command("grep -Rni foo .");
        assert!(d2.is_block());
    }

    #[test]
    fn grep_long_recursive_flag_is_blocked() {
        let d = inspect_command("grep --recursive foo .");
        assert!(d.is_block());
    }

    #[test]
    fn grep_with_exclude_dir_is_allowed() {
        let d = inspect_command("grep -rn --exclude-dir=target foo .");
        assert!(d.is_allow(), "expected allow, got {d:?}");
    }

    #[test]
    fn grep_with_include_glob_is_allowed() {
        let d = inspect_command("grep -rn --include='*.rs' foo .");
        assert!(d.is_allow(), "expected allow, got {d:?}");
    }

    #[test]
    fn non_recursive_grep_is_allowed() {
        assert!(inspect_command("grep foo file.txt").is_allow());
        assert!(inspect_command("grep -i foo file.txt").is_allow());
        assert!(inspect_command("grep -n foo file.txt").is_allow());
    }

    #[test]
    fn find_users_root_without_filter_is_blocked() {
        let d = inspect_command("find /Users/itismyfield");
        assert!(d.is_block());
        if let GuardDecision::Block { reason, suggestion } = &d {
            assert!(reason.contains("/Users"));
            assert!(suggestion.contains("name"));
        }
    }

    #[test]
    fn find_root_without_filter_is_blocked() {
        let d = inspect_command("find /");
        assert!(d.is_block());
    }

    #[test]
    fn find_with_name_filter_is_allowed() {
        assert!(inspect_command("find /Users/itismyfield -name '*.rs'").is_allow());
        assert!(inspect_command("find ~/Documents -iname '*.md'").is_allow());
    }

    #[test]
    fn find_scoped_path_is_allowed() {
        assert!(inspect_command("find ./src").is_allow());
        assert!(inspect_command("find /tmp/build").is_allow());
    }

    #[test]
    fn workspace_root_recursive_scan_is_blocked() {
        // The exact 2026-04-25 reproducer that motivated issue #1128.
        let d = inspect_command(
            "grep -rln -i narration_enabled /Users/itismyfield/.adk/release/workspaces/agentdesk/",
        );
        assert!(d.is_block(), "expected block, got {d:?}");
    }

    #[test]
    fn workspace_root_with_subpath_recursive_is_still_blocked() {
        let d = inspect_command(
            "grep -rn foo /Users/itismyfield/.adk/release/workspaces/agentdesk/src",
        );
        assert!(d.is_block());
    }

    #[test]
    fn rg_suggestion_extracts_pattern_and_path() {
        let d = inspect_command("grep -r needle /haystack");
        if let GuardDecision::Block { suggestion, .. } = d {
            assert!(suggestion.contains("needle"), "suggestion={suggestion}");
            assert!(suggestion.contains("/haystack"));
            assert!(suggestion.contains("!target/**"));
            assert!(suggestion.contains("!node_modules/**"));
            assert!(suggestion.contains("!.git/**"));
        } else {
            panic!("expected block");
        }
    }

    #[test]
    fn format_block_message_shapes_user_output() {
        let d = inspect_command("grep -r foo .");
        let msg = format_block_message(&d).expect("block message");
        assert!(msg.contains("Shell guard"));
        assert!(msg.contains("Reason"));
        assert!(msg.contains("Safer alternative"));
        assert!(msg.contains("#1128"));
    }

    #[test]
    fn format_block_message_returns_none_for_allow() {
        assert!(format_block_message(&GuardDecision::Allow).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn no_output_timeout_kills_idle_child() {
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("bash");
        cmd.args(["-c", "sleep 10"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::services::process::configure_child_process_group(&mut cmd);
        let child = cmd.spawn().expect("spawn");
        let outcome =
            wait_with_no_output_timeout(child, Duration::from_millis(200), Duration::from_secs(30))
                .expect("wait");
        assert_eq!(outcome.timed_out, Some(TimeoutCause::NoOutput));
    }

    #[cfg(unix)]
    #[test]
    fn child_that_completes_quickly_returns_normal_outcome() {
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("bash");
        cmd.args(["-c", "echo hi"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::services::process::configure_child_process_group(&mut cmd);
        let child = cmd.spawn().expect("spawn");
        let outcome =
            wait_with_no_output_timeout(child, Duration::from_secs(5), Duration::from_secs(10))
                .expect("wait");
        assert_eq!(outcome.timed_out, None);
        assert!(String::from_utf8_lossy(&outcome.stdout).contains("hi"));
    }
}
