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
    format_ssh_direct_prompt_notification, format_subagent_notification_card,
    format_system_continuation_note, is_slash_command_control_prompt,
    is_start_anchored_task_notification, should_suppress_local_only_kind_note_after_continuation,
    slash_command_control_kind, slash_command_control_prompt_is_caveat_only,
};

// #3479 rank-10: the pure transcript/rollout prompt scanners live in a capped
// sibling module. The scan-result enums and helpers are re-imported here so the
// stateful idle-relay loops in this parent stay byte-identical.
mod idle_transcript_scan;
use self::idle_transcript_scan::{
    ClaudeIdleTranscriptScan, CodexIdleRolloutScan,
    claude_idle_prompt_observation_should_tail_response,
    codex_idle_prompt_observation_should_tail_response,
    scan_claude_idle_transcript_for_last_prompt, scan_claude_idle_transcript_for_prompt,
    scan_codex_idle_rollout_for_latest_prompt_matching, scan_codex_idle_rollout_for_prompt,
};

// #3479 rank-10: the Discord-IO/SharedData-coupled Claude TUI binding
// rehydration + dead/orphaned-session eviction pass lives in a capped sibling
// module. Every dependency is reached via `use super::*;`, so the move stays
// behavior-identical; the names are re-imported here so the parent's call sites
// (and the test module) stay byte-identical.
#[cfg(unix)]
mod rehydration;
#[cfg(unix)]
use self::rehydration::{
    codex_tui_rehydrated_binding_from_rollout_path, rehydrate_existing_claude_tui_bindings,
    rehydrate_existing_codex_tui_bindings, rehydrated_claude_tui_binding_for_tmux_session,
};
// The dead/orphaned-session predicates and the eviction pass are driven in
// production only transitively (via `rehydrate_existing_claude_tui_bindings`);
// at this module's surface they are referenced only by the unit tests, so the
// re-import is test-gated to keep the non-test lib build free of unused-import
// warnings while leaving the test call sites byte-identical.
#[cfg(all(unix, test))]
use self::rehydration::{
    claude_tui_session_is_dead_orphaned, evict_dead_orphaned_claude_tui_mirrors,
    pane_is_confirmed_dead_orphaned,
};

// #3479: the live-relay TUI-direct prompt anchor COMPLETION lifecycle (`⏳ → ✅`)
// cluster — the visibility gate, the deferred `⏳`-completion drain decision, and
// the reaction-swap completers — moved verbatim to a capped sibling module. Every
// dependency is reached via `use super::*;` (the moved bodies stay byte-identical,
// only `super::formatting` becomes `super::super::formatting` from the child). The
// three helpers reached from sibling discord modules
// (`crate::services::discord::tui_prompt_relay::{...}` call sites in
// `tmux_watcher.rs` and `recovery_engine.rs`) are re-exported at
// `pub(in crate::services::discord)`; the relay-internal drain decision/enum are
// re-imported privately so this parent's call sites (and the test module) stay
// byte-identical.
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

// #3479: the Claude TUI launch-*script* parsing cluster (`ClaudeTuiLaunchInfo`,
// the file/content parsers, and the single-quote shell-word splitter) moved
// verbatim to a capped sibling module. Every item is unix-only, so the module
// decl and the re-import are `#[cfg(unix)]`-gated. Only
// `parse_claude_tui_launch_script` is reached from outside the child (the parent
// `claude_tui_launch_context` caller and, via this parent's `use super::*;`
// glob, the sibling `rehydration` module), so it alone is re-imported here to
// keep those call sites byte-identical.
#[cfg(unix)]
mod launch_script;
#[cfg(unix)]
use self::launch_script::parse_claude_tui_launch_script;

// #3479: the idle-tail transcript start-offset resolution cluster (the #3154
// timestamp-anchor choke point, its timestamp scan + stale-high fallback guard,
// and the #3183 committed-offset clamp) moved verbatim to a capped sibling
// module. Every item is unix-only, so the module decl and the re-imports are
// `#[cfg(unix)]`-gated. Only `resolve_idle_tail_start_offset` and
// `clamp_idle_tail_start_offset_to_committed` are reached from this parent's prod
// call sites; `claude_idle_response_start_offset_after_timestamp` is referenced
// only by the unit tests (its prod callers are now internal to the child), so it
// is re-imported under `#[cfg(all(unix, test))]` to keep the non-test lib build
// free of unused-import warnings. `normalize_transcript_fallback_offset` is fully
// internal to the child and is not re-imported.
#[cfg(unix)]
mod idle_offset_resolution;
#[cfg(all(unix, test))]
use self::idle_offset_resolution::claude_idle_response_start_offset_after_timestamp;
#[cfg(unix)]
use self::idle_offset_resolution::{
    clamp_idle_tail_start_offset_to_committed, resolve_idle_tail_start_offset,
};

// #3715: Codex TUI idle rollout relay/tail logic lives in a sibling module so
// this hot parent stays below both raw-LOC and production-LoC ratchets.
#[cfg(unix)]
mod codex_idle_rollout;
#[cfg(unix)]
use self::codex_idle_rollout::spawn_codex_idle_rollout_relay;

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
const TUI_DIRECT_SYNTHETIC_OWNER_USER_ID: u64 = 1;
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

struct TuiDirectExternalInputLeaseGuard {
    provider: ProviderKind,
    tmux_session_name: String,
    channel_id: ChannelId,
    lease: ExternalInputRelayLease,
    active: bool,
}

