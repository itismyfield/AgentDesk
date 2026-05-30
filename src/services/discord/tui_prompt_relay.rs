use std::collections::HashSet;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::SharedData;
use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::claude_tui::hook_server::{HookEventKind, subscribe_hook_events};
use crate::services::provider::{ProviderKind, ReadOutputResult};
use crate::services::tui_prompt_dedupe::{
    ExternalInputRelayLease, ExternalInputRelayOwner, ObservedTuiPrompt,
    extract_prompt_from_hook_payload, observe_prompt_by_provider_session_at,
    subscribe_observed_prompts,
};
use tracing::Instrument;

const SSH_DIRECT_PROMPT_PREVIEW_LIMIT: usize = 1500;
const CODEX_IDLE_ROLLOUT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// #2843: when the background idle relay loop discovers that a session's
/// transcript path changed, scan this many bytes back from EOF (not from EOF
/// itself) so a prompt already written to the freshly-resolved transcript is
/// still observed and its response relayed.
const CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES: u64 = 65_536;
const CODEX_IDLE_PROMPT_ANCHOR_WAIT: Duration = Duration::from_secs(2);
const CODEX_IDLE_PROMPT_ANCHOR_POLL: Duration = Duration::from_millis(100);
static CODEX_IDLE_ROLLOUT_RELAY_STARTED: AtomicBool = AtomicBool::new(false);
static CLAUDE_IDLE_TRANSCRIPT_RELAY_STARTED: AtomicBool = AtomicBool::new(false);
static CLAUDE_IDLE_RESPONSE_TAILS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

struct ClaudeIdleTailGuard {
    tmux_session_name: String,
}

impl Drop for ClaudeIdleTailGuard {
    fn drop(&mut self) {
        CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.tmux_session_name);
    }
}

struct CodexIdleTailDoneGuard {
    tmux_session_name: Option<String>,
    done_tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl Drop for CodexIdleTailDoneGuard {
    fn drop(&mut self) {
        if let Some(tmux_session_name) = self.tmux_session_name.take() {
            let _ = self.done_tx.send(tmux_session_name);
        }
    }
}

pub(super) fn spawn_tui_prompt_relay(shared: Arc<SharedData>, provider: ProviderKind) {
    #[cfg(unix)]
    if matches!(provider, ProviderKind::Codex) {
        spawn_codex_idle_rollout_relay(shared.clone());
    }
    #[cfg(unix)]
    if matches!(provider, ProviderKind::Claude) {
        spawn_claude_idle_transcript_relay(shared.clone());
    }

    let provider_name = provider.as_str().to_string();
    let observer_span = tracing::info_span!(
        "tui_prompt_relay_observer",
        provider = %provider_name
    );
    super::task_supervisor::spawn_observed("tui_prompt_relay_observer", async move {
        let mut hook_rx = subscribe_hook_events();
        let mut observed_rx = subscribe_observed_prompts();
        loop {
            tokio::select! {
                hook_event = hook_rx.recv() => {
                    match hook_event {
                        Ok(event) if event.provider == provider_name
                            && event.kind == HookEventKind::UserPromptSubmit =>
                        {
                            if let Some(prompt) = extract_prompt_from_hook_payload(&event.payload) {
                                let observation = observe_prompt_by_provider_session_at(
                                    &event.provider,
                                    &event.session_id,
                                    &prompt,
                                    event.received_at,
                                );
                                tracing::debug!(
                                    provider = %event.provider,
                                    session_id = %event.session_id,
                                    observation = ?observation,
                                    "observed TUI UserPromptSubmit hook"
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                provider = %provider_name,
                                skipped,
                                "TUI prompt relay lagged hook events"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
                observed = observed_rx.recv() => {
                    match observed {
                        Ok(prompt) if prompt.provider == provider_name => {
                            relay_observed_prompt(&shared, prompt).await;
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                provider = %provider_name,
                                skipped,
                                "TUI prompt relay lagged observed prompt events"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    }.instrument(observer_span));
}

async fn relay_observed_prompt(shared: &Arc<SharedData>, prompt: ObservedTuiPrompt) {
    let Some(channel_id) = owner_channel_for_prompt(shared, &prompt) else {
        tracing::debug!(
            provider = %prompt.provider,
            tmux_session_name = %prompt.tmux_session_name,
            "skipping SSH-direct TUI prompt notify; no Discord channel mapping"
        );
        return;
    };
    let lease = record_observed_external_turn_lease(shared, &prompt, channel_id);
    let Some(registry) = shared.health_registry() else {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            "skipping SSH-direct TUI prompt notify; health registry unavailable"
        );
        return;
    };
    let notify_http = match super::health::resolve_bot_http(registry.as_ref(), "notify").await {
        Ok(http) => http,
        Err((status, body)) => {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                status = %status,
                body = %body,
                "skipping SSH-direct TUI prompt notify; notify bot unavailable"
            );
            return;
        }
    };
    let content = format_ssh_direct_prompt_notification(
        &prompt.provider,
        &prompt.tmux_session_name,
        &prompt.prompt,
    );
    let anchor_message = match channel_id.say(&*notify_http, content).await {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                error = %error,
                "failed to send SSH-direct TUI prompt notify"
            );
            return;
        }
    };
    crate::services::tui_prompt_dedupe::record_prompt_anchor(
        &prompt.provider,
        &prompt.tmux_session_name,
        channel_id.get(),
        anchor_message.id.get(),
    );
    tracing::info!(
        provider = %prompt.provider,
        channel_id = channel_id.get(),
        tmux_session_name = %prompt.tmux_session_name,
        turn_id = lease.turn_id.as_deref().unwrap_or(""),
        session_key = lease.session_key.as_deref().unwrap_or(""),
        relay_owner = lease.relay_owner.as_str(),
        runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
        anchor_message_id = anchor_message.id.get(),
        "SSH-direct TUI prompt notified; runtime relay will handle output without synthetic inflight"
    );

    #[cfg(unix)]
    maybe_spawn_claude_idle_response_tail(shared.clone(), channel_id, &prompt, &lease).await;
}

fn record_observed_external_turn_lease(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
) -> ExternalInputRelayLease {
    let provider = ProviderKind::from_str(&prompt.provider);
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
        &prompt.tmux_session_name,
    );
    let runtime_kind = binding.as_ref().map(|binding| binding.runtime_kind);
    let relay_output_path = binding
        .as_ref()
        .map(|binding| PathBuf::from(binding.relay_output_path()));
    let relay_owner = external_input_relay_owner_for_output(
        shared,
        &prompt.tmux_session_name,
        relay_output_path.as_deref(),
    );
    let session_key = provider.as_ref().map(|provider| {
        super::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            provider,
            &prompt.tmux_session_name,
        )
    });
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some(external_input_turn_id(
            &prompt.provider,
            channel_id,
            &prompt.tmux_session_name,
            prompt.observed_at,
        )),
        session_key,
        relay_owner,
        runtime_kind,
    };
    crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        &prompt.provider,
        &prompt.tmux_session_name,
        lease.clone(),
    );
    tracing::info!(
        provider = %prompt.provider,
        channel_id = channel_id.get(),
        tmux_session_name = %prompt.tmux_session_name,
        turn_id = lease.turn_id.as_deref().unwrap_or(""),
        session_key = lease.session_key.as_deref().unwrap_or(""),
        relay_owner = lease.relay_owner.as_str(),
        runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
        "observed TUI-direct input as already-submitted external turn"
    );
    lease
}

fn record_external_turn_lease_for_output(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    runtime_kind: RuntimeHandoffKind,
    output_path: &Path,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> ExternalInputRelayLease {
    let relay_owner =
        external_input_relay_owner_for_output(shared, tmux_session_name, Some(output_path));
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some(external_input_turn_id(
            provider.as_str(),
            channel_id,
            tmux_session_name,
            observed_at,
        )),
        session_key: Some(super::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            provider,
            tmux_session_name,
        )),
        relay_owner,
        runtime_kind: Some(runtime_kind),
    };
    crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        provider.as_str(),
        tmux_session_name,
        lease.clone(),
    );
    lease
}

fn external_input_turn_id(
    provider: &str,
    channel_id: ChannelId,
    tmux_session_name: &str,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> String {
    format!(
        "external:{}:{}:{}:{}",
        provider.trim(),
        channel_id.get(),
        tmux_session_name.trim(),
        observed_at.timestamp_millis()
    )
}

fn external_input_relay_owner_for_output(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    output_path: Option<&Path>,
) -> ExternalInputRelayOwner {
    let watcher_alive = shared
        .tmux_watchers
        .tmux_session_is_stale(tmux_session_name)
        .is_some_and(|stale| !stale);
    if !watcher_alive {
        return ExternalInputRelayOwner::TuiPromptRelay;
    }

    let watcher_covers_output = match output_path {
        Some(output_path) => shared
            .tmux_watchers
            .watcher_output_path(tmux_session_name)
            .is_some_and(|watcher_path| Path::new(&watcher_path) == output_path),
        None => true,
    };
    if !watcher_covers_output {
        return ExternalInputRelayOwner::TuiPromptRelay;
    }

    if super::session_relay_sink::session_bound_discord_delivery_enabled() {
        ExternalInputRelayOwner::SessionBoundRelay
    } else {
        ExternalInputRelayOwner::TmuxWatcher
    }
}

#[cfg(unix)]
async fn maybe_spawn_claude_idle_response_tail(
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    lease: &ExternalInputRelayLease,
) {
    if !prompt
        .provider
        .trim()
        .eq_ignore_ascii_case(ProviderKind::Claude.as_str())
    {
        return;
    }
    if lease.relay_owner != ExternalInputRelayOwner::TuiPromptRelay {
        tracing::debug!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            "skipping Claude idle response tail; external turn has another relay owner"
        );
        return;
    }
    if super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get()).is_some() {
        return;
    }
    let Some(binding) = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
        &prompt.tmux_session_name,
    ) else {
        tracing::debug!(
            tmux_session_name = %prompt.tmux_session_name,
            "skipping Claude idle response tail; no runtime binding"
        );
        return;
    };
    if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
        return;
    }

    // #2843: resolve the freshest active transcript (the bound output_path can be
    // stale) and only let a non-stale tmux watcher suppress the tail when it
    // actually covers that transcript. Re-registers the binding if it changed.
    let Some(transcript_path) =
        resolve_idle_relay_transcript(&shared, &prompt.tmux_session_name, channel_id, &binding)
    else {
        return;
    };

    // #2843: if the path changed, don't trust the old binding offset (it indexes
    // a different transcript and would replay old output); the timestamp-based
    // resolution still takes precedence, falling back to the fresh EOF.
    let fallback_offset = if Path::new(&binding.output_path) == transcript_path {
        binding.last_offset
    } else {
        claude_tui_rehydrate_start_offset(&transcript_path)
    };
    let start_offset = claude_idle_response_start_offset_after_timestamp(
        &transcript_path,
        prompt.observed_at,
        fallback_offset,
    );
    spawn_claude_idle_response_tail_once(
        shared,
        prompt.tmux_session_name.clone(),
        channel_id,
        transcript_path,
        start_offset,
        lease.clone(),
    );
}

