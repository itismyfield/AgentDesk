//! Startup checks that must run after runtime dependencies are published and
//! before cluster bootstrap begins.

/// Runs the non-blocking Codex hook trust-hash check during server startup.
///
/// This stage preserves the former `server::run` behavior: the check probes the
/// same resolved Codex executable that session launches use, and any failed
/// check only emits operator diagnostics rather than blocking boot.
pub(crate) fn run() {
    let codex_cli_path = crate::services::codex::resolve_codex_path();
    let codex_cli_present = codex_cli_path.is_some();
    let codex_cli_version = codex_cli_path
        .as_deref()
        .and_then(crate::services::claude_tui::hook_bundle::probe_codex_cli_version);
    let _ = crate::services::claude_tui::hook_bundle::run_codex_hook_startup_self_check(
        codex_cli_present,
        codex_cli_version.as_deref(),
        codex_cli_path.as_deref(),
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn startup_preflight_keeps_the_check_non_blocking_contract() {
        let source = include_str!("startup_preflight.rs");

        assert!(source.contains(
            "let _ = crate::services::claude_tui::hook_bundle::run_codex_hook_startup_self_check"
        ));
        assert!(!source.contains("?;"));
    }
}
