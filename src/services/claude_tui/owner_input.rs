//! Only a live owner scope adds payload commits; legacy writers keep their transport unchanged.
use super::{guarded_outcome, submission_fence};
use crate::services::tui_input::target_epoch::PaneEpoch;
use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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
    allowed: AtomicBool,
    known_before: AtomicBool,
    keyed: AtomicBool,
    holder: crate::services::tui_input::writer_lease::Holder,
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
/// Why the owner refused a leaf before C-s; either way no key reached the pane.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadyRefusal {
    /// The pinned server, pane, process or known execution is no longer the observed one.
    Target(String),
    /// The physical writer lease or the exact attempt binding was not granted.
    Lease(String),
}

/// Before any C-s: the known execution, the server-side pane identity and the writer lease
/// still hold. Commits nothing, so a refusal here leaves no key and no payload start.
pub(crate) fn ready_payload() -> Result<(), ReadyRefusal> {
    let Some(scope) = current() else {
        return Ok(());
    };
    let result = scope.ready();
    if let Err(refusal) = &result {
        let (ReadyRefusal::Target(reason) | ReadyRefusal::Lease(reason)) = refusal;
        scope.refuse(format!("owner_refused: {reason}"));
    }
    result
}

/// After the optional C-s and before content: the idempotent payload-start commit.
pub(crate) fn start_payload() -> Result<(), String> {
    let Some(scope) = current() else {
        return Ok(());
    };
    let result = (scope.commit)(epoch());
    scope.gate_result(result)
}

pub(crate) fn before_payload() -> Result<(), String> {
    ready_payload().map_err(|refusal| format!("{refusal:?}"))?;
    start_payload()
}

/// A key (C-s included) is about to reach the pane; no later refusal is a not-sent proof.
pub(crate) fn keyed() {
    if let Some(scope) = current() {
        scope.keyed.store(true, Ordering::SeqCst);
    }
}

/// The text never became a tmux buffer, so no key followed.
pub(crate) fn load_failed() {
    if let Some(scope) = current() {
        scope.refuse("load_failed".into());
    }
}

/// The leaf's typed veto came before any key; a key already sent keeps it from proving unsent,
/// and a refusal the scope already recorded keeps its own reason.
#[cfg(all(test, unix))]
pub(crate) fn vetoed(reason: &str) {
    if let Some(scope) = current()
        && !scope.keyed.load(Ordering::SeqCst)
        && (scope.refused.lock().unwrap_or_else(|e| e.into_inner())).is_none()
    {
        scope.refuse(format!("vetoed: {reason}"));
    }
}

/// `None` without an owner on this thread; otherwise whether this owner may write `physical`,
/// holding it for the owner's lifetime. A foreign host or tmux owner fails closed.
pub(crate) fn hold_content(host: &str, physical: &str, key: &str) -> Option<Result<(), String>> {
    let scope = current()?;
    Some(if scope.target["host"] != host {
        Err(format!(
            "owner on {} cannot write {key}",
            scope.target["host"]
        ))
    } else if host == "tmux" && scope.target["owner"] != physical {
        Err(format!(
            "owner {} cannot write {key}",
            scope.target["owner"]
        ))
    } else {
        scope.holder.hold(key)
    })
}

