//! A channel leaving Herdr takes its Claude session back to tmux: the Herdr execution ends by its
//! own `/exit`, its row retires, and its pane is released before the tmux turn resumes the session.

use std::path::Path;

use sqlx::PgPool;
use tokio::runtime::Handle;

use crate::db::dispatched_sessions::hosted_execution::{
    HostedCasOutcome, HostedExecution, HostedState, retire_pg,
};
use crate::services::claude::session_handoff::{
    self, EXIT_WAIT, EarlierExecution, ExitSend, LegacyTmux, Liveness,
};
use crate::services::session_host::{
    HerdrPaneView, HerdrTarget, PaneProvider, PaneReading, herdr_endpoints,
};
use crate::services::turn_host::ReturnPlan;

/// The executions a return reads; tests stand in for both hosts.
pub(crate) trait ReturnPorts {
    fn herdr<'a>(&self, record: &'a HostedExecution) -> Box<dyn EarlierExecution + 'a> {
        Box::new(HerdrPane::of(record))
    }

    fn tmux<'a>(&self, logical: &'a str) -> Box<dyn EarlierExecution + 'a> {
        Box::new(LegacyTmux(logical))
    }

    fn exit_wait(&self) -> std::time::Duration {
        EXIT_WAIT
    }
}

pub(crate) struct ProductionPorts;

impl ReturnPorts for ProductionPorts {}

/// What a return turn carries over to its tmux turn.
pub(crate) struct ReturnTurn<'a> {
    pub pool: &'a PgPool,
    pub channel_id: u64,
    pub working_dir: &'a str,
    pub session_id: Option<&'a str>,
    pub force_fresh: bool,
}

/// Moves `plan`'s execution back to tmux; every failure leaves it where it was and stops the turn.
/// A Bound execution ends by one `/exit` and retires; a retired one only finishes its move.
pub(crate) fn execute(
    turn: &ReturnTurn<'_>,
    plan: &ReturnPlan,
    runtime: &Handle,
    ports: &dyn ReturnPorts,
) -> Result<(), String> {
    let record = &plan.record;
    let (logical, nonce) = (record.owner.logical_key.as_str(), &record.execution_nonce);
    if super::herdr_turn::hold_path(nonce)?
        .try_exists()
        .map_err(|error| format!("herdr return: input hold of {nonce}: {error}"))?
    {
        return Err(format!(
            "herdr return: {nonce} may hold an unsubmitted prompt"
        ));
    }
    match ports.tmux(logical).liveness() {
        Liveness::Ended => {}
        Liveness::Running => return Err(format!("herdr return: tmux {logical} still runs")),
        Liveness::Unknown(why) => return Err(format!("herdr return: tmux {logical}: {why}")),
    }
    let account = resumable(turn, record)?;
    if record.state == HostedState::Bound {
        session_handoff::end_by_exit(&mut *ports.herdr(record), ports.exit_wait())?;
        let retire = retire_pg(turn.pool, &plan.row, &record.owner, nonce);
        match runtime.block_on(retire) {
            Ok(HostedCasOutcome::Written) => {}
            Ok(HostedCasOutcome::Stale) => return Err("herdr return: retire CAS stale".into()),
            Err(error) => return Err(format!("herdr return: retire CAS: {error:?}")),
        }
    }
    // The pane leaves the Herdr listing before the marker names tmux, so a turn that sees tmux
    // never meets a listing that withholds its hooks or skips its pane.
    crate::services::tui_prompt_dedupe::release_herdr_execution(logical, nonce);
    // The tmux turn resumes the session only under the account it last ran under.
    if let Some(profile) = account {
        crate::services::tmux_common::write_tmux_session_auth_profile(logical, &profile)?;
    }
    super::tui_session_launch::mark_returned_to_tmux(logical)
}

/// A session the tmux turn will resume is continued only from a cursor O already holds, and only
/// under the account its own launch recorded, which it returns; a fresh turn needs neither.
fn resumable(turn: &ReturnTurn<'_>, record: &HostedExecution) -> Result<Option<String>, String> {
    let Some(session) = turn.session_id.filter(|_| !turn.force_fresh) else {
        return Ok(None);
    };
    // The tmux turn looks for it under the home of the account it launches under, as here.
    let logical = &record.owner.logical_key;
    let overlay = crate::services::discord::org_schema::overlay_from_tmux_session(
        crate::services::provider::ProviderKind::Claude,
        logical,
    )?;
    let (working_dir, home) = (Path::new(turn.working_dir), overlay.home.as_deref());
    let Some(source) = session_handoff::transcript_file(working_dir, session, home)? else {
        return Ok(None);
    };
    if !session_handoff::o_reads(turn.channel_id, &source)? {
        return Err(format!(
            "herdr return: O reads no cursor on {}, so a resume would post it again",
            source.path.display()
        ));
    }
    match crate::services::tmux_common::claude_session_auth(logical, session)? {
        Some(profile) => Ok(Some(profile)),
        None => Err(format!(
            "herdr return: no launch of {session} recorded its account"
        )),
    }
}

/// The Herdr execution of a stored record, read and typed to only through its own gated pane.
pub(crate) struct HerdrPane<'a> {
    record: &'a HostedExecution,
    target: Option<HerdrTarget>,
    view: Option<HerdrPaneView>,
}

impl<'a> HerdrPane<'a> {
    pub(crate) fn of(record: &'a HostedExecution) -> Self {
        let registry = herdr_endpoints();
        let (target, view) = (registry.target(record), registry.view(record));
        Self {
            record,
            target,
            view,
        }
    }
}

impl EarlierExecution for HerdrPane<'_> {
    /// Ended only once its pane no longer runs it and its provider process is gone.
    fn liveness(&mut self) -> Liveness {
        let (Some(target), Some(view)) = (&self.target, &self.view) else {
            return Liveness::Unknown("not on a registered endpoint".into());
        };
        let reading = view.read_execution();
        if view.ended(self.record, &reading) {
            return match target.provider_absent() {
                Ok(true) => Liveness::Ended,
                Ok(false) => Liveness::Unknown("its provider outlived the pane".into()),
                Err(error) => Liveness::Unknown(error),
            };
        }
        match reading {
            PaneReading::Present {
                provider: PaneProvider::Execution(_),
                ..
            } => Liveness::Running,
            other => Liveness::Unknown(format!("{other:?}")),
        }
    }

    fn send_exit(&mut self) -> ExitSend {
        let Some(target) = &self.target else {
            return ExitSend::Untouched("not on a registered endpoint".into());
        };
        let plan = session_handoff::exit_plan();
        session_handoff::exit_sent(super::herdr_turn::composer::run_exit(target, &plan))
    }
}

#[cfg(test)]
#[path = "herdr_return_tests.rs"]
pub(crate) mod tests;
