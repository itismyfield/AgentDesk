//! Reviewed name-only cleanup calls, including source-authority entry points.

use super::*;

// The name-only teardown entries run without a host guard. Each production call is
// listed with why it keeps the name; a new call, or a moved caller going back, fails.
pub(super) fn check_reviewed_calls() {
    const SWEEP: &str = "a spawn-time sweep; each site's reason is checked below";
    const BEFORE_WRITER: &str = "behind the turn-key host check, before the writer posts the row";
    const UNKEYED: &str = "a turn with no session key keeps the main teardown";
    const OWNED: &str = "owned by another piece";
    const MISSING: &str = "a Missing row path keeps it name-only";
    const ENTRY: &str = "forwarded by the guarded entry";
    const CLEAR: &str = "a user clear of a channel with no Herdr endpoint";
    const CALLS: &[(&str, Listed)] = &[
        (
            "record_termination_for_tmux",
            &[
                ("src/services/termination_audit.rs", 1, ENTRY),
                ("src/engine/ops/exec_ops.rs", 1, MISSING),
                (
                    "src/services/discord/tmux_watcher/pre_emit_guard.rs",
                    1,
                    MISSING,
                ),
                (
                    "src/services/discord/tmux_watcher/post_stream_exit.rs",
                    1,
                    MISSING,
                ),
                (
                    "src/services/discord/tmux_watcher/terminal_abort_exits.rs",
                    1,
                    MISSING,
                ),
                (
                    "src/services/discord/watchers/lifecycle/restore.rs",
                    1,
                    MISSING,
                ),
                (
                    "src/services/discord/router/message_handler/provider_isolation.rs",
                    1,
                    BEFORE_WRITER,
                ),
                ("src/services/provider_teardown.rs", 1, UNKEYED),
                ("src/services/codex.rs", 1, OWNED),
                (
                    "src/services/provider/cancel_token_cleanup/executor.rs",
                    1,
                    OWNED,
                ),
                ("src/services/claude_tui/host_input.rs", 1, OWNED),
            ],
        ),
        (
            "cleanup_session_temp_files",
            &[
                ("src/services/tmux_common.rs", 1, ENTRY),
                ("src/services/claude/tui_session_launch.rs", 1, SWEEP),
                ("src/services/claude.rs", 1, SWEEP),
                ("src/services/codex.rs", 1, SWEEP),
                ("src/services/qwen/session_lifecycle.rs", 1, SWEEP),
                (
                    "src/services/discord/router/message_handler/provider_isolation.rs",
                    1,
                    BEFORE_WRITER,
                ),
                ("src/services/discord/tmux_reaper.rs", 2, MISSING),
                (
                    "src/services/discord/commands/control/managed_reset.rs",
                    1,
                    MISSING,
                ),
                ("src/services/turn_lifecycle.rs", 1, MISSING),
            ],
        ),
        (
            "cleanup_session_temp_files_under_source_authority",
            &[
                ("src/services/tmux_common.rs", 1, ENTRY),
                ("src/services/codex/absence_launch.rs", 1, SWEEP),
                (
                    "src/services/tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs",
                    1,
                    CLEAR,
                ),
            ],
        ),
        (
            "end_managed_process_session",
            &[
                (
                    "src/services/discord/commands/control/managed_reset.rs",
                    1,
                    ENTRY,
                ),
                (
                    "src/services/discord/commands/control/clear_teardown.rs",
                    1,
                    CLEAR,
                ),
            ],
        ),
        (
            "reset_managed_process_session",
            &[
                ("src/services/discord/commands/control.rs", 0, ENTRY),
                (
                    "src/services/discord/commands/control/provider_reset.rs",
                    1,
                    MISSING,
                ),
                ("src/services/discord/commands/control/native.rs", 1, NATIVE),
                ("src/services/discord/commands/mod.rs", 0, ENTRY),
                ("src/services/discord/health/recovery.rs", 1, MISSING),
                ("src/services/discord/admin_host_guard.rs", 1, MISSING),
            ],
        ),
    ];
    // Keeping Herdr sessions out of these launch paths belongs to launch admission; none
    // of the reasons below claims more than its own function shows.
    const SWEEPS: &[(&str, &str, Sweep)] = &[
        (
            "src/services/claude.rs",
            "execute_streaming_local_tmux",
            Sweep::C1Gated,
        ),
        (
            "src/services/codex.rs",
            "execute_streaming_local_tmux",
            Sweep::C1Gated,
        ),
        (
            "src/services/codex/absence_launch.rs",
            "execute_observed",
            Sweep::VerifiedAbsenceGuarded,
        ),
        (
            "src/services/claude/tui_session_launch.rs",
            "prepare_and_create_claude_tui_session",
            Sweep::ExistingTuiPath,
        ),
        (
            "src/services/qwen/session_lifecycle.rs",
            "execute_streaming_local_tmux",
            Sweep::ExistingQwenPath,
        ),
    ];
    let sources = production_sources();
    let mut violations = listed_call_violations(&sources, CALLS);
    violations.extend(sweep_site_violations(&sources, SWEEPS));
    assert!(
        violations.is_empty(),
        "name-only teardown calls: {violations:?}"
    );
}

