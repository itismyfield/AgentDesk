use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{
    self, CapturedContext, HookBindingEnvelope, SpawnNonceMarker,
};
use std::collections::HashSet;

type Pane = (String, Option<String>);
#[derive(Default)]
struct UnreadyPane {
    launch: String,
    preferred: Option<String>,
    aliases: HashSet<String>,
}
static FAILED_PANES: LazyLock<Mutex<HashMap<Pane, UnreadyPane>>> = LazyLock::new(Default::default);
#[cfg(test)]
thread_local! { pub(crate) static BLOCK_ALIAS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
thread_local! { pub(crate) static BEFORE_COMPLETE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }

pub(crate) fn register_claude_pane(tmux: &str, channel: u64, binding: TuiRuntimeBinding) {
    register_claude_pane_with(tmux, channel, binding, Record::Stat);
}

pub(crate) fn register_launched_claude_pane(
    tmux: &str,
    channel: u64,
    binding: TuiRuntimeBinding,
) -> bool {
    register_claude_pane_with_cause(tmux, channel, binding, Record::Stat, CauseSource::Launch)
        .is_some_and(Persisted::published)
}

/// A restore names how the pane's binding is logged; see `Record`. `None` when nothing was published;
/// an unpublished `Persisted` when the pane's pin refused it.
pub(crate) fn register_claude_pane_with(
    tmux: &str,
    channel: u64,
    binding: TuiRuntimeBinding,
    record: Record,
) -> Option<Persisted> {
    register_claude_pane_with_cause(tmux, channel, binding, record, CauseSource::Observed)
}

fn register_claude_pane_with_cause(
    tmux: &str,
    channel: u64,
    binding: TuiRuntimeBinding,
    record: Record,
    cause: CauseSource,
) -> Option<Persisted> {
    let key = pane_key(tmux);
    begin_pane_registration(&key, &binding);
    let registered = crate::services::tmux_common::with_tmux_source_authority(tmux, |authority| {
        let cause = match cause {
            CauseSource::Launch => launch_cause(authority, channel, &binding)?,
            other => other,
        };
        register_rehydrated_under_source_authority(
            authority, "claude", channel, binding, record, cause,
        )
    });
    #[cfg(test)]
    if let Some(complete) = BEFORE_COMPLETE.with_borrow_mut(Option::take) {
        complete();
    }
    finish_registration(&key, registered.is_some_and(Persisted::published));
    registered
}

// A launch observation uses only the current execution's matching immutable context.
// Refusing a stale selector before append preserves the current nonce's first launch record.
fn launch_cause(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    channel: u64,
    binding: &TuiRuntimeBinding,
) -> Option<CauseSource> {
    let tmux = authority.session();
    let nonce = match binding_context::observe_spawn_nonce_marker(tmux) {
        SpawnNonceMarker::Known(nonce) => nonce,
        SpawnNonceMarker::Absent => return Some(CauseSource::Observed),
        SpawnNonceMarker::Unreadable => return None,
    };
    match binding_context::context_presence("claude", &nonce) {
        binding_context::ContextPresence::Absent => return Some(CauseSource::Observed),
        binding_context::ContextPresence::Unknown => return None,
        binding_context::ContextPresence::Present => {}
    }
    let context = binding_context::pane_context(tmux, &nonce)?;
    let cause = (binding.runtime_kind == RuntimeHandoffKind::ClaudeTui
        && context.channel_id == Some(channel)
        && context.owner_runtime_root == crate::services::tmux_common::current_tmux_owner_marker()
        && binding.session_id.as_deref().is_some_and(|session| {
            !session.trim().is_empty()
                && context.expected_native_session_id.as_deref() == Some(session)
        })
        && matches!(context.launch_mode.as_str(), "fresh" | "resume"))
    .then_some(CauseSource::Launch);
    #[cfg(test)]
    if cause.is_some()
        && crate::services::claude_tui::source_verify::n2b_mutant("clear-launch-observed")
    {
        return Some(CauseSource::Observed);
    }
    cause
}

/// `register_claude_pane_with` for a caller that holds the pane's source authority across its
/// own judgment, so the registration and its completion land in that same hold.
pub(crate) fn register_claude_pane_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    channel: u64,
    binding: TuiRuntimeBinding,
    record: Record,
) -> Option<Persisted> {
    let key = pane_key(authority.session());
    begin_pane_registration(&key, &binding);
    let registered = register_rehydrated_under_source_authority(
        authority,
        "claude",
        channel,
        binding,
        record,
        CauseSource::Observed,
    );
    finish_under_authority(&key, registered.is_some_and(Persisted::published));
    registered
}

fn begin_pane_registration(key: &Pane, binding: &TuiRuntimeBinding) {
    if let Some(launch) = binding
        .session_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        begin_registration(key, launch);
    }
}

