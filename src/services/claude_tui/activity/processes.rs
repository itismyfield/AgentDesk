//! Descendants only veto cleanup; failure and absence never establish idle.
use crate::services::{
    platform::tmux,
    process::{self, ProcessIdentity, ProcessIdentityProbe},
};
use std::{
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Descendants {
    Present,
    NoneObserved,
    Unknown,
}
pub(crate) fn observe(session: &str) -> Descendants {
    let Some(root) = tmux::pane_pid(session) else {
        return Descendants::Unknown;
    };
    let birth = ProcessIdentity::capture(root);
    let mut command = Command::new("ps");
    command
        .args(["-eo", "pid=,ppid="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    process::configure_child_process_group(&mut command);
    let output = command.spawn().map_err(|_| ()).and_then(|child| {
        process::wait_with_output_timeout(child, Duration::from_secs(2), "activity ancestry")
            .map_err(|_| ())
    });
    let Ok(output) = output else {
        return Descendants::Unknown;
    };
    if !output.status.success()
        || output.stdout.len() > 4 * 1024 * 1024
        || birth.probe(root) != ProcessIdentityProbe::Same
        || tmux::pane_pid(session) != Some(root)
    {
        return Descendants::Unknown;
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return Descendants::Unknown;
    };
    ancestry(root, text)
}
fn ancestry(root: u32, text: &str) -> Descendants {
    let mut rows = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 {
            return Descendants::Unknown;
        }
        let (Ok(pid), Ok(parent)) = (fields[0].parse::<u32>(), fields[1].parse::<u32>()) else {
            return Descendants::Unknown;
        };
        rows.push((pid, parent));
    }
    if !rows.iter().any(|(pid, _)| *pid == root) {
        return Descendants::Unknown;
    }
    // A verified direct child is already a descendant; no absence grants idle.
    if rows
        .iter()
        .any(|(pid, parent)| *pid != root && *parent == root)
    {
        Descendants::Present
    } else {
        Descendants::NoneObserved
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ancestry_distinguishes_error_empty_and_descendants_without_argv() {
        assert_eq!(ancestry(5, "5 1\n7 5\n8 7\n"), Descendants::Present);
        assert_eq!(ancestry(5, "5 1\n7 1\n"), Descendants::NoneObserved);
        for text in ["", "bad ps", "7 1\n"] {
            assert_eq!(ancestry(5, text), Descendants::Unknown);
        }
    }
}
