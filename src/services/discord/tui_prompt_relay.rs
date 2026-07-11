use std::collections::HashSet;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::SharedData;
use super::inflight::{InflightTurnState, RelayOwnerKind, TurnSource};
use super::outbound::delivery_record as dr; // #3089 B2c
use super::turn_bridge::{TurnBridgeContext, spawn_turn_bridge};
use super::turn_view_reconciler::note_tui_anchor_started as started;
use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::claude_tui::hook_server::{HookEventKind, subscribe_hook_events};
use crate::services::memory::TokenUsage;
use crate::services::provider::{CancelToken, ProviderKind, ReadOutputResult};
use crate::services::tui_prompt_dedupe::{
    ExternalInputRelayLease, ExternalInputRelayOwner, ObservedTuiPrompt,
    extract_prompt_from_hook_payload, observe_prompt_by_provider_session_at,
    subscribe_observed_prompts,
};
use tracing::Instrument;

mod injected_prompt_policy;
use self::injected_prompt_policy::{
    InjectedPromptClass, classify_injected_prompt, format_slash_command_control_note,
    format_ssh_direct_prompt_notification, format_system_continuation_note,
    is_slash_command_control_prompt, should_suppress_local_only_kind_note_after_continuation,
    slash_command_control_kind, slash_command_control_prompt_is_caveat_only,
    slash_command_control_prompt_is_local_command_stdout,
};
#[cfg(test)]
use self::injected_prompt_policy::{
    format_subagent_notification_card, is_start_anchored_task_notification,
};
mod task_notification_prompt;

mod idle_transcript_scan;
use self::idle_transcript_scan::{
    ClaudeIdleTranscriptScan, CodexIdleRolloutScan,
    claude_idle_prompt_observation_should_tail_response,
    codex_idle_prompt_observation_should_tail_response,
    scan_claude_idle_transcript_for_last_prompt, scan_claude_idle_transcript_for_prompt,
    scan_codex_idle_rollout_for_latest_prompt_matching, scan_codex_idle_rollout_for_prompt,
};

#[cfg(unix)]
pub(in crate::services::discord) mod rehydration;
#[cfg(all(unix, test))]
use self::rehydration::{
    claude_tui_session_is_dead_orphaned, evict_dead_orphaned_claude_tui_mirrors,
    pane_is_confirmed_dead_orphaned,
};
#[cfg(unix)]
use self::rehydration::{
    codex_tui_rehydrated_binding_from_rollout_path, rehydrate_existing_claude_tui_bindings,
    rehydrate_existing_codex_tui_bindings, rehydrated_claude_tui_binding_for_tmux_session,
};

mod anchor_completion;
mod bridge_completion;
mod bridge_gateway;
use self::anchor_completion::{
    DeferredAnchorCompletionDrain, decide_deferred_anchor_completion_drain,
};
pub(in crate::services::discord) use self::anchor_completion::{
    complete_tui_direct_anchor_lifecycle_for_inflight,
    complete_tui_direct_prompt_anchor_lifecycle_if_present,
    should_complete_tui_direct_anchor_lifecycle,
};
use self::bridge_completion::ensure_tui_direct_bridge_delivery_committed;
use self::bridge_gateway::TuiDirectBridgeGateway;

#[cfg(unix)]
mod launch_script;
#[cfg(unix)]
use self::launch_script::parse_claude_tui_launch_script;

#[cfg(unix)]
mod idle_offset_resolution;
#[cfg(all(unix, test))]
use self::idle_offset_resolution::claude_idle_response_start_offset_after_timestamp;
#[cfg(unix)]
use self::idle_offset_resolution::{
    clamp_idle_tail_start_offset_to_committed, resolve_idle_tail_start_offset,
};

#[cfg(unix)]
mod codex_idle_rollout;
#[cfg(unix)]
use self::codex_idle_rollout::spawn_codex_idle_rollout_relay;
mod relay_ownership;
#[cfg(test)]
use self::relay_ownership::external_input_relay_owner_for_watchers;
#[cfg(unix)]
use self::relay_ownership::resolved_codex_idle_relay_binding;
use self::relay_ownership::{
    TuiDirectExternalInputLeaseGuard, TuiDirectObservedLeaseEarlyReturnGuard,
    bridge_adapter_owns_external_turn, claim_should_adopt_relay_owner,
    clear_external_input_bridge_lease_if_current, clear_observed_external_turn_lease_if_current,
    deferred_claim_requires_bridge_tail_relayer, external_input_relay_binding,
    external_input_relay_output_path, external_input_relay_start_offset,
    observer_should_spawn_bridge_tail, parse_external_input_relay_owner,
    record_external_turn_lease_for_output, record_observed_external_turn_lease,
    session_bound_discord_delivery_enabled,
};

