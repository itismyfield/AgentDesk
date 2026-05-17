use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::SharedData;
use crate::services::claude_tui::hook_server::{HookEventKind, subscribe_hook_events};
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::tui_prompt_dedupe::{
    ObservedTuiPrompt, TuiRuntimeBinding, extract_prompt_from_hook_payload,
    observe_prompt_by_provider_session, runtime_binding_for_tmux_session,
    subscribe_observed_prompts,
};

const SSH_DIRECT_PROMPT_PREVIEW_LIMIT: usize = 1500;
const SSH_DIRECT_INFLIGHT_SLOT_WAIT_ATTEMPTS: usize = 20;
const SSH_DIRECT_INFLIGHT_SLOT_WAIT_MS: u64 = 250;

pub(super) fn spawn_tui_prompt_relay(shared: Arc<SharedData>, provider: ProviderKind) {
    tokio::spawn(async move {
        let mut hook_rx = subscribe_hook_events();
        let mut observed_rx = subscribe_observed_prompts();
        let provider_name = provider.as_str().to_string();
        loop {
            tokio::select! {
                hook_event = hook_rx.recv() => {
                    match hook_event {
                        Ok(event) if event.provider == provider_name
                            && event.kind == HookEventKind::UserPromptSubmit =>
                        {
                            if let Some(prompt) = extract_prompt_from_hook_payload(&event.payload) {
                                let observation = observe_prompt_by_provider_session(
                                    &event.provider,
                                    &event.session_id,
                                    &prompt,
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
    });
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
    let Some(registry) = shared.health_registry() else {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            "skipping SSH-direct TUI prompt notify; health registry unavailable"
        );
        return;
    };
    let Some(slot) = wait_for_inflight_slot_and_pause_for_prompt(shared, &prompt, channel_id).await
    else {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            "skipping SSH-direct TUI prompt notify; runtime handoff binding unavailable or channel stayed busy"
        );
        return;
    };
    let runtime_binding = slot.runtime_binding;
    let start_offset = slot.start_offset;
    let mut pause_guard = slot.pause_guard;
    let notify_http = match super::health::resolve_bot_http(registry.as_ref(), "notify").await {
        Ok(http) => http,
        Err((status, body)) => {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
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
                error = %error,
                "failed to send SSH-direct TUI prompt notify"
            );
            return;
        }
    };
    match synthesize_ssh_direct_inflight(
        &prompt,
        channel_id,
        anchor_message.id,
        &runtime_binding,
        start_offset,
    ) {
        Ok(()) => {
            if let Some(guard) = pause_guard.as_mut() {
                guard.commit_resume_offset(start_offset);
            }
        }
        Err(super::inflight::CreateNewInflightError::AlreadyExists) => {
            let _ =
                super::http::delete_channel_message(&*notify_http, channel_id, anchor_message.id)
                    .await;
            tracing::debug!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                tmux_session_name = %prompt.tmux_session_name,
                "removed SSH-direct TUI prompt notify after concurrent inflight creation"
            );
        }
        Err(super::inflight::CreateNewInflightError::Internal(error)) => {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                error = %error,
                "failed to create SSH-direct TUI inflight binding"
            );
        }
    }
    drop(pause_guard);
}

fn owner_channel_for_prompt(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
) -> Option<ChannelId> {
    shared
        .tmux_watchers
        .owner_channel_for_tmux_session(&prompt.tmux_session_name)
        .or_else(|| {
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(
                &prompt.tmux_session_name,
            )
            .map(ChannelId::new)
        })
}

pub(super) fn format_ssh_direct_prompt_notification(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> String {
    let provider_label = match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => "Claude".to_string(),
        "codex" => "Codex".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => "TUI".to_string(),
    };
    let preview =
        truncate_chars(prompt.trim(), SSH_DIRECT_PROMPT_PREVIEW_LIMIT).replace("```", "` ` `");
    format!(
        "SSH direct input relayed from {provider_label} TUI (`{}`):\n```text\n{}\n```",
        sanitize_inline_code(tmux_session_name),
        preview,
    )
}

fn sanitize_inline_code(value: &str) -> String {
    value.replace('`', "'")
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

fn synthesize_ssh_direct_inflight(
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
    anchor_message_id: MessageId,
    runtime_binding: &TuiRuntimeBinding,
    start_offset: u64,
) -> Result<(), super::inflight::CreateNewInflightError> {
    let Some(state) = build_ssh_direct_inflight_state(
        prompt,
        channel_id,
        anchor_message_id,
        runtime_binding,
        start_offset,
    ) else {
        return Err(super::inflight::CreateNewInflightError::Internal(
            "provider could not be resolved".to_string(),
        ));
    };

    super::inflight::save_inflight_state_create_new(&state)?;
    tracing::info!(
        provider = %state.provider,
        channel_id = state.channel_id,
        tmux_session_name = ?state.tmux_session_name,
        user_msg_id = state.user_msg_id,
        "created SSH-direct TUI inflight binding"
    );
    Ok(())
}

fn build_ssh_direct_inflight_state(
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
    anchor_message_id: MessageId,
    runtime_binding: &TuiRuntimeBinding,
    start_offset: u64,
) -> Option<super::inflight::InflightTurnState> {
    let parsed_tmux = parse_provider_and_channel_from_tmux_name(&prompt.tmux_session_name);
    let provider = ProviderKind::from_str(&prompt.provider)
        .or_else(|| parsed_tmux.as_ref().map(|(provider, _)| provider.clone()))?;
    let channel_name = parsed_tmux
        .as_ref()
        .map(|(_, name)| name.trim().to_string())
        .filter(|name| !name.is_empty());
    let mut state = super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name,
        0,
        anchor_message_id.get(),
        anchor_message_id.get(),
        prompt.prompt.clone(),
        runtime_binding.session_id.clone(),
        Some(prompt.tmux_session_name.clone()),
        Some(runtime_binding.relay_output_path().to_string()),
        runtime_binding.input_fifo_path.clone(),
        start_offset,
    );
    state.runtime_kind = Some(runtime_binding.runtime_kind);
    state.turn_source = super::inflight::TurnSource::ExternalInput;
    state.watcher_owns_live_relay = true;
    state.last_watcher_relayed_offset = Some(start_offset);
    Some(state)
}

struct PromptRelaySlot {
    runtime_binding: TuiRuntimeBinding,
    start_offset: u64,
    pause_guard: Option<WatcherPauseGuard>,
}

enum PromptWatcherPauseAttempt {
    Acquired(WatcherPauseGuard),
    Busy,
    NotNeeded,
}

async fn wait_for_inflight_slot_and_pause_for_prompt(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
) -> Option<PromptRelaySlot> {
    for attempt in 0..=SSH_DIRECT_INFLIGHT_SLOT_WAIT_ATTEMPTS {
        if inflight_exists_for_prompt(prompt, channel_id) {
            if attempt == SSH_DIRECT_INFLIGHT_SLOT_WAIT_ATTEMPTS {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(
                SSH_DIRECT_INFLIGHT_SLOT_WAIT_MS,
            ))
            .await;
            continue;
        }
        let Some(runtime_binding) = runtime_binding_for_tmux_session(&prompt.tmux_session_name)
        else {
            return None;
        };
        let pause_attempt = pause_owner_watcher_for_prompt_relay(
            shared,
            channel_id,
            prompt,
            runtime_binding.relay_output_path(),
            None,
        );
        if matches!(pause_attempt, PromptWatcherPauseAttempt::Busy) {
            if attempt == SSH_DIRECT_INFLIGHT_SLOT_WAIT_ATTEMPTS {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(
                SSH_DIRECT_INFLIGHT_SLOT_WAIT_MS,
            ))
            .await;
            continue;
        }
        let mut pause_guard = match pause_attempt {
            PromptWatcherPauseAttempt::Acquired(guard) => Some(guard),
            PromptWatcherPauseAttempt::Busy => unreachable!(),
            PromptWatcherPauseAttempt::NotNeeded => None,
        };
        let initial_start_offset = runtime_binding_replay_start_offset(&runtime_binding);
        if let Some(guard) = pause_guard.as_mut() {
            guard.arm_abort_resume_offset(initial_start_offset);
        }
        if !inflight_exists_for_prompt(prompt, channel_id) {
            let Some(runtime_binding) = runtime_binding_for_tmux_session(&prompt.tmux_session_name)
            else {
                return None;
            };
            let start_offset = runtime_binding_replay_start_offset(&runtime_binding);
            if let Some(guard) = pause_guard.as_mut() {
                guard.arm_abort_resume_offset(start_offset);
            }
            if !inflight_exists_for_prompt(prompt, channel_id) {
                return Some(PromptRelaySlot {
                    runtime_binding,
                    start_offset,
                    pause_guard,
                });
            }
        }
        drop(pause_guard);
        if attempt == SSH_DIRECT_INFLIGHT_SLOT_WAIT_ATTEMPTS {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(
            SSH_DIRECT_INFLIGHT_SLOT_WAIT_MS,
        ))
        .await;
    }
    None
}

fn inflight_exists_for_prompt(prompt: &ObservedTuiPrompt, channel_id: ChannelId) -> bool {
    ProviderKind::from_str(&prompt.provider)
        .or_else(|| {
            parse_provider_and_channel_from_tmux_name(&prompt.tmux_session_name)
                .map(|(provider, _)| provider)
        })
        .is_some_and(|provider| {
            super::inflight::load_inflight_state(&provider, channel_id.get()).is_some()
        })
}

fn runtime_binding_replay_start_offset(binding: &TuiRuntimeBinding) -> u64 {
    let last_offset = binding.relay_last_offset();
    match std::fs::metadata(binding.relay_output_path()).map(|metadata| metadata.len()) {
        Ok(current_len) if current_len >= last_offset => last_offset,
        Ok(_) => 0,
        Err(_) => last_offset,
    }
}

fn pause_owner_watcher_for_prompt_relay(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    expected_output_path: &str,
    resume_offset: Option<u64>,
) -> PromptWatcherPauseAttempt {
    let owner = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(&prompt.tmux_session_name)
        .unwrap_or(channel_id);
    let Some(watcher) = shared.tmux_watchers.get(&owner) else {
        return PromptWatcherPauseAttempt::NotNeeded;
    };
    if watcher.output_path != expected_output_path {
        tracing::debug!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            watcher_output_path = %watcher.output_path,
            expected_output_path,
            "skipping SSH-direct TUI prompt watcher pause; owner watches a different output path"
        );
        return PromptWatcherPauseAttempt::NotNeeded;
    }
    let already_paused = watcher.paused.load(Ordering::Acquire);
    if already_paused {
        tracing::debug!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            "waiting for SSH-direct TUI prompt watcher pause; owner watcher is already paused"
        );
        return PromptWatcherPauseAttempt::Busy;
    }
    let mut resume_guard = watcher.resume_offset.lock().ok();
    if resume_guard
        .as_ref()
        .and_then(|guard| **guard)
        .is_some_and(|existing_offset| {
            pending_resume_offset_blocks_prompt_pause(Some(existing_offset), resume_offset)
        })
    {
        tracing::debug!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            "waiting for SSH-direct TUI prompt watcher pause; owner watcher has a pending resume offset"
        );
        return PromptWatcherPauseAttempt::Busy;
    }
    watcher.paused.store(true, Ordering::Release);
    if let Some(guard) = resume_guard.as_mut()
        && resume_offset.is_some()
    {
        **guard = resume_offset;
    }
    watcher.pause_epoch.fetch_add(1, Ordering::AcqRel);
    PromptWatcherPauseAttempt::Acquired(WatcherPauseGuard {
        paused: watcher.paused.clone(),
        resume_offset: watcher.resume_offset.clone(),
        pause_epoch: watcher.pause_epoch.clone(),
        changed: true,
        abort_resume_offset: None,
    })
}

