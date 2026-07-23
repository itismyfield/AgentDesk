//! Durable status-card reconciliation for #3607.
//!
//! This sidecar owns only terminal UI status-card edits after the terminal body
//! was already delivered. It deliberately does not read-modify-write inflight
//! response fields or delivery-record frontiers.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};
use serenity::{ChannelId, MessageId};

use super::inflight::{self, InflightTurnState};
use super::{SharedData, http, runtime_store};
use crate::services::provider::ProviderKind;

const SWEEP_INTERVAL_SECS: u64 = 15;
const EDIT_RETRY_GIVE_UP_GRACE_SECS: i64 = (SWEEP_INTERVAL_SECS as i64) * 2;

static SWEEPER_STARTED: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::services::discord) struct TerminalUiObligation {
    #[serde(default)]
    pub(in crate::services::discord) channel_id: u64,
    #[serde(default)]
    pub(in crate::services::discord) provider: String,
    #[serde(default)]
    pub(in crate::services::discord) status_message_id: u64,
    #[serde(default)]
    pub(in crate::services::discord) generation_mtime_ns: i64,
    #[serde(default)]
    pub(in crate::services::discord) pending_state_text: String,
    #[serde(default)]
    pub(in crate::services::discord) completion_text: String,
    #[serde(default)]
    pub(in crate::services::discord) deadline_text: String,
    #[serde(default)]
    pub(in crate::services::discord) created_unix: i64,
    #[serde(default)]
    pub(in crate::services::discord) deadline_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TerminalUiReconcileAction {
    Complete,
    Deadline,
    Wait,
}

#[derive(Debug)]
enum TerminalUiSessionLookup {
    Current(TerminalUiSessionSnapshot),
    Stale(&'static str),
    Missing(&'static str),
}

#[derive(Debug)]
struct TerminalUiSessionSnapshot {
    tmux_session_name: String,
    output_path: Option<String>,
    current_offset: u64,
}

pub(in crate::services::discord) fn terminal_ui_obligation_now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub(in crate::services::discord) fn terminal_ui_obligation_generation_is_current(
    obligation_generation_mtime_ns: i64,
    current_generation_mtime_ns: i64,
) -> bool {
    obligation_generation_mtime_ns != 0
        && current_generation_mtime_ns != 0
        && obligation_generation_mtime_ns == current_generation_mtime_ns
}

pub(in crate::services::discord) fn terminal_ui_reconcile_action(
    pane_idle: bool,
    now_unix: i64,
    deadline_unix: i64,
) -> TerminalUiReconcileAction {
    if pane_idle {
        TerminalUiReconcileAction::Complete
    } else if now_unix >= deadline_unix {
        TerminalUiReconcileAction::Deadline
    } else {
        TerminalUiReconcileAction::Wait
    }
}

pub(in crate::services::discord) fn terminal_ui_generation_mtime_for_inflight(
    state: &InflightTurnState,
) -> i64 {
    state
        .last_watcher_relayed_generation_mtime_ns
        .filter(|generation| *generation != 0)
        .or_else(|| {
            state
                .tmux_session_name
                .as_deref()
                .map(read_tmux_generation_mtime_ns)
                .filter(|generation| *generation != 0)
        })
        .unwrap_or_else(|| runtime_store::process_generation() as i64)
}

#[allow(dead_code)] // #3607 public sidecar API; production currently sweeps via list_obligations.
pub(in crate::services::discord) fn read_obligation(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<TerminalUiObligation> {
    let root = runtime_store::discord_terminal_ui_obligations_root()?;
    read_obligation_in_root(&root, provider.as_str(), channel_id)
}

pub(in crate::services::discord) fn list_obligations() -> Vec<TerminalUiObligation> {
    let Some(root) = runtime_store::discord_terminal_ui_obligations_root() else {
        return Vec::new();
    };
    list_obligations_in_root(&root)
}

#[allow(dead_code)] // #3607 public sidecar API; production currently clears by stored provider key.
pub(in crate::services::discord) fn clear_obligation(
    provider: &ProviderKind,
    channel_id: u64,
) -> bool {
    clear_obligation_by_key(provider.as_str(), channel_id)
}

pub(crate) fn spawn_terminal_ui_obligation_sweeper(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
) {
    let started_key = provider.as_str().to_string();
    {
        let mut started = SWEEPER_STARTED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !started.insert(started_key) {
            return;
        }
    }

    super::task_supervisor::spawn_observed("terminal_ui_obligation_sweeper", async move {
        let mut interval = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
        loop {
            interval.tick().await;
            sweep_terminal_ui_obligations(&http, &shared, &provider).await;
        }
    });
}

async fn sweep_terminal_ui_obligations(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let now_unix = terminal_ui_obligation_now_unix();
    for obligation in list_obligations()
        .into_iter()
        .filter(|obligation| obligation.provider == provider.as_str())
    {
        reconcile_terminal_ui_obligation(http, shared, provider, obligation, now_unix).await;
    }
}

async fn reconcile_terminal_ui_obligation(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    obligation: TerminalUiObligation,
    now_unix: i64,
) {
    if obligation.channel_id == 0 || obligation.status_message_id == 0 {
        let _ = clear_obligation_by_key(&obligation.provider, obligation.channel_id);
        return;
    }
    let channel_id = ChannelId::new(obligation.channel_id);
    let lookup = resolve_terminal_ui_session(shared, provider, channel_id, &obligation).await;
    let snapshot = match lookup {
        TerminalUiSessionLookup::Current(snapshot) => snapshot,
        TerminalUiSessionLookup::Stale(reason) => {
            if clear_obligation_by_key(&obligation.provider, obligation.channel_id) {
                tracing::warn!(
                    provider = %obligation.provider,
                    channel_id = obligation.channel_id,
                    status_message_id = obligation.status_message_id,
                    reason,
                    "cleared stale terminal UI obligation without editing status card"
                );
            }
            return;
        }
        TerminalUiSessionLookup::Missing(reason) => {
            if now_unix
                >= obligation
                    .deadline_unix
                    .saturating_add(EDIT_RETRY_GIVE_UP_GRACE_SECS)
            {
                if clear_obligation_by_key(&obligation.provider, obligation.channel_id) {
                    tracing::warn!(
                        provider = %obligation.provider,
                        channel_id = obligation.channel_id,
                        status_message_id = obligation.status_message_id,
                        reason,
                        "cleared terminal UI obligation after reconcile context stayed unavailable"
                    );
                }
            }
            return;
        }
    };

    let pane_idle = terminal_ui_session_ready_for_input(provider, &snapshot);
    let action = terminal_ui_reconcile_action(pane_idle, now_unix, obligation.deadline_unix);
    let Some(content) = (match action {
        TerminalUiReconcileAction::Complete => Some(obligation.completion_text.as_str()),
        TerminalUiReconcileAction::Deadline => Some(obligation.deadline_text.as_str()),
        TerminalUiReconcileAction::Wait => None,
    }) else {
        return;
    };

    super::discord_io::rate_limit_wait(shared, channel_id).await;
    match http::edit_channel_message(
        http,
        channel_id,
        MessageId::new(obligation.status_message_id),
        content,
    )
    .await
    {
        Ok(_) => {
            let _ = clear_obligation_by_key(&obligation.provider, obligation.channel_id);
        }
        Err(error) => {
            tracing::warn!(
                provider = %obligation.provider,
                channel_id = obligation.channel_id,
                status_message_id = obligation.status_message_id,
                action = ?action,
                error = %error,
                "terminal UI obligation status-card edit failed; will retry"
            );
            if now_unix
                >= obligation
                    .deadline_unix
                    .saturating_add(EDIT_RETRY_GIVE_UP_GRACE_SECS)
            {
                let _ = clear_obligation_by_key(&obligation.provider, obligation.channel_id);
            }
        }
    }
}

async fn resolve_terminal_ui_session(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    obligation: &TerminalUiObligation,
) -> TerminalUiSessionLookup {
    if let Some(state) = inflight::load_inflight_state(provider, channel_id.get()) {
        if state.status_message_id != Some(obligation.status_message_id) {
            return TerminalUiSessionLookup::Stale("status_message_id_mismatch");
        }
        let current_generation_mtime_ns = terminal_ui_generation_mtime_for_inflight(&state);
        if !terminal_ui_obligation_generation_is_current(
            obligation.generation_mtime_ns,
            current_generation_mtime_ns,
        ) {
            return TerminalUiSessionLookup::Stale("generation_mismatch");
        }
        let Some(tmux_session_name) = clean_nonempty(state.tmux_session_name.as_deref()) else {
            return TerminalUiSessionLookup::Missing("inflight_missing_tmux_session");
        };
        let output_path = resolve_terminal_ui_output_path(
            shared,
            tmux_session_name,
            state.output_path.as_deref(),
        );
        return TerminalUiSessionLookup::Current(TerminalUiSessionSnapshot {
            tmux_session_name: tmux_session_name.to_string(),
            current_offset: output_path.as_deref().map(output_file_len).unwrap_or(0),
            output_path,
        });
    }

    let Some(tmux_session_name) =
        resolve_terminal_ui_tmux_session(shared, provider, channel_id).await
    else {
        return TerminalUiSessionLookup::Missing("missing_tmux_session");
    };
    let current_generation_mtime_ns = terminal_ui_generation_mtime_for_tmux(&tmux_session_name);
    if !terminal_ui_obligation_generation_is_current(
        obligation.generation_mtime_ns,
        current_generation_mtime_ns,
    ) {
        return TerminalUiSessionLookup::Stale("generation_mismatch");
    }
    let output_path = resolve_terminal_ui_output_path(shared, &tmux_session_name, None);
    TerminalUiSessionLookup::Current(TerminalUiSessionSnapshot {
        tmux_session_name,
        current_offset: output_path.as_deref().map(output_file_len).unwrap_or(0),
        output_path,
    })
}

async fn resolve_terminal_ui_tmux_session(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<String> {
    if let Some(binding) = shared.tmux_watchers.channel_binding(&channel_id) {
        return Some(binding.tmux_session_name);
    }
    if !provider.uses_managed_tmux_backend() {
        return None;
    }
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    }?;
    let tmux_session_name = provider.build_tmux_session_name(&channel_name);
    if crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name)
        || terminal_ui_generation_mtime_for_tmux(&tmux_session_name) != 0
    {
        Some(tmux_session_name)
    } else {
        None
    }
}

fn resolve_terminal_ui_output_path(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    inflight_output_path: Option<&str>,
) -> Option<String> {
    clean_nonempty(inflight_output_path)
        .map(str::to_string)
        .or_else(|| shared.tmux_watchers.watcher_output_path(tmux_session_name))
        .or_else(|| {
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
                .map(|binding| binding.relay_output_path().to_string())
        })
        .or_else(|| {
            let (output_path, _) = super::turn_bridge::tmux_runtime_paths(tmux_session_name);
            Some(output_path)
        })
}

fn terminal_ui_session_ready_for_input(
    provider: &ProviderKind,
    snapshot: &TerminalUiSessionSnapshot,
) -> bool {
    let runtime_binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
        &snapshot.tmux_session_name,
    );
    let runtime_kind = runtime_binding
        .as_ref()
        .map(|binding| binding.runtime_kind)
        .or_else(|| {
            crate::services::tmux_common::resolve_tmux_runtime_kind_marker(
                &snapshot.tmux_session_name,
            )
        });
    if let Some(output_path) = snapshot.output_path.as_deref()
        && let Some(ready) = crate::services::tui_turn_state::jsonl_ready_for_input(
            provider,
            runtime_kind,
            Path::new(output_path),
            Some(snapshot.current_offset),
        )
    {
        return ready.is_ready();
    }
    crate::services::provider::tmux_session_fallback_ready_for_input(
        &snapshot.tmux_session_name,
        provider,
        runtime_kind,
    )
    .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
}

fn terminal_ui_generation_mtime_for_tmux(tmux_session_name: &str) -> i64 {
    let generation_mtime_ns = read_tmux_generation_mtime_ns(tmux_session_name);
    if generation_mtime_ns != 0 {
        generation_mtime_ns
    } else {
        runtime_store::process_generation() as i64
    }
}

#[cfg(unix)]
fn read_tmux_generation_mtime_ns(tmux_session_name: &str) -> i64 {
    super::tmux::read_generation_file_mtime_ns(tmux_session_name)
}

#[cfg(not(unix))]
fn read_tmux_generation_mtime_ns(_tmux_session_name: &str) -> i64 {
    0
}

fn output_file_len(output_path: &str) -> u64 {
    fs::metadata(output_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn clean_nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn obligation_path_in_root(root: &Path, provider: &str, channel_id: u64) -> PathBuf {
    root.join(provider).join(format!("{channel_id}.json"))
}

#[cfg(test)]
fn write_obligation_in_root(root: &Path, obligation: &TerminalUiObligation) -> Result<(), String> {
    let json = serde_json::to_string_pretty(obligation)
        .map_err(|error| format!("serialize terminal UI obligation: {error}"))?;
    runtime_store::atomic_write(
        &obligation_path_in_root(root, &obligation.provider, obligation.channel_id),
        &json,
    )
}

fn read_obligation_in_root(
    root: &Path,
    provider: &str,
    channel_id: u64,
) -> Option<TerminalUiObligation> {
    let path = obligation_path_in_root(root, provider, channel_id);
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn list_obligations_in_root(root: &Path) -> Vec<TerminalUiObligation> {
    let Ok(provider_entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut obligations = Vec::new();
    for provider_entry in provider_entries.filter_map(Result::ok) {
        let provider_path = provider_entry.path();
        if !provider_path.is_dir() {
            continue;
        }
        let Ok(channel_entries) = fs::read_dir(provider_path) else {
            continue;
        };
        for channel_entry in channel_entries.filter_map(Result::ok) {
            let path = channel_entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Ok(data) = fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(obligation) = serde_json::from_str::<TerminalUiObligation>(&data) {
                obligations.push(obligation);
            }
        }
    }
    obligations.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then(left.channel_id.cmp(&right.channel_id))
    });
    obligations
}

fn clear_obligation_by_key(provider: &str, channel_id: u64) -> bool {
    let Some(root) = runtime_store::discord_terminal_ui_obligations_root() else {
        return false;
    };
    clear_obligation_in_root(&root, provider, channel_id)
}

fn clear_obligation_in_root(root: &Path, provider: &str, channel_id: u64) -> bool {
    let path = obligation_path_in_root(root, provider, channel_id);
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::warn!(
                provider,
                channel_id,
                error = %error,
                "failed to clear terminal UI obligation sidecar"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_obligation(channel_id: u64) -> TerminalUiObligation {
        TerminalUiObligation {
            channel_id,
            provider: ProviderKind::Claude.as_str().to_string(),
            status_message_id: 456,
            generation_mtime_ns: 789,
            pending_state_text: "pending".to_string(),
            completion_text: "complete".to_string(),
            deadline_text: "deadline".to_string(),
            created_unix: 1000,
            deadline_unix: 1600,
        }
    }

    #[test]
    fn obligation_store_round_trips_lists_and_clears() {
        let temp = tempfile::tempdir().expect("tempdir");
        let obligation = sample_obligation(123);

        write_obligation_in_root(temp.path(), &obligation).expect("write obligation");

        assert_eq!(
            read_obligation_in_root(temp.path(), ProviderKind::Claude.as_str(), 123),
            Some(obligation.clone())
        );
        assert_eq!(list_obligations_in_root(temp.path()), vec![obligation]);
        assert!(clear_obligation_in_root(
            temp.path(),
            ProviderKind::Claude.as_str(),
            123
        ));
        assert!(read_obligation_in_root(temp.path(), ProviderKind::Claude.as_str(), 123).is_none());
        assert!(list_obligations_in_root(temp.path()).is_empty());
    }

    #[test]
    fn terminal_ui_obligation_generation_match_requires_nonzero_same_generation() {
        assert!(terminal_ui_obligation_generation_is_current(10, 10));
        assert!(!terminal_ui_obligation_generation_is_current(10, 11));
        assert!(!terminal_ui_obligation_generation_is_current(0, 0));
        assert!(!terminal_ui_obligation_generation_is_current(10, 0));
    }

    #[test]
    fn terminal_ui_reconcile_action_prefers_complete_then_deadline_then_wait() {
        assert_eq!(
            terminal_ui_reconcile_action(true, 10, 10),
            TerminalUiReconcileAction::Complete
        );
        assert_eq!(
            terminal_ui_reconcile_action(false, 10, 10),
            TerminalUiReconcileAction::Deadline
        );
        assert_eq!(
            terminal_ui_reconcile_action(false, 9, 10),
            TerminalUiReconcileAction::Wait
        );
    }
}
