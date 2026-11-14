//! Startup-only menu detection; pane text is diagnostic evidence, never source authority.
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) struct StartupCheck<'a> {
    cwd: &'a Path,
    tmux: Option<&'a str>,
    next_probe: Instant,
    error: Option<String>,
}

impl<'a> StartupCheck<'a> {
    pub(super) fn new(cwd: &'a Path, tmux: Option<&'a str>) -> Self {
        Self {
            cwd,
            tmux,
            next_probe: Instant::now(),
            error: None,
        }
    }

    pub(super) fn check(&mut self) -> Option<String> {
        let tmux = self.tmux?;
        if Instant::now() < self.next_probe {
            return None;
        }
        self.next_probe = Instant::now() + Duration::from_millis(500);
        let pane = crate::services::platform::tmux::capture_pane_timeout(
            tmux,
            -80,
            Duration::from_millis(100),
        )?;
        let (reason, action) = classify(&pane)?;
        Some(format!(
            "Codex startup blocked by {reason} dialog in {}. {action}",
            self.cwd.display()
        ))
    }

    pub(super) fn alive(&mut self, alive: bool) -> bool {
        if !alive {
            return false;
        }
        self.error = self.check();
        self.error.is_none()
    }

    pub(super) fn finish<T>(
        &mut self,
        result: Result<T, String>,
        progressed: impl FnOnce() -> Option<T>,
    ) -> Result<T, String> {
        match (result, self.error.take()) {
            (Err(_), Some(error)) => progressed().ok_or(error),
            (result, _) => result,
        }
    }
}

fn classify(pane: &str) -> Option<(&'static str, &'static str)> {
    let plain = crate::services::tui_input::ansi::strip_ansi_escape_sequences(pane);
    let lines: Vec<_> = plain.lines().rev().take(40).map(str::trim).collect();
    // A live composer below a quoted menu wins over its older selected option.
    let selected = lines
        .iter()
        .find_map(|line| line.strip_prefix(['›', '❯', '>']))?;
    option(selected)?;
    let choices: Vec<_> = lines
        .iter()
        .filter_map(|line| option(line))
        .map(str::to_ascii_lowercase)
        .collect();
    if choices.len() < 2 {
        return None;
    }
    let title = |prefix: &str| {
        lines
            .iter()
            .any(|line| line.to_ascii_lowercase().starts_with(prefix))
    };
    let choice = |prefix: &str| choices.iter().any(|line| line.starts_with(prefix));
    if (title("do you trust") || title("trust this folder?"))
        && (choice("yes, proceed") || choice("trust and continue"))
        && (choice("no, exit") || choice("quit"))
    {
        Some((
            "folder trust",
            "Open Codex in this working folder, complete its trust prompt, then retry.",
        ))
    } else if choice("sign in with chatgpt")
        && (choice("use an api key")
            || choice("sign in with an api key")
            || choice("use an openai api key")
            || choice("sign in with api key")
            || choice("provide your own api key")
            || choice("sign in with device code"))
    {
        Some((
            "sign-in",
            "Complete Codex sign-in for this runtime, then retry.",
        ))
    } else if title("update available")
        && choice("update now (runs")
        && (choice("skip until next version") || choice("skip for now"))
    {
        Some((
            "update",
            "Complete or dismiss the Codex update menu, then retry.",
        ))
    } else {
        None
    }
}

fn option(line: &str) -> Option<&str> {
    let line = line
        .strip_prefix(['›', '❯', '>'])
        .unwrap_or(line)
        .trim_start();
    let (number, text) = line.split_once(". ")?;
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())).then_some(text)
}

#[cfg(unix)]
pub(crate) fn is_startup_dialog_error(error: &str) -> bool {
    error
        .strip_prefix("Codex startup blocked by ")
        .is_some_and(|rest| {
            [
                "folder trust dialog in ",
                "sign-in dialog in ",
                "update dialog in ",
            ]
            .iter()
            .any(|reason| rest.starts_with(reason))
        })
}