impl TuiDirectExternalInputLeaseGuard {
    fn new(
        provider: ProviderKind,
        tmux_session_name: &str,
        channel_id: ChannelId,
        lease: &ExternalInputRelayLease,
    ) -> Self {
        Self {
            provider,
            tmux_session_name: tmux_session_name.to_string(),
            channel_id,
            lease: lease.clone(),
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }

    fn clear_if_current(&self) -> bool {
        clear_external_input_bridge_lease_if_current(
            &self.provider,
            &self.tmux_session_name,
            self.channel_id,
            &self.lease,
        )
    }
}

impl Drop for TuiDirectExternalInputLeaseGuard {
    fn drop(&mut self) {
        // Match the exact lease so a slow timeout cannot clear a newer direct-input turn
        // that reused the same provider/session/channel after this tail started.
        if self.active {
            self.clear_if_current();
        }
    }
}

/// Early-return RAII guard for [`relay_observed_prompt`]: armed right after
/// [`record_observed_external_turn_lease`] records & stores a (possibly
/// `BridgeAdapter`-owned, hence delivery-blocking) lease, it clears that exact
/// lease BY GENERATION on every early-return between the record and the point
/// where the bridge legitimately takes ownership of the in-flight turn.
///
/// WHY clear-by-generation (not by full value): a NEWER turn may have re-taken
/// the same `(provider, tmux_session, channel)` lease while this relay was awaiting
/// the notify HTTP resolve / Discord POST; a by-key or by-value clear could clobber
/// that newer lease (the exact no-clobber race the per-record generation nonce was
/// added to kill, #3041 P1-4 codex). Clearing only the captured generation leaves a
/// newer (even value-identical `Unassigned`) lease untouched.
///
/// SUCCESS-PATH PERSISTENCE: on the path where the bridge posts the card/anchor and
/// retains the in-flight turn, the lease MUST persist so the watcher/sink does not
/// double-deliver. The caller therefore [`disarm`](Self::disarm)s this guard right
/// before the bridge-tail ownership block; only the genuinely-aborting early-returns
/// (registry None, notify resolve `Err`/503, task-card repeat, anchor POST failure)
/// leave the guard armed → its clear releases the lease so legitimate delivery can
/// proceed.
struct TuiDirectObservedLeaseEarlyReturnGuard {
    provider: Option<ProviderKind>,
    tmux_session_name: String,
    channel_id: ChannelId,
    /// Generation of the lease this guard armed with; Drop clears ONLY this exact
    /// generation (sentinel `UNRECORDED` clears nothing).
    generation: u64,
    active: bool,
}

impl TuiDirectObservedLeaseEarlyReturnGuard {
    /// Arm a guard capturing the generation of the just-recorded `lease`. When the
    /// provider string is not a known [`ProviderKind`] the guard is inert (no key to
    /// clear by) but still constructed so callers keep a uniform disarm point.
    fn arm(
        provider_str: &str,
        tmux_session_name: &str,
        channel_id: ChannelId,
        generation: u64,
    ) -> Self {
        Self {
            provider: ProviderKind::from_str(provider_str),
            tmux_session_name: tmux_session_name.to_string(),
            channel_id,
            generation,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TuiDirectObservedLeaseEarlyReturnGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Some(provider) = self.provider.as_ref() else {
            return;
        };
        // Compare-and-clear by the captured generation: a newer same-key lease
        // recorded during a slow notify-resolve / POST await has a DIFFERENT
        // generation and survives this drop (no clobber).
        crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_generation_matches(
            provider.as_str(),
            &self.tmux_session_name,
            self.channel_id.get(),
            self.generation,
        );
    }
}

fn clear_external_input_bridge_lease_if_current(
    provider: &ProviderKind,
    tmux_session_name: &str,
    channel_id: ChannelId,
    lease: &ExternalInputRelayLease,
) -> bool {
    if !bridge_adapter_owns_external_turn(lease.relay_owner) {
        return false;
    }
    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_matches(
        provider.as_str(),
        tmux_session_name,
        channel_id.get(),
        lease,
    )
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
    // #3178 (codex P1 lease-overwrite): run the slash-command-control dedupe BEFORE
    // recording ANY external-input lease. The #3153 double-post (raw echo + expanded
    // `<command-*>` wrapper, ~tens-of-ms apart) must not let the SECOND half record a
    // lease: the table is one-per-(provider,session), so it would overwrite the first
    // turn's lease and its guard would clear that generation, stranding the first
    // bridge tail. Drop the duplicate here, before any lease/anchor/inflight exists;
    // a genuine second /loop / /compact falls outside the 2s window → fresh turn.
    let injected_class = classify_injected_prompt(&prompt.prompt);
    // #3305: hoist the slash-command kind so the first-sighting gate AND the
    // local-only lifecycle-skip below share one computation. `local_only_slash`
    // is true ONLY for a LOCAL-completing pass-through (/effort /compact /cost
    // /context) — it posts a guidance note but mints no turn (an allow-list, so
    // /loop and any unknown command keep full lifecycle, fail-safe).
    let mut local_only_slash = false;
    if matches!(injected_class, InjectedPromptClass::SlashCommandControl) {
        let kind = slash_command_control_kind(&prompt.prompt);
        local_only_slash = super::commands::is_local_only_slash_command_kind(&kind)
            || slash_command_control_prompt_is_caveat_only(&prompt.prompt);
        if !slash_command_control_turn_is_first_sighting(&prompt.tmux_session_name, &kind) {
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
    // #3164 / #750 invariant: the `⏳` MUST be added by the SAME bot identity that
    // later removes it (`remove_reaction_raw` only removes `@me`'s reaction; a
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
    let mut current_turn_anchor_id: Option<u64> = None;
    // #3154 P1-3: function-scope so the post-block bridge-tail guard reads it; set
    // true when the synthetic turn-start is deferred to the detached worker, so the
    // observer skips its own BridgeAdapter tail (no duplicate relay).
    let mut deferred_synthetic_start = false;
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
        let kind = slash_command_control_kind(&prompt.prompt);
        // #3388: in-session /compact rewrites can replay the local command stub
        // seconds after the continuation banner; hide that duplicate note. The
        // real machine-injected /compact (#3262) happens minutes before compaction
        // completes, so it stays outside this narrow replay window.
        if local_only_kind_note_suppressed_by_recent_continuation(&prompt.tmux_session_name, &kind)
        {
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
            format_slash_command_control_note(&prompt.tmux_session_name, &kind, &prompt.prompt);
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
    if injected_class.is_subagent_notification_event() {
        let note = format_subagent_notification_card(&prompt.tmux_session_name, &prompt.prompt);
        if let Err(error) = channel_id.say(&*notify_http, note).await {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                tmux_session_name = %prompt.tmux_session_name,
                error = %error,
                "failed to send subagent_notification machine-event card"
            );
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
                    "rendered system/compact continuation injection as neutral session note; no active-turn lifecycle, assistant output still relayed via bridge tail"
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
    } else {
        // #3178 (codex fix): a SlashCommandControl is a FULL active turn now (anchor +
        // ⏳ + synthetic inflight like HumanTuiDirect). Its anchor content carries the
        // /loop directive body but never the <command-*> wrapper or Compacted stdout;
        // the #3153 duplicate half was already dropped before the lease record above.
        // #3075: a `<task-notification>` auto-turn is a MACHINE event — render a
        // compact structured card and dedupe repeats by task-id (a repeat edits its
        // live card or no-ops; the first sighting posts as the #3099 anchor).
        // HumanTuiDirect keeps the raw render; SystemContinuation handled above (#3100).
        let content = if matches!(injected_class, InjectedPromptClass::SlashCommandControl) {
            let kind = slash_command_control_kind(&prompt.prompt);
            format_slash_command_control_note(&prompt.tmux_session_name, &kind, &prompt.prompt)
        } else if matches!(injected_class, InjectedPromptClass::TaskNotificationEvent) {
            // #3393: background-task / subagent completions arrive ONLY as this
            // `user`-record `<task-notification>` XML — never the stream-json
            // `system` record the footer panel's `system_status_events` parses. So
            // before the card render (which has its OWN store), bridge the SAME
            // payload into the live-panel terminal StatusEvents so footer Tasks /
            // Subagents flip ✓ and the #3391 delivered-ack eviction can fire. Runs
            // for BOTH Post and Repeat outcomes (a repeat re-asserts terminal,
            // which is idempotent at the slot). Footer-mode gated inside the
            // bridge; an unknown/id-less notification is a slot no-op.
            //
            // #3393 finding 2: gate the BRIDGE (not the card) on a START-ANCHORED
            // check — a human prompt QUOTING a notification mid-message still gets
            // its card but pushes NO terminal events, so a quoted live tool-use-id
            // cannot false-close a running slot. Layered with finding 1 (the XML
            // bridge requires a tool_use_id for any terminal End).
            let start_anchored_task_notification =
                is_start_anchored_task_notification(&prompt.prompt);
            if start_anchored_task_notification {
                shared
                    .ui
                    .placeholder_live_events
                    .bridge_task_notification_xml(channel_id, &prompt.prompt);
            }
            match super::tui_task_card::resolve_task_card_content(
                &notify_http,
                shared,
                channel_id,
                &prompt.prompt,
                start_anchored_task_notification,
            )
            .await
            {
                super::tui_task_card::TaskCardOutcome::Post { content } => content,
                super::tui_task_card::TaskCardOutcome::Repeat
                | super::tui_task_card::TaskCardOutcome::SuppressedByFooter => {
                    // #3075 codex P1 #2: a repeat early-returns before the bridge-tail
                    // lease-guard cleanup, but the lease recorded above would dangle and
                    // make `session_bound_external_lease_blocks_delivery` skip legitimate
                    // delivery. Clear exactly the lease THIS observation recorded
                    // (exact-match preserves a newer turn's lease).
                    clear_observed_external_turn_lease_if_current(&prompt, channel_id, &lease);
                    return;
                }
            }
        } else {
            format_ssh_direct_prompt_notification(
                &prompt.provider,
                &prompt.tmux_session_name,
                &prompt.prompt,
            )
        };
        let anchor_message = match channel_id.say(&*notify_http, content).await {
            Ok(message) => message,
            Err(error) => {
                // #3075 codex P2: the `Post` outcome reserved a placeholder card slot
                // (message_id == 0) before this post; on post failure release the
                // reservation we own so the next same-task notification reserves fresh
                // (a lingering placeholder would force later ones to `Pending` no-ops
                // until the 1h stale purge; the NEXT reservation alone is affected).
                if matches!(injected_class, InjectedPromptClass::TaskNotificationEvent) {
                    let task_id =
                        super::tui_task_card::parse_task_notification(&prompt.prompt).task_id;
                    super::tui_task_card::forget_reserved_card(
                        channel_id.get(),
                        task_id.as_deref(),
                    );
                }
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
        // #3176: pin this turn's anchor id — it becomes the synthetic inflight's
        // `user_msg_id` below, so the idle-tail drain-wait can recognise our own row.
        current_turn_anchor_id = Some(anchor_message.id.get());
        // #3075: remember this card so a repeat completion edits it. #3164 codex R3
        // issue-2: keep this IMMEDIATELY after the successful post (before the awaited
        // `⏳` add) — deferring it behind the reaction await widens the task-card
        // `Pending` no-op window and can drop the only repeat/update.
        if matches!(injected_class, InjectedPromptClass::TaskNotificationEvent) {
            super::tui_task_card::record_posted_card(
                channel_id.get(),
                &prompt.prompt,
                anchor_message.id.get(),
            );
        }
        // #3164: add `⏳` with the command(provider) bot so it matches the bot that
        // removes it in `complete_tui_direct_prompt_anchor_lifecycle_if_present`; on
        // unavailable http skip the add (warned above). codex R2 issue-2: add BEFORE
        // the anchor becomes findable (`record_prompt_anchor`) — anchor-first lets a
        // fast watcher complete first and the delayed add re-leaves `⏳ + ✅`.
        if let Some(command_http) = command_http.as_ref() {
            super::formatting::add_reaction_raw(command_http, channel_id, anchor_message.id, '⏳')
                .await;
        }
        crate::services::tui_prompt_dedupe::record_prompt_anchor(
            &prompt.provider,
            &prompt.tmux_session_name,
            channel_id.get(),
            anchor_message.id.get(),
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
                let command_http = command_http
                    .as_ref()
                    .expect("decision returns Complete only when command_http is_some");
                let _ = complete_tui_direct_prompt_anchor_lifecycle_if_present(
                    command_http,
                    &prompt.provider,
                    &prompt.tmux_session_name,
                    channel_id,
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
                || matches!(injected_class, InjectedPromptClass::TaskNotificationEvent),
            "system-continuation injections must not reach active-turn handling",
        );
        // #3154 P1-3: set when the synthetic turn-start is DEFERRED to the detached
        // per-channel worker; the observer then must NOT spawn its own BridgeAdapter
        // tail below (a second observer tail would relay the SAME output twice — the
        // original bug). The worker owns the relay-owner handoff.
        if let Some(provider) = ProviderKind::from_str(&prompt.provider) {
            // #3154 — TEMPORAL fix for turn-interleaving. An INLINE claim while the
            // PRIOR turn's tail still drains seeds `turn_start_offset` from the prior
            // cursor (duplicate relay), and an inline wait starves OTHER channels. So
            // an un-finalized prior turn persists a DURABLE pending-start and hands
            // the claim to a DETACHED per-channel worker (fresh EOF offset); the
            // common no-interleave case stays on the inline fast path.
            let prior = synthetic_start_prior_turn_view(
                shared,
                &provider,
                channel_id,
                &prompt.tmux_session_name,
                anchor_message.id.get(),
            )
            .await;
            if super::tui_direct_pending_start::should_defer_synthetic_turn_start(prior.view) {
                deferred_synthetic_start = true;
                defer_synthetic_turn_start(
                    shared,
                    &provider,
                    channel_id,
                    &prompt,
                    anchor_message.id,
                    &lease,
                );
                tracing::info!(
                    provider = %prompt.provider,
                    channel_id = channel_id.get(),
                    tmux_session_name = %prompt.tmux_session_name,
                    anchor_message_id = anchor_message.id.get(),
                    "deferred TUI-direct synthetic turn-start off the observer loop; prior turn not yet finalized (durable record persisted, detached per-channel worker spawned)"
                );
            } else {
                let claim = claim_tui_direct_synthetic_turn(
                    shared,
                    &provider,
                    channel_id,
                    &prompt.tmux_session_name,
                    &prompt.prompt,
                    anchor_message.id,
                    &lease,
                )
                .await;
                if claim_should_adopt_relay_owner(
                    claim.claimed,
                    lease.relay_owner,
                    claim.relay_owner,
                ) {
                    lease.relay_owner = claim.relay_owner;
                    // Re-record overwrites the lease with a FRESH generation; adopt it
                    // back into `lease` so the bridge-tail guard below captures the
                    // exact stored identity (a stale generation's Drop would clear
                    // nothing / the wrong lease).
                    lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                        provider.as_str(),
                        &prompt.tmux_session_name,
                        lease,
                    );
                }
                // #3350: the INLINE claim records the same #3303 DeferredClaim marker
                // as the deferred worker (drain ✅ / sweep TTL ⚠). SC3/own-row/I5 gates
                // live in the recorder; a pending_start test pins this wiring.
                super::tui_direct_pending_start::record_inline_claim_marker_if_claimed(
                    claim.claimed,
                    &prompt.provider,
                    channel_id.get(),
                    anchor_message.id.get(),
                    &prompt.tmux_session_name,
                    super::tui_direct_pending_start::record_claim_marker_if_watcher_owned,
                );
            }
        }
        tracing::info!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            anchor_message_id = anchor_message.id.get(),
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
    let relay_output_path = external_input_relay_output_path(
        shared,
        &prompt.provider,
        &prompt.tmux_session_name,
        channel_id,
        binding.as_ref(),
    );
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
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    // Capture the RECORDED lease (with its stamped generation) so the caller's
    // later `clear_observed_external_turn_lease_if_current` matches the exact
    // stored identity and never clobbers a newer turn's lease.
    let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        &prompt.provider,
        &prompt.tmux_session_name,
        lease,
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

/// Clear the external-input turn lease recorded by
/// [`record_observed_external_turn_lease`] for THIS observation, if it is still
/// the current lease (exact match).
///
/// Used by the `<task-notification>` edit-repeat path (#3075 codex P1 #2): a
/// repeat records a fresh lease before card resolution but then early-returns,
/// skipping the normal bridge-tail / lease-guard cleanup. Without this, that
/// stale non-`Unassigned` lease would block session-bound / bridge-tail delivery
/// (`session_relay_sink::session_bound_external_lease_blocks_delivery`). The
/// exact-match guard means a newer turn that reused the same
/// provider/session/channel after we recorded ours is left untouched.
fn clear_observed_external_turn_lease_if_current(
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
    lease: &ExternalInputRelayLease,
) -> bool {
    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_matches(
        &prompt.provider,
        &prompt.tmux_session_name,
        channel_id.get(),
        lease,
    )
}

fn external_input_relay_binding(
    provider: &str,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let binding = binding?;
    #[cfg(unix)]
    {
        if provider
            .trim()
            .eq_ignore_ascii_case(ProviderKind::Codex.as_str())
            && binding.runtime_kind == RuntimeHandoffKind::CodexTui
            && let Some(fresh) =
                resolved_codex_idle_relay_binding(tmux_session_name, channel_id, &binding)
        {
            return Some(fresh);
        }
    }
    Some(binding)
}

#[cfg(unix)]
fn resolved_codex_idle_relay_binding(
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let marker =
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name);
    if let Some(marker) = marker
        && marker.rollout_path.exists()
    {
        let marker_path = std::fs::canonicalize(&marker.rollout_path)
            .unwrap_or_else(|_| marker.rollout_path.clone());
        let binding_path = std::fs::canonicalize(&binding.output_path)
            .unwrap_or_else(|_| PathBuf::from(&binding.output_path));
        if marker_path != binding_path {
            let fresh = codex_tui_rehydrated_binding_from_rollout_path(
                tmux_session_name,
                &marker.rollout_path,
                marker.session_id,
            )?;
            crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                ProviderKind::Codex.as_str(),
                tmux_session_name,
                channel_id.get(),
                fresh.clone(),
            );
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                channel_id = channel_id.get(),
                stale_output_path = %binding.output_path,
                rollout_path = %fresh.output_path,
                "refreshed Codex TUI direct relay binding from live rollout marker"
            );
            return Some(fresh);
        }
    }
    Path::new(&binding.output_path)
        .exists()
        .then(|| binding.clone())
}

fn external_input_relay_output_path(
    shared: &Arc<SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Option<PathBuf> {
    let binding = binding?;
    #[cfg(unix)]
    {
        if provider
            .trim()
            .eq_ignore_ascii_case(ProviderKind::Claude.as_str())
            && binding.runtime_kind == RuntimeHandoffKind::ClaudeTui
            && let Some(transcript_path) = resolved_claude_idle_relay_transcript_path(
                shared,
                tmux_session_name,
                channel_id,
                binding,
            )
        {
            return Some(transcript_path);
        }
        if provider
            .trim()
            .eq_ignore_ascii_case(ProviderKind::Codex.as_str())
            && binding.runtime_kind == RuntimeHandoffKind::CodexTui
        {
            return Some(PathBuf::from(&binding.output_path));
        }
    }
    Some(PathBuf::from(binding.relay_output_path()))
}

fn external_input_relay_start_offset(
    provider: &ProviderKind,
    binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> u64 {
    let Some(binding) = binding else {
        return 0;
    };
    if provider == &ProviderKind::Codex && binding.runtime_kind == RuntimeHandoffKind::CodexTui {
        return binding.last_offset;
    }
    binding.relay_last_offset()
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
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    // Return the RECORDED lease (with its stamped generation) so a later
    // exact-match clear targets the precise stored identity.
    crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        provider.as_str(),
        tmux_session_name,
        lease,
    )
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
    external_input_relay_owner_for_watchers(
        &shared.tmux_watchers,
        tmux_session_name,
        output_path,
        session_bound_discord_delivery_enabled(),
    )
}

fn session_bound_discord_delivery_enabled() -> bool {
    #[cfg(unix)]
    {
        super::session_relay_sink::session_bound_discord_delivery_enabled()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn external_input_relay_owner_for_watchers(
    watchers: &super::TmuxWatcherRegistry,
    tmux_session_name: &str,
    output_path: Option<&Path>,
    session_bound_discord_delivery_enabled: bool,
) -> ExternalInputRelayOwner {
    let watcher_alive = watchers
        .tmux_session_is_stale(tmux_session_name)
        .is_some_and(|stale| !stale);
    if !watcher_alive {
        return ExternalInputRelayOwner::BridgeAdapter;
    }

    let watcher_covers_output = match output_path {
        Some(output_path) => watchers
            .watcher_output_path(tmux_session_name)
            .is_some_and(|watcher_path| Path::new(&watcher_path) == output_path),
        None => true,
    };
    if !watcher_covers_output {
        return ExternalInputRelayOwner::BridgeAdapter;
    }

    if session_bound_discord_delivery_enabled {
        // TUI-direct observations do not create a foreground inflight row yet.
        // A session-bound StreamRelay can only be the terminal owner for an
        // external-input turn once such an inflight exists; otherwise the
        // watcher can acknowledge frames without a Discord terminal commit.
        ExternalInputRelayOwner::BridgeAdapter
    } else {
        ExternalInputRelayOwner::TmuxWatcher
    }
}

fn bridge_adapter_owns_external_turn(owner: ExternalInputRelayOwner) -> bool {
    matches!(owner, ExternalInputRelayOwner::BridgeAdapter)
}

/// #3154 P1-3 no-relay-GAP guard: may the OBSERVER loop spawn its own BridgeAdapter
/// idle-response tail? The output must come from EXACTLY ONE owner (never a GAP, never
/// a DUPLICATE). DEFERRED ⇒ the observer cannot yet know the RESOLVED owner (the claim
/// runs later in the detached worker), so it STANDS DOWN unconditionally and the worker
/// re-runs [`deferred_claim_requires_bridge_tail_relayer`] against the resolved owner.
/// NOT deferred ⇒ spawn iff the lease still owns as BridgeAdapter (the inline claim
/// already adopted any watcher handoff, so a watcher-owned lease means the observer
/// stands down). Pairing this with the worker's owner-kind-aware spawn is the proof.
fn observer_should_spawn_bridge_tail(
    deferred_synthetic_start: bool,
    lease_owner: ExternalInputRelayOwner,
) -> bool {
    !deferred_synthetic_start && bridge_adapter_owns_external_turn(lease_owner)
}

/// #3154 P1 (BridgeAdapter-GAP fix). The OWNER-KIND-AWARE decision the deferred
/// worker runs AFTER its claim resolves the relay owner, mirroring the inline path:
/// TmuxWatcher ⇒ the watcher relays so the bridge tail STANDS DOWN (else DUPLICATE);
/// BridgeAdapter ⇒ no watcher relays and the observer already stood down, so the
/// bridge tail MUST run exactly once here (else `relayer_count == 0`, the GAP). The
/// downstream [`maybe_spawn_claude_idle_response_tail`] re-checks
/// `bridge_adapter_owns_external_turn`, so a stale/watcher lease can never spawn a
/// second relayer even if this predicate were called too eagerly.
fn deferred_claim_requires_bridge_tail_relayer(resolved_owner: ExternalInputRelayOwner) -> bool {
    bridge_adapter_owns_external_turn(resolved_owner)
}

/// #3154 P1-3 relay-owner adoption decision. The in-memory lease adopts the claim's
/// `relay_owner` iff the claim SUCCEEDED and the owner changed; re-recording with
/// the claimed (watcher) owner makes [`observer_should_spawn_bridge_tail`] read a
/// watcher-owned lease and stand down (the claimed owner is the SINGLE relayer, no
/// GAP/dup). Shared by the inline and deferred paths so both adopt identically.
fn claim_should_adopt_relay_owner(
    claimed: bool,
    current_owner: ExternalInputRelayOwner,
    claimed_owner: ExternalInputRelayOwner,
) -> bool {
    claimed && current_owner != claimed_owner
}

#[derive(Debug)]
struct TuiDirectSyntheticTurnClaim {
    relay_owner: ExternalInputRelayOwner,
    claimed: bool,
    // #3154 P1 (timestamp-anchor output loss): the post-drain EOF offset the claim
    // seeded into this turn's inflight `turn_start_offset`. The deferred-BridgeAdapter
    // worker anchors its bridge tail to THIS byte boundary instead of a `Utc::now()`
    // scan, which can skip bytes written during the deferred-claim wait window.
    turn_start_offset: u64,
}

async fn finish_tui_direct_synthetic_pre_save_failure(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) {
    // This cleanup runs before the synthetic path increments global_active.
    let _ = super::mailbox_finish_turn(shared, provider, channel_id).await;
}

/// #3358 — offset-authority handover for synthetic inflight creation.
///
/// A synthetic is born at the lagging `relay_last_offset()`; when that lags the
/// watcher's delivered frontier (#3017), a later same-identity re-claim re-seeds
/// the row backward → trips the monotonicity guards (the incident). CARRY-FORWARD
/// the frontier so the synthetic is born at/above every delivered byte.
///
/// #3358 round 2 — GATED: `committed_relay_offset` is `Some` ONLY when the gating
/// accessor proved the watermark belongs to the CURRENT wrapper. After a restart
/// the stream resets to 0; a stale PREVIOUS-generation watermark must NOT clamp
/// forward — that marks future bytes below it as delivered → CONTENT SKIP (worse
/// than the original ERROR-only bug). On mismatch the frontier is `None` and we
/// fall back to pre-fix seeding (`relay_last_offset` only): the rare monotonicity
/// ERROR beats a skip, and backward writes outside this handover stay guarded.
pub(in crate::services::discord) fn synthetic_start_offset_carry_forward(
    relay_last_offset: u64,
    committed_relay_offset: Option<u64>,
) -> u64 {
    relay_last_offset.max(committed_relay_offset.unwrap_or(0))
}

async fn claim_tui_direct_synthetic_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    prompt_text: &str,
    anchor_message_id: MessageId,
    lease: &ExternalInputRelayLease,
) -> TuiDirectSyntheticTurnClaim {
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
    let binding =
        external_input_relay_binding(provider.as_str(), tmux_session_name, channel_id, binding);
    let output_path = external_input_relay_output_path(
        shared,
        provider.as_str(),
        tmux_session_name,
        channel_id,
        binding.as_ref(),
    );
    let relay_last_offset = external_input_relay_start_offset(provider, binding.as_ref());
    // #3358 round 2: carry the committed frontier forward, but ONLY for the
    // CURRENT wrapper generation (stale → `None` → no content skip).
    // The `tmux` module is `#[cfg(unix)]`; on non-unix targets (windows CI
    // cross-compile check) there is no committed frontier to carry forward, so
    // `None` (no carry-forward) is the correct, behavior-preserving default.
    #[cfg(unix)]
    let committed_relay_offset = super::tmux::committed_frontier_for_current_generation(
        shared,
        channel_id,
        tmux_session_name,
    );
    #[cfg(not(unix))]
    let committed_relay_offset: Option<u64> = None;
    let start_offset =
        synthetic_start_offset_carry_forward(relay_last_offset, committed_relay_offset);
    if start_offset > relay_last_offset {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            anchor_message_id = anchor_message_id.get(),
            relay_last_offset,
            committed_relay_offset = committed_relay_offset.unwrap_or(0),
            start_offset,
            "#3358 synthetic inflight offset-authority handover: carried committed relay frontier forward"
        );
    }
    let relay_owner = if tui_direct_watcher_can_own_output(
        &shared.tmux_watchers,
        tmux_session_name,
        output_path.as_deref(),
    ) {
        ExternalInputRelayOwner::TmuxWatcher
    } else {
        ExternalInputRelayOwner::BridgeAdapter
    };
    let relay_owner_kind = match relay_owner {
        ExternalInputRelayOwner::TmuxWatcher => RelayOwnerKind::Watcher,
        ExternalInputRelayOwner::SessionBoundRelay => RelayOwnerKind::SessionBoundRelay,
        _ => RelayOwnerKind::None,
    };

    let cancel_token = Arc::new(CancelToken::new());
    super::turn_bridge::bind_cancel_token_tmux_runtime(
        provider,
        &cancel_token,
        tmux_session_name,
        "tui_direct_synthetic_inflight",
    );
    // #3167 — the self-paced TUI loop / TUI-direct turn is a low-priority
    // background turn; mark it `Background` so a queued external USER
    // intervention is not starved behind the continuously-cycling loop.
    let mut started = super::mailbox_try_start_turn_kinded(
        shared,
        channel_id,
        cancel_token.clone(),
        serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
        anchor_message_id,
        crate::services::turn_orchestrator::ActiveTurnKind::Background,
    )
    .await;
    if !started {
        let snapshot = super::mailbox_snapshot(shared, channel_id).await;
        if snapshot.active_user_message_id != Some(anchor_message_id) {
            if let Some(active_user_message_id) = snapshot.active_user_message_id
                && release_stale_ownerless_tui_direct_mailbox_if_current(
                    shared,
                    provider,
                    channel_id,
                    tmux_session_name,
                    active_user_message_id,
                    anchor_message_id,
                )
                .await
            {
                started = super::mailbox_try_start_turn_kinded(
                    shared,
                    channel_id,
                    cancel_token.clone(),
                    serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                    anchor_message_id,
                    crate::services::turn_orchestrator::ActiveTurnKind::Background,
                )
                .await;
                if started {
                    tracing::info!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        tmux_session_name = %tmux_session_name,
                        anchor_message_id = anchor_message_id.get(),
                        "TUI-direct synthetic inflight claimed after releasing stale ownerless mailbox"
                    );
                }
            }
        }
        if !started {
            let snapshot = super::mailbox_snapshot(shared, channel_id).await;
            if snapshot.active_user_message_id == Some(anchor_message_id) {
                started = true;
            } else {
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel_id = channel_id.get(),
                    tmux_session_name = %tmux_session_name,
                    active_user_message_id = snapshot
                        .active_user_message_id
                        .map(|id| id.get())
                        .unwrap_or(0),
                    anchor_message_id = anchor_message_id.get(),
                    "skipping TUI-direct synthetic inflight; mailbox already owns a different turn"
                );
                return TuiDirectSyntheticTurnClaim {
                    relay_owner,
                    claimed: false,
                    turn_start_offset: start_offset,
                };
            }
        }
    }

    // #3146 Part 1: a TUI-driven turn is now active for this channel (we either
    // just started it via `mailbox_try_start_turn` or already own the matching
    // turn). Clear any stale `📦 … idle N분` recap card the same way the
    // Discord-intake path does (`intake_gate` → `spawn_clear_idle_recap_for_channel`).
    // Without this, a turn that starts from the tmux TUI (user-typed OR the
    // autonomous self-drive loop) never goes through Discord intake, so the
    // recap card kept showing `idle N분` over a live turn.
    //
    // codex R2 P2: capture the recap card id THAT EXISTS NOW (the turn just
    // became active) and clear ONLY that captured id (compare-and-clear on the
    // pointer). The idle-recap policy posts at most once per idle period, so a
    // delayed clear that deleted a LATER legitimately-posted card would lose it
    // for the rest of the idle period (NOT self-healing). Binding the clear to
    // the captured id makes a delayed clear a no-op against any newer card.
    if let Some(pool) = shared.pg_pool.as_ref().cloned()
        && let Some(http) = shared.serenity_http_or_token_fallback()
    {
        // #3148: bump the per-channel turn generation BEFORE the clear. This is
        // the same claim-bump the Discord-intake path does — any idle-recap
        // POST job whose persist CAS captured the pre-bump generation now fails
        // to persist its card over this just-claimed TUI turn. The clear then
        // removes any card the POST already persisted before this claim.
        if let Err(e) = super::idle_recap::bump_turn_generation(
            &pool,
            channel_id.get(),
            provider,
            lease.session_key.as_deref(),
        )
        .await
        {
            tracing::warn!(
                error = %e,
                channel_id = channel_id.get(),
                "idle_recap: failed to bump turn generation on TUI claim"
            );
        }
        super::idle_recap::spawn_clear_captured_idle_recap_for_channel(
            http,
            pool,
            channel_id.get(),
        )
        .await;
    }

    if let Some(existing) = super::inflight::load_inflight_state(provider, channel_id.get())
        && existing.tmux_session_name.as_deref() == Some(tmux_session_name)
        && existing.turn_source == TurnSource::ExternalInput
        && existing.user_msg_id == anchor_message_id.get()
    {
        let mut existing = existing;
        existing.set_relay_owner_kind(relay_owner_kind);
        existing.session_key = lease.session_key.clone();
        existing.runtime_kind = lease.runtime_kind;
        existing.output_path = output_path
            .as_deref()
            .and_then(|path| path.to_str().map(str::to_string));
        existing.last_offset = start_offset;
        existing.turn_start_offset = Some(start_offset);
        // #3099 codex re-review (P2): keep this turn's own injected `⏳` message id
        // pinned so completion cleanup never reads a later injection's overwrite of
        // the shared prompt-anchor slot.
        existing.injected_prompt_message_id = Some(anchor_message_id.get());
        if let Err(error) = super::inflight::save_inflight_state(&existing) {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                error = %error,
                "failed to refresh TUI-direct synthetic inflight ownership"
            );
            if started {
                finish_tui_direct_synthetic_pre_save_failure(shared, provider, channel_id).await;
            }
            return TuiDirectSyntheticTurnClaim {
                relay_owner,
                claimed: false,
                turn_start_offset: start_offset,
            };
        }
        if started {
            super::increment_global_active(shared, "tui_direct_synthetic_refresh");
            shared
                .turn_start_times
                .insert(channel_id, std::time::Instant::now());
        }
        return TuiDirectSyntheticTurnClaim {
            relay_owner,
            claimed: true,
            turn_start_offset: start_offset,
        };
    }

    let inflight_state = build_tui_direct_synthetic_inflight_state(
        provider.clone(),
        channel_id,
        anchor_message_id,
        None,
        prompt_text,
        tmux_session_name,
        output_path.as_deref(),
        start_offset,
        lease,
        relay_owner_kind,
    );
    if let Err(error) = super::inflight::save_inflight_state(&inflight_state) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            error = %error,
            "failed to save TUI-direct synthetic inflight"
        );
        if started {
            finish_tui_direct_synthetic_pre_save_failure(shared, provider, channel_id).await;
        }
        return TuiDirectSyntheticTurnClaim {
            relay_owner,
            claimed: false,
            turn_start_offset: start_offset,
        };
    }

    if started {
        super::increment_global_active(shared, "tui_direct_synthetic_save");
        shared
            .turn_start_times
            .insert(channel_id, std::time::Instant::now());
    }
    tracing::info!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        anchor_message_id = anchor_message_id.get(),
        relay_owner = relay_owner.as_str(),
        mailbox_started = started,
        "created TUI-direct synthetic inflight for already-submitted provider turn"
    );
    TuiDirectSyntheticTurnClaim {
        relay_owner,
        claimed: true,
        turn_start_offset: start_offset,
    }
}

async fn release_stale_ownerless_tui_direct_mailbox_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
    anchor_message_id: MessageId,
) -> bool {
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        return false;
    };
    if state.user_msg_id != active_user_message_id.get()
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
        || !super::inflight::ownerless_external_input_inflight_is_stale(&state)
    {
        return false;
    }

    let finish =
        super::mailbox_finish_turn_if_matches(shared, provider, channel_id, active_user_message_id)
            .await;
    let Some(token) = finish.removed_token.as_ref() else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            "TUI-direct stale ownerless mailbox release skipped because mailbox identity changed"
        );
        return false;
    };
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let global_active_decremented = super::saturating_decrement_global_active(shared);
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        stale_user_message_id = active_user_message_id.get(),
        anchor_message_id = anchor_message_id.get(),
        global_active_decremented,
        had_pending_queue = finish.has_pending,
        "released stale ownerless TUI-direct mailbox before claiming new synthetic inflight"
    );
    true
}

