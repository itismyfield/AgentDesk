use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::session_host::HostLiveness;
use crate::services::tui_prompt_dedupe as dedupe;
use std::collections::HashSet;
use std::sync::{Mutex, Weak};

pub(super) enum RestoreScope {
    Startup,
    Rehydrated(HashSet<String>),
}

fn missing_watcher(shared: &SharedData, name: &str) -> bool {
    !shared.tmux_watchers.has_live_watcher_handle(name)
        && shared
            .tmux_watchers
            .owner_channel_for_tmux_session(name)
            .is_some_and(|channel| shared.tmux_watchers.channel_binding(&channel).is_none())
}

impl RestoreScope {
    pub(super) fn includes(&self, shared: &SharedData, name: &str) -> bool {
        match self {
            Self::Startup => true,
            Self::Rehydrated(names) => names.contains(name) && missing_watcher(shared, name),
        }
    }

    pub(super) async fn probe(
        &self,
        channel: u64,
        channel_name: &str,
        name: &str,
    ) -> Result<Option<DeadSessionCleanup>, ()> {
        if matches!(self, Self::Startup) {
            return Ok(DeadSessionCleanup::probe(channel, channel_name, name).await);
        }
        let observed = crate::services::tmux_diagnostics::probe_tmux_session_pane_liveness(name);
        // A retry only attaches: no stale-marker clearing or dead-session cleanup.
        (HostLiveness::from(observed.await) == HostLiveness::Live)
            .then_some(None)
            .ok_or(())
    }
}

struct Retry {
    shared: Weak<SharedData>,
    http: Arc<serenity::Http>,
    startup_done: bool,
    discovered: bool,
    consumed: bool,
    #[cfg(all(test, unix))]
    attempts: usize,
    #[cfg(all(test, unix))]
    before_retry: Option<Box<dyn FnOnce() + Send>>,
}

static RETRIES: Mutex<Vec<Retry>> = Mutex::new(Vec::new());

fn register(shared: &Arc<SharedData>, http: &Arc<serenity::Http>) {
    let mut retries = RETRIES.lock().unwrap_or_else(|e| e.into_inner());
    retries.retain(|retry| retry.shared.strong_count() > 0);
    if retries
        .iter()
        .any(|retry| retry.shared.ptr_eq(&Arc::downgrade(shared)))
    {
        return;
    }
    retries.push(Retry {
        shared: Arc::downgrade(shared),
        http: http.clone(),
        startup_done: false,
        discovered: false,
        consumed: false,
        #[cfg(all(test, unix))]
        attempts: 0,
        #[cfg(all(test, unix))]
        before_retry: None,
    });
}

/// Only the boot restore arms a runtime; source discovery alone has no restore authority.
pub(in crate::services::discord) async fn restore_tmux_watchers(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
) {
    let settings = shared.settings.read().await;
    let codex = settings.provider == ProviderKind::Codex && settings.agent.is_none();
    drop(settings);
    if codex {
        register(shared, http);
    }
    super::restore_selected_tmux_watchers(http, shared, &RestoreScope::Startup).await;
    if codex {
        finish(shared, false).await;
    }
}

struct Work {
    http: Arc<serenity::Http>,
    names: HashSet<String>,
    #[cfg(all(test, unix))]
    before_retry: Option<Box<dyn FnOnce() + Send>>,
}

fn take_work(shared: &Arc<SharedData>, discovered: bool) -> Option<Work> {
    let mut retries = RETRIES.lock().unwrap_or_else(|e| e.into_inner());
    retries.retain(|retry| retry.shared.strong_count() > 0);
    let retry = retries
        .iter_mut()
        .find(|retry| retry.shared.ptr_eq(&Arc::downgrade(shared)))?;
    if discovered {
        retry.discovered = true;
    } else {
        retry.startup_done = true;
    }
    if retry.consumed || !retry.startup_done || !retry.discovered {
        return None;
    }
    let owned: Vec<_> = dedupe::runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| {
            shared
                .tmux_watchers
                .owner_channel_for_tmux_session(name)
                .is_some()
        })
        .collect();
    if owned.is_empty() {
        return None;
    }
    retry.consumed = true;
    let names: HashSet<_> = owned
        .into_iter()
        .filter(|name| missing_watcher(shared, name))
        .collect();
    #[cfg(all(test, unix))]
    {
        retry.attempts += names.len();
    }
    Some(Work {
        http: retry.http.clone(),
        names,
        #[cfg(all(test, unix))]
        before_retry: retry.before_retry.take(),
    })
}

async fn finish(shared: &Arc<SharedData>, discovered: bool) {
    let Some(work) = take_work(shared, discovered) else {
        return;
    };
    if work.names.is_empty() {
        return;
    }
    #[cfg(all(test, unix))]
    if let Some(before) = work.before_retry {
        before();
    }
    super::restore_selected_tmux_watchers(
        &work.http,
        shared,
        &RestoreScope::Rehydrated(work.names),
    )
    .await;
}

pub(in crate::services::discord) async fn after_rehydration(shared: &Arc<SharedData>) {
    finish(shared, true).await;
}

#[cfg(all(test, unix))]
pub(in crate::services::discord) fn attempts(shared: &Arc<SharedData>) -> usize {
    RETRIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|retry| retry.shared.ptr_eq(&Arc::downgrade(shared)))
        .map_or(0, |retry| retry.attempts)
}

#[cfg(all(test, unix))]
pub(in crate::services::discord) fn before_retry(
    shared: &Arc<SharedData>,
    action: impl FnOnce() + Send + 'static,
) {
    let mut retries = RETRIES.lock().unwrap_or_else(|e| e.into_inner());
    retries
        .iter_mut()
        .find(|retry| retry.shared.ptr_eq(&Arc::downgrade(shared)))
        .expect("boot restore must arm the fixture")
        .before_retry = Some(Box::new(action));
}

#[cfg(all(test, unix))]
pub(in crate::services::discord) fn begin_for_test(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::Http>,
) {
    register(shared, http);
}

/// A tmux session found not live at startup, with the pane liveness that found it.
pub(super) struct DeadSessionCleanup {
    pub(super) channel_id: u64,
    pub(super) channel_name: String,
    pub(super) session_name: String,
    pub(super) observed: HostLiveness,
}
