//! Bounded tmux transport for submission; it has no session cleanup authority.

use std::io::Write;
use std::process::Output;

use super::bounded_tmux::run_bounded_tmux;
use crate::services::claude_tui::host_input::InputTransport;
use crate::services::platform::binary_resolver::runtime_command;
use crate::services::session_host::{HostKey, tmux_key_name};

pub(crate) struct SubmissionTmux;

impl SubmissionTmux {
    fn run(&self, args: &[&str]) -> Result<Output, String> {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let mut command = runtime_command("tmux").map_err(|e| e.to_string())?;
                    command.arg("-u").args(args);
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| e.to_string())?
                        .block_on(run_bounded_tmux(&mut command))
                        .map_err(|e| e.to_string())
                })
                .join()
                .unwrap_or_else(|_| Err("tmux submission worker panicked".into()))
        })
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
        let target = format!("={session}:");
        self.run(&["send-keys", "-t", &target, "-l", "--", text])
    }

    fn load_buffer(&mut self, buffer: &str, text: &str) -> Result<Output, String> {
        // The bounded runner closes stdin, so the buffer stays in a file until tmux exits.
        let mut file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|e| e.to_string())?;
        self.run(&["load-buffer", "-b", buffer, &file.path().to_string_lossy()])
    }

    fn paste_buffer(
        &mut self,
        session: &str,
        buffer: &str,
        delete: bool,
    ) -> Result<Output, String> {
        let target = format!("={session}:");
        let mut args = vec!["paste-buffer", "-p", "-r"];
        if delete {
            args.push("-d");
        }
        args.extend(["-b", buffer, "-t", &target]);
        self.run(&args)
    }

    fn send_keys(&mut self, session: &str, keys: &[HostKey]) -> Result<Output, String> {
        let target = format!("={session}:");
        let mut args = vec!["send-keys", "-t", &target];
        args.extend(keys.iter().map(|key| tmux_key_name(*key)));
        self.run(&args)
    }

    fn capture(&mut self, session: &str, scroll_back: i32) -> Option<String> {
        self.capture_raw(session, scroll_back)
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
