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
    ObservedTuiPrompt, extract_prompt_from_hook_payload, observe_prompt_by_provider_session_at,
    subscribe_observed_prompts,
};

const SSH_DIRECT_PROMPT_PREVIEW_LIMIT: usize = 1500;
const CODEX_IDLE_ROLLOUT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL: Duration = Duration::from_secs(5);
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

    super::task_supervisor::spawn_observed("tui_prompt_relay_observer", async move {
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
    crate::services::tui_prompt_dedupe::record_external_input_relay_lease(
        &prompt.provider,
        &prompt.tmux_session_name,
        Some(channel_id.get()),
    );
    let Some(registry) = shared.health_registry() else {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
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
        anchor_message_id = anchor_message.id.get(),
        "SSH-direct TUI prompt notified; runtime relay will handle output without synthetic inflight"
    );

    #[cfg(unix)]
    maybe_spawn_claude_idle_response_tail(shared.clone(), channel_id, &prompt).await;
}

#[cfg(unix)]
async fn maybe_spawn_claude_idle_response_tail(
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
) {
    if !prompt
        .provider
        .trim()
        .eq_ignore_ascii_case(ProviderKind::Claude.as_str())
    {
        return;
    }
    if super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get()).is_some() {
        return;
    }
    if shared
        .tmux_watchers
        .tmux_session_is_stale(&prompt.tmux_session_name)
        .is_some_and(|stale| !stale)
    {
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

    let transcript_path = PathBuf::from(&binding.output_path);
    let start_offset = claude_idle_response_start_offset_after_timestamp(
        &transcript_path,
        prompt.observed_at,
        binding.last_offset,
    );
    spawn_claude_idle_response_tail_once(
        shared,
        prompt.tmux_session_name.clone(),
        channel_id,
        transcript_path,
        start_offset,
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
) -> bool {
    {
        let mut active = CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !active.insert(tmux_session_name.clone()) {
            return false;
        }
    }

    super::task_supervisor::spawn_observed("claude_idle_response_tail", async move {
        let _tail_guard = ClaudeIdleTailGuard {
            tmux_session_name: tmux_session_name.clone(),
        };
        run_claude_idle_response_tail(
            shared,
            tmux_session_name.clone(),
            channel_id,
            transcript_path,
            start_offset,
        )
        .await;
    });
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
                if shared
                    .tmux_watchers
                    .tmux_session_is_stale(&tmux_session_name)
                    .is_some_and(|stale| !stale)
                {
                    continue;
                }
                let Some(channel_id) = owner_channel_for_tmux_session(&shared, &tmux_session_name)
                else {
                    continue;
                };
                if super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get())
                    .is_some()
                {
                    continue;
                }

                let transcript_path = PathBuf::from(&binding.output_path);
                let scan = match scan_claude_idle_transcript_for_prompt(
                    &transcript_path,
                    binding.last_offset,
                ) {
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
                        if offset != binding.last_offset {
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
                        let observation =
                            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux(
                                ProviderKind::Claude.as_str(),
                                &tmux_session_name,
                                &prompt,
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
                        if claude_idle_prompt_observation_should_tail_response(observation) {
                            spawn_claude_idle_response_tail_once(
                                shared.clone(),
                                tmux_session_name.clone(),
                                channel_id,
                                transcript_path,
                                line_end_offset,
                            );
                        }
                    }
                }
            }

            tokio::time::sleep(CODEX_IDLE_ROLLOUT_POLL_INTERVAL).await;
        }
    });
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
        if existing_binding.is_some() && existing_channel.is_some() {
            continue;
        }
        let Some(channel_id) = resolve_rehydrated_claude_tmux_channel_id(&tmux_session_name) else {
            continue;
        };
        if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name) {
            continue;
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

        let Some(binding) = rehydrated_claude_tui_binding_for_tmux_session(&tmux_session_name)
        else {
            continue;
        };

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
            "rehydrated Claude TUI direct relay binding"
        );
    }
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
        if segments.into_iter().any(|segment| {
            ProviderKind::Claude.build_tmux_session_name(segment) == tmux_session_name
        }) {
            if matched.is_some_and(|existing| existing != binding.channel_id) {
                tracing::warn!(
                    tmux_session_name,
                    channel_id = binding.channel_id,
                    existing_channel_id = matched.unwrap_or_default(),
                    "Claude TUI rehydrate skipped ambiguous exact session-name match"
                );
                return None;
            }
            matched = Some(binding.channel_id);
        }
    }
    matched
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
                        let observation =
                            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux(
                                ProviderKind::Codex.as_str(),
                                &tmux_session_name,
                                &prompt,
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

                        crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                            &tmux_session_name,
                            &binding.output_path,
                            line_end_offset,
                        );
                        active_tails.insert(tmux_session_name.clone());
                        let shared_for_tail = shared.clone();
                        let done_tx_for_tail = done_tx.clone();
                        super::task_supervisor::spawn_observed(
                            "codex_idle_response_tail",
                            async move {
                                let _done_guard = CodexIdleTailDoneGuard {
                                    tmux_session_name: Some(tmux_session_name.clone()),
                                    done_tx: done_tx_for_tail,
                                };
                                run_codex_idle_response_tail(
                                    shared_for_tail,
                                    tmux_session_name.clone(),
                                    channel_id,
                                    rollout_path,
                                    line_end_offset,
                                )
                                .await;
                            },
                        );
                    }
                }
            }

            tokio::time::sleep(CODEX_IDLE_ROLLOUT_POLL_INTERVAL).await;
        }
    });
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
) -> Result<(), String> {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::warn!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
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
            post_tui_idle_response_session_idle(
                shared,
                &provider,
                channel_id,
                tmux_session_name,
                tail_started_at,
            )
            .await;
            tracing::info!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                chars = formatted.chars().count(),
                prompt_anchor_message_id = reference.map(|(_, message_id)| message_id.get()),
                "TUI idle response relayed"
            );
            Ok(())
        }
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                error = %error,
                "failed to relay TUI idle response"
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
) {
    if shared.mailbox(channel_id).cancel_token().await.is_some() {
        tracing::debug!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            "skipping TUI idle response session-idle commit; mailbox turn is active"
        );
        return;
    }

    if super::inflight::load_inflight_state(provider, channel_id.get()).is_some() {
        tracing::debug!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
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