mod synthetic_orphan_reclaim; // #3982 orphan-at-birth reclaim trigger (see module doc)
mod synthetic_start;
mod synthetic_start_wiring; // #4002 shared Path-X wiring with #4082 neutral-note gate
#[cfg(test)]
pub(in crate::services::discord) use self::synthetic_start::synthetic_start_offset_carry_forward;
pub(in crate::services::discord) use self::synthetic_start::tui_direct_watcher_synthetic_inflight_matches;
use self::synthetic_start::{
    build_tui_direct_synthetic_inflight_state, restore_pending_starts,
    tui_direct_synthetic_inflight_active_for_prompt,
};
#[cfg(unix)]
#[allow(unused_imports)]
use self::synthetic_start::{
    codex_ownerless_external_input_inflight_needs_rollout_recovery,
    finish_tui_direct_synthetic_turn_if_current, wait_for_tui_direct_watcher_synthetic_claim,
};
#[cfg(test)]
use self::synthetic_start::{
    finish_tui_direct_synthetic_pre_save_failure, pending_start_abort_cleanup_fn,
};
#[cfg(test)]
use self::synthetic_start::{
    release_stale_ownerless_tui_direct_mailbox_if_current, tui_direct_watcher_can_own_output,
};

#[cfg(unix)]
mod claude_idle_bridge;
#[cfg(unix)]
mod claude_idle_runtime;
#[cfg(unix)]
mod claude_idle_tail;
#[cfg(unix)]
#[allow(unused_imports)]
use self::claude_idle_bridge::build_tui_direct_bridge_inflight_state;
#[cfg(all(unix, test))]
use self::claude_idle_bridge::{
    compose_tui_idle_response, forward_idle_stream_into_bridge,
    tui_idle_tail_should_commit_runtime_binding_offset,
};
#[cfg(unix)]
use self::claude_idle_bridge::{
    idle_stream_message_is_content, stream_tui_idle_response_through_bridge,
    tui_idle_tail_stream_should_commit_runtime_binding_offset,
};
#[cfg(unix)]
pub(super) use self::claude_idle_runtime::resolve_rehydrated_claude_tmux_channel_id;
#[cfg(unix)]
use self::claude_idle_runtime::{
    DEAD_ORPHANED_PANE_PROBE_DELAY, DEAD_ORPHANED_PANE_PROBE_SAMPLES,
    advance_claude_tmux_runtime_binding_offset, claude_tui_rehydrate_start_offset,
    claude_tui_runtime_binding_matches_launch, resolve_idle_relay_transcript,
    resolve_rehydrated_tmux_channel_id, resolved_claude_idle_relay_transcript_path,
    spawn_claude_idle_transcript_relay,
};
#[cfg(unix)]
pub(in crate::services::discord) use self::claude_idle_runtime::{
    claude_tui_launch_context, other_session_claimed_transcripts,
};
#[cfg(all(unix, test))]
use self::claude_idle_runtime::{
    rehydrated_channel_id_for_segment, rehydrated_claude_channel_id_for_segment,
};
#[cfg(all(unix, test))]
use self::claude_idle_tail::{
    inflight_is_current_turn_synthetic, wait_for_transient_state_to_clear,
};
#[cfg(unix)]
use self::claude_idle_tail::{
    maybe_spawn_claude_idle_response_tail, spawn_claude_idle_response_tail_once,
};

