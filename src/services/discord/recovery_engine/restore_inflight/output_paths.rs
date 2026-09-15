//! Discover the live transcript from the existing tmux pane.
#[cfg(unix)]
use super::*;

#[cfg(unix)]
fn tmux_pane_pid(tmux_session_name: &str) -> Option<u32> {
    let mut cmd = Command::new("tmux");
    binary_resolver::apply_runtime_path(&mut cmd);
    let output = cmd
        .args([
            "display-message",
            "-p",
            "-t",
            &tmux_exact_target(tmux_session_name),
            "#{pane_pid}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

#[cfg(unix)]
pub(in crate::services::discord::recovery_engine) fn detect_live_tmux_output_path(
    tmux_session_name: &str,
    fallback_path: &str,
) -> Result<Option<DetectedRebindOutputPath>, StaleOutputCandidate> {
    let Some(pane_pid) = tmux_pane_pid(tmux_session_name) else {
        return Ok(None);
    };
    let mut cmd = Command::new("lsof");
    binary_resolver::apply_runtime_path(&mut cmd);
    let output = match cmd.args(["-Fn", "-p", &pane_pid.to_string()]).output() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = match String::from_utf8(output.stdout) {
        Ok(stdout) => stdout,
        Err(_) => return Ok(None),
    };
    let candidates = parse_lsof_output_candidates(&stdout);
    detect_rebind_output_path_from_candidates(fallback_path, candidates)
}