pub(crate) fn note_claude_pane_registration(tmux: &str, launch: Option<&str>, ok: bool) {
    let Some(launch) = launch.filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let key = pane_key(tmux);
    if ok {
        finish_registration(&key, true);
    } else {
        begin_registration(&key, launch);
    }
}

/// `note_claude_pane_registration(.., true)` for a caller holding the pane's source authority.
pub(crate) fn note_claude_pane_registered_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    launch: Option<&str>,
) {
    if launch.is_some_and(|s| !s.trim().is_empty()) {
        finish_under_authority(&pane_key(authority.session()), true);
    }
}

fn pane_key(tmux: &str) -> Pane {
    let nonce = match binding_context::observe_spawn_nonce_marker(tmux) {
        SpawnNonceMarker::Known(nonce) => Some(nonce),
        _ => None,
    };
    (tmux.to_owned(), nonce)
}

fn begin_registration(key: &Pane, launch: &str) {
    let tmux = key.0.as_str();
    let context = key
        .1
        .as_deref()
        .and_then(|n| binding_context::pane_context(tmux, n));
    let mut failed = FAILED_PANES.lock().unwrap_or_else(|p| p.into_inner());
    let pane = failed.entry(key.clone()).or_default();
    pane.launch = launch.to_owned();
    pane.aliases.insert(launch.to_owned());
    let expected = context.and_then(|c| c.expected_native_session_id);
    pane.aliases.extend(expected.clone());
    let state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let newest = state
        .tmux_by_provider_session
        .iter()
        .filter(|(k, v)| k.provider == "claude" && v.value == tmux && k.key != launch)
        .max_by_key(|(_, v)| v.recorded_at)
        .map(|(k, _)| k.key.clone());
    pane.preferred = newest.or_else(|| pane.preferred.clone()).or(expected);
    pane.aliases.extend(
        state
            .tmux_by_provider_session
            .iter()
            .filter(|(k, v)| k.provider == "claude" && v.value == tmux)
            .map(|(k, _)| k.key.clone()),
    );
}

fn finish_registration(key: &Pane, ok: bool) {
    crate::services::tmux_common::with_tmux_source_authority(&key.0, |_| {
        finish_under_authority(key, ok);
    });
}

fn finish_under_authority(key: &Pane, ok: bool) {
    let tmux = key.0.as_str();
    let mut failed = FAILED_PANES.lock().unwrap_or_else(|p| p.into_inner());
    match binding_context::observe_spawn_nonce_marker(tmux) {
        SpawnNonceMarker::Known(current) if key.1.as_deref() != Some(current.as_str()) => {
            // A confirmed replacement retires only the captured incarnation.
            failed.remove(key);
            return;
        }
        SpawnNonceMarker::Unreadable => return,
        SpawnNonceMarker::Absent if key.1.is_some() => return,
        _ => {}
    }
    if !ok {
        return;
    }
    let Some(pane) = failed.get(key) else { return };
    #[cfg(test)]
    if BLOCK_ALIAS.get() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let ready = state.runtime_by_tmux.get(tmux).is_some_and(|b| {
        b.value.runtime_kind == RuntimeHandoffKind::ClaudeTui
            && b.value.session_id.as_deref().is_some_and(|session| {
                state
                    .tmux_by_provider_session
                    .get(&PromptKey::new("claude", session))
                    .is_some_and(|m| m.value == tmux)
            })
    }) && pane.aliases.iter().all(|alias| {
        state
            .tmux_by_provider_session
            .get(&PromptKey::new("claude", alias))
            .is_none_or(|m| m.value == tmux)
    });
    if ready {
        // Cached commands stay newer than the launch selector for existing hook waiters.
        for alias in
            std::iter::once(&pane.launch)
                .chain(pane.aliases.iter().filter(|alias| {
                    *alias != &pane.launch && Some(*alias) != pane.preferred.as_ref()
                }))
                .chain(pane.preferred.as_ref())
        {
            state.tmux_by_provider_session.insert(
                PromptKey::new("claude", alias),
                TimedValue {
                    value: tmux.to_owned(),
                    recorded_at: Instant::now(),
                },
            );
        }
        failed.remove(key);
    }
}

pub(crate) fn pane_registration_failed(
    command: &str,
    envelope: Option<&HookBindingEnvelope>,
) -> bool {
    let failed = FAILED_PANES.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(HookBindingEnvelope {
        context: CapturedContext::Captured(ctx),
        ..
    }) = envelope
        && ctx.schema == 1
        && ctx.provider == "claude"
    {
        return failed.contains_key(&(ctx.tmux_session.clone(), Some(ctx.execution_nonce.clone())));
    }
    let state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let mapped = state
        .tmux_by_provider_session
        .get(&PromptKey::new("claude", command.trim()));
    failed.iter().any(|((tmux, _), pane)| {
        pane.aliases.contains(command.trim()) && mapped.is_none_or(|m| &m.value == tmux)
    })
}