// ===========================================================================
// #3154 — deferred synthetic turn-start (off the observer loop)
// ===========================================================================

/// Build the
/// [`PriorTurnObservation`](super::tui_direct_pending_start::PriorTurnObservation)
/// for the synthetic-start deferral decision: read inflight, mailbox, and the
/// fresh runtime binding. Besides the pure decision view it carries the live
/// FOREIGN inflight's identity (codex r2) so the worker can pin it on the
/// aborted-anchor marker even when the row vanishes before the ABORT cleanup.
async fn synthetic_start_prior_turn_view(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    own_anchor_id: u64,
) -> super::tui_direct_pending_start::PriorTurnObservation {
    let inflight = super::inflight::load_inflight_state(provider, channel_id.get())
        .filter(|state| !super::inflight::ownerless_external_input_inflight_is_stale(state));
    let inflight_present = inflight.is_some();
    let inflight_is_own_anchor = inflight
        .as_ref()
        .map(|state| {
            state.turn_source == TurnSource::ExternalInput
                && state.tmux_session_name.as_deref() == Some(tmux_session_name)
                && state.user_msg_id == own_anchor_id
        })
        .unwrap_or(false);
    let foreign_inflight_identity = inflight
        .as_ref()
        .filter(|_| !inflight_is_own_anchor)
        .map(|state| (state.user_msg_id, state.started_at.clone()));

    let snapshot = super::mailbox_snapshot(shared, channel_id).await;
    // A BACKGROUND turn (monitor relay / self-paced loop) does not block — only a
    // real (non-background) active turn is a blocking prior turn (mirrors the
    // idle-queue kickoff gate at mod.rs `idle_queue_snapshot_has_kickable_backlog`).
    let mailbox_blocking_turn_present =
        snapshot.cancel_token.is_some() && !snapshot.active_turn_kind.is_background();
    let mailbox_turn_is_own_anchor =
        snapshot.active_user_message_id == Some(MessageId::new(own_anchor_id));

    let runtime_binding_present =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
            .is_some();

    super::tui_direct_pending_start::PriorTurnObservation {
        view: super::tui_direct_pending_start::PriorTurnView {
            inflight_present,
            inflight_is_own_anchor,
            mailbox_blocking_turn_present,
            mailbox_turn_is_own_anchor,
            runtime_binding_present,
        },
        foreign_inflight_identity,
    }
}