const SSH_DIRECT_PROMPT_PREVIEW_LIMIT: usize = 1500;
const CODEX_IDLE_ROLLOUT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const CLAUDE_IDLE_INFLIGHT_DRAIN_WAIT: Duration = Duration::from_secs(5);
const CLAUDE_IDLE_INFLIGHT_DRAIN_POLL: Duration = Duration::from_millis(100);
/// #2843: when the background idle relay loop discovers that a session's
/// transcript path changed, scan this many bytes back from EOF (not from EOF
/// itself) so a prompt already written to the freshly-resolved transcript is
/// still observed and its response relayed.
const CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES: u64 = 65_536;
const CODEX_IDLE_PROMPT_ANCHOR_WAIT: Duration = Duration::from_secs(2);
const CODEX_IDLE_PROMPT_ANCHOR_POLL: Duration = Duration::from_millis(100);
const TUI_DIRECT_SYNTHETIC_CLAIM_WAIT: Duration = Duration::from_secs(2);
const TUI_DIRECT_SYNTHETIC_CLAIM_POLL: Duration = Duration::from_millis(100);
pub(in crate::services::discord) const TUI_DIRECT_SYNTHETIC_OWNER_USER_ID: u64 = 1;
static CODEX_IDLE_ROLLOUT_RELAY_STARTED: AtomicBool = AtomicBool::new(false);
static CLAUDE_IDLE_TRANSCRIPT_RELAY_STARTED: AtomicBool = AtomicBool::new(false);
static CLAUDE_IDLE_RESPONSE_TAILS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// #3178: dedupe window for the machine slash-command control turn. The #3153
/// `/loop` double-post arrives as TWO independent observed prompts — the raw
/// `/loop …` ScheduleWakeup echo AND the Claude Code expanded `<command-*>`
/// wrapper — both within a short window and both mapping to the same command
/// kind. They are the two halves of ONE injection and arrive within tens to
/// hundreds of milliseconds, so a tight 2s window collapses them to a SINGLE
/// active turn (the duplicate half is dropped BEFORE any lease/anchor is
/// created). A genuine SECOND `/loop` or `/compact` arrives seconds later, well
/// outside this window, so it gets its own active turn (no over-suppression).
const SLASH_COMMAND_CONTROL_DEDUPE_WINDOW: Duration = Duration::from_secs(2);
const COMPACT_REPLAY_KIND_NOTE_SUPPRESSION_WINDOW: Duration = Duration::from_secs(30);
/// #3178: last-seen timestamp per (tmux_session, command_kind) for the
/// slash-command-control turn, so the two halves of the #3153 double-post create
/// the active turn only once. Keyed by a stable string built from the REAL
/// command name (`/loop`, `/compact`, or the command name itself) so the raw echo
/// and the expanded `<command-*>` wrapper for the SAME command collapse to one
/// entry, while two DIFFERENT commands within the window never collapse together.
static SLASH_COMMAND_CONTROL_LAST_POSTED: LazyLock<
    Mutex<std::collections::HashMap<String, std::time::Instant>>,
> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
static SYSTEM_CONTINUATION_LAST_RENDERED: LazyLock<
    Mutex<std::collections::HashMap<String, std::time::Instant>>,
> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

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

    // #3154 restart durability: restore any durable pending synthetic
    // turn-starts for this provider before the observer loop runs, so a dcserver
    // restart mid-wait neither loses the wakeup turn nor resubmits its prompt.
    restore_pending_starts(&shared, &provider);

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
    // #3811: TUI-direct is an id-0 synthetic turn — clear any stale interactive 요청 anchor.
    let live_events = &shared.ui.placeholder_live_events;
    live_events.set_turn_request_anchor(channel_id, None);
    let recap_provider = ProviderKind::from_str_or_unsupported(&prompt.provider);
    // #3178 (codex P1 lease-overwrite): run the slash-command-control dedupe BEFORE
    // recording ANY external-input lease. The #3153 double-post (raw echo + expanded
    // `<command-*>` wrapper, ~tens-of-ms apart) must not let the SECOND half record a
    // lease: the table is one-per-(provider,session), so it would overwrite the first
    // turn's lease and its guard would clear that generation, stranding the first
    // bridge tail. Drop the duplicate here, before any lease/anchor/inflight exists;
    // a genuine second /loop / /compact falls outside the 2s window → fresh turn.
    let relay_prompt_decision = relay_observed_prompt_injected_prompt_decision(&prompt.prompt);
    let injected_class = relay_prompt_decision.injected_class;
    let task_notification =
        task_notification_prompt::observe(shared, &prompt, channel_id, injected_class);
    // #3305/#4033/#4082: one pure decision drives dedupe and the lifecycle
    // skips, so stdout halves and compact continuation records use the same
    // classifier for note rendering, external-owner selection, and synthetic start.
    let local_only_slash = relay_prompt_decision.local_only_slash;
    if matches!(injected_class, InjectedPromptClass::SlashCommandControl) {
        let kind = relay_prompt_decision
            .slash_command_kind
            .as_deref()
            .expect("slash command control decisions carry a kind");
        if !slash_command_control_turn_is_first_sighting(&prompt.tmux_session_name, kind) {
            // #3153 second half within the 2s window: drop before any lease/anchor/
            // inflight exists; the first half already relays via its own bridge tail.
            tracing::info!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                tmux_session_name = %prompt.tmux_session_name,
                slash_command_kind = %kind,
                "deduped near-simultaneous machine slash-command control half (within 2s window); dropped BEFORE recording any external-input lease so the first active turn's lease is preserved"
            );
            return;
        }
    }
    let mut lease = record_observed_external_turn_lease(shared, &prompt, channel_id);
    // #3041 P1-4 codex: arm an early-return RAII guard the instant the lease is
    // recorded — every failure early-return below would otherwise leave the lease set
    // for the full TTL, blocking watcher/sink delivery ~10min. The guard clears
    // EXACTLY the recorded generation on drop and is DISARMED on the success path
    // before the bridge-tail ownership block (a bridge-owned turn keeps its lease).
    let mut observed_lease_early_return_guard = TuiDirectObservedLeaseEarlyReturnGuard::arm(
        &prompt.provider,
        &prompt.tmux_session_name,
        channel_id,
        lease.generation,
    );
    let Some(task_gate) = task_notification_prompt::resolve_gate(
        shared,
        &prompt,
        channel_id,
        injected_class,
        &lease,
        task_notification,
    )
    .await
    else {
        return;
    };
    let notify_http = task_gate.notify_http;
    let task_card_anchor = task_gate.card_anchor;
    // A TUI-direct prompt has now been observed for this channel. Bump idle
    // recap generation before the pre-claim clear so an in-flight recap POST job
    // cannot persist a stale card after a deferred/local-only/aborted synthetic
    // start path (#3296, #3878).
    if let Some(pool) = shared.pg_pool.as_ref().cloned() {
        if let Err(e) = super::idle_recap::bump_turn_generation(
            &pool,
            channel_id.get(),
            &recap_provider,
            lease.session_key.as_deref(),
        )
        .await
        {
            tracing::warn!(
                error = %e,
                channel_id = channel_id.get(),
                provider = %recap_provider.as_str(),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                "idle_recap: failed to bump turn generation on TUI-direct observation"
            );
        }
        super::idle_recap::spawn_clear_captured_idle_recap_for_channel(
            notify_http.clone(),
            pool,
            channel_id.get(),
        )
        .await;
    }
    // #3164 / #750 invariant: the `⏳` MUST be added by the SAME bot identity that
    // later removes it (the shared reaction remove path only removes `@me`; a
    // different bot leaves the hourglass forever). The note BODY may be any bot
    // (`notify_http`), but the reaction never falls back to it — on resolve failure
    // we skip the add (warn). R2 issue-1: resolve the add-bot from the SAME source
    // the completion removes with (this relay's `serenity_http_or_token_fallback()`,
    // the watcher's `http`), NOT a name-only `resolve_bot_http` (which in a multi-
    // runtime/same-provider deployment can pick a DIFFERENT account and re-break add≡remove).
    let command_http = shared.serenity_http_or_token_fallback();
    if command_http.is_none() {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            "skipping TUI-direct ⏳ reaction; provider serenity http unavailable \
             (adding with a different bot would leave an un-removable hourglass)"
        );
    }
    let suppresses_user_turn_lifecycle = injected_class.suppresses_user_turn_lifecycle();
    // #3176: anchor id of THIS turn's synthetic inflight (set only on the anchor-
    // posting branch); the idle-tail drain-wait uses it to identity-pin our own row
    // (no self-deadlock) while still waiting on a genuinely distinct previous turn.
    let current_turn_anchor_id: Option<u64>;
    // #3154 P1-3: function-scope so the post-block bridge-tail guard reads it; set
    // true when the synthetic turn-start is deferred to the detached worker, so the
    // observer skips its own BridgeAdapter tail (no duplicate relay).
    let deferred_synthetic_start: bool;
    if local_only_slash {
        // #3305: a LOCAL-completing pass-through (/effort /compact /cost /context)
        // renders in the TUI but starts no model turn. Post ONLY the kind-only
        // guidance note (the operator still sees the command ran) and RETURN —
        // before the `disarm()` below — so the armed `observed_lease_early_return
        // _guard` clears EXACTLY the lease this observation recorded (generation-
        // exact, #3041 infra, no new cleanup path). No anchor → no ⏳ (so the #3164
        // add≡remove invariant holds trivially: no add, no asymmetry); no
        // `claim_tui_direct_synthetic_turn` → no synthetic inflight → the next
        // injection is not FOREIGN-ABORTed and the #3302 sweeper sees no fake row.
        let kind = relay_prompt_decision
            .slash_command_kind
            .as_deref()
            .expect("local-only slash decisions carry a kind");
        // #3388: in-session /compact rewrites can replay the local command stub
        // seconds after the continuation banner; hide that duplicate note. The
        // real machine-injected /compact (#3262) happens minutes before compaction
        // completes, so it stays outside this narrow replay window.
        if local_only_kind_note_suppressed_by_recent_continuation(&prompt.tmux_session_name, kind) {
            tracing::info!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                session = %prompt.tmux_session_name,
                kind = %kind,
                "suppressed local-only slash-command kind note after recent system continuation note"
            );
            return;
        }
        let note =
            format_slash_command_control_note(&prompt.tmux_session_name, kind, &prompt.prompt);
        match channel_id.say(&*notify_http, note).await {
            Ok(message) => tracing::info!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                tmux_session_name = %prompt.tmux_session_name,
                slash_command_kind = %kind,
                note_message_id = message.id.get(),
                "rendered local-only pass-through slash command as a kind-only note; no active-turn lifecycle (no ⏳/anchor/synthetic inflight), no model turn to relay"
            ),
            Err(error) => tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                tmux_session_name = %prompt.tmux_session_name,
                slash_command_kind = %kind,
                error = %error,
                "failed to send local-only pass-through slash command note"
            ),
        }
        return;
    }
    if suppresses_user_turn_lifecycle {
        // #3178 (codex fix): only SystemContinuation reaches here now —
        // SlashCommandControl was removed from `suppresses_user_turn_lifecycle`
        // and takes the active-turn `else` block below.
        let note = format_system_continuation_note(&prompt.tmux_session_name, &prompt.prompt);
        match channel_id.say(&*notify_http, note).await {
            Ok(message) => {
                record_system_continuation_note_rendered(&prompt.tmux_session_name);
                tracing::info!(
                    provider = %prompt.provider,
                    channel_id = channel_id.get(),
                    tmux_session_name = %prompt.tmux_session_name,
                    note_message_id = message.id.get(),
                    "rendered system/compact continuation injection as neutral session note; no active-turn lifecycle, no external turn owner, no synthetic inflight"
                );
            }
            Err(error) => {
                tracing::warn!(
                    provider = %prompt.provider,
                    channel_id = channel_id.get(),
                    tmux_session_name = %prompt.tmux_session_name,
                    error = %error,
                    "failed to send system/compact continuation session note"
                );
            }
        }
        return;
    } else {
        // #3178 (codex fix): a SlashCommandControl is a FULL active turn now (anchor +
        // ⏳ + synthetic inflight like HumanTuiDirect). Its anchor content carries the
        // /loop directive body but never the <command-*> wrapper or Compacted stdout;
        // the #3153 duplicate half was already dropped before the lease record above.
        // #4055: a `<task-notification>` auto-turn's card was already confirmed
        // through the durable authority above. This block only attaches the
        // existing card id to the normal synthetic-turn lifecycle.
        // HumanTuiDirect keeps the raw render; SystemContinuation handled above (#3100).
        let content = if matches!(injected_class, InjectedPromptClass::SlashCommandControl) {
            let kind = relay_prompt_decision
                .slash_command_kind
                .as_deref()
                .expect("slash command control decisions carry a kind");
            format_slash_command_control_note(&prompt.tmux_session_name, kind, &prompt.prompt)
        } else if matches!(injected_class, InjectedPromptClass::TaskNotificationEvent) {
            String::new()
        } else {
            format_ssh_direct_prompt_notification(
                &prompt.provider,
                &prompt.tmux_session_name,
                &prompt.prompt,
            )
        };
        let anchor_message_id = if let Some(message_id) = task_card_anchor {
            message_id
        } else {
            match channel_id.say(&*notify_http, content).await {
                Ok(message) => message.id,
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
            }
        };
        // #3176: pin this turn's anchor id — it becomes the synthetic inflight's
        // `user_msg_id` below, so the idle-tail drain-wait can recognise our own row.
        current_turn_anchor_id = Some(anchor_message_id.get());
        // Add before the anchor becomes findable so fast completion cannot re-leave ⏳.
        started(
            shared,
            channel_id,
            anchor_message_id,
            lease.generation,
            "tui_anchor_start",
        )
        .await;
        crate::services::tui_prompt_dedupe::record_prompt_anchor(
            &prompt.provider,
            &prompt.tmux_session_name,
            channel_id.get(),
            anchor_message_id.get(),
        );
        // #3174: turn-identity guard on the ⏳ lifecycle. A lease-gated completion
        // firing inside the sub-second notify+⏳-add window left a deferred marker;
        // now that THIS turn's anchor exists, drain it (⏳ → ✅ swap). P1: drain ONLY
        // the `lease.generation` match (never cross-consume a newer turn). P2: PEEK
        // first, consume only with `command_http`, else leave it within its TTL.
        // Pure decision: [`decide_deferred_anchor_completion_drain`]; no-marker no-op.
        match decide_deferred_anchor_completion_drain(
            &prompt.provider,
            &prompt.tmux_session_name,
            lease.generation,
            command_http.is_some(),
        ) {
            DeferredAnchorCompletionDrain::Complete => {
                // command_http is available (decision required it) — deliver.
                let _ = complete_tui_direct_prompt_anchor_lifecycle_if_present(
                    shared,
                    &prompt.provider,
                    &prompt.tmux_session_name,
                    channel_id,
                    lease.generation,
                    "relay_record_prompt_anchor_drains_deferred_lease_gated_completion",
                )
                .await;
            }
            DeferredAnchorCompletionDrain::LeftIntactHttpUnavailable => {
                // #3174 codex P2: command_http unavailable — the decision LEFT the
                // marker set (did not consume it), so the deferred completion stays
                // claimable (within its TTL) instead of losing the ⏳ → ✅ swap.
                tracing::warn!(
                    provider = %prompt.provider,
                    channel_id = channel_id.get(),
                    tmux_session_name = %prompt.tmux_session_name,
                    turn_lease_generation = lease.generation,
                    "#3174: deferred lease-gated completion owed but command_http unavailable; left marker intact (fail-open) rather than dropping the ⏳ swap"
                );
            }
            DeferredAnchorCompletionDrain::NoMarker => {}
        }
        // #3099: a `<task-notification>` auto-turn earns the same synthetic ownership
        // as human direct input (its ⏳ cleanup anchors on its own id), so it does NOT
        // short-circuit here. Only SystemContinuation skips this active-turn block.
        debug_assert!(
            injected_class.is_human_active_turn()
                || matches!(
                    injected_class,
                    InjectedPromptClass::TaskNotificationEvent
                        | InjectedPromptClass::SlashCommandControl
                ),
            "passive system injections must not reach active-turn handling",
        );
        // #3154 P1-3 / #4002 / #4082: run the shared synthetic-start wiring. It reads the
        // prior-turn view and either DEFERS to the detached per-channel worker when
        // a prior turn is still draining — the observer then must NOT spawn its own
        // BridgeAdapter tail below (a second observer tail would relay the SAME
        // output twice — the original bug); the worker owns the relay-owner handoff
        // — else INLINE-claims a passive synthetic inflight and adopts the resolved
        // relay_owner into `lease` for the post-block bridge-tail ownership guard.
        // The helper also carries the classifier-derived external-turn gate so a
        // neutral note cannot claim the mailbox if this seam is called accidentally.
        deferred_synthetic_start = synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
            shared,
            &prompt.provider,
            channel_id,
            &prompt,
            anchor_message_id,
            &relay_prompt_decision,
            &mut lease,
        )
        .await;
        tracing::info!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            anchor_message_id = anchor_message_id.get(),
            synthetic_inflight = tui_direct_synthetic_inflight_active_for_prompt(&prompt.provider, channel_id, &prompt.tmux_session_name),
            "SSH-direct TUI prompt notified; runtime relay attached synthetic ownership when possible"
        );
    }

    // #3041 P1-4 codex: SUCCESS PATH reached — the card/anchor (or system-continuation
    // note) was posted and the bridge legitimately retains the in-flight turn. DISARM
    // the early-return guard so the lease PERSISTS for this turn (otherwise the
    // watcher/sink would double-deliver). From here on, persistence is governed by the
    // bridge-tail block's own `TuiDirectExternalInputLeaseGuard` (bridge-owner leases)
    // or simply left set (Unassigned / session-bound-owned leases the sink delivers).
    observed_lease_early_return_guard.disarm();

    #[cfg(unix)]
    {
        // #3154 P1-3: when the synthetic turn-start was deferred, the detached
        // worker owns the relay-owner handoff (it claims after the prior turn
        // drains and re-records the lease as the watcher owner). The observer
        // must NOT also spawn a BridgeAdapter tail here on the pre-claim lease,
        // or the SAME output relays twice once the watcher claims.
        let mut lease_guard: Option<TuiDirectExternalInputLeaseGuard> = if deferred_synthetic_start
        {
            None
        } else {
            ProviderKind::from_str(&prompt.provider).and_then(|provider| {
                (matches!(provider, ProviderKind::Claude)
                    && observer_should_spawn_bridge_tail(
                        deferred_synthetic_start,
                        lease.relay_owner,
                    ))
                .then(|| {
                    TuiDirectExternalInputLeaseGuard::new(
                        provider,
                        &prompt.tmux_session_name,
                        channel_id,
                        &lease,
                    )
                })
            })
        };
        if observer_should_spawn_bridge_tail(deferred_synthetic_start, lease.relay_owner)
            && maybe_spawn_claude_idle_response_tail(
                shared.clone(),
                channel_id,
                &prompt,
                &lease,
                current_turn_anchor_id,
                // Inline / non-deferred path keeps the original `observed_at`
                // timestamp-scan anchoring (no deferred-claim wait window here).
                None,
            )
            .await
            && let Some(guard) = lease_guard.as_mut()
        {
            guard.disarm();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = &mut lease;
        let _ = deferred_synthetic_start;
    }
}

