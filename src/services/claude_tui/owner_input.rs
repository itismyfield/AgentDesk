//! Only a live owner scope adds payload commits; legacy writers keep their transport unchanged.
use super::{guarded_outcome, submission_fence};
use crate::services::tui_input::target_epoch::PaneEpoch;
use std::{
    cell::RefCell,
    sync::{Arc, Mutex},
};

pub(crate) type Commit = Arc<dyn Fn(Option<PaneEpoch>) -> Result<(), String> + Send + Sync>;
pub(crate) type Pin = Arc<dyn Fn(Option<PaneEpoch>, String) -> Result<(), String> + Send + Sync>;
pub(crate) type Hold = Arc<dyn Fn(serde_json::Value) -> Result<(), String> + Send + Sync>;
struct Scope {
    provider: String,
    target: serde_json::Value,
    channel: u64,
    commit: Commit,
    pin: Pin,
    hold: Hold,
    #[cfg(unix)]
    epoch: Mutex<Option<crate::services::tui_input::target_epoch::LiveEpoch>>,
    refused: Mutex<Option<submission_fence::PreSubmitPersistenceRefused>>,
}
thread_local! { static CURRENT:RefCell<Option<Arc<Scope>>>=const { RefCell::new(None) }; }
fn current() -> Option<Arc<Scope>> {
    CURRENT.with(|v| v.borrow().clone())
}
pub(crate) fn active() -> bool {
    current().is_some()
}
pub(crate) fn epoch() -> Option<PaneEpoch> {
    #[cfg(unix)]
    return current().and_then(|s| {
        s.epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|e| e.record())
    });
    #[cfg(not(unix))]
    None
}
pub(crate) fn before_payload() -> Result<(), String> {
    let Some(scope) = current() else {
        return Ok(());
    };
    let result = (|| {
        if scope.target["host"] == "tmux" {
            condition(scope.target["owner"].as_str().ok_or("missing owner")?, "1")?;
        } else {
            (scope.pin)(None, "herdr:conservative-writer".into())?;
        }
        (scope.commit)(epoch())
    })();
    if let Err(reason) = &result {
        *scope.refused.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(submission_fence::PreSubmitPersistenceRefused {
                reason: reason.clone(),
            });
    }
    result
}
pub(crate) fn payload<R>(send: impl FnOnce() -> Result<R, String>) -> Result<R, String> {
    before_payload()?;
    send()
}
#[cfg(unix)]
pub(crate) fn observe_pin(
    session: &str,
    pin: &crate::services::tui_input::submission_tmux::guarded::PaneIdentity,
) -> Result<(), String> {
    let Some(scope) = current() else {
        return Ok(());
    };
    if scope.target["owner"] != session {
        return Err("foreign pane owner".into());
    }
    let live = crate::services::tui_input::target_epoch::LiveEpoch::observe(
        &scope.provider,
        &scope.target,
        pin,
    )?;
    let record = live.record();
    let mut epoch = scope.epoch.lock().unwrap_or_else(|e| e.into_inner());
    if epoch.as_ref().is_some_and(|old| old.record() != record) {
        return Err("epoch changed during effect".into());
    }
    let key = serde_json::to_string(&(
        &record.host,
        &record.endpoint,
        &record.physical[0],
        &record.physical[3],
        &record.physical[4],
    ))
    .map_err(|e| e.to_string())?;
    (scope.pin)(Some(record.clone()), key)?;
    *epoch = Some(live);
    Ok(())
}
pub(crate) fn condition(session: &str, previous: &str) -> Result<String, String> {
    let Some(scope) = current() else {
        return Ok(previous.into());
    };
    #[cfg(unix)]
    {
        scope
            .epoch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or("epoch not observed")?
            .condition(session, previous)
    }
    #[cfg(not(unix))]
    {
        let _ = (scope, session);
        Err("live epoch unsupported".into())
    }
}
#[cfg(unix)]
pub(crate) fn request(
    session: &str,
) -> Option<std::rc::Rc<super::host_input::guarded_submission::Request>> {
    let scope = current()?;
    if scope.target["host"] != "tmux" {
        return None;
    }
    Some(std::rc::Rc::new(
        super::host_input::guarded_submission::Request {
            target: super::host_input::InputTarget::legacy_tmux(session),
            gate: std::rc::Rc::new(super::host_input::LegacyTmuxGate),
            steering: false,
        },
    ))
}
impl submission_fence::SubmissionFence for Scope {
    fn before_first_payload(&self) -> Result<(), submission_fence::PreSubmitPersistenceRefused> {
        self.refusal().map_or(Ok(()), Err)
    }
    fn refusal(&self) -> Option<submission_fence::PreSubmitPersistenceRefused> {
        self.refused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn idle_stash_context(&self) -> Option<submission_fence::IdleStashContext> {
        Some(submission_fence::IdleStashContext {
            channel: self.channel,
            target: self.target["owner"].as_str()?.into(),
            available: self.target["host"] == "tmux",
        })
    }
    fn idle_stash_admits(&self, target: &str) -> bool {
        self.target["owner"] == target && epoch().is_some()
    }
    fn claim_idle_stash(
        &self,
        target: &str,
    ) -> Result<(), submission_fence::PreSubmitPersistenceRefused> {
        if self.target["owner"] != target {
            return Err(submission_fence::PreSubmitPersistenceRefused {
                reason: "foreign stash target".into(),
            });
        }
        self.before_first_payload()
    }
}
impl guarded_outcome::Owner for Scope {
    fn started(&self) {}
    fn finished(&self, _: &super::host_input::InputRun) {}
    fn preparation_held(&self, hold: &super::input::PreparationHold) {
        let value = serde_json::json!({"target":hold.target,"reason":hold.reason,"phase":format!("{:?}",hold.phase),"detail":hold.detail,"epoch":epoch()});
        if let Err(reason) = (self.hold)(value) {
            *self.refused.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(submission_fence::PreSubmitPersistenceRefused { reason });
        }
    }
}
#[cfg(all(test, unix))]
pub(crate) fn with_scope<R>(
    provider: String,
    target: serde_json::Value,
    channel: u64,
    commit: Commit,
    pin: Pin,
    hold: Hold,
    run: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<Arc<Scope>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|v| *v.borrow_mut() = self.0.take());
        }
    }
    let scope = Arc::new(Scope {
        provider,
        target,
        channel,
        commit,
        pin,
        hold,
        epoch: Mutex::new(None),
        refused: Mutex::new(None),
    });
    let _restore = Restore(CURRENT.with(|v| v.replace(Some(scope.clone()))));
    submission_fence::with_scope(Some(scope.clone()), || {
        guarded_outcome::with_owner(Some(scope), run)
    })
}

pub(crate) fn argv_payload<R>(
    context: &crate::services::tui_prompt_dedupe::binding_context::BindingContext,
    send: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    if let Some(scope) = current() {
        if context.provider != "codex"
            || scope.provider != "codex"
            || scope.target["host"] != "tmux"
            || scope.target["execution_nonce"] != context.execution_nonce
            || scope.target["owner"] != context.tmux_session
            || scope.target["root"] != context.owner_runtime_root
        {
            return Err("foreign argv execution".into());
        }
        (scope.commit)(None)?;
    }
    send()
}
