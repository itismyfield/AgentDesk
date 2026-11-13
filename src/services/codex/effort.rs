//! Resolve launch effort without changing the process environment.
fn select(
    channel: Option<&str>,
    environment: Option<&str>,
    fallback: Option<&str>,
) -> Option<String> {
    [channel, environment, fallback]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(super) fn resolve(channel: Option<&str>, fallback: Option<&str>) -> Option<String> {
    let environment = std::env::var("AGENTDESK_CODEX_REASONING_EFFORT").ok();
    select(channel, environment.as_deref(), fallback)
}

#[cfg(unix)]
pub(super) fn from_env() -> Option<String> {
    resolve(None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex::{
        CodexLaunchOptions, build_codex_exec_args, build_codex_wrapper_cli_args,
    };

    #[test]
    fn channel_effort_outranks_env_and_wrapper_high() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _env = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_REASONING_EFFORT",
            std::ffi::OsStr::new("low"),
        );
        let chosen = resolve(Some("ultra"), Some("high"));
        assert_eq!(chosen.as_deref(), Some("ultra"));
        let options = CodexLaunchOptions::new("continue").with_reasoning_effort(chosen.as_deref());
        let direct = build_codex_exec_args(&options);
        assert!(
            direct
                .windows(2)
                .any(|pair| pair == ["-c", "model_reasoning_effort=\"ultra\""])
        );
        assert!(
            !direct
                .iter()
                .any(|arg| arg.contains("reasoning_effort=\"low\""))
        );
        let wrapper = build_codex_wrapper_cli_args(&options, "/fixture/codex");
        assert!(
            wrapper
                .windows(2)
                .any(|pair| pair == ["--reasoning-effort", "ultra"])
        );
        let script = crate::services::codex::render_codex_wrapper_tmux_script(
            "",
            "/fixture/agentdesk",
            "/fixture/output",
            "/fixture/fifo",
            "/fixture/prompt",
            "/fixture/cwd",
            "/fixture/codex",
            Some("model"),
            None,
            None,
            None,
            None,
            Some("ultra"),
            None,
        );
        assert!(script.contains("'--reasoning-effort'"));
        assert_eq!(script.matches("'ultra'").count(), 1);
        assert!(!script.contains("'low'"));
        assert_eq!(resolve(None, Some("high")).as_deref(), Some("low"));
        assert_eq!(select(None, None, Some("high")).as_deref(), Some("high"));
        assert_eq!(select(None, None, None), None);
    }
}
