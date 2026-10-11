//! A Claude guarded session: created at a fixed size, held in manual size mode and read back.

use std::process::Output;

use super::{
    SessionPresence, classify_has_session_output, install_dead_marker_hooks, log_kill_request,
    tmux_command, tmux_output,
};

/// Server pid and the session, window and pane ids of one created session.
const CREATED_IDENTITY: &str = "#{pid} #{session_id} #{window_id} #{pane_id}";

/// [`super::create_session`] at `cols`×`rows` held in manual size mode, so an attaching client
/// cannot shrink it; a failure after create removes exactly the created session and is an `Err`.
pub fn create_session_sized(
    session_name: &str,
    working_dir: Option<&str>,
    shell_command: &str,
    cols: u16,
    rows: u16,
) -> Result<Output, String> {
    let (x, y) = (cols.to_string(), rows.to_string());
    let mut cmd = tmux_command().map_err(|e| format!("Failed to create tmux session: {e}"))?;
    cmd.args(["new-session", "-d", "-P", "-F", CREATED_IDENTITY]);
    cmd.args(["-s", session_name, "-x", &x, "-y", &y]);
    if let Some(dir) = working_dir {
        cmd.args(["-c", dir]);
    }
    cmd.arg(shell_command);
    cmd.env_remove("CLAUDECODE");
    let output = cmd
        .output()
        .map_err(|e| format!("Failed to create tmux session: {e}"))?;
    if !output.status.success() {
        return Ok(output);
    }
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Err(error) = hold_and_confirm_size(&identity, &x, &y) {
        let cleanup = remove_created_session(session_name, &identity);
        return Err(format!(
            "tmux session {session_name} not held at {x}x{y}: {error}; {cleanup}"
        ));
    }
    install_dead_marker_hooks(session_name);
    Ok(output)
}

/// Switch the created window to manual `x`×`y`, then read back that the same single pane has it.
fn hold_and_confirm_size(identity: &str, x: &str, y: &str) -> Result<(), String> {
    let ids: Vec<&str> = identity.split(' ').collect();
    let [_, _, window, pane] = ids[..] else {
        return Err(format!("unreadable created identity {identity:?}"));
    };
    let resized = tmux_output(["resize-window", "-t", window, "-x", x, "-y", y])
        .map_err(|e| format!("resize-window spawn failed: {e}"))?;
    if !resized.status.success() {
        let stderr = String::from_utf8_lossy(&resized.stderr);
        return Err(format!("resize-window failed: {}", stderr.trim()));
    }
    let format = format!(
        "{CREATED_IDENTITY} #{{pane_width}} #{{pane_height}} #{{window_panes}} #{{window-size}}"
    );
    let shown = tmux_output(["display-message", "-p", "-t", pane, &format])
        .map_err(|e| format!("geometry query spawn failed: {e}"))?;
    let observed = String::from_utf8_lossy(&shown.stdout).trim().to_string();
    let expected = format!("{identity} {x} {y} 1 manual");
    // A failed or empty query is never read as the expected size.
    if !shown.status.success() || observed != expected {
        let stderr = String::from_utf8_lossy(&shown.stderr);
        return Err(format!(
            "geometry {observed:?} is not {expected:?} ({})",
            stderr.trim()
        ));
    }
    Ok(())
}

/// Kill the created session only while the same server still holds that session id, then say
/// whether it is confirmed gone; a recycled id on a new server is left alone and reported.
fn remove_created_session(session_name: &str, identity: &str) -> String {
    let ids: Vec<&str> = identity.split(' ').collect();
    let [server_pid, session, _, _] = ids[..] else {
        return format!("cleanup unconfirmed: created identity of {session_name} unknown");
    };
    log_kill_request(session_name, "sized launch not confirmed");
    let condition = format!("#{{==:#{{pid}}/#{{session_id}},{server_pid}/{session}}}");
    let kill = format!("kill-session -t '{session}'");
    let killed = tmux_output(["if-shell", "-F", "-t", session, &condition, &kill]);
    let presence = tmux_output(["has-session", "-t", session]);
    match presence.as_ref().map(classify_has_session_output) {
        Ok(SessionPresence::Missing) => format!("created session {session} removed"),
        _ => format!(
            "cleanup unconfirmed: session {session} of {session_name} kill={:?} probe={:?}",
            killed.map(|o| o.status),
            presence.map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
        ),
    }
}

#[cfg(all(test, unix))]
#[path = "sized_cleanup_tests.rs"]
mod tests;