/// The pinned pane id and the identity condition the tmux server checks atomically around an
/// owner paste or Enter; `None` leaves the command as main runs it.
pub(crate) fn pinned() -> Result<Option<(String, String)>, String> {
    let Some(scope) = current() else {
        return Ok(None);
    };
    if scope.target["host"] != "tmux" {
        return Ok(None);
    }
    let owner = scope.target["owner"].as_str().ok_or("missing owner")?;
    let pane = epoch().ok_or("epoch not observed")?.physical[3].clone();
    Ok(Some((pane, condition(owner, "1")?)))
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
    (scope.pin)(Some(record.clone()), pin_key(&record)?)?;
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
        if self.target["owner"] != target {
            return false;
        }
        #[cfg(unix)]
        if epoch().is_none() {
            let Some(pin) =
                crate::services::tui_input::submission_tmux::guarded::PaneIdentity::observe(target)
            else {
                return false;
            };
            if observe_pin(target, &pin).is_err() {
                return false;
            }
        }
        epoch().is_some()
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
        if hold.may_requeue() {
            self.known_before.store(true, Ordering::SeqCst);
        }
        let value = serde_json::json!({"target":hold.target,"reason":hold.reason,"phase":format!("{:?}",hold.phase),"detail":hold.detail,"epoch":epoch()});
        if let Err(reason) = (self.hold)(value) {
            *self.refused.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(submission_fence::PreSubmitPersistenceRefused { reason });
        }
    }
}
#[cfg(all(test, unix))]
pub(crate) fn capture_scope<R>(
    provider: String,
    target: serde_json::Value,
    channel: u64,
    commit: Commit,
    pin: Pin,
    hold: Hold,
    run: impl FnOnce() -> R,
) -> (R, Option<NotSentProof>) {
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
        allowed: AtomicBool::new(false),
        known_before: AtomicBool::new(false),
        keyed: AtomicBool::new(false),
        holder: Default::default(),
        epoch: Mutex::new(None),
        refused: Mutex::new(None),
    });
    let _restore = Restore(CURRENT.with(|v| v.replace(Some(scope.clone()))));
    if scope.target["host"] == "tmux"
        && let Some(session) = scope.target["owner"].as_str()
    {
        let key = crate::services::tui_input::writer_lease::key("tmux", session);
        let _ = scope.holder.hold(&key);
    }
    if scope.target["host"] == "tmux"
        && let Some(session) = scope.target["owner"].as_str()
        && let Some(pin) =
            crate::services::tui_input::submission_tmux::guarded::PaneIdentity::observe(session)
    {
        let _ = observe_pin(session, &pin);
    }
    let result = submission_fence::with_scope(Some(scope.clone()), || {
        guarded_outcome::with_owner(Some(scope.clone()), run)
    });
    let proof = (!scope.allowed.load(Ordering::SeqCst)
        && !scope.keyed.load(Ordering::SeqCst)
        && scope.known_before.load(Ordering::SeqCst))
    .then(|| scope.target.get("_attempt").cloned())
    .flatten()
    .map(|attempt| NotSentProof {
        attempt,
        reason: scope
            .refused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or_else(
                || "preparation held before payload".into(),
                |e| e.reason.clone(),
            ),
    });
    (result, proof)
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
    capture_scope(provider, target, channel, commit, pin, hold, run).0
}

pub(crate) fn argv_payload<R>(
    context: &crate::services::tui_prompt_dedupe::binding_context::BindingContext,
    send: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    crate::services::tui_input::writer_lease::admits("tmux", &context.tmux_session)?;
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
        scope.gate_result((scope.commit)(None))?;
    }
    send()
}

impl Scope {
    fn gate_result(&self, result: Result<(), String>) -> Result<(), String> {
        match &result {
            Ok(()) => {
                self.allowed.store(true, Ordering::SeqCst);
            }
            Err(reason) => self.refuse(reason.clone()),
        }
        result
    }
    fn refuse(&self, reason: String) {
        self.known_before.store(true, Ordering::SeqCst);
        *self.refused.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(submission_fence::PreSubmitPersistenceRefused { reason });
    }
    fn ready(&self) -> Result<(), ReadyRefusal> {
        if self.target["host"] != "tmux" {
            return (self.pin)(None, "herdr:conservative-writer".into())
                .map_err(ReadyRefusal::Lease);
        }
        #[cfg(unix)]
        {
            let owner = self.target["owner"].as_str().unwrap_or_default();
            let observed =
                crate::services::tui_input::submission_tmux::guarded::PaneIdentity::observe(owner)
                    .ok_or_else(|| ReadyRefusal::Target("pane identity unavailable".into()))?;
            let live = crate::services::tui_input::target_epoch::LiveEpoch::observe(
                &self.provider,
                &self.target,
                &observed,
            )
            .map_err(ReadyRefusal::Target)?
            .record();
            if epoch().as_ref() != Some(&live) {
                return Err(ReadyRefusal::Target("pinned pane identity changed".into()));
            }
            let key = pin_key(&live).map_err(ReadyRefusal::Lease)?;
            (self.pin)(Some(live), key).map_err(ReadyRefusal::Lease)
        }
        #[cfg(not(unix))]
        Err(ReadyRefusal::Target("live epoch unsupported".into()))
    }
}
#[cfg(unix)]
fn pin_key(record: &PaneEpoch) -> Result<String, String> {
    serde_json::to_string(&(
        &record.host,
        &record.endpoint,
        &record.physical[0],
        &record.physical[3],
        &record.physical[4],
    ))
    .map_err(|e| e.to_string())
}
#[cfg(test)]
pub(crate) struct NotSentProof {
    attempt: serde_json::Value,
    reason: String,
}
#[cfg(test)]
impl NotSentProof {
    pub(crate) fn into_reason(self, key: &serde_json::Value) -> Result<String, String> {
        if self.attempt != *key {
            return Err("NotSent proof belongs to another attempt".into());
        }
        Ok(self.reason)
    }
}
