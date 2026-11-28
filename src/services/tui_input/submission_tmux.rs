//! Bounded tmux transport for submission; it has no session cleanup authority.

use std::process::Output;

use crate::services::claude_tui::host_input::InputTransport;
use crate::services::session_host::{HostKey, SubmitPayload, TmuxHost};

pub mod guarded;

pub(crate) struct SubmissionTmux;

impl SubmissionTmux {
    fn run(&self, args: &[&str]) -> Result<Output, String> {
        TmuxHost.run_submission(args)
    }

    pub(crate) fn pane_size(&self, session: &str) -> Option<(usize, usize)> {
        let target = format!("={session}:");
        let output = self
            .run(&[
                "display-message",
                "-p",
                "-t",
                &target,
                "#{pane_width} #{pane_height}",
            ])
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let raw = String::from_utf8(output.stdout).ok()?;
        let mut parts = raw.split_ascii_whitespace();
        let width = parts.next()?.parse::<usize>().ok()?;
        let height = parts.next()?.parse::<usize>().ok()?;
        (width > 0 && height > 0 && parts.next().is_none()).then_some((width, height))
    }

    pub(crate) fn capture_raw(&self, session: &str, scroll_back: i32) -> Option<String> {
        let target = format!("={session}:");
        let scroll_back = scroll_back.to_string();
        let output = self
            .run(&[
                "capture-pane",
                "-p",
                "-e",
                "-t",
                &target,
                "-S",
                &scroll_back,
            ])
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl InputTransport for SubmissionTmux {
    fn send_literal(&mut self, session: &str, text: &str) -> Result<Output, String> {
        TmuxHost
            .submission_input(session, SubmitPayload::Literal(text))
            .into_output()
    }

    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
        TmuxHost
            .submission_input("", SubmitPayload::LoadText { buffer, text })
            .into_output()
    }

    fn paste_buffer(
        &mut self,
        session: &str,
        buffer: &str,
        delete: bool,
    ) -> Result<Output, String> {
        TmuxHost
            .submission_input(session, SubmitPayload::Paste { buffer, delete })
            .into_output()
    }

    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String> {
        TmuxHost
            .submission_input(session, SubmitPayload::Keys(keys))
            .into_output()
    }

    fn capture(&mut self, session: &str, scroll_back: i32) -> Option<String> {
        self.capture_raw(session, scroll_back)
    }

    fn capture_draft(&mut self, session: &str) -> Option<String> {
        self.capture_raw(session, -80)
    }

    fn pane_alive(&mut self, session: &str) -> bool {
        let target = format!("={session}:");
        self.run(&["display-message", "-p", "-t", &target, "#{pane_dead}"])
            .is_ok_and(|output| {
                output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "0"
            })
    }

    fn present(&mut self, session: &str) -> bool {
        self.pane_alive(session)
    }

    fn retire(&mut self, _session: &str, _reason_code: &str, _reason: &str) {}
}