#[cfg(unix)]
fn claude_idle_response_start_offset_after_timestamp(
    transcript_path: &Path,
    turn_started_at: chrono::DateTime<chrono::Utc>,
    fallback_offset: u64,
) -> u64 {
    match crate::services::claude_tui::transcript_tail::claude_transcript_timestamp_at_or_after(
        transcript_path,
        turn_started_at,
    ) {
        Ok(Some(offset)) => offset,
        Ok(None) => normalize_transcript_fallback_offset(transcript_path, fallback_offset),
        Err(error) => {
            tracing::debug!(
                transcript_path = %transcript_path.display(),
                error = %error,
                fallback_offset,
                "Claude idle transcript timestamp scan failed; using fallback offset"
            );
            normalize_transcript_fallback_offset(transcript_path, fallback_offset)
        }
    }
}

#[cfg(unix)]
fn normalize_transcript_fallback_offset(transcript_path: &Path, fallback_offset: u64) -> u64 {
    match std::fs::metadata(transcript_path).map(|metadata| metadata.len()) {
        Ok(file_len) if fallback_offset > file_len => 0,
        _ => fallback_offset,
    }
}

#[cfg(unix)]
fn spawn_claude_idle_response_tail_once(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    transcript_path: PathBuf,
    start_offset: u64,
    lease: ExternalInputRelayLease,
) -> bool {
    {
        let mut active = CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !active.insert(tmux_session_name.clone()) {
            return false;
        }
    }

    let span = tracing::info_span!(
        "claude_idle_response_tail",
        provider = ProviderKind::Claude.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        turn_id = lease.turn_id.as_deref().unwrap_or(""),
        session_key = lease.session_key.as_deref().unwrap_or(""),
        relay_owner = lease.relay_owner.as_str(),
        runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
    );
    super::task_supervisor::spawn_observed(
        "claude_idle_response_tail",
        async move {
            let _tail_guard = ClaudeIdleTailGuard {
                tmux_session_name: tmux_session_name.clone(),
            };
            run_claude_idle_response_tail(
                shared,
                tmux_session_name.clone(),
                channel_id,
                transcript_path,
                start_offset,
                lease,
            )
            .await;
        }
        .instrument(span),
    );
    true
}

#[cfg(unix)]
fn spawn_claude_idle_transcript_relay(shared: Arc<SharedData>) {
    if CLAUDE_IDLE_TRANSCRIPT_RELAY_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    super::task_supervisor::spawn_observed("claude_idle_transcript_relay", async move {
        let mut next_rehydrate = tokio::time::Instant::now();
        loop {
            let now = tokio::time::Instant::now();
            if now >= next_rehydrate {
                rehydrate_existing_claude_tui_bindings();
                next_rehydrate = now + CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL;
            }
            for (tmux_session_name, binding) in
                crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
                    RuntimeHandoffKind::ClaudeTui,
                )
            {
                let Some(channel_id) = owner_channel_for_tmux_session(&shared, &tmux_session_name)
                else {
                    continue;
                };
                if super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get())
                    .is_some()
                {
                    continue;
                }

                // #2843: resolve the freshest transcript (re-registering the
                // binding if the bound path was stale) and apply the corrected
                // watcher guard, instead of skipping on tmux_session_is_stale
                // alone — a watcher pointed at a missing/stale file is non-stale
                // by heartbeat yet does not relay direct-TUI output.
                let Some(transcript_path) = resolve_idle_relay_transcript(
                    &shared,
                    &tmux_session_name,
                    channel_id,
                    &binding,
                ) else {
                    continue;
                };
                let path_changed = Path::new(&binding.output_path) != transcript_path;
                let scan_offset = if path_changed {
                    // #2843 (codex P1): path changed — scan a bounded lookback
                    // instead of starting at EOF, so a prompt already written to
                    // the freshly-resolved transcript is still found (the
                    // observed-prompt path uses timestamp recovery, but this
                    // background-loop half must not miss the prompt it recovers).
                    claude_tui_rehydrate_start_offset(&transcript_path)
                        .saturating_sub(CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES)
                } else {
                    binding.last_offset
                };
                // #2843 (codex round-2 P1): the lookback window can hold several
                // finished turns; relaying the first would re-relay an old turn.
                // On a path change select the NEWEST prompt in the window (the
                // just-typed one); unchanged-path incremental tailing keeps
                // first-prompt semantics so it never skips a queued prompt.
                let scan_result = if path_changed {
                    scan_claude_idle_transcript_for_last_prompt(&transcript_path, scan_offset)
                } else {
                    scan_claude_idle_transcript_for_prompt(&transcript_path, scan_offset)
                };
                let scan = match scan_result {
                    Ok(scan) => scan,
                    Err(error) => {
                        tracing::debug!(
                            tmux_session_name = %tmux_session_name,
                            transcript_path = %transcript_path.display(),
                            error = %error,
                            "Claude idle transcript relay scan skipped"
                        );
                        continue;
                    }
                };

                match scan {
                    ClaudeIdleTranscriptScan::NoPrompt { offset } => {
                        if offset != scan_offset {
                            advance_claude_tmux_runtime_binding_offset(
                                &tmux_session_name,
                                &transcript_path,
                                offset,
                            );
                        }
                    }
                    ClaudeIdleTranscriptScan::Prompt {
                        prompt,
                        line_end_offset,
                        ..
                    } => {
                        let observed_at = chrono::Utc::now();
                        let observation =
                            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_at(
                                ProviderKind::Claude.as_str(),
                                &tmux_session_name,
                                &prompt,
                                observed_at,
                            );
                        tracing::info!(
                            tmux_session_name = %tmux_session_name,
                            channel_id = channel_id.get(),
                            observation = ?observation,
                            "Claude idle transcript relay observed prompt"
                        );
                        advance_claude_tmux_runtime_binding_offset(
                            &tmux_session_name,
                            &transcript_path,
                            line_end_offset,
                        );
                        if !claude_idle_prompt_observation_should_tail_response(observation) {
                            continue;
                        }
                        let lease = record_external_turn_lease_for_output(
                            &shared,
                            &ProviderKind::Claude,
                            channel_id,
                            &tmux_session_name,
                            binding.runtime_kind,
                            &transcript_path,
                            observed_at,
                        );
                        tracing::info!(
                            tmux_session_name = %tmux_session_name,
                            channel_id = channel_id.get(),
                            turn_id = lease.turn_id.as_deref().unwrap_or(""),
                            session_key = lease.session_key.as_deref().unwrap_or(""),
                            relay_owner = lease.relay_owner.as_str(),
                            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                            "Claude idle transcript relay selected external turn owner"
                        );
                        if lease.relay_owner == ExternalInputRelayOwner::TuiPromptRelay {
                            spawn_claude_idle_response_tail_once(
                                shared.clone(),
                                tmux_session_name.clone(),
                                channel_id,
                                transcript_path,
                                line_end_offset,
                                lease,
                            );
                        } else {
                            tracing::debug!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                observation = ?observation,
                                relay_owner = lease.relay_owner.as_str(),
                                "Claude idle transcript relay yielded response tail"
                            );
                        }
                    }
                }
            }

            tokio::time::sleep(CODEX_IDLE_ROLLOUT_POLL_INTERVAL).await;
        }
    }
    .instrument(tracing::info_span!(
        "claude_idle_transcript_relay",
        provider = ProviderKind::Claude.as_str(),
        runtime_kind = RuntimeHandoffKind::ClaudeTui.as_str(),
    )));
}

#[cfg(unix)]
fn rehydrate_existing_claude_tui_bindings() {
    let sessions = match crate::services::platform::tmux::list_session_names() {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::debug!(error = %error, "Claude TUI binding rehydrate skipped; tmux sessions unavailable");
            return;
        }
    };

    for tmux_session_name in sessions {
        let existing_binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
            &tmux_session_name,
        );
        let existing_channel =
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&tmux_session_name);
        let fresh_binding = rehydrated_claude_tui_binding_for_tmux_session(&tmux_session_name);
        let channel_id = match resolve_rehydrated_claude_tmux_channel_id(&tmux_session_name)
            .or(existing_channel)
        {
            Some(channel_id) => channel_id,
            None => continue,
        };
        if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name) {
            continue;
        }
        if let (Some(existing), Some(_)) = (&existing_binding, existing_channel) {
            if existing.runtime_kind == RuntimeHandoffKind::ClaudeTui
                && Path::new(&existing.output_path).exists()
                && match fresh_binding.as_ref() {
                    Some(fresh) => claude_tui_runtime_binding_matches_launch(existing, fresh),
                    None => true,
                }
            {
                continue;
            }
        }
        if let Some(fresh) = fresh_binding {
            let should_refresh = match existing_binding.as_ref() {
                Some(existing) => {
                    !claude_tui_runtime_binding_matches_launch(existing, &fresh)
                        || !Path::new(&existing.output_path).exists()
                }
                None => true,
            };
            if should_refresh {
                crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                    ProviderKind::Claude.as_str(),
                    &tmux_session_name,
                    channel_id,
                    fresh.clone(),
                );
                tracing::info!(
                    tmux_session_name = %tmux_session_name,
                    channel_id,
                    transcript_path = %fresh.output_path,
                    last_offset = fresh.last_offset,
                    "rehydrated Claude TUI direct relay binding from launch script"
                );
                continue;
            }
        }
        if let Some(binding) = existing_binding {
            if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
                continue;
            }
            if Path::new(&binding.output_path).exists() {
                crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                    ProviderKind::Claude.as_str(),
                    &tmux_session_name,
                    channel_id,
                    binding.clone(),
                );
                tracing::info!(
                    tmux_session_name = %tmux_session_name,
                    channel_id,
                    transcript_path = %binding.output_path,
                    last_offset = binding.last_offset,
                    "rehydrated Claude TUI direct relay channel binding"
                );
            }
            continue;
        }
    }
}