/// Persist a durable pending-start record and spawn the detached per-channel
/// worker. Returns immediately (non-blocking for the observer loop).
fn defer_synthetic_turn_start(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    anchor_message_id: MessageId,
    lease: &ExternalInputRelayLease,
) {
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let record = super::tui_direct_pending_start::TuiDirectPendingStart {
        provider: provider.as_str().to_string(),
        channel_id: channel_id.get(),
        tmux_session_name: prompt.tmux_session_name.clone(),
        prompt_text: prompt.prompt.clone(),
        anchor_message_id: anchor_message_id.get(),
        lease_relay_owner: lease.relay_owner.as_str().to_string(),
        lease_runtime_kind: lease.runtime_kind.map(|k| k.as_str().to_string()),
        lease_turn_id: lease.turn_id.clone(),
        lease_session_key: lease.session_key.clone(),
        generation: shared.restart.current_generation,
        created_at_ms: now_ms,
        observed_at_ms: prompt.observed_at.timestamp_millis().max(0) as u64,
        state: super::tui_direct_pending_start::PendingStartState::Waiting,
        attempt_count: 0,
    };
    if let Err(error) = super::tui_direct_pending_start::persist(&record) {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            error = %error,
            "failed to persist durable TUI-direct pending-start record; spawning worker anyway off the in-memory presence index"
        );
    }
    super::tui_direct_pending_start::spawn_worker(
        shared.clone(),
        record,
        pending_start_view_fn(),
        pending_start_claim_fn(),
        pending_start_abort_cleanup_fn(),
    );
}

/// The worker's per-poll view builder (see [`synthetic_start_prior_turn_view`]).
fn pending_start_view_fn() -> super::tui_direct_pending_start::ViewFn {
    Box::new(|shared, record| {
        Box::pin(async move {
            let provider = ProviderKind::from_str(&record.provider)?;
            let channel_id = ChannelId::new(record.channel_id);
            Some(
                synthetic_start_prior_turn_view(
                    shared,
                    &provider,
                    channel_id,
                    &record.tmux_session_name,
                    record.anchor_message_id,
                )
                .await,
            )
        })
    })
}

/// The worker's claim action: rehydrate the lease (in case a restart dropped the
/// in-memory map), then run the normal [`claim_tui_direct_synthetic_turn`] which
/// reads the runtime binding FRESH and seeds `turn_start_offset = relay_last_offset()`
/// (post-drain == EOF) with `response_sent_offset = 0`.
fn pending_start_claim_fn() -> super::tui_direct_pending_start::ClaimFn {
    Box::new(|shared, record| {
        Box::pin(async move {
            let Some(provider) = ProviderKind::from_str(&record.provider) else {
                return false;
            };
            let channel_id = ChannelId::new(record.channel_id);
            let anchor_message_id = MessageId::new(record.anchor_message_id);

            // Rehydrate the external-input lease from the durable record's
            // fields (a restart clears the in-memory lease map). NEVER resubmit
            // the provider prompt — only the relay lease is restored.
            let mut lease = ExternalInputRelayLease::unassigned(Some(record.channel_id));
            lease.turn_id = record.lease_turn_id.clone();
            lease.session_key = record.lease_session_key.clone();
            lease.relay_owner = parse_external_input_relay_owner(&record.lease_relay_owner);
            lease.runtime_kind = record
                .lease_runtime_kind
                .as_deref()
                .and_then(RuntimeHandoffKind::from_str);
            let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                provider.as_str(),
                &record.tmux_session_name,
                lease,
            );

            // #3154 design point 6: register the turn with the single-authority
            // finalizer BEFORE the claim saves the inflight + (implicitly, via
            // the lease/inflight) releases the watcher gate — mirrors the bridge
            // register-before-unpause at turn_bridge/mod.rs.
            shared.turn_finalizer.register_start(
                super::turn_finalizer::TurnKey::new(
                    channel_id,
                    record.anchor_message_id,
                    shared.restart.current_generation,
                ),
                provider.clone(),
                super::inflight::RelayOwnerKind::Watcher,
                // #3016 phase-5a: prime the reconcile cache at register time.
                shared,
            );

            let claim = claim_tui_direct_synthetic_turn(
                shared,
                &provider,
                channel_id,
                &record.tmux_session_name,
                &record.prompt_text,
                anchor_message_id,
                &lease,
            )
            .await;

            // #3154 P1-3: adopt the claim's relay_owner into the in-memory lease
            // EXACTLY like the inline (non-deferred) path does (see
            // `relay_observed_prompt` lines ~854-865). The claim may decide the
            // tmux WATCHER owns this turn's output; if so, the persisted lease
            // (rehydrated as BridgeAdapter) is stale. Without re-recording it,
            // the observer-side `maybe_spawn_claude_idle_response_tail` / bridge
            // tail keeps running on the stale BridgeAdapter lease while the
            // watcher relays the same output → DUPLICATE relay (the original
            // bug). Re-record with the adopted owner so any bridge-tail guard
            // reading the lease sees the watcher owns it and stops.
            if claim_should_adopt_relay_owner(claim.claimed, lease.relay_owner, claim.relay_owner) {
                let mut adopted = lease.clone();
                adopted.relay_owner = claim.relay_owner;
                let _ = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                    provider.as_str(),
                    &record.tmux_session_name,
                    adopted,
                );
                tracing::info!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    prior_relay_owner = lease.relay_owner.as_str(),
                    adopted_relay_owner = claim.relay_owner.as_str(),
                    "tui_direct_pending_start: deferred claim adopted watcher relay_owner into the in-memory lease (bridge tail will stand down)"
                );
            }

            // #3154 P1 (BridgeAdapter-GAP fix). The observer stood down for ALL
            // deferred starts because it could not know the RESOLVED owner before
            // the claim ran. Now that the claim has resolved it, the worker is the
            // single place that knows the owner kind, so it MIRRORS the inline path:
            // when the claim resolved to the BridgeAdapter (no watcher will relay
            // this turn), the worker spawns EXACTLY ONE bridge tail here — otherwise
            // the synthetic turn's output is never relayed (relayer_count == 0). When
            // the claim resolved to the watcher this predicate is false (the watcher
            // is the sole relayer; spawning would double-relay). The spawn is on the
            // detached worker task (unix), exactly like the observer's unix-only tail.
            #[cfg(unix)]
            if claim.claimed && deferred_claim_requires_bridge_tail_relayer(claim.relay_owner) {
                // The lease the bridge tail reads must reflect the resolved owner.
                // `claim_should_adopt_relay_owner` above is false for the BridgeAdapter
                // case (the rehydrated lease was already BridgeAdapter), so re-read the
                // stored lease (or fall back to the rehydrated one) and ensure it carries
                // the resolved owner before handing it to the self-gating tail.
                let mut tail_lease =
                    crate::services::tui_prompt_dedupe::external_input_relay_lease(
                        provider.as_str(),
                        &record.tmux_session_name,
                        record.channel_id,
                    )
                    .unwrap_or_else(|| lease.clone());
                tail_lease.relay_owner = claim.relay_owner;
                // #3154 P1 (timestamp-anchor output loss): `observed_at` is NO LONGER
                // used to anchor the tail's start offset for this deferred path — we
                // pass the claim's post-drain EOF `turn_start_offset` explicitly below
                // (see `explicit_start_offset`). It remains on the struct only for the
                // tail's tracing/lease bookkeeping; a `Utc::now()` timestamp scan here
                // would skip bytes written during the deferred-claim wait window.
                let observed = ObservedTuiPrompt {
                    provider: record.provider.clone(),
                    tmux_session_name: record.tmux_session_name.clone(),
                    prompt: record.prompt_text.clone(),
                    observed_at: chrono::Utc::now(),
                };
                let spawned = maybe_spawn_claude_idle_response_tail(
                    shared.clone(),
                    channel_id,
                    &observed,
                    &tail_lease,
                    Some(record.anchor_message_id),
                    // Anchor to the claim's post-drain EOF offset (source of truth
                    // for this synthetic turn's first byte) — NOT a timestamp scan.
                    Some(claim.turn_start_offset),
                )
                .await;
                tracing::info!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    resolved_relay_owner = claim.relay_owner.as_str(),
                    bridge_tail_spawned = spawned,
                    "tui_direct_pending_start: deferred claim resolved to BridgeAdapter owner; worker spawned the bridge tail (no relay GAP)"
                );
            }
            claim.claimed
        })
    })
}

