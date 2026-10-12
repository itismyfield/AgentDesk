//! A pinned pane admits one conditional Enter; ambiguous replies never authorize another.

use super::SubmissionTmux;
use crate::services::claude_tui::host_input::{InputRefusal, InputRun, StopCause};
use crate::services::tui_input::submission::Refusal;
use std::process::Output;

const SNAPSHOT: &str = "#{session_id} #{window_id} #{pane_id} #{pane_width} #{pane_height} #{window-size} #{window_panes} #{pane_dead} #{pid} #{pane_pid}";
const APPLIED: &[u8] = b"CG_ENTER_APPLIED\n";
const HELD: &[u8] = b"CG_ENTER_HELD\n";

#[derive(Debug, PartialEq, Eq)]
struct Geometry {
    session: String,
    window: String,
    pane: String,
    width: usize,
    height: usize,
    server_pid: String,
    pane_pid: String,
}

fn held() -> InputRefusal {
    InputRefusal::Composer(Refusal::UnpredictableRender)
}

fn identity(raw: &str, prefix: char) -> Option<String> {
    let digits = raw.strip_prefix(prefix)?;
    (!digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit())).then(|| raw.to_string())
}

fn positive_pid(raw: &str) -> Option<String> {
    (raw.parse::<u32>().ok()? > 0 && raw.bytes().all(|c| c.is_ascii_digit()))
        .then(|| raw.to_string())
}

impl Geometry {
    fn read(output: Output) -> Option<Self> {
        if !output.status.success() || !output.stderr.is_empty() {
            return None;
        }
        let raw = String::from_utf8(output.stdout).ok()?;
        let fields: Vec<_> = raw.split_ascii_whitespace().collect();
        let [
            session,
            window,
            pane,
            width,
            height,
            "manual",
            "1",
            "0",
            server_pid,
            pane_pid,
        ] = fields.as_slice()
        else {
            return None;
        };
        let width = width.parse().ok()?;
        let height = height.parse().ok()?;
        if width < 160 || height < 40 {
            return None;
        }
        Some(Self {
            session: identity(session, '$')?,
            window: identity(window, '@')?,
            pane: identity(pane, '%')?,
            width,
            height,
            server_pid: positive_pid(server_pid)?,
            pane_pid: positive_pid(pane_pid)?,
        })
    }

    fn condition(&self) -> String {
        let checks = [
            format!("#{{==:#{{session_id}},{}}}", self.session),
            format!("#{{==:#{{window_id}},{}}}", self.window),
            format!("#{{==:#{{pane_id}},{}}}", self.pane),
            format!("#{{==:#{{pane_width}},{}}}", self.width),
            format!("#{{==:#{{pane_height}},{}}}", self.height),
            "#{==:#{window-size},manual}".into(),
            "#{==:#{window_panes},1}".into(),
            "#{==:#{pane_dead},0}".into(),
            format!("#{{==:#{{pid}},{}}}", self.server_pid),
            format!("#{{==:#{{pane_pid}},{}}}", self.pane_pid),
        ];
        checks
            .into_iter()
            .reduce(|a, b| format!("#{{&&:{a},{b}}}"))
            .unwrap()
    }
}

pub(crate) struct GuardedPane {
    geometry: Geometry,
}

impl SubmissionTmux {
    pub(crate) fn pin_guarded(&self, session: &str) -> Result<GuardedPane, InputRefusal> {
        let target = format!("={session}:");
        let output = self
            .run(&["display-message", "-p", "-t", &target, SNAPSHOT])
            .map_err(|_| InputRefusal::Composer(Refusal::CaptureUnavailable))?;
        let geometry = Geometry::read(output).ok_or_else(held)?;
        Ok(GuardedPane { geometry })
    }
}

impl GuardedPane {
    pub(crate) fn geometry_matches(&self) -> bool {
        SubmissionTmux
            .run(&["display-message", "-p", "-t", &self.geometry.pane, SNAPSHOT])
            .ok()
            .and_then(Geometry::read)
            .is_some_and(|g| g == self.geometry)
    }

    pub(crate) fn capture_draft(&self) -> Option<String> {
        if !self.geometry_matches() {
            return None;
        }
        let output = SubmissionTmux
            .run(&[
                "capture-pane",
                "-p",
                "-e",
                "-t",
                &self.geometry.pane,
                "-S",
                "-80",
            ])
            .ok()?;
        (output.status.success() && output.stderr.is_empty())
            .then(|| String::from_utf8(output.stdout).ok())
            .flatten()
    }

    // Consuming the pin keeps a lost acknowledgement from being retried with the same admission.
    pub(crate) fn enter(self) -> InputRun {
        let condition = self.geometry.condition();
        let yes = format!(
            "send-keys -t {} Enter ; display-message -p CG_ENTER_APPLIED",
            self.geometry.pane
        );
        let result = SubmissionTmux.run(&[
            "if-shell",
            "-F",
            "-t",
            &self.geometry.pane,
            &condition,
            &yes,
            "display-message -p CG_ENTER_HELD",
        ]);
        classify_enter(result)
    }
}

fn classify_enter(result: Result<Output, String>) -> InputRun {
    match result {
        Ok(output)
            if output.status.success() && output.stderr.is_empty() && output.stdout == APPLIED =>
        {
            InputRun::Applied
        }
        Ok(output)
            if output.status.success() && output.stderr.is_empty() && output.stdout == HELD =>
        {
            InputRun::Refused(held())
        }
        result => InputRun::Indeterminate {
            confirmed: 0,
            cause: StopCause::Send(match result {
                Err(error) => error,
                Ok(output) => format!(
                    "conditional Enter acknowledgement unavailable: status={} stdout={:?} stderr={:?}",
                    output.status, output.stdout, output.stderr
                ),
            }),
        },
    }
}

#[cfg(test)]
mod protocol_tests;
#[cfg(test)]
mod transport_tests;