#[cfg(unix)]
fn claude_tui_runtime_binding_matches_launch(
    existing: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    fresh: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> bool {
    existing.runtime_kind == RuntimeHandoffKind::ClaudeTui
        && existing.output_path == fresh.output_path
        && existing.session_id == fresh.session_id
}

#[cfg(unix)]
fn rehydrated_claude_tui_binding_for_tmux_session(
    tmux_session_name: &str,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let launch_script_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    )?;
    let launch = parse_claude_tui_launch_script(Path::new(&launch_script_path)).ok()?;
    let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &launch.working_dir,
        &launch.session_id,
        None,
    )
    .ok()?;
    if !transcript_path.exists() {
        return None;
    }
    let start_offset = claude_tui_rehydrate_start_offset(&transcript_path);
    Some(crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript_path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(launch.session_id),
        last_offset: start_offset,
        relay_last_offset: None,
    })
}

#[cfg(unix)]
fn transcript_mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// #2843: the working directory and launch-script mtime of a Claude TUI session.
/// The working_dir locates the Claude project directory when the stored
/// binding's transcript path is stale; the launch mtime (session start proxy)
/// discriminates this session's transcripts from older sessions' that share the
/// same cwd.
#[cfg(unix)]
pub(in crate::services::discord) fn claude_tui_launch_context(
    tmux_session_name: &str,
) -> Option<(PathBuf, std::time::SystemTime)> {
    let launch_script_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    )?;
    let launch_mtime = transcript_mtime(Path::new(&launch_script_path));
    let launch = parse_claude_tui_launch_script(Path::new(&launch_script_path)).ok()?;
    Some((launch.working_dir, launch_mtime))
}

/// #2843 multi-session fix: transcripts that authoritatively belong to OTHER
/// live Claude TUI tmux sessions (so the freshest scan never steals them).
/// Three sources, unioned:
///   1. The live watcher's `output_path` for each other session — the ground
///      truth of the transcript that session is *currently* tailing, including
///      after Claude rotated its session_id mid-session (the launch script then
///      holds a stale id, so this is the only source that captures the rotated
///      file). This is what fixes concurrent adk-cc threads swapping each
///      other's rotated transcripts.
///   2. Each other session's launch-script transcript — source of truth for
///      SSH-direct sessions that never register a runtime binding or spawn a
///      relay watcher.
///   3. Other sessions' registered runtime bindings — belt-and-suspenders.
#[cfg(unix)]
pub(in crate::services::discord) fn other_session_claimed_transcripts(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> std::collections::HashSet<PathBuf> {
    let mut claimed: std::collections::HashSet<PathBuf> =
        crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
            RuntimeHandoffKind::ClaudeTui,
        )
        .into_iter()
        .filter(|(other_session, _)| other_session != tmux_session_name)
        .map(|(_, other_binding)| PathBuf::from(other_binding.output_path))
        .collect();
    for entry in shared.tmux_watchers.iter() {
        if entry.key() == tmux_session_name {
            continue;
        }
        let output_path = entry.value().output_path.clone();
        if !output_path.is_empty() {
            claimed.insert(PathBuf::from(output_path));
        }
    }
    if let Ok(sessions) = crate::services::platform::tmux::list_session_names() {
        for other_session in sessions {
            if other_session == tmux_session_name {
                continue;
            }
            if let Some(other_binding) =
                rehydrated_claude_tui_binding_for_tmux_session(&other_session)
            {
                claimed.insert(PathBuf::from(other_binding.output_path));
            }
        }
    }
    claimed
}

/// #2843: resolve the freshest active Claude transcript for a tmux session.
/// The stored runtime binding's `output_path` can be stale — an older session_id
/// the launch script still references, or a missing AgentDesk rollout jsonl —
/// while the live Claude TUI writes its transcript to a newer `<uuid>.jsonl`
/// under the project directory. Compare the bound path (if it exists) against
/// the newest transcript scanned from the launch-script working_dir and return
/// whichever is newest, plus the session_id (UUID stem) to re-register so future
/// Discord-turn recovery and offset advances reconcile against the right path.
#[cfg(unix)]
fn freshest_claude_transcript_for_session(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> Option<(PathBuf, Option<String>)> {
    // #2843 multi-session fix: when the bound (launch-script) transcript still
    // EXISTS, it is the authoritative per-session identity — trust it and do NOT
    // override with a project-newer file. Picking max-by-mtime across the whole
    // project dir was wrong for a cwd shared by several Claude sessions: a
    // *different* session's (or an orphaned older session's) newer transcript
    // gets pulled in, thrashing the binding against launch rehydration (~5s) and
    // mis-tailing relay output. The project scan now only fills in when the
    // bound transcript is genuinely missing (the legitimate stale/rotated-away
    // case), and even then skips transcripts other live sessions claim.
    let bound_path = PathBuf::from(&binding.output_path);
    if bound_path.exists() {
        return Some((bound_path, binding.session_id.clone()));
    }
    // Bound transcript is gone — fall back to the freshest project transcript,
    // excluding files that authoritatively belong to other live Claude TUI tmux
    // sessions (live watcher path + launch-script transcript + registered
    // binding) so we still never steal another session's transcript.
    let claimed_by_other_sessions = other_session_claimed_transcripts(shared, tmux_session_name);
    claude_tui_launch_context(tmux_session_name)
        .and_then(|(cwd, launch_mtime)| {
            crate::services::claude_tui::transcript_tail::latest_claude_transcript_for_cwd(
                &cwd,
                launch_mtime,
                None,
                &claimed_by_other_sessions,
            )
        })
        .map(|path| {
            let session_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string);
            (path, session_id)
        })
}

/// #2843: re-register the runtime binding to a freshly-resolved transcript so
/// later reads, offset advances, and Discord-turn recovery all converge on it.
#[cfg(unix)]
fn refresh_claude_runtime_binding(
    tmux_session_name: &str,
    channel_id: ChannelId,
    transcript_path: &Path,
    session_id: Option<String>,
) {
    crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
        ProviderKind::Claude.as_str(),
        tmux_session_name,
        channel_id.get(),
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript_path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id,
            last_offset: claude_tui_rehydrate_start_offset(transcript_path),
            relay_last_offset: None,
        },
    );
    tracing::info!(
        tmux_session_name = %tmux_session_name,
        channel_id = channel_id.get(),
        transcript_path = %transcript_path.display(),
        "refreshed Claude TUI relay binding to freshest active transcript (#2843)"
    );
}

/// #2843: decide whether the Claude idle relay should tail this session and on
/// which transcript. Returns `Some(path)` to tail, or `None` to skip because a
/// heartbeat-fresh watcher already covers the current transcript. Side effect:
/// re-registers the binding when a fresher transcript is resolved.
///
/// `tmux_session_is_stale` checks only cancel/heartbeat, so a watcher pointed at
/// a missing/stale file reports non-stale and would wrongly suppress relay of
/// direct-TUI output. We only let a non-stale watcher suppress when the binding
/// points at the freshest existing transcript.
#[cfg(unix)]
fn resolve_idle_relay_transcript(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> Option<PathBuf> {
    let (transcript_path, resolved_session_id) =
        freshest_claude_transcript_for_session(shared, tmux_session_name, binding).unwrap_or_else(
            || {
                (
                    PathBuf::from(&binding.output_path),
                    binding.session_id.clone(),
                )
            },
        );

    // #2843 (codex P0): a non-stale watcher may suppress the idle tail ONLY when
    // the watcher itself is tailing the freshest transcript. Comparing the
    // runtime binding's path is wrong — re-registering the binding does not
    // retarget the running watcher, so the binding can be fresh while the
    // watcher still tails a stale/missing file (then the idle tail would be
    // wrongly suppressed and direct-TUI output lost). Use the watcher's own
    // output path.
    let watcher_covers_current_transcript = shared
        .tmux_watchers
        .tmux_session_is_stale(tmux_session_name)
        .is_some_and(|stale| !stale)
        && transcript_path.exists()
        && shared
            .tmux_watchers
            .watcher_output_path(tmux_session_name)
            .is_some_and(|watcher_path| Path::new(&watcher_path) == transcript_path);
    if watcher_covers_current_transcript {
        return None;
    }

    if Path::new(&binding.output_path) != transcript_path {
        refresh_claude_runtime_binding(
            tmux_session_name,
            channel_id,
            &transcript_path,
            resolved_session_id,
        );
    }
    Some(transcript_path)
}

#[cfg(unix)]
fn resolve_rehydrated_claude_tmux_channel_id(tmux_session_name: &str) -> Option<u64> {
    let mut matched: Option<u64> = None;
    for binding in super::settings::list_registered_channel_bindings() {
        if binding.owner_provider != ProviderKind::Claude {
            continue;
        }
        let channel_id_text = binding.channel_id.to_string();
        let mut segments = vec![channel_id_text.as_str()];
        if let Some(fallback_name) = binding
            .fallback_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            segments.push(fallback_name);
        }
        for segment in segments {
            let Some(candidate_channel_id) = rehydrated_claude_channel_id_for_segment(
                tmux_session_name,
                segment,
                binding.channel_id,
            ) else {
                continue;
            };
            if matched.is_some_and(|existing| existing != candidate_channel_id) {
                tracing::warn!(
                    tmux_session_name,
                    channel_id = candidate_channel_id,
                    existing_channel_id = matched.unwrap_or_default(),
                    "Claude TUI rehydrate skipped ambiguous exact session-name match"
                );
                return None;
            }
            matched = Some(candidate_channel_id);
        }
    }
    matched
}

#[cfg(unix)]
fn rehydrated_claude_channel_id_for_segment(
    tmux_session_name: &str,
    segment: &str,
    parent_channel_id: u64,
) -> Option<u64> {
    let base_session_name = ProviderKind::Claude.build_tmux_session_name(segment);
    if base_session_name == tmux_session_name {
        return Some(parent_channel_id);
    }

    let (provider, session_segment) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)?;
    if provider != ProviderKind::Claude {
        return None;
    }
    let (_base_provider, base_segment) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(&base_session_name)?;
    let thread_suffix = session_segment
        .strip_prefix(&base_segment)?
        .strip_prefix("-t")?;
    if thread_suffix.is_empty() || !thread_suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    thread_suffix.parse::<u64>().ok()
}