fn pending_resume_offset_blocks_prompt_pause(
    existing_resume_offset: Option<u64>,
    requested_resume_offset: Option<u64>,
) -> bool {
    existing_resume_offset.is_some() && requested_resume_offset.is_none()
}

struct WatcherPauseGuard {
    paused: Arc<AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<AtomicU64>,
    changed: bool,
    abort_resume_offset: Option<u64>,
}

impl WatcherPauseGuard {
    fn arm_abort_resume_offset(&mut self, offset: u64) {
        self.abort_resume_offset = Some(offset);
    }

    fn commit_resume_offset(&mut self, offset: u64) {
        self.abort_resume_offset = None;
        self.set_resume_offset(offset);
    }

    fn set_resume_offset(&self, offset: u64) {
        if let Ok(mut guard) = self.resume_offset.lock() {
            *guard = Some(offset);
        }
    }
}

impl Drop for WatcherPauseGuard {
    fn drop(&mut self) {
        if self.changed {
            if let Some(offset) = self.abort_resume_offset.take()
                && let Ok(mut guard) = self.resume_offset.lock()
                && guard.is_none()
            {
                *guard = Some(offset);
            }
            self.pause_epoch.fetch_add(1, Ordering::AcqRel);
            self.paused.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;

    #[test]
    fn formats_ssh_direct_prompt_notification() {
        let output = format_ssh_direct_prompt_notification("claude", "AgentDesk-claude-a", "hi");

        assert!(output.contains("SSH direct input relayed from Claude TUI"));
        assert!(output.contains("`AgentDesk-claude-a`"));
        assert!(output.contains("```text\nhi\n```"));
    }

    #[test]
    fn formats_ssh_direct_prompt_notification_with_truncation() {
        let prompt = "x".repeat(SSH_DIRECT_PROMPT_PREVIEW_LIMIT + 20);
        let output = format_ssh_direct_prompt_notification("codex", "AgentDesk-codex-a", &prompt);

        assert!(output.contains("SSH direct input relayed from Codex TUI"));
        assert!(output.contains("..."));
        assert!(output.len() < prompt.len() + 120);
    }

    #[test]
    fn formats_ssh_direct_prompt_notification_escapes_code_fence() {
        let output = format_ssh_direct_prompt_notification("codex", "tmux`name", "a ``` fence");

        assert!(output.contains("`tmux'name`"));
        assert!(output.contains("a ` ` ` fence"));
    }

    #[test]
    fn builds_external_input_inflight_from_notify_anchor() {
        let prompt = ObservedTuiPrompt {
            provider: "codex".to_string(),
            tmux_session_name: "AgentDesk-codex-review-cdx".to_string(),
            prompt: "typed over ssh".to_string(),
        };
        let runtime_binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/tmp/agentdesk-test-codex-rollout.jsonl".to_string(),
            relay_output_path: Some("/tmp/agentdesk-test-codex-wrapper.jsonl".to_string()),
            input_fifo_path: None,
            session_id: Some("thread-123".to_string()),
            last_offset: 100,
            relay_last_offset: Some(123),
        };
        let state = build_ssh_direct_inflight_state(
            &prompt,
            ChannelId::new(42),
            MessageId::new(9001),
            &runtime_binding,
            123,
        )
        .expect("inflight state");

        assert_eq!(state.provider, "codex");
        assert_eq!(state.channel_id, 42);
        assert_eq!(state.user_msg_id, 9001);
        assert_eq!(state.current_msg_id, 9001);
        assert_eq!(state.user_text, "typed over ssh");
        assert_eq!(state.session_id.as_deref(), Some("thread-123"));
        assert_eq!(
            state.output_path.as_deref(),
            Some("/tmp/agentdesk-test-codex-wrapper.jsonl")
        );
        assert_eq!(state.input_fifo_path, None);
        assert_eq!(
            state.turn_source,
            super::super::inflight::TurnSource::ExternalInput
        );
        assert_eq!(state.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert!(state.watcher_owns_live_relay);
        assert_eq!(
            state.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-review-cdx")
        );
        assert_eq!(state.channel_name.as_deref(), Some("review-cdx"));
        assert_eq!(state.turn_start_offset, Some(123));
        assert_eq!(state.last_offset, 123);
        assert_eq!(state.last_watcher_relayed_offset, Some(123));
    }

    #[test]
    fn replay_start_offset_uses_handoff_offset_not_late_live_eof() {
        let path = std::env::temp_dir().join(format!(
            "agentdesk-tui-runtime-binding-{}-{}.jsonl",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, b"already written assistant bytes").expect("write temp rollout");
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("thread-123".to_string()),
            last_offset: 7,
            relay_last_offset: None,
        };

        assert_eq!(runtime_binding_replay_start_offset(&binding), 7);

        let truncated_binding = TuiRuntimeBinding {
            last_offset: 10_000,
            ..binding
        };
        assert_eq!(runtime_binding_replay_start_offset(&truncated_binding), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn replay_start_offset_uses_relay_path_when_runtime_path_differs() {
        let runtime_path = std::env::temp_dir().join(format!(
            "agentdesk-tui-runtime-binding-runtime-{}-{}.jsonl",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let relay_path = std::env::temp_dir().join(format!(
            "agentdesk-tui-runtime-binding-relay-{}-{}.jsonl",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&runtime_path, b"rollout bytes beyond runtime offset")
            .expect("write runtime temp");
        std::fs::write(&relay_path, b"wrapper bytes beyond relay offset")
            .expect("write relay temp");
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: runtime_path.display().to_string(),
            relay_output_path: Some(relay_path.display().to_string()),
            input_fifo_path: None,
            session_id: Some("thread-123".to_string()),
            last_offset: 3,
            relay_last_offset: Some(8),
        };

        assert_eq!(runtime_binding_replay_start_offset(&binding), 8);

        let truncated = TuiRuntimeBinding {
            relay_last_offset: Some(10_000),
            ..binding
        };
        assert_eq!(runtime_binding_replay_start_offset(&truncated), 0);
        let _ = std::fs::remove_file(runtime_path);
        let _ = std::fs::remove_file(relay_path);
    }

    #[test]
    fn watcher_pause_guard_replays_start_offset_on_abort_drop() {
        let paused = Arc::new(AtomicBool::new(true));
        let resume_offset = Arc::new(std::sync::Mutex::new(None));
        let pause_epoch = Arc::new(AtomicU64::new(7));
        {
            let mut guard = WatcherPauseGuard {
                paused: paused.clone(),
                resume_offset: resume_offset.clone(),
                pause_epoch: pause_epoch.clone(),
                changed: true,
                abort_resume_offset: None,
            };
            guard.arm_abort_resume_offset(42);
        }

        assert!(!paused.load(Ordering::Acquire));
        assert_eq!(*resume_offset.lock().expect("resume offset lock"), Some(42));
        assert_eq!(pause_epoch.load(Ordering::Acquire), 8);
    }

    #[test]
    fn watcher_pause_guard_preserves_existing_resume_offset_on_abort_drop() {
        let paused = Arc::new(AtomicBool::new(true));
        let resume_offset = Arc::new(std::sync::Mutex::new(Some(77)));
        let pause_epoch = Arc::new(AtomicU64::new(7));
        {
            let mut guard = WatcherPauseGuard {
                paused: paused.clone(),
                resume_offset: resume_offset.clone(),
                pause_epoch: pause_epoch.clone(),
                changed: true,
                abort_resume_offset: None,
            };
            guard.arm_abort_resume_offset(42);
        }

        assert!(!paused.load(Ordering::Acquire));
        assert_eq!(*resume_offset.lock().expect("resume offset lock"), Some(77));
        assert_eq!(pause_epoch.load(Ordering::Acquire), 8);
    }

    #[test]
    fn pending_resume_offset_blocks_prompt_pause_without_requested_offset() {
        assert!(pending_resume_offset_blocks_prompt_pause(Some(77), None));
        assert!(!pending_resume_offset_blocks_prompt_pause(None, None));
        assert!(!pending_resume_offset_blocks_prompt_pause(
            Some(77),
            Some(42)
        ));
    }
}
