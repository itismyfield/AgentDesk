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

pub(crate) fn register_claude_pane(tmux: &str, channel: u64, binding: TuiRuntimeBinding) {
    let launch = binding.session_id.clone();
    note_claude_pane_registration(tmux, launch.as_deref(), false);
    let registered = register_rehydrated_tmux_runtime_binding("claude", tmux, channel, binding);
    note_claude_pane_registration(tmux, launch.as_deref(), registered);
}

pub(crate) fn note_claude_pane_registration(tmux: &str, launch: Option<&str>, ok: bool) {
    let Some(launch) = launch.filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let nonce = match binding_context::observe_spawn_nonce_marker(tmux) {
        SpawnNonceMarker::Known(nonce) => Some(nonce),
        _ => None,
    };
    let context = nonce
        .as_deref()
        .and_then(|n| binding_context::pane_context(tmux, n));
    let key = (tmux.to_owned(), nonce);
    let mut failed = FAILED_PANES.lock().unwrap_or_else(|p| p.into_inner());
    if !ok {
        let pane = failed.entry(key).or_default();
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
        return;
    }
    let Some(pane) = failed.get(&key) else { return };
    #[cfg(test)]
    if BLOCK_ALIAS.get() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let ready = pane.launch == launch
        && state.runtime_by_tmux.get(tmux).is_some_and(|b| {
            b.value.runtime_kind == RuntimeHandoffKind::ClaudeTui
                && b.value.session_id.as_deref() == Some(launch)
        })
        && pane.aliases.iter().all(|alias| {
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
        failed.remove(&key);
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
    failed
        .values()
        .any(|pane| pane.aliases.contains(command.trim()))
}