#[cfg(unix)]
fn claude_tui_rehydrate_start_offset(transcript_path: &Path) -> u64 {
    std::fs::metadata(transcript_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

#[cfg(unix)]
fn advance_claude_tmux_runtime_binding_offset(
    tmux_session_name: &str,
    transcript_path: &Path,
    offset: u64,
) -> bool {
    crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
        tmux_session_name,
        transcript_path.to_str().unwrap_or_default(),
        offset,
    )
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct ClaudeTuiLaunchInfo {
    working_dir: PathBuf,
    session_id: String,
}

#[cfg(unix)]
fn parse_claude_tui_launch_script(path: &Path) -> Result<ClaudeTuiLaunchInfo, String> {
    let script = std::fs::read_to_string(path)
        .map_err(|error| format!("read Claude TUI launch script {}: {error}", path.display()))?;
    parse_claude_tui_launch_script_content(&script)
        .ok_or_else(|| format!("parse Claude TUI launch script {}", path.display()))
}

#[cfg(unix)]
fn parse_claude_tui_launch_script_content(script: &str) -> Option<ClaudeTuiLaunchInfo> {
    let mut working_dir: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    for line in script.lines() {
        let words = shell_words_from_line(line.trim());
        if words.first().is_some_and(|word| word == "cd") {
            if let Some(dir) = words.get(1).filter(|value| !value.trim().is_empty()) {
                working_dir = Some(PathBuf::from(dir));
            }
            continue;
        }
        if !words.first().is_some_and(|word| word == "exec") {
            continue;
        }
        for pair in words.windows(2) {
            if matches!(pair[0].as_str(), "--session-id" | "--resume") && !pair[1].trim().is_empty()
            {
                session_id = Some(pair[1].clone());
                break;
            }
        }
    }
    Some(ClaudeTuiLaunchInfo {
        working_dir: working_dir?,
        session_id: session_id?,
    })
}

#[cfg(unix)]
fn shell_words_from_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut saw_word = false;
    let mut in_single = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            saw_word = true;
            continue;
        }

        if ch.is_whitespace() {
            if saw_word {
                words.push(std::mem::take(&mut current));
                saw_word = false;
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                saw_word = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    saw_word = true;
                }
            }
            _ => {
                current.push(ch);
                saw_word = true;
            }
        }
    }

    if saw_word {
        words.push(current);
    }
    words
}

#[cfg(unix)]
fn spawn_codex_idle_rollout_relay(shared: Arc<SharedData>) {
    if CODEX_IDLE_ROLLOUT_RELAY_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    super::task_supervisor::spawn_observed("codex_idle_rollout_relay", async move {
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut active_tails: HashSet<String> = HashSet::new();

        loop {
            while let Ok(tmux_session_name) = done_rx.try_recv() {
                active_tails.remove(&tmux_session_name);
            }

            for (tmux_session_name, binding) in
                crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
                    RuntimeHandoffKind::CodexTui,
                )
            {
                if active_tails.contains(&tmux_session_name) {
                    continue;
                }
                let Some(channel_id) = owner_channel_for_tmux_session(&shared, &tmux_session_name)
                else {
                    continue;
                };
                if super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id.get())
                    .is_some()
                {
                    continue;
                }

                let rollout_path = PathBuf::from(&binding.output_path);
                let scan =
                    match scan_codex_idle_rollout_for_prompt(&rollout_path, binding.last_offset) {
                        Ok(scan) => scan,
                        Err(error) => {
                            tracing::debug!(
                                tmux_session_name = %tmux_session_name,
                                rollout_path = %rollout_path.display(),
                                error = %error,
                                "codex idle rollout relay scan skipped"
                            );
                            continue;
                        }
                    };

                match scan {
                    CodexIdleRolloutScan::NoPrompt { offset } => {
                        if offset != binding.last_offset {
                            crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                                &tmux_session_name,
                                &binding.output_path,
                                offset,
                            );
                        }
                    }
                    CodexIdleRolloutScan::Prompt {
                        prompt,
                        line_end_offset,
                    } => {
                        let observed_at = chrono::Utc::now();
                        let observation =
                            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_at(
                                ProviderKind::Codex.as_str(),
                                &tmux_session_name,
                                &prompt,
                                observed_at,
                            );
                        tracing::info!(
                            tmux_session_name = %tmux_session_name,
                            channel_id = channel_id.get(),
                            observation = ?observation,
                            "codex idle rollout relay observed prompt"
                        );
                        if !codex_idle_prompt_observation_should_tail_response(observation) {
                            crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                                &tmux_session_name,
                                &binding.output_path,
                                line_end_offset,
                            );
                            continue;
                        }
                        let lease = record_external_turn_lease_for_output(
                            &shared,
                            &ProviderKind::Codex,
                            channel_id,
                            &tmux_session_name,
                            binding.runtime_kind,
                            &rollout_path,
                            observed_at,
                        );
                        tracing::info!(
                            tmux_session_name = %tmux_session_name,
                            channel_id = channel_id.get(),
                            turn_id = lease.turn_id.as_deref().unwrap_or(""),
                            session_key = lease.session_key.as_deref().unwrap_or(""),
                            relay_owner = lease.relay_owner.as_str(),
                            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                            "codex idle rollout relay selected external turn owner"
                        );
                        if lease.relay_owner != ExternalInputRelayOwner::TuiPromptRelay {
                            crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                                &tmux_session_name,
                                &binding.output_path,
                                line_end_offset,
                            );
                            tracing::debug!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                                session_key = lease.session_key.as_deref().unwrap_or(""),
                                relay_owner = lease.relay_owner.as_str(),
                                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                                "codex idle rollout relay yielded response tail to selected owner"
                            );
                            continue;
                        }

                        crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                            &tmux_session_name,
                            &binding.output_path,
                            line_end_offset,
                        );
                        active_tails.insert(tmux_session_name.clone());
                        let shared_for_tail = shared.clone();
                        let done_tx_for_tail = done_tx.clone();
                        let tail_tmux_session_name = tmux_session_name.clone();
                        let tail_rollout_path = rollout_path.clone();
                        let tail_lease = lease.clone();
                        let tail_span = tracing::info_span!(
                            "codex_idle_response_tail",
                            provider = ProviderKind::Codex.as_str(),
                            channel_id = channel_id.get(),
                            tmux_session_name = %tmux_session_name,
                            turn_id = lease.turn_id.as_deref().unwrap_or(""),
                            session_key = lease.session_key.as_deref().unwrap_or(""),
                            relay_owner = lease.relay_owner.as_str(),
                            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                        );
                        super::task_supervisor::spawn_observed(
                            "codex_idle_response_tail",
                            async move {
                                let _done_guard = CodexIdleTailDoneGuard {
                                    tmux_session_name: Some(tail_tmux_session_name.clone()),
                                    done_tx: done_tx_for_tail,
                                };
                                run_codex_idle_response_tail(
                                    shared_for_tail,
                                    tail_tmux_session_name,
                                    channel_id,
                                    tail_rollout_path,
                                    line_end_offset,
                                    tail_lease,
                                )
                                .await;
                            }
                            .instrument(tail_span),
                        );
                    }
                }
            }

            tokio::time::sleep(CODEX_IDLE_ROLLOUT_POLL_INTERVAL).await;
        }
    }
    .instrument(tracing::info_span!(
        "codex_idle_rollout_relay",
        provider = ProviderKind::Codex.as_str(),
        runtime_kind = RuntimeHandoffKind::CodexTui.as_str(),
    )));
}

fn codex_idle_prompt_observation_should_tail_response(
    observation: crate::services::tui_prompt_dedupe::PromptObservation,
) -> bool {
    // The turn bridge owns Discord-originated Codex prompts. The idle rollout
    // relay is only for text typed directly into the Codex TUI; tailing
    // suppressed Discord/recent duplicates can replay stale prior-turn output
    // after a newer Discord message has already started.
    matches!(
        observation,
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    )
}

fn claude_idle_prompt_observation_should_tail_response(
    observation: crate::services::tui_prompt_dedupe::PromptObservation,
) -> bool {
    // The turn bridge owns Discord-originated prompts. Claude's idle tail is
    // only a recovery path for operator text typed directly into the TUI; if
    // we tail suppressed Discord/recent duplicates here, the bridge-delivered
    // answer is posted a second time after inflight clears.
    matches!(
        observation,
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    )
}