/// #3296 (supersedes the #3282 `⏳ → ⚠` swap): the worker's terminal-ABORT
/// reconcile hook. The input was ALREADY provider-submitted by ABORT time (the
/// abort drops only the synthetic OWNERSHIP claim), so the anchor's `⏳` is
/// still TRUE — the old `⚠` swap branded ANSWERED messages as failures. So:
/// KEEP the `⏳` and record a durable aborted-anchor marker pinning the
/// FOREIGN prior inflight's identity — the worker's LAST-VIEW identity first,
/// the cleanup-instant row only as the no-view fallback (codex r3,
/// `pin_abort_foreign_identity`). The marker stays uncovered unless a commit
/// tombstone proves the prior owner committed (`record_for_abort`'s 대조;
/// force-clear/stop/recovery deletions stay uncovered). The watcher drain
/// flips it `⏳ → ✅` ONLY when THAT turn commits; the sweeper flips it
/// `⏳ → ⚠` after the TTL with no holding inflight (hard-cap bounded).
/// Recorded even when http is unavailable; every later reaction op resolves
/// the shared http INSIDE the marker module — the add≡remove identity (#3164).
fn pending_start_abort_cleanup_fn() -> super::tui_direct_pending_start::AbortCleanupFn {
    Box::new(|_shared, record, last_view_foreign| {
        Box::pin(async move {
            // Defensive (I5): a corrupted durable record could carry a zero
            // anchor id — `MessageId::new(0)` panics and a zero-id marker could
            // never be reconciled. `record()` rejects it too; skip outright.
            if record.anchor_message_id == 0 {
                return;
            }
            // codex r3: LAST-VIEW first — a SUCCESSOR row may hold the slot by
            // now (prior row committed); the row read is a lazy fallback only.
            let foreign = super::tui_direct_pending_start::pin_abort_foreign_identity(
                last_view_foreign,
                || {
                    ProviderKind::from_str(&record.provider)
                        .and_then(|provider| {
                            super::inflight::load_inflight_state(&provider, record.channel_id)
                        })
                        .map(|state| (state.user_msg_id, state.started_at))
                },
            );
            match super::tui_direct_abort_marker::record_for_abort(
                record.provider.clone(),
                record.channel_id,
                record.anchor_message_id,
                record.tmux_session_name.clone(),
                foreign,
            ) {
                Ok(marker) => tracing::info!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    foreign_user_msg_id = ?marker.foreign_user_msg_id,
                    tombstone_covered = marker.covered_at_ms.is_some(),
                    "tui_direct_pending_start: synthetic turn-start ABORTed; anchor keeps ⏳ and a durable aborted-anchor marker was recorded — reconcile lands ✅ on the recorded foreign turn's commit (tombstone-covered when it already committed) or ⚠ via the sweep bound (#3296)"
                ),
                Err(error) => tracing::warn!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    anchor_message_id = record.anchor_message_id,
                    error = %error,
                    "tui_direct_pending_start: failed to persist the aborted-anchor marker; anchor ⏳ may linger until manual cleanup (#3296)"
                ),
            }
        })
    })
}

fn parse_external_input_relay_owner(value: &str) -> ExternalInputRelayOwner {
    match value {
        "bridge_adapter" => ExternalInputRelayOwner::BridgeAdapter,
        "tui_prompt_relay" => ExternalInputRelayOwner::TuiPromptRelay,
        "tmux_watcher" => ExternalInputRelayOwner::TmuxWatcher,
        "session_bound_relay" => ExternalInputRelayOwner::SessionBoundRelay,
        _ => ExternalInputRelayOwner::Unassigned,
    }
}

/// #3154 restart durability: restore durable pending-start records during
/// provider relay startup. Rehydrates the in-memory presence index (so the
/// watcher / idle-queue gates hold immediately) and respawns the worker for each
/// record whose provider matches.
fn restore_pending_starts(shared: &Arc<SharedData>, provider: &ProviderKind) {
    for record in super::tui_direct_pending_start::load_all() {
        if !record.provider.eq_ignore_ascii_case(provider.as_str()) {
            continue;
        }
        // Re-mark present (load_all does not touch the index) so the gates hold
        // before the worker's first poll.
        super::tui_direct_pending_start::mark_present_on_restore(
            &record.provider,
            record.channel_id,
        );
        tracing::info!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            "restored durable TUI-direct pending-start record on relay startup; respawning detached worker (prompt NOT resubmitted)"
        );
        super::tui_direct_pending_start::spawn_worker(
            shared.clone(),
            record,
            pending_start_view_fn(),
            pending_start_claim_fn(),
            pending_start_abort_cleanup_fn(),
        );
    }
}

// #3016 phase-5b2: `publish_tui_direct_watcher_finalize_debt` was removed. It
// stored the per-handle `mailbox_finalize_owed` flag (#1452) for TUI-direct
// synthetic turns, but that flag has no remaining finalize-decision readers —
// the watcher finalizes on the confirmed-completion / structural signal
// (`normal_completion = true`) and the ledger's `register_start` is the
// authority — so the producer was a dead write and is gone.

fn tui_direct_watcher_can_own_output(
    watchers: &super::TmuxWatcherRegistry,
    tmux_session_name: &str,
    output_path: Option<&Path>,
) -> bool {
    let watcher_alive = watchers
        .tmux_session_is_stale(tmux_session_name)
        .is_some_and(|stale| !stale);
    if !watcher_alive {
        return false;
    }
    match output_path {
        Some(output_path) => watchers
            .watcher_output_path(tmux_session_name)
            .is_some_and(|watcher_path| Path::new(&watcher_path) == output_path),
        None => true,
    }
}

fn tui_direct_synthetic_inflight_active_for_prompt(
    provider: &str,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> bool {
    let Some(provider) = ProviderKind::from_str(provider) else {
        return false;
    };
    tui_direct_synthetic_inflight_matches(
        super::inflight::load_inflight_state(&provider, channel_id.get()).as_ref(),
        tmux_session_name,
    )
}

fn tui_direct_synthetic_inflight_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
    })
}

fn tui_direct_watcher_synthetic_inflight_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
            && state.effective_relay_owner_kind() == RelayOwnerKind::Watcher
    })
}

#[cfg(unix)]
fn codex_ownerless_external_input_inflight_needs_rollout_recovery(
    state: &InflightTurnState,
    tmux_session_name: &str,
) -> bool {
    if state.turn_source != TurnSource::ExternalInput
        || state.runtime_kind != Some(RuntimeHandoffKind::CodexTui)
        || state.effective_relay_owner_kind() != RelayOwnerKind::None
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
        || state.injected_prompt_message_id.is_none()
        || state.current_msg_id != 0
        || state.response_sent_offset != 0
        || !state.full_response.trim().is_empty()
        || state.last_watcher_relayed_offset.is_some()
        || state.terminal_delivery_committed
    {
        return false;
    }
    // At this point the inflight is ownerless and no Discord delivery has ever
    // started. Recovery must run whether `output_path` is stale/missing or
    // already points at the live rollout: an earlier deploy can interrupt after
    // repairing the path but before the bridge posts a response.
    true
}

#[cfg(unix)]
async fn wait_for_tui_direct_watcher_synthetic_claim(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> bool {
    let deadline = tokio::time::Instant::now() + TUI_DIRECT_SYNTHETIC_CLAIM_WAIT;
    loop {
        if tui_direct_watcher_synthetic_inflight_matches(
            super::inflight::load_inflight_state(provider, channel_id.get()).as_ref(),
            tmux_session_name,
        ) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(TUI_DIRECT_SYNTHETIC_CLAIM_POLL.min(deadline - now)).await;
    }
}

#[cfg(unix)]
async fn finish_tui_direct_synthetic_turn_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    reason: &'static str,
) {
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        return;
    };
    if !tui_direct_synthetic_inflight_matches(Some(&state), tmux_session_name) {
        return;
    }
    let snapshot = super::mailbox_snapshot(shared, channel_id).await;
    // user_msg_id == 0 (a TUI-direct turn with no anchored Discord user
    // message) maps to `None`, matching the mailbox's `active_user_message_id`
    // for such turns; `MessageId::new(0)` would panic.
    if snapshot.active_user_message_id != super::inflight::optional_message_id(state.user_msg_id) {
        return;
    }
    super::inflight::clear_inflight_state(provider, channel_id.get());
    let finish = super::mailbox_finish_turn(shared, provider, channel_id).await;
    if finish.removed_token.is_some() {
        super::saturating_decrement_global_active(shared);
    }
    if finish.mailbox_online && finish.has_pending {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            reason,
        );
    }
}

