//! Typed submission stops and preparation holds reach the attempt's owner through nested adapters.

use super::host_input::InputRun;
#[cfg(test)]
use super::input;
use super::input::{PreparationHold, UserReceipt};
use crate::services::provider::CancelToken;
use std::cell::RefCell;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SubmissionError {
    Legacy(String),
    Guarded(InputRun),
    Held(PreparationHold),
}

impl From<String> for SubmissionError {
    fn from(error: String) -> Self {
        Self::Legacy(error)
    }
}

impl SubmissionError {
    pub(crate) fn from_run(run: InputRun) -> Result<(), Self> {
        match run {
            InputRun::Applied => Ok(()),
            stopped => Err(Self::Guarded(stopped)),
        }
    }
    pub(crate) fn into_legacy(self) -> String {
        match self {
            Self::Legacy(error) => error,
            Self::Guarded(run) => {
                finish(&run);
                format!("claude tui guarded submission held: {run:?}")
            }
            Self::Held(hold) => {
                preparation_held(&hold);
                hold.detail
            }
        }
    }
}

pub(crate) trait Owner: Send + Sync {
    fn started(&self);
    fn finished(&self, run: &InputRun);
    fn preparation_held(&self, _hold: &PreparationHold) {}
    fn receipt(&self, _receipt: UserReceipt) {}
    fn native_session_id(&self) -> Option<String> {
        None
    }
    fn receipt_finished(&self, _session: &str) {}
}

/// Seals the attempt's transcript window before the provider releases its turn lock.
#[cfg(unix)]
pub(crate) struct ReceiptEnd {
    owner: Option<Arc<dyn Owner>>,
    session: String,
}
#[cfg(unix)]
impl Drop for ReceiptEnd {
    fn drop(&mut self) {
        if let Some(owner) = &self.owner {
            owner.receipt_finished(&self.session);
        }
    }
}
#[cfg(unix)]
pub(crate) fn seal_on_exit(session: &str) -> ReceiptEnd {
    ReceiptEnd {
        owner: OWNER.with(|slot| slot.borrow().as_ref().and_then(|scope| scope.owner.clone())),
        session: session.into(),
    }
}

struct Scope {
    owner: Option<Arc<dyn Owner>>,
    result: Option<InputRun>,
    preparation: Option<PreparationHold>,
    receipt_sid: Option<String>,
    fresh_path: Option<String>,
}
thread_local! { static OWNER: RefCell<Option<Scope>> = const { RefCell::new(None) }; }
thread_local! {
    static RECEIPT_SINK: RefCell<Option<Arc<crate::services::provider::native_stop::ReceiptSink>>> = const { RefCell::new(None) };
}

