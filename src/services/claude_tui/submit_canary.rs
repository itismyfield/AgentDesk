//! One boot-selected channel and pinned execution for the whole provider submission attempt.

use super::guarded_outcome::{self, SubmissionError};
use super::host_input::guarded_submission::Request;
use super::host_input::{InputRefusal, InputRun, InputTarget, MutationGate};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::session_host::HostKind;
use crate::services::tmux_common::{
    self as tc,
    host_marker::{self, HostKindMarker},
};
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext, SpawnNonceMarker};
use std::{cell::RefCell, rc::Rc};

struct Channel {
    id: u64,
    selected: bool,
    host: Option<String>,
    request: RefCell<Option<Rc<Request>>>,
}
thread_local! { static CHANNEL: RefCell<Option<Rc<Channel>>> = const { RefCell::new(None) }; }
struct Restore(Option<Rc<Channel>>);
impl Drop for Restore {
    fn drop(&mut self) {
        CHANNEL.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}
pub(crate) fn with_channel<R>(id: u64, run: impl FnOnce() -> R) -> R {
    let selected = crate::config::session_hosts::claude_guarded(Some(id));
    let scope = Rc::new(Channel {
        id,
        selected,
        host: selected
            .then(|| {
                crate::config::session_hosts::local_node()
                    .or_else(binding_context::stable_host_identity)
            })
            .flatten(),
        request: RefCell::new(None),
    });
    let _restore = Restore(CHANNEL.with(|slot| slot.replace(Some(scope))));
    run()
}
fn configured() -> bool {
    crate::config::session_hosts::with_boot(|boot| {
        boot.is_some_and(|b| !b.config().claude_guard_channels.is_empty())
    })
}
pub(crate) fn enabled() -> bool {
    CHANNEL.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or_else(configured, |scope| scope.selected)
    })
}
pub(crate) fn hold_error(error: String) -> String {
    if enabled() {
        stopped(InputRefusal::Unknown).into_legacy()
    } else {
        error
    }
}
pub(crate) fn require_profile(supported: bool) -> Result<(), String> {
    if enabled() && !supported {
        Err(stopped(InputRefusal::Unknown).into_legacy())
    } else {
        Ok(())
    }
}
#[cfg(unix)]
pub(crate) fn require_warm(existing: bool, live: bool, resume: bool) -> Result<(), String> {
    require_profile(!existing || (live && resume))
}

struct ExecutionGate {
    channel: u64,
    host: Option<String>,
    context: BindingContext,
}
impl MutationGate for ExecutionGate {
    fn admit(&self, session: &str) -> Result<(), InputRefusal> {
        if self.host.is_none() {
            return Err(InputRefusal::Unknown);
        }
        let ctx = &self.context;
        if session != ctx.tmux_session
            || ctx.channel_id != Some(self.channel)
            || ctx.provider != "claude"
            || ctx.owner_runtime_root != tc::current_tmux_owner_marker()
            || ctx.host != self.host
            || ctx
                .expected_native_session_id
                .as_deref()
                .is_none_or(str::is_empty)
            || !matches!(ctx.launch_mode.as_str(), "fresh" | "resume")
            || binding_context::observe_spawn_nonce_marker(session)
                != SpawnNonceMarker::Known(ctx.execution_nonce.clone())
            || binding_context::execution_context("claude", &ctx.execution_nonce)
                .ok()
                .as_ref()
                != Some(ctx)
        {
            return Err(InputRefusal::IdentityMismatch);
        }
        match host_marker::read_host_kind_marker(session) {
            HostKindMarker::Known(HostKind::Tmux) => {}
            HostKindMarker::Known(_) => return Err(InputRefusal::Conflict),
            _ => return Err(InputRefusal::Unknown),
        }
        match tc::resolve_tmux_runtime_kind_marker(session) {
            Some(RuntimeHandoffKind::ClaudeTui) => Ok(()),
            Some(_) => Err(InputRefusal::Conflict),
            None => Err(InputRefusal::Unknown),
        }
    }
}
struct Unproven;
impl MutationGate for Unproven {
    fn admit(&self, _session: &str) -> Result<(), InputRefusal> {
        Err(InputRefusal::Unknown)
    }
}
pub(crate) fn request(session: &str) -> Option<Rc<Request>> {
    let scope = match CHANNEL.with(|slot| slot.borrow().clone()) {
        Some(scope) => scope,
        None => {
            return configured().then(|| {
                Rc::new(Request {
                    target: InputTarget::Refused(InputRefusal::Unknown),
                    gate: Rc::new(Unproven),
                    steering: false,
                })
            });
        }
    };
    if !scope.selected {
        return None;
    }
    if let Some(request) = scope.request.borrow().as_ref() {
        return Some(Rc::clone(request));
    }
    let context = match binding_context::observe_spawn_nonce_marker(session) {
        SpawnNonceMarker::Known(nonce) => binding_context::execution_context("claude", &nonce).ok(),
        _ => None,
    };
    let gate: Rc<dyn MutationGate> = context.map_or_else(
        || Rc::new(Unproven) as Rc<dyn MutationGate>,
        |context| {
            Rc::new(ExecutionGate {
                channel: scope.id,
                host: scope.host.clone(),
                context,
            })
        },
    );
    let target = match gate.admit(session) {
        Ok(()) => InputTarget::Tmux(session.to_owned()),
        Err(refusal) => InputTarget::Refused(refusal),
    };
    let request = Rc::new(Request {
        target,
        gate,
        steering: false,
    });
    scope.request.replace(Some(Rc::clone(&request)));
    Some(request)
}

/// Checked under the turn lock; only a confirmed missing session gets fresh admission.
#[cfg(unix)]
pub(crate) fn preflight(
    session: &str,
    channel: Option<u64>,
    profile: &str,
    old: (bool, bool),
    cleanup: impl FnOnce(),
) -> Result<(bool, bool), SubmissionError> {
    if let Some(run) = guarded_outcome::held(session) {
        return Err(SubmissionError::Guarded(run));
    }
    if !enabled() {
        return Ok(old);
    }
    let channel_matches = CHANNEL.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|scope| Some(scope.id) == channel)
    });
    if !channel_matches {
        return Err(stopped(InputRefusal::IdentityMismatch));
    }
    use crate::services::platform::tmux::{SessionPresence, session_presence};
    let fresh = match session_presence(session) {
        SessionPresence::Missing => (false, old.1),
        SessionPresence::Present => {
            let request = request(session).ok_or_else(|| stopped(InputRefusal::Unknown))?;
            request.target.tmux_session().map_err(stopped)?;
            request.gate.admit(session).map_err(stopped)?;
            if !tc::tmux_session_auth_profile_matches(session, profile) {
                return Err(stopped(InputRefusal::IdentityMismatch));
            }
            (true, true)
        }
        SessionPresence::ProbeFailed => return Err(stopped(InputRefusal::Unknown)),
    };
    cleanup();
    Ok(fresh)
}
pub(crate) fn stopped(refusal: InputRefusal) -> SubmissionError {
    let run = InputRun::Refused(refusal);
    guarded_outcome::finish(&run);
    SubmissionError::Guarded(run)
}
