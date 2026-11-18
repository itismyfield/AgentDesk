use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Child;

use crate::services::process::{configure_child_process_group, kill_pid_tree};

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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_child_process_group(command.as_std_mut());
    let Ok(child) = command.spawn() else {
        return ResourceAdmission::Unavailable;
    };
    let mut probe = HeadroomProbe(child);
    match tokio::time::timeout(wait, probe.output()).await {
        Ok(Ok(output)) => classify(&output),
        Ok(Err(_)) => ResourceAdmission::Unavailable,
        Err(_) => {
            probe.terminate();
            let _ = probe.0.wait().await;
            ResourceAdmission::WaitingForHeadroom
        }
    }
}

struct HeadroomProbe(Child);

impl HeadroomProbe {
    async fn output(&mut self) -> std::io::Result<Output> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let out = self.0.stdout.as_mut().expect("piped headroom stdout");
        let err = self.0.stderr.as_mut().expect("piped headroom stderr");
        // Keep the leader unreaped while descendants may still hold its pipes.
        tokio::try_join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr))?;
        let status = self.0.wait().await?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    fn terminate(&mut self) {
        if let Some(pid) = self.0.id() {
            kill_pid_tree(pid);
        }
    }
}

impl Drop for HeadroomProbe {
    fn drop(&mut self) {
        self.terminate();
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
