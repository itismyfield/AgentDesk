//! Backend-neutral local output. The caller has already fenced the DB owner.
pub(super) struct Capture {
    pub backend: &'static str,
    pub format: &'static str,
    pub available: bool,
    pub alive: bool,
    pub text: String,
    pub reason: Option<&'static str>,
    pub truncated: bool,
}

pub(super) fn capture(session_name: &str, lines: i32) -> Capture {
    if let Some(output) = crate::services::session_backend::capture_process_output(
        session_name,
        lines.clamp(1, 2000) as usize,
    ) {
        return match output {
            Ok(output) => Capture {
                backend: "process",
                format: "jsonl",
                available: true,
                alive: output.alive,
                text: output.text,
                reason: None,
                truncated: output.truncated,
            },
            Err(reason) => unavailable("process", "jsonl", reason),
        };
    }
    #[cfg(unix)]
    {
        if let Some(text) =
            crate::services::platform::tmux::capture_pane(session_name, -lines.clamp(1, 2000))
        {
            let mut start = text.len().saturating_sub(256 * 1024);
            while !text.is_char_boundary(start) {
                start += 1;
            }
            return Capture {
                backend: "tmux",
                format: "terminal",
                available: true,
                alive: true,
                text: text[start..].into(),
                reason: None,
                truncated: start > 0,
            };
        }
    }
    unavailable("unattached", "unknown", "session_output_not_attached")
}

fn unavailable(backend: &'static str, format: &'static str, reason: &'static str) -> Capture {
    Capture {
        backend,
        format,
        available: false,
        alive: false,
        text: String::new(),
        reason: Some(reason),
        truncated: false,
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn windows_missing_process_is_unavailable_instead_of_empty_tmux_success() {
        let capture = super::capture("nonexistent-output-fixture", 80);
        assert!(!capture.available);
        assert!(!capture.alive);
        assert_eq!(capture.backend, "unattached");
        assert_eq!(capture.reason, Some("session_output_not_attached"));
    }
}