#[derive(Debug, PartialEq, Eq)]
enum CodexIdleRolloutScan {
    NoPrompt {
        offset: u64,
    },
    Prompt {
        prompt: String,
        line_end_offset: u64,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum ClaudeIdleTranscriptScan {
    NoPrompt {
        offset: u64,
    },
    Prompt {
        prompt: String,
        prompt_start_offset: u64,
        line_end_offset: u64,
    },
}

fn scan_claude_idle_transcript_for_prompt(
    transcript_path: &Path,
    start_offset: u64,
) -> Result<ClaudeIdleTranscriptScan, String> {
    let mut file = std::fs::File::open(transcript_path).map_err(|error| {
        format!(
            "open Claude transcript {}: {error}",
            transcript_path.display()
        )
    })?;
    let file_len = file
        .metadata()
        .map_err(|error| {
            format!(
                "stat Claude transcript {}: {error}",
                transcript_path.display()
            )
        })?
        .len();
    let mut offset = if start_offset > file_len {
        0
    } else {
        start_offset
    };
    file.seek(SeekFrom::Start(offset)).map_err(|error| {
        format!(
            "seek Claude transcript {}: {error}",
            transcript_path.display()
        )
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader.read_line(&mut line).map_err(|error| {
            format!(
                "read Claude transcript {}: {error}",
                transcript_path.display()
            )
        })?;
        if bytes_read == 0 {
            return Ok(ClaudeIdleTranscriptScan::NoPrompt { offset });
        }
        offset = offset.saturating_add(bytes_read as u64);
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            if !line.ends_with('\n') {
                return Ok(ClaudeIdleTranscriptScan::NoPrompt {
                    offset: line_start_offset,
                });
            }
            continue;
        };
        if let Some(prompt) =
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt(&json)
        {
            return Ok(ClaudeIdleTranscriptScan::Prompt {
                prompt,
                prompt_start_offset: line_start_offset,
                line_end_offset: offset,
            });
        }
    }
}

/// #2843 (codex round-2 P1): scan `[start_offset, EOF)` and return the LAST
/// (newest, closest to EOF) user prompt rather than the first.
///
/// The path-change lookback reads a bounded byte window that can contain
/// several already-finished turns. Selecting the first prompt would re-relay an
/// old turn (`observe_prompt_by_tmux` only suppresses pending Discord prompts or
/// recent duplicates, so an older prompt inside the window is misclassified as
/// SSH-direct and tailed again). The just-typed prompt is always the newest
/// entry in the window, so returning the last prompt catches the current turn
/// without replaying stale backlog. Incremental tailing on an unchanged path
/// keeps first-prompt semantics via [`scan_claude_idle_transcript_for_prompt`].
fn scan_claude_idle_transcript_for_last_prompt(
    transcript_path: &Path,
    start_offset: u64,
) -> Result<ClaudeIdleTranscriptScan, String> {
    let mut file = std::fs::File::open(transcript_path).map_err(|error| {
        format!(
            "open Claude transcript {}: {error}",
            transcript_path.display()
        )
    })?;
    let file_len = file
        .metadata()
        .map_err(|error| {
            format!(
                "stat Claude transcript {}: {error}",
                transcript_path.display()
            )
        })?
        .len();
    let mut offset = if start_offset > file_len {
        0
    } else {
        start_offset
    };
    file.seek(SeekFrom::Start(offset)).map_err(|error| {
        format!(
            "seek Claude transcript {}: {error}",
            transcript_path.display()
        )
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut last_prompt: Option<ClaudeIdleTranscriptScan> = None;

    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader.read_line(&mut line).map_err(|error| {
            format!(
                "read Claude transcript {}: {error}",
                transcript_path.display()
            )
        })?;
        if bytes_read == 0 {
            return Ok(last_prompt.unwrap_or(ClaudeIdleTranscriptScan::NoPrompt { offset }));
        }
        offset = offset.saturating_add(bytes_read as u64);
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            if !line.ends_with('\n') {
                // Partial trailing line: stop before consuming it. Return the
                // newest COMPLETE prompt found so far; otherwise leave the cursor
                // at the partial line so the next tick re-reads it once complete.
                //
                // #2843 (codex round-3/round-4): deferring here — returning the
                // scan start so a later tick re-picks the newest prompt once the
                // partial completes — is NOT viable: `resolve_idle_relay_transcript`
                // re-registers the binding to the fresh path with `last_offset`
                // pinned at EOF BEFORE this scan runs, so the next tick has
                // `path_changed == false` and the first-prompt scanner starts at
                // that pinned EOF, dropping the deferred (current) turn entirely.
                // Returning the last complete prompt instead never drops the
                // current turn: the relayed prompt advances the cursor to its
                // own line end, and any prompt written after it (e.g. one that
                // was mid-write this tick) is caught on the next tick by the
                // unchanged-path first-prompt scanner.
                //
                // Residual: if the freshly-resolved transcript is one we already
                // relayed earlier and then returned to (multi-session mtime
                // flip-back) AND its just-typed prompt is mid-write at scan time,
                // the last complete prompt can be an already-relayed older turn,
                // re-surfaced once (bounded by the 30s recent-duplicate dedup in
                // observe_prompt_by_tmux). Distinguishing that from the dominant
                // single-session case ([prompt][its streaming response]) needs
                // per-transcript relayed-offset memory, which is the relay
                // delivery-lease / cursor-unification consolidation, not #2843.
                return Ok(last_prompt.unwrap_or(ClaudeIdleTranscriptScan::NoPrompt {
                    offset: line_start_offset,
                }));
            }
            continue;
        };
        if let Some(prompt) =
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt(&json)
        {
            last_prompt = Some(ClaudeIdleTranscriptScan::Prompt {
                prompt,
                prompt_start_offset: line_start_offset,
                line_end_offset: offset,
            });
        }
    }
}

fn scan_codex_idle_rollout_for_prompt(
    rollout_path: &Path,
    start_offset: u64,
) -> Result<CodexIdleRolloutScan, String> {
    let mut file = std::fs::File::open(rollout_path)
        .map_err(|error| format!("open Codex rollout {}: {error}", rollout_path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|error| format!("stat Codex rollout {}: {error}", rollout_path.display()))?
        .len();
    let mut offset = if start_offset > file_len {
        0
    } else {
        start_offset
    };
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek Codex rollout {}: {error}", rollout_path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        let line_start_offset = offset;
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|error| format!("read Codex rollout {}: {error}", rollout_path.display()))?;
        if bytes_read == 0 {
            return Ok(CodexIdleRolloutScan::NoPrompt { offset });
        }
        offset = offset.saturating_add(bytes_read as u64);
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            if !line.ends_with('\n') {
                return Ok(CodexIdleRolloutScan::NoPrompt {
                    offset: line_start_offset,
                });
            }
            continue;
        };
        if let Some(prompt) =
            crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt(&json)
        {
            return Ok(CodexIdleRolloutScan::Prompt {
                prompt,
                line_end_offset: offset,
            });
        }
    }
}

#[cfg(unix)]
async fn run_codex_idle_response_tail(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    rollout_path: PathBuf,
    start_offset: u64,
    lease: ExternalInputRelayLease,
) {
    let tail_started_at = chrono::Utc::now();
    let tmux_for_tail = tmux_session_name.clone();
    let rollout_for_tail = rollout_path.clone();
    let tail_result = tokio::task::spawn_blocking(move || {
        collect_codex_idle_response(rollout_for_tail, start_offset, tmux_for_tail)
    })
    .await;

    let (response, final_offset) = match tail_result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                rollout_path = %rollout_path.display(),
                error = %error,
                "codex idle rollout response tail failed"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                rollout_path = %rollout_path.display(),
                error = %error,
                "codex idle rollout response tail panicked"
            );
            return;
        }
    };

    let response = response.trim();
    if response.is_empty() {
        crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
            &tmux_session_name,
            rollout_path.to_str().unwrap_or_default(),
            final_offset,
        );
        return;
    }
    let delivery_result = deliver_tui_idle_response(
        &shared,
        ProviderKind::Codex,
        channel_id,
        &tmux_session_name,
        response,
        tail_started_at,
        &lease,
    )
    .await;
    if tui_idle_tail_should_commit_runtime_binding_offset(response, delivery_result.is_ok()) {
        crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
            &tmux_session_name,
            rollout_path.to_str().unwrap_or_default(),
            final_offset,
        );
    }
}

#[cfg(unix)]
fn collect_codex_idle_response(
    rollout_path: PathBuf,
    start_offset: u64,
    tmux_session_name: String,
) -> Result<(String, u64), String> {
    let (tx, rx) = mpsc::channel();
    let read_result = crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
        &rollout_path,
        start_offset,
        None,
        tx,
        None,
        || crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name),
    )?;

    let mut streamed = String::new();
    let mut done_result: Option<String> = None;
    let mut error_result: Option<String> = None;
    let mut sideband = Vec::new();
    for message in rx.try_iter() {
        match message {
            StreamMessage::Text { content } => streamed.push_str(&content),
            StreamMessage::Done { result, .. } => done_result = Some(result),
            StreamMessage::Error {
                message, stderr, ..
            } => {
                let mut combined = message;
                if !stderr.trim().is_empty() {
                    combined.push_str("\n");
                    combined.push_str(stderr.trim());
                }
                error_result = Some(combined);
            }
            StreamMessage::TaskNotification {
                status, summary, ..
            } => {
                if !summary.trim().is_empty() {
                    sideband.push(format!("[{status}] {summary}"));
                }
            }
            _ => {}
        }
    }

    let offset = match read_result {
        ReadOutputResult::Completed { offset }
        | ReadOutputResult::Cancelled { offset }
        | ReadOutputResult::SessionDied { offset } => offset,
    };
    let response = compose_tui_idle_response(done_result, error_result, streamed, sideband);
    Ok((response, offset))
}

#[cfg(unix)]
async fn run_claude_idle_response_tail(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    transcript_path: PathBuf,
    start_offset: u64,
    lease: ExternalInputRelayLease,
) {
    let tail_started_at = chrono::Utc::now();
    let tmux_for_tail = tmux_session_name.clone();
    let transcript_for_tail = transcript_path.clone();
    let tail_result = tokio::task::spawn_blocking(move || {
        collect_claude_idle_response(transcript_for_tail, start_offset, tmux_for_tail)
    })
    .await;

    let (response, final_offset) = match tail_result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                transcript_path = %transcript_path.display(),
                error = %error,
                "Claude idle transcript response tail failed"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                transcript_path = %transcript_path.display(),
                error = %error,
                "Claude idle transcript response tail panicked"
            );
            return;
        }
    };

    let response = response.trim();
    if response.is_empty() {
        advance_claude_tmux_runtime_binding_offset(
            &tmux_session_name,
            &transcript_path,
            final_offset,
        );
        return;
    }
    let delivery_result = deliver_tui_idle_response(
        &shared,
        ProviderKind::Claude,
        channel_id,
        &tmux_session_name,
        response,
        tail_started_at,
        &lease,
    )
    .await;
    if tui_idle_tail_should_commit_runtime_binding_offset(response, delivery_result.is_ok()) {
        advance_claude_tmux_runtime_binding_offset(
            &tmux_session_name,
            &transcript_path,
            final_offset,
        );
    }
}

#[cfg(unix)]
fn collect_claude_idle_response(
    transcript_path: PathBuf,
    start_offset: u64,
    tmux_session_name: String,
) -> Result<(String, u64), String> {
    let (tx, rx) = mpsc::channel();
    let transcript_path_string = transcript_path.display().to_string();
    let read_result = crate::services::session_backend::read_output_file_until_result(
        &transcript_path_string,
        start_offset,
        tx,
        None,
        crate::services::provider::SessionProbe::tmux(tmux_session_name, ProviderKind::Claude),
    )?;

    let offset = match read_result {
        ReadOutputResult::Completed { offset }
        | ReadOutputResult::Cancelled { offset }
        | ReadOutputResult::SessionDied { offset } => offset,
    };
    Ok((collect_tui_idle_response_messages(rx), offset))
}