#[cfg(unix)]
async fn maybe_spawn_claude_idle_response_tail(
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    lease: &ExternalInputRelayLease,
    current_turn_anchor_id: Option<u64>,
    // #3154 P1 (timestamp-anchor output loss): when `Some`, anchor the tail's
    // start to THIS explicit transcript byte offset (the deferred claim's
    // post-drain EOF `turn_start_offset`) and SKIP the `observed_at` timestamp
    // scan. The timestamp scan picks the first transcript line at/after
    // `prompt.observed_at`; for the worker-spawned deferred-BridgeAdapter path
    // that timestamp is a `Utc::now()` synthesized AFTER the claim wait, so the
    // scan skips every byte written during the wait window — those bytes belong
    // to this synthetic turn and would be lost. The post-drain EOF offset is the
    // exact turn boundary (no skip, no re-relay of prior-turn bytes). `None`
    // preserves the original timestamp-scan behaviour for the inline /
    // non-deferred path.
    explicit_start_offset: Option<u64>,
) -> bool {
    if !prompt
        .provider
        .trim()
        .eq_ignore_ascii_case(ProviderKind::Claude.as_str())
    {
        return false;
    }
    if !bridge_adapter_owns_external_turn(lease.relay_owner) {
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
        return false;
    }
    if !wait_for_claude_inflight_to_clear(
        channel_id,
        &prompt.tmux_session_name,
        current_turn_anchor_id,
    )
    .await
    {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            wait_ms = CLAUDE_IDLE_INFLIGHT_DRAIN_WAIT.as_millis(),
            "skipping Claude idle response tail; previous inflight did not drain"
        );
        return false;
    }
    let Some(binding) = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
        &prompt.tmux_session_name,
    ) else {
        tracing::debug!(
            tmux_session_name = %prompt.tmux_session_name,
            "skipping Claude idle response tail; no runtime binding"
        );
        return false;
    };
    if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
        return false;
    }

    // #2843: resolve the freshest active transcript (the bound output_path can be
    // stale) and only let a non-stale tmux watcher suppress the tail when it
    // actually covers that transcript. Re-registers the binding if it changed.
    let Some(transcript_path) = resolve_idle_relay_transcript(
        &shared,
        &prompt.tmux_session_name,
        channel_id,
        &binding,
        false,
    ) else {
        return false;
    };

    // #2843: if the path changed, don't trust the old binding offset (it indexes
    // a different transcript and would replay old output); the timestamp-based
    // resolution still takes precedence, falling back to the fresh EOF.
    let fallback_offset = if Path::new(&binding.output_path) == transcript_path {
        binding.last_offset
    } else {
        claude_tui_rehydrate_start_offset(&transcript_path)
    };
    let start_offset = resolve_idle_tail_start_offset(
        &transcript_path,
        explicit_start_offset,
        prompt.observed_at,
        fallback_offset,
    );
    spawn_claude_idle_response_tail_once(
        shared,
        prompt.tmux_session_name.clone(),
        channel_id,
        transcript_path,
        start_offset,
        prompt.prompt.clone(),
        lease.clone(),
    )
}

#[cfg(unix)]
/// #3176: is the present inflight THIS turn's own TUI-direct synthetic row?
///
/// The drain-wait must only skip waiting on the inflight WE just created for the
/// current turn. The discriminator is the precise current-turn identity —
/// `ExternalInput` + same tmux session + `user_msg_id == this turn's anchor
/// message id` — NOT merely same-session `ExternalInput` (which would also match a
/// still-draining PREVIOUS same-session TUI turn and wrongly skip it, risking
/// interleaved or lost delivery). When the current turn created no synthetic
/// (`current_turn_anchor_id == None` — e.g. system-continuation / slash-control
/// paths), nothing here is "ours", so any present inflight remains a previous turn
/// and still blocks.
#[cfg(unix)]
fn inflight_is_current_turn_synthetic(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
    current_turn_anchor_id: Option<u64>,
) -> bool {
    match (state, current_turn_anchor_id) {
        (Some(state), Some(anchor_id)) => {
            state.turn_source == TurnSource::ExternalInput
                && state.tmux_session_name.as_deref() == Some(tmux_session_name)
                && state.user_msg_id == anchor_id
        }
        _ => false,
    }
}

#[cfg(unix)]
async fn wait_for_claude_inflight_to_clear(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_turn_anchor_id: Option<u64>,
) -> bool {
    let mut observed_inflight = false;
    let cleared = wait_for_transient_state_to_clear(
        CLAUDE_IDLE_INFLIGHT_DRAIN_WAIT,
        CLAUDE_IDLE_INFLIGHT_DRAIN_POLL,
        || {
            // #3176: a present inflight only BLOCKS this turn's idle tail when it
            // belongs to a DIFFERENT (previous) turn. Our OWN synthetic for THIS turn
            // (created upstream in the notify/anchor block) must not be waited on —
            // doing so self-deadlocks (we created it; it never "drains" within the
            // window), permanently skipping the relay and silently dropping every
            // subsequent response. Identity-pinned via `inflight_is_current_turn_synthetic`.
            let state =
                super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get());
            let blocking = state.is_some()
                && !inflight_is_current_turn_synthetic(
                    state.as_ref(),
                    tmux_session_name,
                    current_turn_anchor_id,
                );
            observed_inflight |= blocking;
            blocking
        },
    )
    .await;
    if observed_inflight && cleared {
        tracing::info!(
            provider = ProviderKind::Claude.as_str(),
            channel_id = channel_id.get(),
            wait_ms = CLAUDE_IDLE_INFLIGHT_DRAIN_WAIT.as_millis(),
            "Claude idle response tail waited for previous inflight to drain"
        );
    }
    cleared
}

#[cfg(unix)]
async fn wait_for_transient_state_to_clear<F>(
    timeout: Duration,
    poll_interval: Duration,
    mut is_present: F,
) -> bool
where
    F: FnMut() -> bool,
{
    if !is_present() {
        return true;
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return !is_present();
        }
        tokio::time::sleep(poll_interval.min(deadline - now)).await;
        if !is_present() {
            return true;
        }
    }
}