/// Why a spawn-time sweep of a session's temp files keeps the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sweep {
    /// A refused C1 host gate earlier in the function returns before the sweep.
    C1Gated,
    /// The existing D/P10 teardown of a TUI launch stays in front of it.
    ExistingTuiPath,
    /// The existing Qwen launch path, which no host gate covers yet.
    ExistingQwenPath,
    /// Herdr is refused at entry and a captured absence is rechecked under the source fence.
    VerifiedAbsenceGuarded,
}

/// Direct temp cleanup calls per (file, function) against the sweep list;
/// each reason must hold in its function: a C1 gate before the call, or no C1 gate at all.
fn sweep_site_violations(
    sources: &BTreeMap<String, String>,
    sweeps: &[(&str, &str, Sweep)],
) -> Vec<String> {
    let call = regex::Regex::new(r"\bcleanup_session_temp_files(?:_under_source_authority)?\s*\(")
        .unwrap();
    let mut found: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    let files: BTreeSet<&str> = sweeps.iter().map(|(file, ..)| *file).collect();
    let mut violations = Vec::new();
    for file in &files {
        let code = code_tokens(&sources[*file]);
        for at in call.find_iter(&code).map(|m| m.start()) {
            let Some(function) = enclosing_fn(&code, at) else {
                violations.push(format!("{file}: sweep outside any function"));
                continue;
            };
            found
                .entry((file.to_string(), function))
                .or_default()
                .push(at);
        }
    }
    for (file, function, why) in sweeps {
        let key = (file.to_string(), function.to_string());
        let Some(calls) = found.remove(&key) else {
            violations.push(format!("{file}#{function}: listed sweep not found"));
            continue;
        };
        let code = code_tokens(&sources[*file]);
        let body = fn_body(&code, &format!("fn {function}("));
        let gated = |at: usize| code[body.start..at].contains("teardown_tmux(");
        let holds = match why {
            Sweep::C1Gated => calls.iter().all(|at| gated(*at)),
            Sweep::ExistingTuiPath => {
                !file.contains("/qwen/") && !code[body.clone()].contains("teardown_tmux(")
            }
            Sweep::ExistingQwenPath => {
                file.contains("/qwen/") && !code[body.clone()].contains("teardown_tmux(")
            }
            Sweep::VerifiedAbsenceGuarded => calls.iter().all(|at| {
                let before: String = code[body.start..*at]
                    .chars()
                    .filter(|c| !c.is_whitespace() && *c != ',')
                    .collect();
                let host = "ifcrate::services::herdr_launch::herdr_configured_for_tui_launch(report_channel_id){returnErr(crate::services::herdr_launch::HERDR_NOT_ADMITTED.to_string());}";
                let fence = "with_tmux_source_authority(tmux_session_name|authority|{";
                let guard = "ifcaptured.is_some_and(|captured|{!observation.is_some_and(|observed|{clear_relaunch::absent_matches_under(authorityobservedcaptured)})}){returnfalse;}";
                before.contains(host)
                    && before
                        .rsplit_once(fence)
                        .is_some_and(|(_, fenced)| fenced.contains(guard))
            }),
        };
        if calls.len() != 1 || !holds {
            violations.push(format!("{file}#{function}: x{} {why:?}", calls.len()));
        }
    }
    violations.extend(
        found
            .into_keys()
            .map(|(file, function)| format!("{file}#{function}: unlisted sweep")),
    );
    violations
}