#[cfg(unix)]
fn collect_tui_idle_response_messages(rx: mpsc::Receiver<StreamMessage>) -> String {
    let mut streamed = String::new();
    let mut done_result: Option<String> = None;
    let mut error_result: Option<String> = None;
    let mut sideband = Vec::new();
    for message in rx.try_iter() {
        match message {
            StreamMessage::Text { content } => streamed.push_str(&content),
            StreamMessage::Done { result, .. } => done_result = Some(result),
            StreamMessage::Error {
                message, stderr, ..
            } => {
                let mut combined = message;
                if !stderr.trim().is_empty() {
                    combined.push_str("\n");
                    combined.push_str(stderr.trim());
                }
                error_result = Some(combined);
            }
            StreamMessage::TaskNotification {
                status, summary, ..
            } => {
                if !summary.trim().is_empty() {
                    sideband.push(format!("[{status}] {summary}"));
                }
            }
            _ => {}
        }
    }
    compose_tui_idle_response(done_result, error_result, streamed, sideband)
}

#[cfg(unix)]
fn compose_tui_idle_response(
    done_result: Option<String>,
    error_result: Option<String>,
    streamed: String,
    sideband: Vec<String>,
) -> String {
    let body = done_result
        .or(error_result)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or(streamed);
    let body = super::response_sanitizer::strip_leading_tui_response_chrome(&body);
    let sideband = sideband
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if sideband.is_empty() {
        body
    } else if body.trim().is_empty() {
        sideband.join("\n")
    } else {
        format!("{}\n\n{}", sideband.join("\n"), body)
    }
}

#[cfg(unix)]
async fn deliver_tui_idle_response(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    response: &str,
    tail_started_at: chrono::DateTime<chrono::Utc>,
    lease: &ExternalInputRelayLease,
) -> Result<(), String> {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::warn!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            "skipping TUI idle response relay; Discord HTTP unavailable"
        );
        return Err(format!(
            "discord http unavailable for provider {}",
            provider.as_str()
        ));
    };
    let formatted = if shared.status_panel_v2_enabled {
        super::formatting::format_for_discord_with_status_panel(response, &provider)
    } else {
        super::formatting::format_for_discord_with_provider(response, &provider)
    };
    let anchor = prompt_anchor_for_response_after_wait(
        provider.as_str(),
        tmux_session_name,
        channel_id.get(),
    )
    .await;
    let reference = anchor.map(|anchor| {
        (
            ChannelId::new(anchor.channel_id),
            MessageId::new(anchor.message_id),
        )
    });
    match super::formatting::send_long_message_raw_with_reference(
        &http, channel_id, &formatted, shared, reference,
    )
    .await
    {
        Ok(()) => {
            if let Some(anchor) = anchor {
                crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
                    provider.as_str(),
                    tmux_session_name,
                    anchor,
                );
            }
            crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                provider.as_str(),
                tmux_session_name,
                channel_id.get(),
            );
            match super::inflight::clear_inflight_state_if_matches_tmux_response(
                &provider,
                channel_id.get(),
                tmux_session_name,
                response,
            ) {
                super::inflight::GuardedClearOutcome::Cleared => {
                    tracing::info!(
                        channel_id = channel_id.get(),
                        tmux_session_name = %tmux_session_name,
                        provider = %provider.as_str(),
                        turn_id = lease.turn_id.as_deref().unwrap_or(""),
                        session_key = lease.session_key.as_deref().unwrap_or(""),
                        relay_owner = lease.relay_owner.as_str(),
                        runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                        "TUI idle response relay cleared matching inflight state"
                    );
                }
                super::inflight::GuardedClearOutcome::IoError => {
                    tracing::warn!(
                        channel_id = channel_id.get(),
                        tmux_session_name = %tmux_session_name,
                        provider = %provider.as_str(),
                        turn_id = lease.turn_id.as_deref().unwrap_or(""),
                        session_key = lease.session_key.as_deref().unwrap_or(""),
                        relay_owner = lease.relay_owner.as_str(),
                        runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                        "TUI idle response relay could not clear matching inflight state"
                    );
                }
                _ => {}
            }
            post_tui_idle_response_session_idle(
                shared,
                &provider,
                channel_id,
                tmux_session_name,
                tail_started_at,
                lease,
            )
            .await;
            tracing::info!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                chars = formatted.chars().count(),
                prompt_anchor_message_id = reference.map(|(_, message_id)| message_id.get()),
                "TUI idle response relayed"
            );
            crate::services::observability::emit_relay_delivery(
                provider.as_str(),
                channel_id.get(),
                None,
                lease.session_key.as_deref(),
                lease.turn_id.as_deref(),
                reference.map(|(_, message_id)| message_id.get()),
                "tui_prompt_relay",
                "post",
                None,
                None,
                true,
                Some("transitional idle tail"),
            );
            Ok(())
        }
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                error = %error,
                "failed to relay TUI idle response"
            );
            crate::services::observability::emit_relay_delivery(
                provider.as_str(),
                channel_id.get(),
                None,
                lease.session_key.as_deref(),
                lease.turn_id.as_deref(),
                reference.map(|(_, message_id)| message_id.get()),
                "tui_prompt_relay",
                "post",
                None,
                None,
                false,
                Some("transitional idle tail failed"),
            );
            Err(error.to_string())
        }
    }
}

#[cfg(unix)]
fn tui_idle_tail_should_commit_runtime_binding_offset(
    response: &str,
    discord_delivery_succeeded: bool,
) -> bool {
    response.trim().is_empty() || discord_delivery_succeeded
}

#[cfg(unix)]
async fn post_tui_idle_response_session_idle(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    tail_started_at: chrono::DateTime<chrono::Utc>,
    lease: &ExternalInputRelayLease,
) {
    if shared.mailbox(channel_id).cancel_token().await.is_some() {
        tracing::debug!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            "skipping TUI idle response session-idle commit; mailbox turn is active"
        );
        return;
    }

    if super::inflight::load_inflight_state(provider, channel_id.get()).is_some() {
        tracing::debug!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            "skipping TUI idle response session-idle commit; inflight state is active"
        );
        return;
    }

    let session_key = super::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        provider,
        tmux_session_name,
    );
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let agent_id = super::resolve_channel_role_binding(channel_id, channel_name.as_deref())
        .map(|binding| binding.role_id);

    match super::internal_api::mark_session_idle_if_not_newer_live(
        &session_key,
        provider.as_str(),
        agent_id.as_deref(),
        tail_started_at,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = %session_key,
                "skipping TUI idle response session-idle commit; session row is absent or newer live"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = %session_key,
                error = %error,
                "failed to commit TUI idle response session idle"
            );
            return;
        }
    }

    tracing::info!(
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        provider = %provider.as_str(),
        turn_id = lease.turn_id.as_deref().unwrap_or(""),
        session_key = %session_key,
        "TUI idle response committed session idle"
    );
}

#[cfg(unix)]
async fn prompt_anchor_for_response_after_wait(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor> {
    let deadline = tokio::time::Instant::now() + CODEX_IDLE_PROMPT_ANCHOR_WAIT;
    loop {
        if let Some(anchor) = crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
            provider,
            tmux_session_name,
            channel_id,
        ) {
            return Some(anchor);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        tokio::time::sleep(CODEX_IDLE_PROMPT_ANCHOR_POLL.min(deadline - now)).await;
    }
}

fn owner_channel_for_prompt(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
) -> Option<ChannelId> {
    owner_channel_for_tmux_session(shared, &prompt.tmux_session_name)
}

fn owner_channel_for_tmux_session(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> Option<ChannelId> {
    shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)
        .or_else(|| {
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux_session_name)
                .map(ChannelId::new)
        })
}