#[cfg(unix)]
fn spawn_claude_idle_response_tail_once(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    transcript_path: PathBuf,
    start_offset: u64,
    prompt_text: String,
    lease: ExternalInputRelayLease,
) -> bool {
    // #3183: never re-relay output the watcher already committed delivery for. Both spawn paths
    // (observed-prompt + background poll loop) funnel through here, so clamping once covers both.
    // #3183 codex (CRITICAL outage-safety): `committed_relay_offset` is a
    // PER-CHANNEL watermark, not per-transcript. A stale-high watermark left by a
    // PREVIOUS wrapper (e.g. 5000) would, after a respawn whose fresh transcript
    // starts near 0, clamp this tail forward and SKIP the new turn's response —
    // exactly the relay-loss the idle tail exists to prevent. Run the SAME
    // generation-aware regression resets the watcher / idle-JSONL sink run BEFORE
    // consulting the watermark (session_relay_sink.rs): a truncated/respawned
    // transcript (EOF below the watermark) or a wrapper-generation change resets the
    // watermark to 0, so the fresh range is relayed; only a watermark that genuinely covers THIS transcript clamps (dedupe).
    let transcript_len = std::fs::metadata(&transcript_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    super::tmux::reset_stale_relay_watermark_if_output_regressed(
        shared.as_ref(),
        channel_id,
        &tmux_session_name,
        transcript_len,
        "idle_response_tail",
    );
    super::tmux::reset_relay_watermark_on_generation_change(
        shared.as_ref(),
        channel_id,
        &tmux_session_name,
        "idle_response_tail",
    );
    // #3089 B2c (#3235): durable-frontier dedup clamp (flag OFF → in-memory) survives restart.
    let committed_offset = dr::effective_committed_offset(
        &shared,
        &ProviderKind::Claude,
        channel_id,
        &tmux_session_name,
    );
    let start_offset = clamp_idle_tail_start_offset_to_committed(start_offset, committed_offset);
    if committed_offset > 0 {
        tracing::debug!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            committed_offset,
            start_offset,
            "Claude idle response tail start offset clamped to watcher committed delivery offset"
        );
    }
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
                prompt_text,
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
                // #3105 (codex P2): `rehydrate_existing_claude_tui_bindings` is a
                // fully BLOCKING pass (synchronous `tmux` subprocess calls + a
                // `std::thread::sleep` between multi-sample pane probes); running it
                // inline would stall the executor for samples×delay plus tmux latency.
                // Move it onto the blocking pool via `spawn_blocking` (the sync core
                // and its unit-testable logic are unchanged).
                let shared_for_rehydrate = shared.clone();
                let rehydrate_result = tokio::task::spawn_blocking(move || {
                    rehydrate_existing_claude_tui_bindings(&shared_for_rehydrate);
                })
                .await;
                if let Err(error) = rehydrate_result {
                    tracing::warn!(
                        error = %error,
                        "Claude TUI binding rehydrate task panicked or was cancelled"
                    );
                }
                next_rehydrate = now + CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL;
            }
            for (tmux_session_name, binding) in
                crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
                    RuntimeHandoffKind::ClaudeTui,
                )
            {
                let Some(channel_id) =
                    owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, &tmux_session_name)
                else {
                    // #3018/#3306/#3656: registry miss ⇒ drop; chokepoint repairs.
                    continue;
                };
                if super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get())
                    .is_some()
                {
                    continue;
                }

                // #2843: resolve the freshest transcript (re-register stale bound
                // paths) + corrected watcher guard — heartbeat misses stale files.
                let Some(transcript_path) = resolve_idle_relay_transcript(
                    &shared,
                    &tmux_session_name,
                    channel_id,
                    &binding,
                    !session_bound_discord_delivery_enabled(),
                ) else {
                    continue;
                };
                // #3402: restore footer slots a restart wiped while tasks kept
                // running (one-shot per channel+session; footer-mode gated inside).
                shared.ui.placeholder_live_events.rehydrate_slots_once_for_session(
                    channel_id,
                    binding.session_id.as_deref(),
                    &transcript_path,
                );
                let path_changed = Path::new(&binding.output_path) != transcript_path;
                let scan_offset = if path_changed {
                    // #2843 (codex P1): path changed — scan a bounded lookback
                    // instead of starting at EOF so a prompt already written to
                    // the freshly-resolved transcript is still found.
                    claude_tui_rehydrate_start_offset(&transcript_path)
                        .saturating_sub(CLAUDE_IDLE_FRESH_TRANSCRIPT_LOOKBACK_BYTES)
                } else {
                    binding.last_offset
                };
                // #2843 (codex round-2 P1): the lookback can hold several finished
                // turns — on a path change select the NEWEST prompt (the just-typed
                // one); unchanged-path tailing keeps first-prompt semantics.
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
                        entry_id,
                        ..
                    } => {
                        let observed_at = chrono::Utc::now();
                        // #3540: pass the entry's STABLE identity so an
                        // already-relayed prompt re-encountered after a watermark
                        // reset / jsonl head rotation is suppressed by identity
                        // (`SuppressedReplayedEntry`) and never mints a phantom
                        // synthetic inflight. `entry_id == None` falls back to the
                        // content-keyed 30s recent-observed dedup (pre-#3540).
                        let observation =
                            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
                                ProviderKind::Claude.as_str(),
                                &tmux_session_name,
                                &prompt,
                                entry_id.as_deref(),
                                observed_at,
                            );
                        tracing::info!(
                            tmux_session_name = %tmux_session_name,
                            channel_id = channel_id.get(),
                            observation = ?observation,
                            entry_id = entry_id.as_deref().unwrap_or(""),
                            "Claude idle transcript relay observed prompt"
                        );
                        advance_claude_tmux_runtime_binding_offset(
                            &tmux_session_name,
                            &transcript_path,
                            line_end_offset,
                        );
                        // #3305: a LOCAL-completing pass-through command's
                        // `<command-*>` transcript echo (/effort /compact /cost
                        // /context) never starts a model turn, so do NOT select an
                        // external turn owner / wait for a synthetic claim / spawn a
                        // response tail for it. Skipping here (after advancing the
                        // offset so it is not re-scanned) keeps the inflight table
                        // empty so the next injection is not FOREIGN-ABORTed; the
                        // broadcast relay still posts the kind-only guidance note. A
                        // /loop echo is off the allow-list and keeps full lifecycle.
                        if is_local_only_slash_command_prompt(&prompt) {
                            tracing::info!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                slash_command_kind = %slash_command_control_kind(&prompt),
                                "Claude idle transcript relay skipped local-only pass-through slash command (no external turn owner / synthetic claim / response tail)"
                            );
                            continue;
                        }
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
                        if wait_for_tui_direct_watcher_synthetic_claim(
                            &ProviderKind::Claude,
                            channel_id,
                            &tmux_session_name,
                        )
                        .await
                        {
                            tracing::info!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                                session_key = lease.session_key.as_deref().unwrap_or(""),
                                "Claude idle transcript relay yielded to TUI-direct synthetic watcher inflight"
                            );
                            continue;
                        }
                        if bridge_adapter_owns_external_turn(lease.relay_owner) {
                            let tail_spawned = spawn_claude_idle_response_tail_once(
                                shared.clone(),
                                tmux_session_name.clone(),
                                channel_id,
                                transcript_path,
                                line_end_offset,
                                prompt,
                                lease.clone(),
                            );
                            if !tail_spawned {
                                clear_external_input_bridge_lease_if_current(
                                    &ProviderKind::Claude,
                                    &tmux_session_name,
                                    channel_id,
                                    &lease,
                                );
                            }
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

/// #3105 (codex P2): the eviction in `evict_dead_orphaned_claude_tui_mirrors` is
/// destructive (it tombstones the dedupe mirror and so removes self-heal), so the
/// liveness check that gates it must be conservative against a TRANSIENT
/// pane-probe flake. We require the "no live pane" verdict to hold across multiple
/// samples (with a short delay between them) — a single negative read must never
/// declare a session dead. `1` would reproduce the original single-sample bug.
#[cfg(unix)]
const DEAD_ORPHANED_PANE_PROBE_SAMPLES: usize = 3;

/// Delay between consecutive pane probes. A genuinely-live session that briefly
/// flaked recovers within one of these windows; a genuinely-gone session stays
/// dead across all of them. Kept small so the (rare) eviction path adds at most
/// a few hundred ms to a single rehydrate pass that runs every 5s.
#[cfg(unix)]
const DEAD_ORPHANED_PANE_PROBE_DELAY: Duration = Duration::from_millis(75);

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

#[cfg(unix)]
fn resolved_claude_idle_relay_transcript_path(
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
    allow_watcher_suppression: bool,
) -> Option<PathBuf> {
    let transcript_path =
        resolved_claude_idle_relay_transcript_path(shared, tmux_session_name, channel_id, binding)?;

    if !allow_watcher_suppression {
        return Some(transcript_path);
    }

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

    Some(transcript_path)
}

#[cfg(unix)]
pub(super) fn resolve_rehydrated_claude_tmux_channel_id(tmux_session_name: &str) -> Option<u64> {
    resolve_rehydrated_tmux_channel_id(&ProviderKind::Claude, tmux_session_name)
}

#[cfg(unix)]
pub(super) fn resolve_rehydrated_tmux_channel_id(
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> Option<u64> {
    let mut matched: Option<u64> = None;
    for binding in super::settings::list_registered_channel_bindings() {
        if &binding.owner_provider != provider {
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
            let Some(candidate_channel_id) = rehydrated_channel_id_for_segment(
                provider,
                tmux_session_name,
                segment,
                binding.channel_id,
            ) else {
                continue;
            };
            if matched.is_some_and(|existing| existing != candidate_channel_id) {
                tracing::warn!(
                    tmux_session_name,
                    provider = provider.as_str(),
                    channel_id = candidate_channel_id,
                    existing_channel_id = matched.unwrap_or_default(),
                    "TUI rehydrate skipped ambiguous exact session-name match"
                );
                return None;
            }
            matched = Some(candidate_channel_id);
        }
    }
    matched
}

#[cfg(all(unix, test))]
fn rehydrated_claude_channel_id_for_segment(
    tmux_session_name: &str,
    segment: &str,
    parent_channel_id: u64,
) -> Option<u64> {
    rehydrated_channel_id_for_segment(
        &ProviderKind::Claude,
        tmux_session_name,
        segment,
        parent_channel_id,
    )
}

#[cfg(unix)]
fn rehydrated_channel_id_for_segment(
    provider: &ProviderKind,
    tmux_session_name: &str,
    segment: &str,
    parent_channel_id: u64,
) -> Option<u64> {
    let base_session_name = provider.build_tmux_session_name(segment);
    if base_session_name == tmux_session_name {
        return Some(parent_channel_id);
    }

    let (session_provider, session_segment) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)?;
    if &session_provider != provider {
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
async fn run_claude_idle_response_tail(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    transcript_path: PathBuf,
    start_offset: u64,
    prompt_text: String,
    lease: ExternalInputRelayLease,
) {
    let _lease_guard = TuiDirectExternalInputLeaseGuard::new(
        ProviderKind::Claude,
        &tmux_session_name,
        channel_id,
        &lease,
    );

    // #3256: STREAM the operator's external-input prose THROUGH a single bridge
    // turn instead of pre-collecting the whole response and posting it as one
    // batched `[Text{full}, Done]` at turn end. The transcript reader
    // (`read_output_file_until_result`) already emits each `StreamMessage`
    // (Text / ToolUse / ToolResult / Done / OutputOffset) in real time; we feed
    // those frames into the SAME bridge `tx` a normal Discord turn uses, so a
    // LONG continuous autonomous turn relays prose progressively within ONE
    // intake card / ONE `spawn_turn_bridge` instead of accumulating until the
    // next user-message turn boundary.
    //
    // The reader runs on a blocking OS thread; it forwards every frame onto an
    // intermediate channel (`reader_rx`) and reports the final transcript byte
    // offset over `offset_tx` after it returns (done / idle / dead). We BUFFER
    // the leading frames until the first content frame arrives so that a turn
    // that produces NO prose still takes the original empty-response path (no
    // intake card, just advance the binding offset + finish the synthetic turn)
    // — preserving today's behavior for the common no-op case.
    let (reader_tx, reader_rx) = mpsc::channel::<StreamMessage>();
    let (offset_tx, offset_rx) = tokio::sync::oneshot::channel::<Result<u64, String>>();
    let transcript_for_reader = transcript_path.clone();
    let tmux_for_reader = tmux_session_name.clone();
    std::thread::Builder::new()
        .name("claude_idle_response_tail_reader".to_string())
        .spawn(move || {
            let transcript_string = transcript_for_reader.display().to_string();
            let read_result = crate::services::session_backend::read_output_file_until_result(
                &transcript_string,
                start_offset,
                reader_tx,
                None,
                crate::services::provider::SessionProbe::tmux(
                    tmux_for_reader,
                    ProviderKind::Claude,
                ),
            );
            let offset_result = read_result.map(|result| match result {
                ReadOutputResult::Completed { offset }
                | ReadOutputResult::Cancelled { offset }
                | ReadOutputResult::SessionDied { offset } => offset,
            });
            let _ = offset_tx.send(offset_result);
        })
        .expect("spawn claude idle response tail reader thread");

    // Buffer leading frames on the blocking pool until the first content frame
    // (or the reader closes). `prefix` carries the frames already pulled,
    // `has_content` tells us whether the bridge should run, and we hand the live
    // `reader_rx` back to drain the remainder into the bridge.
    let buffered = tokio::task::spawn_blocking(move || {
        let mut prefix: Vec<StreamMessage> = Vec::new();
        let mut has_content = false;
        while let Ok(message) = reader_rx.recv() {
            let is_content = idle_stream_message_is_content(&message);
            let is_terminal = matches!(message, StreamMessage::Done { .. });
            prefix.push(message);
            if is_content {
                has_content = true;
                break;
            }
            if is_terminal {
                break;
            }
        }
        (prefix, has_content, reader_rx)
    })
    .await;

    let (prefix, has_content, reader_rx) = match buffered {
        Ok(buffered) => buffered,
        Err(error) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                transcript_path = %transcript_path.display(),
                error = %error,
                "Claude idle transcript response tail buffering panicked"
            );
            finish_tui_direct_synthetic_turn_if_current(
                &shared,
                &ProviderKind::Claude,
                channel_id,
                &tmux_session_name,
                "claude_tui_direct_tail_panicked",
            )
            .await;
            return;
        }
    };

    if !has_content {
        // No prose / no terminal body for this turn: keep today's no-card empty
        // path. Drain any residual frames so the reader thread can finish, then
        // commit the binding offset.
        let _ = tokio::task::spawn_blocking(move || while reader_rx.recv().is_ok() {}).await;
        if let Ok(Ok(final_offset)) = offset_rx.await {
            advance_claude_tmux_runtime_binding_offset(
                &tmux_session_name,
                &transcript_path,
                final_offset,
            );
        }
        finish_tui_direct_synthetic_turn_if_current(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            &tmux_session_name,
            "claude_tui_direct_empty_response",
        )
        .await;
        return;
    }

    let delivery_result = stream_tui_idle_response_through_bridge(
        &shared,
        ProviderKind::Claude,
        channel_id,
        &tmux_session_name,
        &transcript_path,
        start_offset,
        &prompt_text,
        prefix,
        reader_rx,
        &lease,
    )
    .await;
    if delivery_result.is_err() {
        finish_tui_direct_synthetic_turn_if_current(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            &tmux_session_name,
            "claude_tui_direct_delivery_failed",
        )
        .await;
    }
    // #3041 / #3256: advance the runtime-binding offset on successful delivery so
    // the watcher / idle paths never double-send this turn's bytes. The reader
    // reports the authoritative final offset over `offset_rx`.
    let final_offset = match offset_rx.await {
        Ok(Ok(offset)) => Some(offset),
        _ => None,
    };
    if let Some(final_offset) = final_offset
        && tui_idle_tail_stream_should_commit_runtime_binding_offset(delivery_result.is_ok())
    {
        advance_claude_tmux_runtime_binding_offset(
            &tmux_session_name,
            &transcript_path,
            final_offset,
        );
    }
}

/// #3256: a transcript-reader frame counts as "content" for the idle-tail
/// stream-through when it carries body the operator actually produced — prose
/// (`Text`), an authoritative terminal body (`Done` with a non-empty result),
/// or a transport error. A bare terminal `Done` with an empty result (the
/// synthetic completion frame the reader emits at turn end) or pure control /
/// offset frames are NOT content; if the whole turn yields only those, the
/// idle tail takes the no-card empty path (preserving today's behavior).
#[cfg(unix)]
fn idle_stream_message_is_content(message: &StreamMessage) -> bool {
    match message {
        // #3256: a `Text`/`Done` body that is ONLY leading TUI chrome (e.g.
        // `No response requested.` / `Continue from where you left off.`) is NOT
        // real content — the old path stripped that chrome with
        // `strip_leading_tui_response_chrome` and produced an empty response, i.e.
        // the no-card empty path. Strip BEFORE the emptiness test so a chrome-only
        // turn keeps spawning no placeholder card (parity with prior behavior).
        StreamMessage::Text { content } => {
            !super::response_sanitizer::strip_leading_tui_response_chrome(content)
                .trim()
                .is_empty()
        }
        StreamMessage::Done { result, .. } => {
            !super::response_sanitizer::strip_leading_tui_response_chrome(result)
                .trim()
                .is_empty()
        }
        StreamMessage::Error { message, .. } => !message.trim().is_empty(),
        _ => false,
    }
}

/// #3256: the stream-through path commits the runtime-binding offset whenever
/// the single bridge turn delivered successfully. (The empty-response branch
/// commits independently before finishing the synthetic turn.)
#[cfg(unix)]
fn tui_idle_tail_stream_should_commit_runtime_binding_offset(
    discord_delivery_succeeded: bool,
) -> bool {
    discord_delivery_succeeded
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
#[allow(dead_code)]
async fn relay_tui_idle_response_through_bridge(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &Path,
    start_offset: u64,
    // #3089 A6b r2 [High]: the tail's authoritative end offset. Plumbed into the
    // bridge stream as `OutputOffset` ONLY when the A6b flag is ON (OFF-safe — see
    // `codex_external_input_bridge_stream_messages`) so codex external-input's
    // `ordered_range` becomes true and the cutover reaches the controller.
    final_offset: u64,
    prompt_text: &str,
    response: &str,
    lease: &ExternalInputRelayLease,
) -> Result<(), String> {
    let _lease_guard = TuiDirectExternalInputLeaseGuard::new(
        provider.clone(),
        tmux_session_name,
        channel_id,
        lease,
    );
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
    // #3097: resolve the provider-specific compact threshold so the status panel reflects the configured value (e.g. `context_compact_percent_claude`) instead of the hardcoded 0 it used previously.
    let context_compact_percent = super::adk_session::fetch_context_thresholds(shared.api_port)
        .await
        .compact_pct_for(&provider);
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
    let current_msg_id = super::gateway::send_intake_placeholder(
        http.clone(),
        shared.clone(),
        channel_id,
        reference,
        // #3082 P2-3: a TUI idle-response placeholder is an ACTIVE-turn card, not a queued "📬" notice — it must not wait on the answer-flush barrier.
        false,
    )
    .await?;
    let user_msg_id = anchor
        .map(|anchor| MessageId::new(anchor.message_id))
        .unwrap_or(current_msg_id);
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let inflight_state = build_tui_direct_bridge_inflight_state(
        provider.clone(),
        channel_id,
        user_msg_id,
        current_msg_id,
        prompt_text,
        tmux_session_name,
        output_path,
        start_offset,
        lease,
    );
    let bridge = TurnBridgeContext {
        provider: provider.clone(),
        gateway: Arc::new(TuiDirectBridgeGateway {
            http,
            shared: shared.clone(),
            provider: provider.clone(),
        }),
        channel_id,
        user_msg_id: Some(user_msg_id),
        user_text_owned: prompt_text.to_string(),
        request_owner_name: "TUI direct".to_string(),
        role_binding: None,
        adk_session_key: lease.session_key.clone(),
        adk_session_name: Some(tmux_session_name.to_string()),
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent,
        current_msg_id: Some(current_msg_id),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: Some(start_offset),
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: true, // #3089 A6b: scope the controller OR-in
        inflight_state,
    };

    spawn_turn_bridge(shared.clone(), Arc::new(CancelToken::new()), rx, bridge);
    // #3089 A6b r2 [High]: feed the bridge `[Text?, OutputOffset?(flag-gated), Done]`.
    // The flag-gated `OutputOffset` advances `tmux_last_offset` to `final_offset` so
    // codex external-input's `ordered_range` is true and the cutover reaches the
    // controller; OFF → no `OutputOffset` → byte-identical legacy `NoRange`.
    for message in
        super::tui_prompt_relay_controller_cutover::codex_external_input_bridge_stream_messages(
            response,
            final_offset,
        )
    {
        tx.send(message)
            .map_err(|error| format!("send TUI-direct bridge stream event: {error}"))?;
    }
    drop(tx);

    match tokio::time::timeout(Duration::from_secs(180), completion_rx).await {
        Ok(_) => {
            ensure_tui_direct_bridge_delivery_committed(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
                tmux_session_name,
                lease,
                anchor.map(|anchor| anchor.message_id),
                false,
            )?;
            if let Some(anchor) = anchor {
                crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
                    provider.as_str(),
                    tmux_session_name,
                    anchor,
                );
            }
            tracing::info!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                current_msg_id = current_msg_id.get(),
                prompt_anchor_message_id = anchor.map(|anchor| anchor.message_id),
                "TUI-direct bridge adapter completed response relay"
            );
            Ok(())
        }
        Err(_) => Err(format!(
            "TUI-direct bridge adapter timed out waiting for completion for provider {}",
            provider.as_str()
        )),
    }
}

