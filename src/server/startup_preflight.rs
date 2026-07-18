//! Startup checks that must run after runtime dependencies are published and
//! before cluster bootstrap begins.

/// Runs the non-blocking Codex hook trust-hash check during server startup.
///
/// This stage preserves the former `server::run` behavior: the check probes the
/// same resolved Codex executable that session launches use, and any failed
/// check only emits operator diagnostics rather than blocking boot.
pub(crate) fn run() {
    run_with(
        crate::services::codex::resolve_codex_path,
        crate::services::claude_tui::hook_bundle::probe_codex_cli_version,
        crate::services::claude_tui::hook_bundle::run_codex_hook_startup_self_check,
    );
}

fn run_with<Resolve, Probe, Check>(resolve: Resolve, probe: Probe, check: Check)
where
    Resolve: FnOnce() -> Option<String>,
    Probe: FnOnce(&str) -> Option<String>,
    Check: FnOnce(bool, Option<&str>, Option<&str>) -> bool,
{
    let codex_cli_path = resolve();
    let codex_cli_present = codex_cli_path.is_some();
    let codex_cli_version = codex_cli_path.as_deref().and_then(probe);
    let _ = check(
        codex_cli_present,
        codex_cli_version.as_deref(),
        codex_cli_path.as_deref(),
    );
}

// The startup stage is diagnostics-only. Changing it to propagate a `Result`
// becomes a compile error instead of a runtime boot failure.
const _: fn() = run;

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::run_with;

    #[test]
    fn startup_preflight_runs_the_resolved_check_without_propagating_failure() {
        let observed = RefCell::new(None);
        let check_calls = Cell::new(0);

        let () = run_with(
            || Some("/opt/agentdesk/codex".to_string()),
            |path| {
                assert_eq!(path, "/opt/agentdesk/codex");
                Some("codex-cli 9.99.99".to_string())
            },
            |present, version, path| {
                check_calls.set(check_calls.get() + 1);
                *observed.borrow_mut() =
                    Some((present, version.map(str::to_owned), path.map(str::to_owned)));
                false
            },
        );

        assert_eq!(check_calls.get(), 1);
        assert_eq!(
            observed.into_inner(),
            Some((
                true,
                Some("codex-cli 9.99.99".to_string()),
                Some("/opt/agentdesk/codex".to_string()),
            ))
        );
    }
}
