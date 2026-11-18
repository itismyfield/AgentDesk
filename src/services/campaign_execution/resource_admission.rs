use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceAdmission {
    Ready,
    WaitingForHeadroom,
    Unavailable,
}

pub(super) async fn wait_for_headroom(
    checkout: &Path,
    script: &Path,
    wait: Duration,
) -> ResourceAdmission {
    // Keep the existing headroom policy, with the optional reclaim hook disabled.
    let mut command = tokio::process::Command::new("/bin/bash");
    command
        .arg(script)
        .args(["1", "1"])
        .env("ADK_REPO", checkout)
        .env("WH_RECLAIM", "/dev/null")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(wait, command.output()).await {
        Ok(Ok(output)) => classify(&output),
        Ok(Err(_)) => ResourceAdmission::Unavailable,
        Err(_) => ResourceAdmission::WaitingForHeadroom,
    }
}

fn classify(output: &Output) -> ResourceAdmission {
    let stdout = std::str::from_utf8(&output.stdout).unwrap_or_default();
    let mark = stdout
        .trim_end()
        .strip_prefix("HEADROOM_SUSTAINED ok=1 at ");
    if output.status.success()
        && output.stderr.is_empty()
        && mark.is_some_and(|time| chrono::NaiveTime::parse_from_str(time, "%H:%M:%S").is_ok())
    {
        ResourceAdmission::Ready
    } else {
        ResourceAdmission::Unavailable
    }
}

#[cfg(all(test, unix))]
#[path = "resource_admission_tests.rs"]
mod tests;