/// #3256: STREAM-THROUGH variant of `relay_tui_idle_response_through_bridge`
/// for the Claude external-input idle path.
///
/// Identical bridge setup — EXACTLY ONE intake placeholder card and EXACTLY ONE
/// `spawn_turn_bridge` per external turn — but instead of pre-collecting the
/// whole response and feeding the bridge one synthetic `[Text{full}, Done]`,
/// it forwards the transcript reader's LIVE `StreamMessage`s into the same
/// bridge `tx` AS THEY ARRIVE (`prefix` = the frames already buffered upstream,
/// including the first content frame; `reader_rx` = the remaining live stream).
/// The bridge consumes them exactly as it does for a normal Discord turn:
/// `Text` chunks edit the one card progressively, the terminal `Done`
/// finalizes the turn EXACTLY ONCE.
///
/// Behavior-preservation: for a SHORT turn the prefix + a quick `Done` arrive
/// back-to-back, so the bridge still posts one card with the full prose and
/// finalizes once — observably identical to the old collect-then-send path. The
/// only change is that a LONG turn now relays prose incrementally within that
/// one card instead of all-at-once at turn end.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn stream_tui_idle_response_through_bridge(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &Path,
    start_offset: u64,
    prompt_text: &str,
    prefix: Vec<StreamMessage>,
    reader_rx: mpsc::Receiver<StreamMessage>,
    lease: &ExternalInputRelayLease,
) -> Result<(), String> {
    let _lease_guard = TuiDirectExternalInputLeaseGuard::new(
        provider.clone(),
        tmux_session_name,
        channel_id,
        lease,
    );
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
    // #3097: resolve the provider-specific compact threshold so the status
    // panel reflects the configured value.
    let context_compact_percent = super::adk_session::fetch_context_thresholds(shared.api_port)
        .await
        .compact_pct_for(&provider);
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
    // EXACTLY ONE intake placeholder card per external turn.
    let current_msg_id = super::gateway::send_intake_placeholder(
        http.clone(),
        shared.clone(),
        channel_id,
        reference,
        false,
    )
    .await?;
    let user_msg_id = anchor
        .map(|anchor| MessageId::new(anchor.message_id))
        .unwrap_or(current_msg_id);
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let inflight_state = build_tui_direct_bridge_inflight_state(
        provider.clone(),
        channel_id,
        user_msg_id,
        current_msg_id,
        prompt_text,
        tmux_session_name,
        output_path,
        start_offset,
        lease,
    );
    let bridge = TurnBridgeContext {
        provider: provider.clone(),
        gateway: Arc::new(TuiDirectBridgeGateway {
            http,
            shared: shared.clone(),
            provider: provider.clone(),
        }),
        channel_id,
        user_msg_id: Some(user_msg_id),
        user_text_owned: prompt_text.to_string(),
        request_owner_name: "TUI direct".to_string(),
        role_binding: None,
        adk_session_key: lease.session_key.clone(),
        adk_session_name: Some(tmux_session_name.to_string()),
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent,
        current_msg_id: Some(current_msg_id),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: Some(start_offset),
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: true, // #3089 A6b: scope the controller OR-in
        inflight_state,
    };

    // EXACTLY ONE spawn_turn_bridge per external turn.
    spawn_turn_bridge(shared.clone(), Arc::new(CancelToken::new()), rx, bridge);

    // Forward the buffered prefix + the live reader stream into the SINGLE
    // bridge `tx` on a blocking thread (the reader receiver and the bridge
    // sender are both sync `mpsc`). The bridge finalizes on the first terminal
    // `Done`; we send a fallback `Done` only if the reader closed without one
    // so the bridge always finalizes EXACTLY ONCE.
    let forward_handle =
        tokio::task::spawn_blocking(move || forward_idle_stream_into_bridge(prefix, reader_rx, tx));

    // #3256: the forward thread runs for the WHOLE turn — it only returns once the
    // transcript reader closes (turn done / idle / dead), having forwarded every
    // prose frame plus the terminal `Done` into the bridge. Join it FIRST so the
    // completion wait does not race the turn's real duration. A long autonomous
    // turn (many minutes, well past any fixed wall-clock) therefore streams in
    // full and still reports success — the previous `timeout(180s, completion_rx)`
    // placed before this join made >180s turns return `Err` despite a normal
    // delivery, which skipped the runtime-binding offset commit and risked a
    // duplicate re-relay on the next idle poll.
    let _ = forward_handle.await;

    // Only NOW bound the post-`Done` bridge finalization (Discord edit/flush),
    // which should land within seconds of the terminal frame being forwarded.
    let completion = tokio::time::timeout(Duration::from_secs(180), completion_rx).await;

    match completion {
        Ok(_) => {
            ensure_tui_direct_bridge_delivery_committed(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
                tmux_session_name,
                lease,
                anchor.map(|anchor| anchor.message_id),
                true,
            )?;
            if let Some(anchor) = anchor {
                crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
                    provider.as_str(),
                    tmux_session_name,
                    anchor,
                );
            }
            tracing::info!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                current_msg_id = current_msg_id.get(),
                prompt_anchor_message_id = anchor.map(|anchor| anchor.message_id),
                "TUI-direct bridge adapter completed streamed response relay"
            );
            Ok(())
        }
        Err(_) => Err(format!(
            "TUI-direct bridge adapter timed out waiting for completion for provider {}",
            provider.as_str()
        )),
    }
}

/// #3256: forward the buffered prefix and the live transcript-reader stream into
/// the bridge sender, preserving message ordering and guaranteeing a terminal
/// `Done` reaches the bridge exactly once.
///
/// - Leading TUI chrome (`No response requested.` / `Continue from where you
///   left off.`) is stripped from the FIRST non-empty `Text` frame, matching
///   the old `compose_tui_idle_response` behavior so the streamed card never
///   flashes that chrome.
/// - The transcript reader normally emits a terminal `Done` itself; if the
///   stream closes WITHOUT one (e.g. dead session mid-stream), a synthetic
///   `Done` is appended so the bridge still finalizes. A `Done` is forwarded at
///   most once — subsequent frames after a `Done` are dropped, since the bridge
///   has already claimed the turn outcome ("first wins").
///
/// Returns the number of `Text`-content frames forwarded (used by tests to
/// prove progressive relay: more than one before the terminal `Done`).
#[cfg(unix)]
fn forward_idle_stream_into_bridge(
    prefix: Vec<StreamMessage>,
    reader_rx: mpsc::Receiver<StreamMessage>,
    tx: mpsc::Sender<StreamMessage>,
) -> usize {
    let mut first_text_seen = false;
    let mut done_forwarded = false;
    let mut text_frames_forwarded = 0usize;

    let forward = |message: StreamMessage,
                   first_text_seen: &mut bool,
                   done_forwarded: &mut bool,
                   text_frames_forwarded: &mut usize|
     -> bool {
        if *done_forwarded {
            // Bridge already finalized on the terminal Done; drop trailing
            // frames (e.g. the reader's synthetic empty Done after the real
            // result Done) to avoid any double-finalize ambiguity.
            return true;
        }
        let message = match message {
            StreamMessage::Text { content } if !*first_text_seen && !content.trim().is_empty() => {
                *first_text_seen = true;
                let stripped =
                    super::response_sanitizer::strip_leading_tui_response_chrome(&content);
                StreamMessage::Text { content: stripped }
            }
            other => other,
        };
        if matches!(message, StreamMessage::Text { ref content } if !content.trim().is_empty()) {
            *text_frames_forwarded += 1;
        }
        let is_done = matches!(message, StreamMessage::Done { .. });
        if tx.send(message).is_err() {
            // Bridge receiver gone; stop forwarding.
            return false;
        }
        if is_done {
            *done_forwarded = true;
        }
        true
    };

    for message in prefix {
        if !forward(
            message,
            &mut first_text_seen,
            &mut done_forwarded,
            &mut text_frames_forwarded,
        ) {
            return text_frames_forwarded;
        }
    }
    while let Ok(message) = reader_rx.recv() {
        if !forward(
            message,
            &mut first_text_seen,
            &mut done_forwarded,
            &mut text_frames_forwarded,
        ) {
            return text_frames_forwarded;
        }
    }

    if !done_forwarded {
        let _ = tx.send(StreamMessage::Done {
            result: String::new(),
            session_id: None,
        });
    }
    text_frames_forwarded
}

#[cfg(unix)]
fn build_tui_direct_bridge_inflight_state(
    provider: ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    current_msg_id: MessageId,
    prompt_text: &str,
    tmux_session_name: &str,
    output_path: &Path,
    start_offset: u64,
    lease: &ExternalInputRelayLease,
) -> InflightTurnState {
    build_tui_direct_synthetic_inflight_state(
        provider,
        channel_id,
        user_msg_id,
        Some(current_msg_id),
        prompt_text,
        tmux_session_name,
        Some(output_path),
        start_offset,
        lease,
        RelayOwnerKind::None,
    )
}

fn build_tui_direct_synthetic_inflight_state(
    provider: ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    current_msg_id: Option<MessageId>,
    prompt_text: &str,
    tmux_session_name: &str,
    output_path: Option<&Path>,
    start_offset: u64,
    lease: &ExternalInputRelayLease,
    relay_owner_kind: RelayOwnerKind,
) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        provider,
        channel_id.get(),
        None,
        TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
        user_msg_id.get(),
        current_msg_id.map(MessageId::get).unwrap_or(0),
        prompt_text.to_string(),
        None,
        Some(tmux_session_name.to_string()),
        output_path.and_then(|path| path.to_str().map(str::to_string)),
        None,
        start_offset,
    );
    state.current_msg_len = "...".len();
    state.session_key = lease.session_key.clone();
    state.runtime_kind = lease.runtime_kind;
    state.turn_source = TurnSource::ExternalInput;
    state.set_relay_owner_kind(relay_owner_kind);
    // #3099 codex re-review (P2): pin THIS turn's injected `⏳` message id onto
    // the inflight so the `user_msg_id == 0` completion cleanup can target this
    // turn's own message instead of whatever later injection has since
    // overwritten the single shared prompt-anchor slot.
    state.injected_prompt_message_id = Some(user_msg_id.get());
    state
}

#[cfg(unix)]
#[allow(dead_code)]
fn tui_idle_tail_should_commit_runtime_binding_offset(
    response: &str,
    discord_delivery_succeeded: bool,
) -> bool {
    response.trim().is_empty() || discord_delivery_succeeded
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

/// Local-completing slash-control prompts skip synthetic turn ownership.
fn is_local_only_slash_command_prompt(prompt: &str) -> bool {
    if !is_slash_command_control_prompt(prompt) {
        return false;
    }
    let kind = slash_command_control_kind(prompt);
    super::commands::is_local_only_slash_command_kind(&kind)
        || slash_command_control_prompt_is_caveat_only(prompt)
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