fn owner_channel_for_prompt(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
) -> Option<ChannelId> {
    let provider = ProviderKind::from_str(&prompt.provider)?;
    owner_channel_for_tmux_session(shared, &provider, &prompt.tmux_session_name)
}

/// Resolve the owner channel for a tmux session.
///
/// #3018: the authoritative `tmux_watchers` registry (which holds the 1:1
/// `by_tmux_session`/`tmux_session_by_channel`/`owner_channel_by_tmux_session`
/// invariant) is the SINGLE source of truth. The `tui_prompt_dedupe` cache is a
/// best-effort, expiry-based mirror only — it must NEVER act as a reverse
/// authority for resolution, because when the two disagree it produces
/// wrong-channel routing or silent relay skips at the call sites.
///
/// When the registry misses but the dedupe cache still holds a mapping we have
/// observable drift: we emit an explicit warn and return `None` so the drift is
/// surfaced rather than silently papered over by routing to the stale mirror.
fn owner_channel_for_tmux_session(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> Option<ChannelId> {
    let registry_owner = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name);
    let dedupe_owner =
        crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux_session_name);
    let resolved =
        resolve_owner_channel_authoritatively(tmux_session_name, registry_owner, dedupe_owner);
    if resolved.is_none() && registry_owner.is_none() && dedupe_owner.is_some() {
        super::idle_relay_drift::on_idle_relay_drift(shared, provider.clone(), tmux_session_name);
    }
    resolved
}