pub(super) fn format_ssh_direct_prompt_notification(
    _provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> String {
    let prompt = strip_terminal_controls(prompt);
    let preview =
        truncate_chars(prompt.trim(), SSH_DIRECT_PROMPT_PREVIEW_LIMIT).replace("```", "` ` `");
    format!(
        "터미널에 직접 주입된 입력 (tmux : `{}`):\n```text\n{}\n```",
        sanitize_inline_code(tmux_session_name),
        preview,
    )
}

fn sanitize_inline_code(value: &str) -> String {
    value.replace('`', "'")
}

fn strip_terminal_controls(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek().copied() == Some('[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t' {
            continue;
        }
        output.push(ch);
    }
    output
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ssh_direct_prompt_notification() {
        let output = format_ssh_direct_prompt_notification("claude", "AgentDesk-claude-a", "hi");

        assert!(output.contains("터미널에 직접 주입된 입력"));
        assert!(output.contains("(tmux : `AgentDesk-claude-a`)"));
        assert!(output.contains("```text\nhi\n```"));
    }

    #[test]
    fn formats_ssh_direct_prompt_notification_with_truncation() {
        let prompt = "x".repeat(SSH_DIRECT_PROMPT_PREVIEW_LIMIT + 20);
        let output = format_ssh_direct_prompt_notification("codex", "AgentDesk-codex-a", &prompt);

        assert!(output.contains("터미널에 직접 주입된 입력"));
        assert!(output.contains("(tmux : `AgentDesk-codex-a`)"));
        assert!(output.contains("..."));
        assert!(output.len() < prompt.len() + 120);
    }

    #[test]
    fn formats_ssh_direct_prompt_notification_escapes_code_fence() {
        let output = format_ssh_direct_prompt_notification("codex", "tmux`name", "a ``` fence");

        assert!(output.contains("(tmux : `tmux'name`)"));
        assert!(output.contains("a ` ` ` fence"));
    }

    #[test]
    fn formats_ssh_direct_prompt_notification_strips_terminal_controls() {
        let output = format_ssh_direct_prompt_notification(
            "claude",
            "AgentDesk-claude-a",
            "\u{15}\u{1b}[31mhello\u{1b}[0m\n\tworld",
        );

        assert!(output.contains("hello\n\tworld"));
        assert!(!output.contains('\u{15}'));
        assert!(!output.contains('\u{1b}'));
    }

    // U-4 Bare control bytes (BEL, FF, DEL, C1 NEXT LINE) in the SSH-direct
    // notification path must be silently dropped — they would otherwise
    // disrupt Discord rendering or terminal mirrors that re-paste the text.
    // Newline, carriage return, and tab are preserved by design.
    #[test]
    fn notification_strip_drops_bare_control_bytes_but_keeps_whitespace() {
        let raw = "\u{07}ring\u{0c}page\u{7f}del\u{85}c1\n\tkeep";

        let output = format_ssh_direct_prompt_notification("claude", "tmux-1", raw);

        for forbidden in ['\u{07}', '\u{0c}', '\u{7f}', '\u{85}'] {
            assert!(
                !output.contains(forbidden),
                "control byte {:?} leaked into notification: {:?}",
                forbidden,
                output
            );
        }
        assert!(output.contains("ringpagedelc1\n\tkeep"));
    }

    #[cfg(unix)]
    #[test]
    fn parses_claude_tui_launch_script_content() {
        let script = concat!(
            "#!/bin/bash\n",
            "cd '/tmp/project'\\''s dir'\n",
            "exec '/usr/local/bin/claude' '--dangerously-skip-permissions' '--session-id' '01234567-89ab-cdef-0123-456789abcdef' '--settings' '/tmp/settings.json'\n",
        );

        assert_eq!(
            parse_claude_tui_launch_script_content(script),
            Some(ClaudeTuiLaunchInfo {
                working_dir: PathBuf::from("/tmp/project's dir"),
                session_id: "01234567-89ab-cdef-0123-456789abcdef".to_string(),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_rehydrate_thread_session_resolves_thread_channel_id() {
        let parent_channel_id = 1479671298497183835;
        let thread_id = 1504455726595051591_u64;
        let tmux_session_name =
            ProviderKind::Claude.build_tmux_session_name(&format!("adk-cc-t{thread_id}"));

        assert_eq!(
            rehydrated_claude_channel_id_for_segment(
                &tmux_session_name,
                "adk-cc",
                parent_channel_id
            ),
            Some(thread_id)
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_rehydrate_thread_session_rejects_non_numeric_suffix() {
        let tmux_session_name = ProviderKind::Claude.build_tmux_session_name("adk-cc-tthread");

        assert_eq!(
            rehydrated_claude_channel_id_for_segment(
                &tmux_session_name,
                "adk-cc",
                1479671298497183835
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_rehydrate_binding_match_requires_current_launch_transcript() {
        let existing = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/old-transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("old-session".to_string()),
            last_offset: 10,
            relay_last_offset: None,
        };
        let fresh = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/current-transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("current-session".to_string()),
            last_offset: 20,
            relay_last_offset: None,
        };

        assert!(!claude_tui_runtime_binding_matches_launch(
            &existing, &fresh
        ));
        assert!(claude_tui_runtime_binding_matches_launch(&fresh, &fresh));
    }

    #[cfg(all(unix, feature = "legacy-sqlite-tests"))]
    #[test]
    fn rehydrates_claude_tui_binding_from_launch_script_and_exact_session_name() {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join(".adk");
        let config_dir = root.join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("agentdesk.yaml"),
            r#"
server:
  port: 8791
agents:
  - id: adk-dashboard
    name: "Dashboard"
    provider: claude
    channels:
      claude:
        id: "1490141479707086938"
        name: "adk-dash-cc"
"#,
        )
        .expect("config");
        let claude_home = temp.path().join(".claude");
        let prev_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let prev_claude_home = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &root);
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude_home);
        }

        let result = (|| {
            let tmux_session_name = crate::services::provider::ProviderKind::Claude
                .build_tmux_session_name("adk-dash-cc");
            let working_dir = temp.path().join("workspace");
            std::fs::create_dir_all(&working_dir).expect("working dir");
            let session_id = "01234567-89ab-cdef-0123-456789abcdef";
            let transcript_path =
                crate::services::claude_tui::transcript_tail::claude_transcript_path(
                    &working_dir,
                    session_id,
                    Some(&claude_home),
                )
                .expect("transcript path");
            std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
                .expect("transcript parent dir");
            let before = concat!(
                "{\"type\":\"system\",\"subtype\":\"init\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old answer\"}]}}\n",
            );
            let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"direct prompt during restart\"}]}}\n";
            let after = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"new answer\"}]}}\n";
            let transcript_body = format!("{before}{prompt}{after}");
            std::fs::write(&transcript_path, &transcript_body).expect("transcript");
            let launch_script_path = crate::services::tmux_common::session_temp_path(
                &tmux_session_name,
                crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
            );
            std::fs::write(
                &launch_script_path,
                format!(
                    "#!/bin/bash\ncd {}\nexec {} '--dangerously-skip-permissions' '--session-id' '{}' '--settings' '/tmp/settings.json'\n",
                    crate::services::process::shell_escape(&working_dir.display().to_string()),
                    crate::services::process::shell_escape("/usr/local/bin/claude"),
                    session_id,
                ),
            )
            .expect("launch script");

            (
                resolve_rehydrated_claude_tmux_channel_id(&tmux_session_name)
                    .expect("resolved channel"),
                rehydrated_claude_tui_binding_for_tmux_session(&tmux_session_name)
                    .expect("rehydrated binding"),
                transcript_body.len() as u64,
            )
        })();

        match prev_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match prev_claude_home {
            Some(value) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", value) },
            None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
        }

        let (channel_id, binding, expected_start_offset) = result;
        assert_eq!(channel_id, 1490141479707086938);
        assert_eq!(binding.runtime_kind, RuntimeHandoffKind::ClaudeTui);
        assert_eq!(
            binding.session_id.as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(binding.last_offset, expected_start_offset);
        assert!(
            binding
                .output_path
                .ends_with("01234567-89ab-cdef-0123-456789abcdef.jsonl")
        );
    }

    // U-11 Missing transcripts still start at zero; existing transcripts
    // always start at their current EOF.
    #[cfg(unix)]
    #[test]
    fn claude_rehydrate_start_offset_returns_zero_for_missing_transcript() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("never-written.jsonl");

        assert_eq!(claude_tui_rehydrate_start_offset(&missing), 0);
    }

    #[cfg(unix)]
    #[test]
    fn claude_rehydrate_start_offset_uses_current_eof() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("current.jsonl");
        let before = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n";
        let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"direct claude prompt\"}]},\"sessionId\":\"s1\"}\n";
        let after = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]},\"sessionId\":\"s1\"}\n";
        let body = format!("{before}{prompt}{after}");
        std::fs::write(&transcript, &body).expect("write transcript");

        assert_eq!(
            claude_tui_rehydrate_start_offset(&transcript),
            body.len() as u64
        );
    }

    #[test]
    fn codex_idle_rollout_scan_finds_user_prompt_and_stops_at_prompt_end() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout = dir.path().join("rollout.jsonl");
        let before = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}\n";
        let prompt = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"direct prompt\"}]}}\n";
        let after = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n";
        std::fs::write(&rollout, format!("{before}{prompt}{after}")).expect("write rollout");

        assert_eq!(
            scan_codex_idle_rollout_for_prompt(&rollout, 0).expect("scan"),
            CodexIdleRolloutScan::Prompt {
                prompt: "direct prompt".to_string(),
                line_end_offset: (before.len() + prompt.len()) as u64,
            }
        );
        assert_eq!(
            scan_codex_idle_rollout_for_prompt(&rollout, (before.len() + prompt.len()) as u64,)
                .expect("scan after prompt"),
            CodexIdleRolloutScan::NoPrompt {
                offset: (before.len() + prompt.len() + after.len()) as u64,
            }
        );
    }

    #[test]
    fn codex_idle_rollout_scan_preserves_partial_trailing_jsonl() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout = dir.path().join("rollout.jsonl");
        let complete = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}\n";
        let partial =
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\"";
        std::fs::write(&rollout, format!("{complete}{partial}")).expect("write rollout");

        assert_eq!(
            scan_codex_idle_rollout_for_prompt(&rollout, 0).expect("scan partial"),
            CodexIdleRolloutScan::NoPrompt {
                offset: complete.len() as u64,
            }
        );
    }

    #[test]
    fn codex_idle_rollout_scan_restarts_when_file_shrinks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout = dir.path().join("rollout.jsonl");
        let prompt = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"after shrink\"}]}}\n";
        std::fs::write(&rollout, prompt).expect("write rollout");

        assert_eq!(
            scan_codex_idle_rollout_for_prompt(&rollout, 99_999).expect("scan shrunken"),
            CodexIdleRolloutScan::Prompt {
                prompt: "after shrink".to_string(),
                line_end_offset: prompt.len() as u64,
            }
        );
    }

    // U-17 Claude transcript scan must restart from offset 0 when the
    // recorded offset is past the current file length — this is the
    // /compact path, where Claude rewrites the transcript and our
    // previously-persisted offset would otherwise leak past the EOF and
    // skip all newly-written prompts.
    #[test]
    fn claude_idle_transcript_scan_restarts_when_file_shrinks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"after compact\"}]},\"sessionId\":\"s1\"}\n";
        std::fs::write(&transcript, prompt).expect("write transcript");

        let scan = scan_claude_idle_transcript_for_prompt(&transcript, 99_999)
            .expect("scan shrunken transcript");
        match scan {
            ClaudeIdleTranscriptScan::Prompt {
                prompt: text,
                line_end_offset,
                prompt_start_offset,
            } => {
                assert_eq!(text, "after compact");
                assert_eq!(line_end_offset, prompt.len() as u64);
                assert_eq!(prompt_start_offset, 0);
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn claude_idle_response_start_offset_prefers_timestamp_boundary() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let first = r#"{"timestamp":"2026-05-28T00:00:00Z","type":"assistant"}"#;
        let second = r#"{"timestamp":"2026-05-28T00:00:10Z","type":"assistant"}"#;
        std::fs::write(&transcript, format!("{first}\n{second}\n")).expect("write transcript");
        let turn_started_at = chrono::DateTime::parse_from_rfc3339("2026-05-28T00:00:10Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let offset =
            claude_idle_response_start_offset_after_timestamp(&transcript, turn_started_at, 0);

        assert_eq!(offset, first.len() as u64 + 1);
    }

    #[cfg(unix)]
    #[test]
    fn claude_idle_response_start_offset_resets_stale_fallback_after_shrink() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        std::fs::write(&transcript, "{}\n").expect("write transcript");
        let turn_started_at = chrono::DateTime::parse_from_rfc3339("2026-05-28T00:00:10Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let offset =
            claude_idle_response_start_offset_after_timestamp(&transcript, turn_started_at, 99_999);

        assert_eq!(offset, 0);
    }

    #[test]
    fn codex_idle_prompt_tails_only_new_ssh_direct_prompt() {
        assert!(codex_idle_prompt_observation_should_tail_response(
            crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
        ));
        assert!(!codex_idle_prompt_observation_should_tail_response(
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedDiscordDuplicate
        ));
        assert!(!codex_idle_prompt_observation_should_tail_response(
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedRecentDuplicate
        ));
        assert!(!codex_idle_prompt_observation_should_tail_response(
            crate::services::tui_prompt_dedupe::PromptObservation::Ignored
        ));
    }

    #[test]
    fn claude_idle_prompt_tails_only_new_ssh_direct_prompt() {
        assert!(claude_idle_prompt_observation_should_tail_response(
            crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
        ));
        assert!(!claude_idle_prompt_observation_should_tail_response(
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedDiscordDuplicate
        ));
        assert!(!claude_idle_prompt_observation_should_tail_response(
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedRecentDuplicate
        ));
        assert!(!claude_idle_prompt_observation_should_tail_response(
            crate::services::tui_prompt_dedupe::PromptObservation::Ignored
        ));
    }

    #[test]
    fn claude_idle_transcript_scan_finds_user_prompt_and_stops_at_prompt_end() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let before = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n";
        let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"direct claude prompt\"}]},\"sessionId\":\"s1\"}\n";
        let after = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]},\"sessionId\":\"s1\"}\n";
        std::fs::write(&transcript, format!("{before}{prompt}{after}")).expect("write transcript");

        assert_eq!(
            scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("scan"),
            ClaudeIdleTranscriptScan::Prompt {
                prompt: "direct claude prompt".to_string(),
                prompt_start_offset: before.len() as u64,
                line_end_offset: (before.len() + prompt.len()) as u64,
            }
        );
        assert_eq!(
            scan_claude_idle_transcript_for_prompt(
                &transcript,
                (before.len() + prompt.len()) as u64,
            )
            .expect("scan after prompt"),
            ClaudeIdleTranscriptScan::NoPrompt {
                offset: (before.len() + prompt.len() + after.len()) as u64,
            }
        );
    }

    #[test]
    fn claude_idle_transcript_scan_ignores_meta_user_prompt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let meta = "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"_\"}]},\"sessionId\":\"s1\"}\n";
        let synthetic = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"No response requested.\"}]},\"sessionId\":\"s1\"}\n";
        let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"real prompt\"}]},\"sessionId\":\"s1\"}\n";
        std::fs::write(&transcript, format!("{meta}{synthetic}{prompt}"))
            .expect("write transcript");

        assert_eq!(
            scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("scan"),
            ClaudeIdleTranscriptScan::Prompt {
                prompt: "real prompt".to_string(),
                prompt_start_offset: (meta.len() + synthetic.len()) as u64,
                line_end_offset: (meta.len() + synthetic.len() + prompt.len()) as u64,
            }
        );
    }

    #[test]
    fn claude_idle_transcript_scan_preserves_partial_trailing_jsonl() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let complete = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n";
        let partial = "{\"type\":\"user\",\"message\":{\"role\":\"user\"";
        std::fs::write(&transcript, format!("{complete}{partial}")).expect("write transcript");

        assert_eq!(
            scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("scan partial"),
            ClaudeIdleTranscriptScan::NoPrompt {
                offset: complete.len() as u64,
            }
        );
    }

    #[test]
    fn claude_idle_transcript_scan_for_last_prompt_selects_newest_in_window() {
        // #2843 (codex round-2 P1): a path-change lookback window holding an old
        // finished turn followed by the just-typed prompt must relay only the
        // newest prompt, not the first (which would re-relay the old turn).
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let old_prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"old finished turn\"}]},\"sessionId\":\"s1\"}\n";
        let old_answer = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old answer\"}]},\"sessionId\":\"s1\"}\n";
        let new_prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"just typed prompt\"}]},\"sessionId\":\"s1\"}\n";
        std::fs::write(&transcript, format!("{old_prompt}{old_answer}{new_prompt}"))
            .expect("write transcript");

        // First-prompt scan would return the OLD turn (the regression).
        assert_eq!(
            scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("first scan"),
            ClaudeIdleTranscriptScan::Prompt {
                prompt: "old finished turn".to_string(),
                prompt_start_offset: 0,
                line_end_offset: old_prompt.len() as u64,
            }
        );
        // Last-prompt scan returns the just-typed prompt instead.
        assert_eq!(
            scan_claude_idle_transcript_for_last_prompt(&transcript, 0).expect("last scan"),
            ClaudeIdleTranscriptScan::Prompt {
                prompt: "just typed prompt".to_string(),
                prompt_start_offset: (old_prompt.len() + old_answer.len()) as u64,
                line_end_offset: (old_prompt.len() + old_answer.len() + new_prompt.len()) as u64,
            }
        );
    }

    #[test]
    fn claude_idle_transcript_scan_for_last_prompt_none_when_no_prompt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let init = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n";
        let answer = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]},\"sessionId\":\"s1\"}\n";
        std::fs::write(&transcript, format!("{init}{answer}")).expect("write transcript");

        assert_eq!(
            scan_claude_idle_transcript_for_last_prompt(&transcript, 0).expect("scan"),
            ClaudeIdleTranscriptScan::NoPrompt {
                offset: (init.len() + answer.len()) as u64,
            }
        );
    }

    #[test]
    fn claude_idle_transcript_scan_for_last_prompt_returns_complete_then_catches_next() {
        // #2843 (codex round-3/round-4): a partial trailing line is NOT consumed
        // and does NOT defer the already-found complete prompt. Deferring would
        // drop the current turn (resolve pins the binding at EOF before the
        // scan, so the next tick starts past the deferred prompt). Returning the
        // last complete prompt never drops the current turn: a prompt written
        // after it (mid-write this tick) is caught on the next tick by the
        // unchanged-path first-prompt scanner from the relayed prompt's line end.
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"complete prompt\"}]},\"sessionId\":\"s1\"}\n";
        let next_partial = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"next";
        std::fs::write(&transcript, format!("{prompt}{next_partial}")).expect("write transcript");

        // Last-prompt scan returns the complete prompt, ignoring the partial.
        assert_eq!(
            scan_claude_idle_transcript_for_last_prompt(&transcript, 0).expect("scan"),
            ClaudeIdleTranscriptScan::Prompt {
                prompt: "complete prompt".to_string(),
                prompt_start_offset: 0,
                line_end_offset: prompt.len() as u64,
            }
        );

        // Once the trailing line completes, the next tick's first-prompt scanner
        // from the relayed prompt's line end catches it — nothing is dropped.
        let next = format!("{next_partial} prompt\"}}]}},\"sessionId\":\"s1\"}}\n");
        std::fs::write(&transcript, format!("{prompt}{next}")).expect("rewrite transcript");
        assert_eq!(
            scan_claude_idle_transcript_for_prompt(&transcript, prompt.len() as u64)
                .expect("next-tick scan"),
            ClaudeIdleTranscriptScan::Prompt {
                prompt: "next prompt".to_string(),
                prompt_start_offset: prompt.len() as u64,
                line_end_offset: (prompt.len() + next.len()) as u64,
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn tui_idle_response_preserves_sideband_notifications_with_done() {
        let output = compose_tui_idle_response(
            Some("final answer".to_string()),
            None,
            "streamed answer".to_string(),
            vec![
                "[started] subagent launched".to_string(),
                "[completed] monitor finished".to_string(),
            ],
        );

        assert_eq!(
            output,
            "[started] subagent launched\n[completed] monitor finished\n\nfinal answer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tui_idle_response_strips_leading_resume_prompt_chrome() {
        let output = compose_tui_idle_response(
            Some("No response requested.fix2_3".to_string()),
            None,
            String::new(),
            Vec::new(),
        );

        assert_eq!(output, "fix2_3");
    }

    #[cfg(unix)]
    #[test]
    fn tui_idle_response_preserves_legitimate_no_response_sentence() {
        let output = compose_tui_idle_response(
            Some("No response requested. But here is the explanation.".to_string()),
            None,
            String::new(),
            Vec::new(),
        );

        assert_eq!(
            output,
            "No response requested. But here is the explanation."
        );
    }

    #[cfg(unix)]
    #[test]
    fn tui_idle_response_preserves_middle_resume_prompt_chrome_text() {
        let output = compose_tui_idle_response(
            Some("Hello\nNo response requested. trailing".to_string()),
            None,
            String::new(),
            Vec::new(),
        );

        assert_eq!(output, "Hello\nNo response requested. trailing");
    }

    #[cfg(unix)]
    #[test]
    fn tui_idle_response_returns_empty_when_body_is_only_resume_prompt_chrome() {
        let output = compose_tui_idle_response(
            Some("No response requested.".to_string()),
            None,
            String::new(),
            Vec::new(),
        );

        assert_eq!(output, "");
    }

    #[cfg(unix)]
    #[test]
    fn tui_idle_response_strips_multiple_leading_resume_prompt_chrome_chunks() {
        let output = compose_tui_idle_response(
            Some(
                "Continue from where you left off.\nNo response requested.\nfinal answer"
                    .to_string(),
            ),
            None,
            String::new(),
            Vec::new(),
        );

        assert_eq!(output, "final answer");
    }

    #[cfg(unix)]
    #[test]
    fn tui_idle_response_does_not_trim_when_no_resume_prompt_chrome() {
        let output = compose_tui_idle_response(
            Some("  intentional leading spaces".to_string()),
            None,
            String::new(),
            Vec::new(),
        );

        assert_eq!(output, "  intentional leading spaces");
    }

    #[cfg(unix)]
    #[test]
    fn idle_response_tail_discord_send_failure_does_not_advance_runtime_binding_offset() {
        assert!(!tui_idle_tail_should_commit_runtime_binding_offset(
            "final answer",
            false
        ));
        assert!(tui_idle_tail_should_commit_runtime_binding_offset(
            "final answer",
            true
        ));
        assert!(tui_idle_tail_should_commit_runtime_binding_offset(
            "", false
        ));
    }
}
