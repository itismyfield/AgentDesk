//! A pinned pane admits conditional payload and one Enter; ambiguous replies authorize no retry.

use super::SubmissionTmux;
use crate::services::claude_tui::host_input::{InputRefusal, InputTransport};
use crate::services::claude_tui::input::TuiInputAction;
use crate::services::tui_input::submission::Refusal;
use std::process::Output;

const SNAPSHOT: &str = "#{session_id} #{window_id} #{pane_id} #{pane_width} #{pane_height} #{window-size} #{window_panes} #{pane_dead} #{pid} #{pane_pid} #{pane_in_mode} #{pane_input_off} #{session_attached} #{window_linked}";
const APPLIED: &[u8] = b"CG_ENTER_APPLIED\n";
const HELD: &[u8] = b"CG_ENTER_HELD\n";
const PAYLOAD_APPLIED: &[u8] = b"CG_PAYLOAD_APPLIED\n";
const PAYLOAD_HELD: &[u8] = b"CG_PAYLOAD_HELD\n";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EffectOutcome {
    Applied,
    Held,
    Unknown(String),
}

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
            "#{==:#{pane_in_mode},0}".into(),
            "#{==:#{pane_input_off},0}".into(),
            "#{==:#{session_attached},0}".into(),
            "#{==:#{window_linked},0}".into(),
        ];
        checks
            .into_iter()
            .reduce(|a, b| format!("#{{&&:{a},{b}}}"))
            .unwrap()
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
    session: String,
    geometry: Geometry,
    #[cfg(unix)]
    stash_attempted: bool,
}

pub(crate) struct GuardedObserver {
    geometry: Geometry,
}

impl GuardedObserver {
    pub(crate) fn capture_draft(&self) -> Option<String> {
        capture_guarded(&self.geometry)
    }
}

struct OwnedBuffer(String);

impl Drop for OwnedBuffer {
    fn drop(&mut self) {
        let _ = SubmissionTmux.run(&["delete-buffer", "-b", &self.0]);
    }
}

fn tmux_quote(text: &str) -> String {
    let mut quoted = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' | '"' | '$' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
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
        let pane = GuardedPane {
            session: session.to_owned(),
            geometry,
            #[cfg(unix)]
            stash_attempted: false,
        };
        #[cfg(unix)]
        crate::services::claude_tui::owner_input::observe_pin(session, &pane.identity())
            .map_err(|_| InputRefusal::IdentityMismatch)?;
        Ok(pane)
    }
}

impl GuardedPane {
    pub(crate) fn observer(&self) -> GuardedObserver {
        GuardedObserver {
            geometry: self.geometry.clone(),
        }
    }

    pub(crate) fn session_matches(&self, session: &str) -> bool {
        self.session == session
    }

    pub(crate) fn geometry_matches(&self) -> bool {
        SubmissionTmux
            .run(&["display-message", "-p", "-t", &self.geometry.pane, SNAPSHOT])
            .ok()
            .and_then(Geometry::read)
            .is_some_and(|g| g == self.geometry)
    }

    pub(crate) fn capture_draft(&self) -> Option<String> {
        capture_guarded(&self.geometry)
    }

    #[cfg(unix)]
    pub(crate) fn capture_viewport(&self) -> Option<String> {
        let output = SubmissionTmux
            .run(&[
                "display-message",
                "-p",
                "-t",
                &self.geometry.pane,
                SNAPSHOT,
                ";",
                "capture-pane",
                "-p",
                "-e",
                "-t",
                &self.geometry.pane,
                "-S",
                "0",
            ])
            .ok()?;
        let capture = read_capture(output, &self.geometry)?;
        self.geometry_matches().then_some(capture)
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

    #[cfg(unix)]
    pub(crate) fn dimensions(&self) -> (usize, usize) {
        (self.geometry.width, self.geometry.height)
    }

    #[cfg(unix)]
    pub(crate) fn stash(&mut self) -> EffectOutcome {
        if std::mem::replace(&mut self.stash_attempted, true) {
            return EffectOutcome::Held;
        }
        self.effect(&format!("send-keys -t {} C-s", self.geometry.pane), true)
    }

    pub(crate) fn payload(&self, action: &TuiInputAction) -> EffectOutcome {
        let command = match action {
            TuiInputAction::Literal(text) => format!(
                "send-keys -t {} -l -- {}",
                self.geometry.pane,
                tmux_quote(text)
            ),
            TuiInputAction::PasteBuffer(text) => {
                let buffer = OwnedBuffer(format!("cgb2-{}", uuid::Uuid::new_v4()));
                match SubmissionTmux.load_buffer(&buffer.0, text) {
                    Ok(output)
                        if output.status.success()
                            && output.stderr.is_empty()
                            && output.stdout.is_empty() => {}
                    result => {
                        return EffectOutcome::Unknown(format!(
                            "guarded load-buffer acknowledgement unavailable: {result:?}"
                        ));
                    }
                }
                if crate::services::claude_tui::owner_input::before_payload().is_err() {
                    return EffectOutcome::Held;
                }
                return self.effect(
                    &format!(
                        "paste-buffer -p -r -d -b {} -t {}",
                        buffer.0, self.geometry.pane
                    ),
                    true,
                );
            }
            _ => return EffectOutcome::Held,
        };
        if crate::services::claude_tui::owner_input::before_payload().is_err() {
            return EffectOutcome::Held;
        }
        self.effect(&command, true)
    }

    fn effect(&self, command: &str, payload: bool) -> EffectOutcome {
        let Ok(condition) = crate::services::claude_tui::owner_input::condition(
            &self.session,
            &self.geometry.condition(),
        ) else {
            return EffectOutcome::Held;
        };
        let (applied, held) = if payload {
            (PAYLOAD_APPLIED, PAYLOAD_HELD)
        } else {
            (APPLIED, HELD)
        };
        let yes = format!(
            "{command} ; display-message -p {}",
            String::from_utf8_lossy(applied).trim_end()
        );
        let no = format!(
            "display-message -p {}",
            String::from_utf8_lossy(held).trim_end()
        );
        classify_effect(
            SubmissionTmux.run(&[
                "if-shell",
                "-F",
                "-t",
                &self.geometry.pane,
                &condition,
                &yes,
                &no,
            ]),
            applied,
            held,
        )
    }

    // Consuming the pin keeps a lost acknowledgement from being retried with the same admission.
    pub(crate) fn enter(self) -> EffectOutcome {
        self.effect(&format!("send-keys -t {} Enter", self.geometry.pane), false)
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

fn classify_effect(result: Result<Output, String>, applied: &[u8], held: &[u8]) -> EffectOutcome {
    match result {
        Ok(output)
            if output.status.success() && output.stderr.is_empty() && output.stdout == applied =>
        {
            EffectOutcome::Applied
        }
        Ok(output)
            if output.status.success() && output.stderr.is_empty() && output.stdout == held =>
        {
            EffectOutcome::Held
        }
        result => EffectOutcome::Unknown(match result {
            Err(error) => error,
            Ok(output) => format!(
                "conditional pane effect acknowledgement unavailable: status={} stdout={:?} stderr={:?}",
                output.status, output.stdout, output.stderr
            ),
        }),
    }
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
