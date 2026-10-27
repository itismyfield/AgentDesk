//! A pinned pane: one snapshot proves its identity and geometry before each capture.

use super::SubmissionTmux;
use crate::services::claude_tui::host_input::InputRefusal;
use crate::services::tui_input::submission::Refusal;
use std::process::Output;

const SNAPSHOT: &str = "#{session_id} #{window_id} #{pane_id} #{pane_width} #{pane_height} #{window-size} #{window_panes} #{pane_dead} #{pid} #{pane_pid} #{pane_in_mode} #{pane_input_off} #{session_attached} #{window_linked}";

#[derive(Clone, Debug, PartialEq, Eq)]
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
            "0",
            "0",
            "0",
            "0",
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
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaneIdentity {
    session: String,
    window: String,
    pane: String,
    server_pid: String,
    pane_pid: String,
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
        // A complete snapshot can prove unsupported geometry; a failed read cannot.
        let unavailable = || InputRefusal::Composer(Refusal::CaptureUnavailable);
        if !output.status.success() || !output.stderr.is_empty() {
            return Err(unavailable());
        }
        let raw = std::str::from_utf8(&output.stdout).map_err(|_| unavailable())?;
        let fields: Vec<_> = raw.split_ascii_whitespace().collect();
        if fields.len() != 14
            || identity(fields[0], '$').is_none()
            || identity(fields[1], '@').is_none()
            || identity(fields[2], '%').is_none()
            || !matches!(fields[5], "manual" | "latest" | "smallest" | "largest")
            || [3, 4, 6, 7, 8, 9, 10, 11, 12, 13]
                .iter()
                .any(|&index| fields[index].parse::<u32>().is_err())
        {
            return Err(unavailable());
        }
        let geometry = Geometry::read(output).ok_or_else(held)?;
        Ok(GuardedPane { geometry })
    }
}

impl GuardedPane {
    pub(crate) fn capture_draft(&self) -> Option<String> {
        capture_guarded(&self.geometry)
    }

    #[cfg(unix)]
    pub(crate) fn identity(&self) -> PaneIdentity {
        PaneIdentity {
            session: self.geometry.session.clone(),
            window: self.geometry.window.clone(),
            pane: self.geometry.pane.clone(),
            server_pid: self.geometry.server_pid.clone(),
            pane_pid: self.geometry.pane_pid.clone(),
        }
    }
}

fn capture_guarded(geometry: &Geometry) -> Option<String> {
    let output = SubmissionTmux
        .run(&[
            "display-message",
            "-p",
            "-t",
            &geometry.pane,
            SNAPSHOT,
            ";",
            "capture-pane",
            "-p",
            "-e",
            "-J",
            "-t",
            &geometry.pane,
            "-S",
            "-80",
        ])
        .ok()?;
    read_capture(output, geometry)
}

fn read_capture(mut output: Output, geometry: &Geometry) -> Option<String> {
    if !output.status.success() || !output.stderr.is_empty() {
        return None;
    }
    let raw = String::from_utf8(std::mem::take(&mut output.stdout)).ok()?;
    let (snapshot, capture) = raw.split_once('\n')?;
    output.stdout = snapshot.as_bytes().to_vec();
    (Geometry::read(output)? == *geometry).then(|| capture.to_owned())
}

#[cfg(test)]
mod protocol_tests;
#[cfg(test)]
pub(crate) mod transport_tests;

#[cfg(unix)]
impl PaneIdentity {
    pub(crate) fn epoch_fields(&self) -> [String; 5] {
        [
            self.server_pid.clone(),
            self.session.clone(),
            self.window.clone(),
            self.pane.clone(),
            self.pane_pid.clone(),
        ]
    }
    pub(crate) fn observe(session: &str) -> Option<Self> {
        let out = SubmissionTmux
            .run(&[
                "display-message",
                "-p",
                "-t",
                &format!("={session}:"),
                "#{pid} #{session_id} #{window_id} #{pane_id} #{pane_pid}",
            ])
            .ok()?;
        if !out.status.success() || !out.stderr.is_empty() {
            return None;
        }
        let text = String::from_utf8(out.stdout).ok()?;
        let fields: Vec<_> = text.split_ascii_whitespace().collect();
        let [server, s, w, p, pid] = fields.as_slice() else {
            return None;
        };
        Some(Self {
            server_pid: positive_pid(server)?,
            session: identity(s, '$')?,
            window: identity(w, '@')?,
            pane: identity(p, '%')?,
            pane_pid: positive_pid(pid)?,
        })
    }
}