pub(crate) fn with_receipt_sink<R>(cancel: Option<&CancelToken>, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<crate::services::provider::native_stop::ReceiptSink>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RECEIPT_SINK.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(
        RECEIPT_SINK
            .with(|slot| slot.replace(cancel.map(|token| token.native_stop.receipt_sink()))),
    );
    run()
}
struct Restore(Option<Scope>);
impl Drop for Restore {
    fn drop(&mut self) {
        OWNER.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

pub(crate) fn with_owner<R>(owner: Option<Arc<dyn Owner>>, run: impl FnOnce() -> R) -> R {
    scoped(owner, run).0
}
#[cfg(unix)]
pub(crate) fn capture<R>(run: impl FnOnce() -> R) -> (R, Option<InputRun>) {
    let owner = OWNER.with(|slot| slot.borrow().as_ref().and_then(|scope| scope.owner.clone()));
    let (result, scope) = scoped(owner, run);
    (result, scope.result)
}
pub(crate) fn capture_preparation(
    run: impl FnOnce() -> Result<(), SubmissionError>,
) -> Result<(), SubmissionError> {
    let owner = OWNER.with(|slot| slot.borrow().as_ref().and_then(|scope| scope.owner.clone()));
    let (result, scope) = scoped(owner, || {
        let result = run();
        if let Err(SubmissionError::Held(ref hold)) = result {
            preparation_held(hold);
        }
        result
    });
    // The inner scope already notified its owner; carry only the captured disposition outward.
    if let Some(run) = scope.result {
        OWNER.with(|slot| {
            if let Some(parent) = slot.borrow_mut().as_mut() {
                parent.result = Some(run);
            }
        });
    }
    match (result, scope.preparation) {
        (Err(_), Some(hold)) => Err(SubmissionError::Held(hold)),
        (result, _) => result,
    }
}
fn scoped<R>(owner: Option<Arc<dyn Owner>>, run: impl FnOnce() -> R) -> (R, Scope) {
    let previous = OWNER.with(|slot| {
        let (receipt_sid, fresh_path) = slot
            .borrow()
            .as_ref()
            .filter(|scope| match (&scope.owner, &owner) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            })
            .map(|scope| (scope.receipt_sid.clone(), scope.fresh_path.clone()))
            .unwrap_or_default();
        slot.replace(Some(Scope {
            owner,
            result: None,
            preparation: None,
            receipt_sid,
            fresh_path,
        }))
    });
    let _restore = Restore(previous);
    let result = run();
    let stopped = OWNER.with(|slot| {
        slot.borrow_mut()
            .take()
            .expect("submission scope remains installed")
    });
    (result, stopped)
}
pub(crate) fn preparation_held(hold: &PreparationHold) {
    let owner = OWNER.with(|slot| {
        slot.borrow_mut().as_mut().and_then(|scope| {
            if scope
                .preparation
                .as_ref()
                .is_none_or(|old| old.may_requeue() && !hold.may_requeue())
            {
                scope.preparation = Some(hold.clone());
            }
            scope.owner.clone()
        })
    });
    if let Some(owner) = owner {
        owner.preparation_held(hold);
    }
}
/// The provider's resolved SID stays authoritative through nested submission adapters.
#[cfg(unix)]
pub(crate) fn set_receipt_session_id(sid: &str) {
    OWNER.with(|slot| {
        if let Some(scope) = slot.borrow_mut().as_mut() {
            scope.receipt_sid = Some(sid.into());
        }
    });
}
pub(crate) fn receipt(session: &str, prompt: &str) {
    let (owner, sid, fresh_path) = OWNER.with(|slot| {
        let scope = slot.borrow();
        (
            scope.as_ref().and_then(|s| s.owner.clone()),
            scope.as_ref().and_then(|s| s.receipt_sid.clone()),
            scope.as_ref().and_then(|s| s.fresh_path.clone()),
        )
    });
    let sink = RECEIPT_SINK.with(|slot| slot.borrow().clone());
    if owner.is_some() || sink.is_some() {
        let sid = sid.or_else(|| owner.as_ref().and_then(|owner| owner.native_session_id()));
        let receipt = if let Some(path) = fresh_path {
            sid.as_deref()
                .and_then(|sid| UserReceipt::before_fresh(session, prompt, &path, sid))
        } else {
            UserReceipt::before(session, prompt, sid.as_deref())
        };
        if let Some(receipt) = receipt {
            if let Some(sink) = sink
                && let Some(copy) = receipt.copy_for_native()
            {
                sink.receipt(copy);
            }
            if let Some(owner) = owner {
                owner.receipt(receipt);
            }
        }
    }
}

#[cfg(unix)]
pub(crate) fn fresh_receipt(session: &str, prompt: &str, path: &str, sid: &str) {
    set_fresh_receipt_source(path, sid);
    let owner = OWNER.with(|slot| slot.borrow().as_ref().and_then(|scope| scope.owner.clone()));
    if let Some(owner) = owner
        && let Some(receipt) = UserReceipt::before_fresh(session, prompt, path, sid)
    {
        owner.receipt(receipt);
    }
}

#[cfg(unix)]
pub(crate) fn set_fresh_receipt_source(path: &str, sid: &str) {
    set_receipt_session_id(sid);
    OWNER.with(|slot| {
        if let Some(scope) = slot.borrow_mut().as_mut() {
            scope.fresh_path = Some(path.to_owned());
        }
    });
}
pub(crate) fn begin() {
    let owner = OWNER.with(|slot| slot.borrow().as_ref().and_then(|scope| scope.owner.clone()));
    if let Some(owner) = owner {
        owner.started();
    }
}
pub(crate) fn finish(run: &InputRun) {
    if let Some(sink) = RECEIPT_SINK.with(|slot| slot.borrow().clone()) {
        sink.finished(run);
    }
    let owner = OWNER.with(|slot| {
        slot.borrow_mut().as_mut().and_then(|scope| {
            scope.result = Some(run.clone());
            scope.owner.clone()
        })
    });
    if let Some(owner) = owner {
        owner.finished(run);
    }
}

pub(crate) fn readiness_error(session: &str, error: String) -> SubmissionError {
    let preparation = OWNER.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|scope| scope.preparation.as_ref())
            .filter(|hold| hold.target == session)
            .cloned()
    });
    preparation.map_or(SubmissionError::Legacy(error), SubmissionError::Held)
}

#[cfg(test)]
pub fn send_fresh_prompt(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
) -> Result<(), String> {
    input::send_prompt_with_readiness(
        session,
        prompt,
        input::PromptReadinessKind::FreshTurn,
        cancel,
    )
    .map_err(SubmissionError::into_legacy)
}
#[cfg(test)]
pub fn send_warm_followup(
    session: &str,
    prompt: &str,
    cancel: Option<&CancelToken>,
) -> Result<(), String> {
    input::send_warm_followup_owned(session, prompt, cancel).map_err(SubmissionError::into_legacy)
}

#[cfg(unix)]
pub(crate) enum OwnedWarmOutcome {
    Legacy(super::hosting::ClaudeTuiWarmFollowupOutcome),
    Stopped(InputRun),
}

#[cfg(unix)]
pub(crate) fn warm_owned(
    run: impl FnOnce() -> super::hosting::ClaudeTuiWarmFollowupOutcome,
) -> OwnedWarmOutcome {
    let (legacy, input) = capture(run);
    match input {
        Some(stopped) if stopped != InputRun::Applied => OwnedWarmOutcome::Stopped(stopped),
        _ => OwnedWarmOutcome::Legacy(legacy),
    }
}
