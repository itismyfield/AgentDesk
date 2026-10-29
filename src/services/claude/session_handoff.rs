//! A Claude session moving between tmux and Herdr keeps its id: the earlier execution ends by its
//! own `/exit` before the other host resumes; nothing here kills, and anything unobserved stops.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::services::claude_tui::host_input::InputRun;
use crate::services::claude_tui::input::TuiInputAction;
use crate::services::tui_o::shadow::SourceId;

/// How long an `/exit` sent to an empty composer may take to end its provider.
pub(crate) const EXIT_WAIT: Duration = Duration::from_secs(20);
const EXIT_POLL: Duration = Duration::from_millis(250);

/// Whether the earlier execution's provider still runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Liveness {
    Running,
    Ended,
    Unknown(String),
}

/// What one `/exit` attempt did to the earlier execution's composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExitSend {
    Sent,
    /// Nothing was typed: a draft, a dialog, a busy screen or a refused gate.
    Untouched(String),
    /// Part of it may have been typed.
    Unclear(String),
}

/// The earlier execution, on the host the session leaves.
pub(crate) trait EarlierExecution {
    fn liveness(&mut self) -> Liveness;
    /// Types `/exit` and Enter once, only into an empty ready composer.
    fn send_exit(&mut self) -> ExitSend;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    /// Nothing ran any more, so nothing was typed.
    Already,
    ByExit,
}

/// The `/exit` the composer receives: typed, then submitted.
pub(crate) fn exit_plan() -> [TuiInputAction; 2] {
    [
        TuiInputAction::Literal("/exit".into()),
        TuiInputAction::Enter,
    ]
}

/// Ends `earlier` before its session runs elsewhere. A provider already gone is never typed to,
/// so a retry after a lost reply sends nothing; one that outlives `wait` stays and stops the turn.
pub(crate) fn end_by_exit(
    earlier: &mut dyn EarlierExecution,
    wait: Duration,
) -> Result<Ended, String> {
    match earlier.liveness() {
        Liveness::Ended => return Ok(Ended::Already),
        Liveness::Unknown(why) => return Err(format!("earlier claude unobserved: {why}")),
        Liveness::Running => {}
    }
    match earlier.send_exit() {
        ExitSend::Sent => {}
        ExitSend::Untouched(why) => return Err(format!("earlier claude kept running: {why}")),
        ExitSend::Unclear(why) => return Err(format!("earlier claude exit unclear: {why}")),
    }
    let deadline = Instant::now() + wait;
    loop {
        if earlier.liveness() == Liveness::Ended {
            return Ok(Ended::ByExit);
        }
        if Instant::now() >= deadline {
            return Err(format!("earlier claude did not exit within {wait:?}"));
        }
        std::thread::sleep(EXIT_POLL.min(wait));
    }
}

/// How an input run on the earlier composer maps onto an exit attempt.
pub(crate) fn exit_sent(run: InputRun) -> ExitSend {
    match run {
        InputRun::Applied => ExitSend::Sent,
        InputRun::Refused(refusal) => ExitSend::Untouched(format!("{refusal:?}")),
        InputRun::Cancelled { confirmed: 0 } => ExitSend::Untouched("cancelled".into()),
        other => ExitSend::Unclear(format!("{other:?}")),
    }
}

/// The legacy tmux session of a logical key; its pane ends with the provider it execs.
pub(crate) struct LegacyTmux<'a>(pub(crate) &'a str);

impl EarlierExecution for LegacyTmux<'_> {
    /// A tmux with no server socket runs nothing; a socket that cannot be read stays unobserved.
    fn liveness(&mut self) -> Liveness {
        use crate::services::platform::tmux::{PaneLiveness, pane_liveness};
        match super::host_gate::session_exists(self.0) {
            Ok(true) => {}
            Ok(false) => return Liveness::Ended,
            Err(why) => return Liveness::Unknown(why),
        }
        match pane_liveness(self.0) {
            PaneLiveness::Live => Liveness::Running,
            PaneLiveness::DeadOrAbsent => Liveness::Ended,
            PaneLiveness::ProbeError => Liveness::Unknown("tmux probe failed".into()),
        }
    }

    fn send_exit(&mut self) -> ExitSend {
        use crate::services::claude_tui::host_input::{capture_draft_with, run_legacy};
        let session = self.0;
        let capture = || capture_draft_with("tmux", session, Duration::from_secs(5));
        let plan = exit_plan();
        let run = super::herdr_turn::composer::when_ready_to_exit(capture, None, || {
            run_legacy(session, &plan, None)
        });
        exit_sent(run)
    }
}

/// The transcript source a resume of `session_id` continues under Claude home `home` (the
/// daemon's when `None`); `None` only when the file is certainly absent, never for a failed read.
pub(crate) fn transcript_file(
    working_dir: &Path,
    session_id: &str,
    home: Option<&Path>,
) -> Result<Option<SourceId>, String> {
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        working_dir,
        session_id,
        home,
    )?;
    match std::fs::metadata(&path) {
        Ok(meta) => Ok(Some(SourceId {
            session_id: session_id.to_owned(),
            path,
            dev: meta.dev(),
            ino: meta.ino(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("resume transcript {}: {error}", path.display())),
    }
}

/// Whether the channel's O writer would continue a stored cursor on `source`'s file, as its own
/// bind names it, rather than read it anew; a store that cannot be read answers nothing.
pub(crate) fn o_reads(channel: u64, source: &SourceId) -> Result<bool, String> {
    let root = crate::config::runtime_root().ok_or("no runtime root")?;
    let Some(store) = crate::services::tui_o::store::OStore::existing(&root) else {
        return Ok(false);
    };
    let path = source.path.display();
    let cursors =
        (store.peek_cursors(channel)).map_err(|error| format!("O cursors of {path}: {error:?}"))?;
    crate::services::tui_o::writer::claude_cursor_continues(&cursors, source)
        .map_err(|error| format!("O cursor of {path}: {error}"))
}

/// The SessionStart of execution `nonce` resuming `session_id`, by its own launch context.
pub(crate) fn resumed_start(
    event: &crate::services::claude_tui::hook_server::HookEvent,
    session_id: &str,
    nonce: &str,
) -> bool {
    event.provider == "claude"
        && event.kind == crate::services::claude_tui::hook_server::HookEventKind::SessionStart
        && event.session_id == session_id
        && event.payload["source"] == "resume"
        && event.original_context.as_ref().is_some_and(|context| {
            context.execution_nonce == nonce
                && context.launch_mode == "resume"
                && context.expected_native_session_id.as_deref() == Some(session_id)
        })
}

#[cfg(test)]
#[path = "session_handoff_tests.rs"]
mod tests;