/// Pure decision core for [`owner_channel_for_tmux_session`], split out so the
/// registry-single-authority + drift-alert behaviour is unit-testable.
fn resolve_owner_channel_authoritatively(
    tmux_session_name: &str,
    registry_owner: Option<ChannelId>,
    dedupe_owner: Option<u64>,
) -> Option<ChannelId> {
    match (registry_owner, dedupe_owner) {
        (Some(registry_channel), _) => Some(registry_channel),
        (None, Some(dedupe_channel)) => {
            // #3018: registry miss + mirror hit == drift; never route from the mirror
            // (not a reverse authority). #3306: rate-limit this single drift-alert WARN.
            let warn_decision = super::idle_relay_drift::should_emit_drift_warn(tmux_session_name);
            if warn_decision.emit {
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    dedupe_channel_id = dedupe_channel,
                    suppressed_count = warn_decision.suppressed_count,
                    drift_age_secs = warn_decision.drift_age_secs,
                    "tmux-session→channel registry miss while dedupe mirror has a mapping; \
                     treating registry as single authority and dropping (drift alert)"
                );
            }
            None
        }
        (None, None) => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RelayObservedPromptInjectionDecision {
    injected_class: InjectedPromptClass,
    slash_command_kind: Option<String>,
    local_only_slash: bool,
}

impl RelayObservedPromptInjectionDecision {
    fn starts_external_turn_lifecycle(&self) -> bool {
        !self.local_only_slash
            && !self.injected_class.suppresses_user_turn_lifecycle()
            && !self.injected_class.is_subagent_notification_event()
    }
}

pub(in crate::services::discord) fn observed_prompt_starts_external_turn_lifecycle(
    prompt: &str,
) -> bool {
    relay_observed_prompt_injected_prompt_decision(prompt).starts_external_turn_lifecycle()
}

/// Pure classification used before relay lease/ownership side effects.
fn relay_observed_prompt_injected_prompt_decision(
    prompt: &str,
) -> RelayObservedPromptInjectionDecision {
    let injected_class = classify_injected_prompt(prompt);
    let slash_command_kind = matches!(injected_class, InjectedPromptClass::SlashCommandControl)
        .then(|| slash_command_control_kind(prompt));
    let local_only_slash = matches!(injected_class, InjectedPromptClass::SlashCommandControl)
        && is_local_only_slash_command_prompt(prompt);

    RelayObservedPromptInjectionDecision {
        injected_class,
        slash_command_kind,
        local_only_slash,
    }
}

/// Local-completing slash-control prompts skip synthetic turn ownership.
fn is_local_only_slash_command_prompt(prompt: &str) -> bool {
    if !is_slash_command_control_prompt(prompt) {
        return false;
    }
    let kind = slash_command_control_kind(prompt);
    super::commands::is_local_only_slash_command_kind(&kind)
        || slash_command_control_prompt_is_caveat_only(prompt)
        || slash_command_control_prompt_is_local_command_stdout(prompt)
}

/// Dedupe the two slash-control halves before lease/anchor/synthetic ownership.
fn slash_command_control_turn_is_first_sighting(tmux_session_name: &str, kind: &str) -> bool {
    let now = std::time::Instant::now();
    let key = format!("{tmux_session_name}\u{0}{kind}");
    let mut guard = SLASH_COMMAND_CONTROL_LAST_POSTED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard.retain(|_, posted_at| {
        now.duration_since(*posted_at) < SLASH_COMMAND_CONTROL_DEDUPE_WINDOW
    });
    if let Some(posted_at) = guard.get(&key) {
        if now.duration_since(*posted_at) < SLASH_COMMAND_CONTROL_DEDUPE_WINDOW {
            return false;
        }
    }
    guard.insert(key, now);
    true
}

fn record_system_continuation_note_rendered(tmux_session_name: &str) {
    let now = std::time::Instant::now();
    let mut guard = SYSTEM_CONTINUATION_LAST_RENDERED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard.retain(|_, rendered_at| {
        now.checked_duration_since(*rendered_at)
            .is_none_or(|age| age < COMPACT_REPLAY_KIND_NOTE_SUPPRESSION_WINDOW)
    });
    guard.insert(tmux_session_name.to_string(), now);
}

fn local_only_kind_note_suppressed_by_recent_continuation(
    tmux_session_name: &str,
    kind: &str,
) -> bool {
    let now = std::time::Instant::now();
    let mut guard = SYSTEM_CONTINUATION_LAST_RENDERED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard.retain(|_, rendered_at| {
        now.checked_duration_since(*rendered_at)
            .is_none_or(|age| age < COMPACT_REPLAY_KIND_NOTE_SUPPRESSION_WINDOW)
    });
    should_suppress_local_only_kind_note_after_continuation(
        kind,
        guard.get(tmux_session_name).copied(),
        now,
    )
}

#[cfg(test)]
mod tests;