/// Name of the innermost `fn` whose body holds byte `at` of token-only code.
fn enclosing_fn(code: &str, at: usize) -> Option<String> {
    let signature = regex::Regex::new(r"\bfn\s+(\w+)").unwrap();
    signature
        .captures_iter(code)
        .filter(|found| found.get(0).unwrap().start() < at)
        .filter_map(|found| {
            let open =
                found.get(0).unwrap().end() + code[found.get(0).unwrap().end()..].find('{')?;
            let mut depth = 0usize;
            let close = code[open..].char_indices().find_map(|(offset, ch)| {
                match ch {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(open + offset)
            })?;
            (open < at && at < close).then(|| (open, found[1].to_string()))
        })
        .max_by_key(|(open, _)| *open)
        .map(|(_, name)| name)
}

#[test]
fn authority_cleanup_inventory_follows_aliases_and_function_values() {
    const COMMON: &str = "src/services/tmux_common.rs";
    const OTHER: &str = "src/services/other.rs";
    const REVIEWED: Listed = &[(COMMON, 1, "the reviewed call")];
    for needle in [
        "cleanup_session_temp_files_under_source_authority",
        "cleanup_native_clear_fallback_under_source_authority",
    ] {
        let calls = [(needle, REVIEWED)];
        let base = format!(
            "pub(crate) fn {needle}(a: &Authority, n: &str) {{}}\n\
             fn reviewed(a: &Authority, n: &str) {{ {needle}(a, n); }}\n\
             pub(crate) use self::{needle} as wipe;\n"
        );
        let mut sources = BTreeMap::from([(COMMON.to_string(), base)]);
        assert!(listed_call_violations(&sources, &calls).is_empty());
        sources.insert(
            OTHER.to_string(),
            "fn extra(a: &Authority, n: &str) { crate::services::tmux_common::wipe(a, n); }"
                .to_string(),
        );
        let found = listed_call_violations(&sources, &calls);
        assert!(
            found
                .iter()
                .any(|v| v.contains(&format!("unlisted {needle} x1"))),
            "{needle}: {found:?}"
        );
        sources.insert(
            OTHER.to_string(),
            "fn extra() { let later = crate::services::tmux_common::wipe; }".to_string(),
        );
        let found = listed_call_violations(&sources, &calls);
        assert!(
            found.iter().any(|v| v.contains("taken as a value x1")),
            "{needle}: {found:?}"
        );
    }
}

#[test]
fn authority_sweep_requires_the_host_and_captured_guards() {
    const FILE: &str = "src/services/codex/absence_launch.rs";
    const HOST: &str = "if crate::services::herdr_launch::herdr_configured_for_tui_launch(report_channel_id) { return Err(crate::services::herdr_launch::HERDR_NOT_ADMITTED.to_string()); }";
    const GUARD: &str = "if captured.is_some_and(|captured| { !observation.is_some_and(|observed| { clear_relaunch::absent_matches_under(authority, observed, captured) }) }) { return false; }";
    let source = format!(
        "fn execute_observed() {{ {HOST}\n\
         crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {{\n\
         {GUARD}\n\
         crate::services::tmux_common::cleanup_session_temp_files_under_source_authority(authority, tmux_session_name);\n\
         true }}); }}"
    );
    let sweeps = [(FILE, "execute_observed", Sweep::VerifiedAbsenceGuarded)];
    let scan = |source| sweep_site_violations(&BTreeMap::from([(FILE.into(), source)]), &sweeps);
    assert!(scan(source.clone()).is_empty());
    for missing in [HOST, GUARD] {
        let found = scan(source.replace(missing, ""));
        assert!(
            found.iter().any(|v| v.contains("VerifiedAbsenceGuarded")),
            "removed {missing}: {found:?}"
        );
    }
    let after = source
        .replace(GUARD, "")
        .replace("true });", &format!("{GUARD} true }});"));
    assert!(
        scan(after)
            .iter()
            .any(|v| v.contains("VerifiedAbsenceGuarded"))
    );
}
