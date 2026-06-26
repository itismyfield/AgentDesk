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

// #3479 rank-5: the pure injected-prompt classification + formatting policy
// lives in a capped sibling module. Names are re-imported below so the stateful
// dedupe/bridge call sites in this parent stay byte-identical.
mod injected_prompt_policy;
use self::injected_prompt_policy::{
    InjectedPromptClass, classify_injected_prompt, format_slash_command_control_note,
    format_ssh_direct_prompt_notification, format_system_continuation_note,
    is_slash_command_control_prompt, is_start_anchored_task_notification,
    should_suppress_local_only_kind_note_after_continuation, slash_command_control_kind,
    slash_command_control_prompt_is_caveat_only,
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
    // #3099 / #3100: a compact/system continuation prologue is NOT a human request —
    // it renders as a neutral note (no ⏳/anchor/synthetic turn) but must NOT short-
    // circuit the whole relay (P1): the bridge tail below still relays its assistant
    // output. #3178: a SlashCommandControl is NO LONGER suppressed — it takes the
    // active-turn `else` block (anchor + ⏳ + synthetic inflight) so mid-/loop input
    // queues cleanly. Only SystemContinuation suppresses the lifecycle here.
    let is_system_continuation = injected_class.suppresses_user_turn_lifecycle();
    debug_assert!(
        injected_class.still_delivers_assistant_output(),
        "every injected class must still deliver assistant output via the bridge tail",
    );
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
    if is_system_continuation {
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
fn codex_ownerless_external_input_inflight_needs_rollout_repair(
    state: &InflightTurnState,
    tmux_session_name: &str,
    rollout_path: &Path,
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
    let Some(output_path) = state.output_path.as_deref() else {
        return true;
    };
    let output_path = Path::new(output_path);
    if !output_path.exists() {
        return true;
    }
    let current = std::fs::canonicalize(output_path).unwrap_or_else(|_| output_path.to_path_buf());
    let rollout =
        std::fs::canonicalize(rollout_path).unwrap_or_else(|_| rollout_path.to_path_buf());
    current != rollout
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
fn spawn_codex_idle_rollout_relay(shared: Arc<SharedData>) {
    if CODEX_IDLE_ROLLOUT_RELAY_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    super::task_supervisor::spawn_observed("codex_idle_rollout_relay", async move {
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut active_tails: HashSet<String> = HashSet::new();
        let mut next_rehydrate = tokio::time::Instant::now();

        loop {
            // #3711: direct Codex TUI tmux sessions survive dcserver restarts,
            // but their in-memory rollout binding does not.
            let now = tokio::time::Instant::now();
            if now >= next_rehydrate {
                let shared_for_rehydrate = shared.clone();
                let rehydrate_result = tokio::task::spawn_blocking(move || {
                    rehydrate_existing_codex_tui_bindings(&shared_for_rehydrate);
                })
                .await;
                if let Err(error) = rehydrate_result {
                    tracing::warn!(
                        error = %error,
                        "Codex TUI binding rehydrate task panicked or was cancelled"
                    );
                }
                next_rehydrate = now + CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL;
            }

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
                let Some(channel_id) =
                    owner_channel_for_tmux_session(&shared, &ProviderKind::Codex, &tmux_session_name)
                else {
                    // #3018/#3306/#3656: registry miss ⇒ drop; Codex repair-ineligible.
                    continue;
                };
                let binding = resolved_codex_idle_relay_binding(
                    &tmux_session_name,
                    channel_id,
                    &binding,
                )
                .unwrap_or(binding);
                let rollout_path = PathBuf::from(&binding.output_path);
                if !rollout_path.exists() {
                    tracing::debug!(
                        tmux_session_name = %tmux_session_name,
                        rollout_path = %rollout_path.display(),
                        "codex idle rollout relay skipped missing rollout path"
                    );
                    continue;
                }
                if let Some(inflight) =
                    super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id.get())
                {
                    if codex_ownerless_external_input_inflight_needs_rollout_repair(
                        &inflight,
                        &tmux_session_name,
                        &rollout_path,
                    ) {
                        match scan_codex_idle_rollout_for_latest_prompt_matching(
                            &rollout_path,
                            &inflight.user_text,
                        ) {
                            Ok(Some(CodexIdleRolloutScan::Prompt {
                                prompt,
                                line_end_offset,
                                ..
                            })) => {
                                if let Some(anchor_id) = inflight.injected_prompt_message_id {
                                    crate::services::tui_prompt_dedupe::record_prompt_anchor(
                                        ProviderKind::Codex.as_str(),
                                        &tmux_session_name,
                                        channel_id.get(),
                                        anchor_id,
                                    );
                                }
                                let observed_at = chrono::Utc::now();
                                let lease = record_external_turn_lease_for_output(
                                    &shared,
                                    &ProviderKind::Codex,
                                    channel_id,
                                    &tmux_session_name,
                                    binding.runtime_kind,
                                    &rollout_path,
                                    observed_at,
                                );
                                let mut repaired = inflight;
                                repaired.output_path =
                                    rollout_path.to_str().map(ToString::to_string);
                                repaired.last_offset = line_end_offset;
                                repaired.turn_start_offset = Some(line_end_offset);
                                repaired.session_key = lease.session_key.clone();
                                repaired.runtime_kind = lease.runtime_kind;
                                repaired.set_relay_owner_kind(RelayOwnerKind::None);
                                if let Err(error) = super::inflight::save_inflight_state(&repaired)
                                {
                                    tracing::warn!(
                                        tmux_session_name = %tmux_session_name,
                                        channel_id = channel_id.get(),
                                        rollout_path = %rollout_path.display(),
                                        error = %error,
                                        "failed to repair Codex ownerless external-input inflight with live rollout path"
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
                                    "codex_idle_response_tail_repair",
                                    provider = ProviderKind::Codex.as_str(),
                                    channel_id = channel_id.get(),
                                    tmux_session_name = %tmux_session_name,
                                    turn_id = lease.turn_id.as_deref().unwrap_or(""),
                                    session_key = lease.session_key.as_deref().unwrap_or(""),
                                    relay_owner = lease.relay_owner.as_str(),
                                    runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                                );
                                tracing::warn!(
                                    tmux_session_name = %tmux_session_name,
                                    channel_id = channel_id.get(),
                                    rollout_path = %rollout_path.display(),
                                    line_end_offset,
                                    "repaired Codex ownerless external-input inflight and resumed response tail from live rollout"
                                );
                                super::task_supervisor::spawn_observed(
                                    "codex_idle_response_tail_repair",
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
                                            prompt,
                                            tail_lease,
                                        )
                                        .await;
                                    }
                                    .instrument(tail_span),
                                );
                                continue;
                            }
                            Ok(Some(CodexIdleRolloutScan::NoPrompt { .. })) => {}
                            Ok(None) => {
                                tracing::debug!(
                                    tmux_session_name = %tmux_session_name,
                                    channel_id = channel_id.get(),
                                    rollout_path = %rollout_path.display(),
                                    "Codex ownerless external-input inflight repair skipped; prompt not found in rollout"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    tmux_session_name = %tmux_session_name,
                                    channel_id = channel_id.get(),
                                    rollout_path = %rollout_path.display(),
                                    error = %error,
                                    "Codex ownerless external-input inflight repair scan failed"
                                );
                            }
                        }
                    }
                    if !super::inflight::ownerless_external_input_inflight_is_stale(&inflight) {
                        continue;
                    }
                    tracing::debug!(
                        channel_id = channel_id.get(),
                        tmux_session_name = %tmux_session_name,
                        user_msg_id = inflight.user_msg_id,
                        updated_at = %inflight.updated_at,
                        "codex idle rollout relay ignored stale ownerless TUI-direct inflight blocker"
                    );
                }

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
                        entry_id,
                    } => {
                        let observed_at = chrono::Utc::now();
                        let observation =
                            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
                                ProviderKind::Codex.as_str(),
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
                        if wait_for_tui_direct_watcher_synthetic_claim(
                            &ProviderKind::Codex,
                            channel_id,
                            &tmux_session_name,
                        )
                        .await
                        {
                            crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                                &tmux_session_name,
                                &binding.output_path,
                                line_end_offset,
                            );
                            tracing::info!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                                session_key = lease.session_key.as_deref().unwrap_or(""),
                                "codex idle rollout relay yielded to TUI-direct synthetic watcher inflight"
                            );
                            continue;
                        }
                        if !bridge_adapter_owns_external_turn(lease.relay_owner) {
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
                                    prompt,
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

#[cfg(unix)]
async fn run_codex_idle_response_tail(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    rollout_path: PathBuf,
    start_offset: u64,
    prompt_text: String,
    lease: ExternalInputRelayLease,
) {
    let _lease_guard = TuiDirectExternalInputLeaseGuard::new(
        ProviderKind::Codex,
        &tmux_session_name,
        channel_id,
        &lease,
    );
    let (reader_tx, reader_rx) = mpsc::channel::<StreamMessage>();
    let (offset_tx, offset_rx) = tokio::sync::oneshot::channel::<Result<u64, String>>();
    let rollout_for_reader = rollout_path.clone();
    let tmux_for_reader = tmux_session_name.clone();
    std::thread::Builder::new()
        .name("codex_idle_response_tail_reader".to_string())
        .spawn(move || {
            let read_result =
                crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
                    &rollout_for_reader,
                    start_offset,
                    None,
                    reader_tx,
                    None,
                    || {
                        crate::services::tmux_diagnostics::tmux_session_has_live_pane(
                            &tmux_for_reader,
                        )
                    },
                )
                .map(|result| match result {
                    ReadOutputResult::Completed { offset }
                    | ReadOutputResult::Cancelled { offset }
                    | ReadOutputResult::SessionDied { offset } => offset,
                });
            let _ = offset_tx.send(read_result);
        })
        .expect("spawn codex idle response tail reader thread");

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
                rollout_path = %rollout_path.display(),
                error = %error,
                "codex idle rollout response tail buffering panicked"
            );
            finish_tui_direct_synthetic_turn_if_current(
                &shared,
                &ProviderKind::Codex,
                channel_id,
                &tmux_session_name,
                "codex_tui_direct_tail_panicked",
            )
            .await;
            return;
        }
    };

    if !has_content {
        let _ = tokio::task::spawn_blocking(move || while reader_rx.recv().is_ok() {}).await;
        if let Ok(Ok(final_offset)) = offset_rx.await {
            crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                &tmux_session_name,
                rollout_path.to_str().unwrap_or_default(),
                final_offset,
            );
        }
        finish_tui_direct_synthetic_turn_if_current(
            &shared,
            &ProviderKind::Codex,
            channel_id,
            &tmux_session_name,
            "codex_tui_direct_empty_response",
        )
        .await;
        return;
    }
    let delivery_result = stream_tui_idle_response_through_bridge(
        &shared,
        ProviderKind::Codex,
        channel_id,
        &tmux_session_name,
        &rollout_path,
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
            &ProviderKind::Codex,
            channel_id,
            &tmux_session_name,
            "codex_tui_direct_delivery_failed",
        )
        .await;
    }
    match offset_rx.await {
        Ok(Ok(final_offset))
            if tui_idle_tail_stream_should_commit_runtime_binding_offset(
                delivery_result.is_ok(),
            ) =>
        {
            crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                &tmux_session_name,
                rollout_path.to_str().unwrap_or_default(),
                final_offset,
            );
        }
        Ok(Err(error)) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                rollout_path = %rollout_path.display(),
                error = %error,
                "codex idle rollout response tail failed"
            );
            finish_tui_direct_synthetic_turn_if_current(
                &shared,
                &ProviderKind::Codex,
                channel_id,
                &tmux_session_name,
                "codex_tui_direct_tail_failed",
            )
            .await;
        }
        _ => {}
    }
}

#[cfg(unix)]
#[allow(dead_code)]
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
mod tests {
    use super::*;
    use crate::services::discord::gateway::TurnGateway;

    fn compact_command_name_first_stub() -> &'static str {
        "<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args></command-args>"
    }

    /// Scoped env-var override for inflight persistence tests. `AGENTDESK_ROOT_DIR`
    /// is process-global, so serialize it with the shared test env lock.
    struct EnvRootGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvRootGuard {
        fn set(path: &std::path::Path) -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    // ====================================================================
    // #3154 P2-2 (c) — the KEY residual risk: prove there is NO relay GAP
    // (not merely no duplicate) on the deferred synthetic-start path.
    //
    // The relay of a turn's output must come from EXACTLY ONE owner. The
    // deferred path has two participants:
    //   * the OBSERVER's BridgeAdapter idle-response tail, and
    //   * the deferred worker's claimed (watcher) owner.
    // If the observer skips but the worker never adopts the watcher owner, the
    // output is dropped (a GAP). If both run, it relays twice (a DUPLICATE).
    // These tests pin BOTH production decisions against the REAL lease store.
    // ====================================================================

    /// Deferred ⇒ the observer must STAND DOWN (skip its bridge tail). RED if
    /// `observer_should_spawn_bridge_tail` stops honoring `deferred`.
    #[test]
    fn deferred_observer_skips_bridge_tail() {
        // Lease still reads as BridgeAdapter (pre-claim) — the dangerous case:
        // without the deferred guard the observer WOULD spawn a second relay.
        assert!(
            !observer_should_spawn_bridge_tail(true, ExternalInputRelayOwner::BridgeAdapter),
            "deferred path: the observer must NOT spawn its own bridge tail \
             (the worker owns the relay handoff) — else DUPLICATE relay"
        );
        // Non-deferred + BridgeAdapter owner ⇒ observer relays (the normal path).
        assert!(observer_should_spawn_bridge_tail(
            false,
            ExternalInputRelayOwner::BridgeAdapter
        ));
        // Non-deferred but a watcher already owns it ⇒ observer stands down.
        assert!(!observer_should_spawn_bridge_tail(
            false,
            ExternalInputRelayOwner::TmuxWatcher
        ));
    }

    /// The no-GAP invariant end-to-end against the REAL lease store. When the
    /// synthetic start is deferred and the worker's claim resolves to the tmux
    /// WATCHER, the adoption re-records the lease as watcher-owned. We then prove
    /// EXACTLY ONE relayer remains:
    ///   (1) the observer stands down (deferred), AND
    ///   (2) a watcher relayer exists (the persisted lease is watcher-owned, so
    ///       the bridge-tail guard reads it and the watcher is the relay owner).
    /// Together: not zero (no GAP) and not two (no duplicate).
    #[test]
    fn deferred_claim_adopts_watcher_owner_exactly_one_relayer_no_gap() {
        let provider = "claude";
        let tmux = "tmux-3154-p2-2-c";
        let channel_id: u64 = 770_000_000_000_001;

        // Worker rehydrates the lease as BridgeAdapter (the persisted pre-claim
        // owner), records it, then the claim resolves to the WATCHER.
        let mut lease = ExternalInputRelayLease::unassigned(Some(channel_id));
        lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
        let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            provider, tmux, lease,
        );

        let claimed_owner = ExternalInputRelayOwner::TmuxWatcher;
        let claimed = true;

        // PRODUCTION decision: should we adopt the claimed owner? (Mirrors the
        // real inline + deferred adoption call sites.)
        assert!(
            claim_should_adopt_relay_owner(claimed, lease.relay_owner, claimed_owner),
            "a successful claim that flips the owner MUST adopt — RED if adoption \
             is skipped, which would leave a stale BridgeAdapter lease and the \
             observer/bridge tail would relay a SECOND copy"
        );

        // Perform the adoption exactly as the deferred worker does: re-record the
        // lease with the claimed owner into the REAL store.
        let mut adopted = lease.clone();
        adopted.relay_owner = claimed_owner;
        crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            provider, tmux, adopted,
        );

        // (2) The persisted lease now reads as watcher-owned: a relayer EXISTS.
        let stored = crate::services::tui_prompt_dedupe::external_input_relay_lease(
            provider, tmux, channel_id,
        )
        .expect("lease present after adoption");
        assert_eq!(
            stored.relay_owner,
            ExternalInputRelayOwner::TmuxWatcher,
            "after adoption the watcher owns the relay — a relayer EXISTS (no GAP)"
        );

        // (1) With the watcher owning the lease, the observer stands down whether
        // or not we re-check the deferred flag — so the watcher is the SOLE
        // relayer. Count relayers explicitly: observer(0) + watcher(1) == 1.
        let observer_relays = observer_should_spawn_bridge_tail(true, stored.relay_owner);
        let watcher_relays = matches!(stored.relay_owner, ExternalInputRelayOwner::TmuxWatcher);
        let relayer_count = u8::from(observer_relays) + u8::from(watcher_relays);
        assert_eq!(
            relayer_count, 1,
            "EXACTLY ONE relayer on the deferred path: not zero (no GAP) and not \
             two (no duplicate). RED if adoption is dropped (relayer_count==0, GAP) \
             or if the observer ignores `deferred` (relayer_count==2, duplicate)."
        );

        // Hygiene: clear the test lease.
        let _ = crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
            provider, tmux, channel_id,
        );
    }

    /// #3154 P1 (BridgeAdapter-GAP) — the PARALLEL no-GAP invariant for the OTHER
    /// resolved owner. When the deferred claim resolves to the BridgeAdapter (NO
    /// watcher will relay this turn), there must STILL be exactly one relayer: the
    /// worker spawns the bridge tail. We count relayers explicitly:
    ///   * observer(0) — stood down on the deferred path, AND
    ///   * watcher(0) — the resolved owner is the BridgeAdapter, not the watcher, SO
    ///   * worker bridge tail(1) — `deferred_claim_requires_bridge_tail_relayer` fires.
    ///
    /// RED before this fix: the worker never spawned a bridge tail for the
    /// BridgeAdapter owner, so observer(0) + watcher(0) + worker(0) == 0 == GAP.
    /// Neutralizing the new branch the OTHER direction (forcing the worker to spawn
    /// for the WATCHER owner) is covered by the watcher test below staying at 1.
    #[test]
    fn deferred_claim_resolves_bridge_owner_exactly_one_relayer_no_gap() {
        // Deferred ⇒ the observer stands down regardless of owner (it cannot know
        // the resolved owner pre-claim and hands the decision to the worker).
        let observer_relays =
            observer_should_spawn_bridge_tail(true, ExternalInputRelayOwner::BridgeAdapter);
        assert!(
            !observer_relays,
            "deferred path: the observer always stands down (the worker owns the \
             post-claim bridge-tail decision)"
        );

        // The claim resolved to the BridgeAdapter: the watcher will NOT relay.
        let resolved_owner = ExternalInputRelayOwner::BridgeAdapter;
        let watcher_relays = matches!(resolved_owner, ExternalInputRelayOwner::TmuxWatcher);
        assert!(!watcher_relays, "BridgeAdapter owner ⇒ no watcher relayer");

        // PRODUCTION decision: the worker MUST spawn its bridge tail for the
        // BridgeAdapter owner — this is the GAP fix.
        let worker_bridge_tail = deferred_claim_requires_bridge_tail_relayer(resolved_owner);
        assert!(
            worker_bridge_tail,
            "BridgeAdapter-owned deferred claim MUST get a worker bridge tail — \
             RED before this fix (worker spawned nothing ⇒ relayer_count == 0 == GAP)"
        );

        let relayer_count =
            u8::from(observer_relays) + u8::from(watcher_relays) + u8::from(worker_bridge_tail);
        assert_eq!(
            relayer_count, 1,
            "EXACTLY ONE relayer on the deferred BridgeAdapter path: not zero (no \
             GAP) and not two (no duplicate). RED if the worker bridge tail is \
             dropped (count == 0, GAP) or if the observer also relays (count == 2)."
        );
    }

    /// #3154 P1 (BridgeAdapter-GAP) — the symmetric guard: when the deferred claim
    /// resolves to the WATCHER, the worker must NOT spawn a bridge tail (the watcher
    /// is the sole relayer). This pins the owner-kind-awareness in the OTHER
    /// direction: neutralizing the branch so the worker spawns unconditionally would
    /// push the watcher path to relayer_count == 2 (DUPLICATE) and turn this RED.
    #[test]
    fn deferred_claim_resolves_watcher_owner_worker_bridge_tail_stands_down() {
        let resolved_owner = ExternalInputRelayOwner::TmuxWatcher;
        let observer_relays = observer_should_spawn_bridge_tail(true, resolved_owner);
        let watcher_relays = matches!(resolved_owner, ExternalInputRelayOwner::TmuxWatcher);
        let worker_bridge_tail = deferred_claim_requires_bridge_tail_relayer(resolved_owner);
        assert!(
            !worker_bridge_tail,
            "watcher-owned deferred claim MUST NOT get a worker bridge tail — else \
             DUPLICATE relay (the watcher already relays)"
        );
        let relayer_count =
            u8::from(observer_relays) + u8::from(watcher_relays) + u8::from(worker_bridge_tail);
        assert_eq!(
            relayer_count, 1,
            "EXACTLY ONE relayer on the deferred watcher path (the watcher); RED if \
             the worker also spawns a bridge tail (count == 2, DUPLICATE)."
        );
    }

    /// Adoption must NOT fire when the claim FAILED — a false claim leaves the
    /// owner untouched (the worker retries; nothing relays yet, by design).
    #[test]
    fn failed_claim_does_not_adopt_owner() {
        assert!(
            !claim_should_adopt_relay_owner(
                false,
                ExternalInputRelayOwner::BridgeAdapter,
                ExternalInputRelayOwner::TmuxWatcher,
            ),
            "a failed claim must not re-record a watcher owner (the turn was not \
             actually claimed) — RED if adoption ignores the `claimed` flag"
        );
        // No-op when the owner did not change.
        assert!(!claim_should_adopt_relay_owner(
            true,
            ExternalInputRelayOwner::TmuxWatcher,
            ExternalInputRelayOwner::TmuxWatcher,
        ));
    }

    /// #3296 (RED-3): the ABORT cleanup hook records a durable aborted-anchor
    /// marker and NO LONGER applies any reaction itself — the old #3282 path
    /// swapped `⏳ → ⚠` here, branding answered messages as failures. RED on
    /// the pre-#3296 code: no marker module/store exists and a `⚠` is added.
    /// codex r2 reverses the r1 tail: with the foreign row gone at the record
    /// instant the marker must pin the worker's LAST-VIEW identity and stay
    /// UNCOVERED unless a commit tombstone proves the deletion was a commit —
    /// RED on the r1 code (row-absence alone pre-covered the marker, false-✅
    /// ing force-cleared unanswered anchors).
    /// (Reaction-op accounting lives in `tui_direct_abort_marker`'s own tests;
    /// this hook performs Discord IO only through that module, never directly.)
    #[test]
    fn abort_cleanup_records_marker_and_keeps_hourglass() {
        // Durable BASE-root injection via the marker module's THREAD-LOCAL
        // test seam (never the process-global `AGENTDESK_ROOT_DIR` env —
        // mutating it races env-reading tests that hold no lock, e.g. the
        // pending-start worker tests' `persist()`). The current-thread
        // `block_on` below keeps the cleanup future on this thread so the
        // override resolves inside it.
        struct RootReset;
        impl Drop for RootReset {
            fn drop(&mut self) {
                super::super::tui_direct_abort_marker::set_test_root_override(None);
            }
        }
        let _root_reset = RootReset;
        let temp = tempfile::tempdir().unwrap();
        super::super::tui_direct_abort_marker::set_test_root_override(Some(
            temp.path().to_path_buf(),
        ));

        let shared = super::super::make_shared_data_for_tests();
        let record = super::super::tui_direct_pending_start::TuiDirectPendingStart {
            provider: "claude".to_string(),
            channel_id: 4242,
            tmux_session_name: "tmux-4242".to_string(),
            prompt_text: "/loop tick".to_string(),
            anchor_message_id: 777_001,
            lease_relay_owner: "bridge_adapter".to_string(),
            lease_runtime_kind: Some("claude_tui".to_string()),
            lease_turn_id: None,
            lease_session_key: None,
            generation: 0,
            created_at_ms: 0,
            observed_at_ms: 0,
            state: super::super::tui_direct_pending_start::PendingStartState::Waiting,
            attempt_count: 0,
        };
        let cleanup = pending_start_abort_cleanup_fn();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // No inflight row exists for this channel in the test env (the row
        // vanished post-final-view); the worker's last-view identity is what
        // the marker must pin (codex r2).
        let last_view = Some((888_777_u64, "2026-06-10 12:00:00".to_string()));
        rt.block_on(cleanup(&shared, &record, last_view.clone()));

        let markers = super::super::tui_direct_abort_marker::load_for_channel("claude", 4242);
        assert_eq!(
            markers.len(),
            1,
            "the ABORT hook must persist exactly one durable aborted-anchor \
             marker — RED on the old ⚠-swap path (no marker store existed)"
        );
        assert_eq!(
            markers[0].anchor_message_id, 777_001,
            "identity-pinned (I4)"
        );
        assert_eq!(
            markers[0].foreign_user_msg_id,
            Some(888_777),
            "row gone at the record instant ⇒ the LAST-VIEW identity is pinned \
             (codex r2) — RED if None (the marker would be sweep-only)"
        );
        assert_eq!(
            markers[0].covered_at_ms, None,
            "no commit tombstone ⇒ UNCOVERED (codex r2 — RED on the r1 \
             pre-covered promotion: bare row-absence is not commit evidence)"
        );

        // With a commit tombstone matching the last-view identity, the same
        // row-gone abort records COVERED (evidence-backed — the deletion WAS
        // the prior owner's terminal commit).
        super::super::tui_direct_abort_marker::record_commit_tombstone_at(
            55_000,
            "claude",
            "tmux-4242",
            4242,
            888_777,
            "2026-06-10 12:00:00",
        );
        let record_b = super::super::tui_direct_pending_start::TuiDirectPendingStart {
            anchor_message_id: 777_002,
            ..record.clone()
        };
        rt.block_on(cleanup(&shared, &record_b, last_view));
        let covered = super::super::tui_direct_abort_marker::load_for_channel("claude", 4242)
            .into_iter()
            .find(|m| m.anchor_message_id == 777_002)
            .expect("second marker recorded");
        assert_eq!(
            covered.covered_at_ms,
            Some(55_000),
            "matching tombstone at record time ⇒ evidence-backed cover (r2)"
        );

        // Zero anchor id (I5): nothing recorded, nothing panics.
        let zero = super::super::tui_direct_pending_start::TuiDirectPendingStart {
            anchor_message_id: 0,
            ..record
        };
        rt.block_on(cleanup(&shared, &zero, None));
        assert_eq!(
            super::super::tui_direct_abort_marker::load_for_channel("claude", 4242).len(),
            2,
            "a zero anchor id must never be recorded (I5)"
        );
    }

    // #3018: the tmux_watchers registry is the SINGLE authority for
    // tmux-session→channel resolution. When the registry has a mapping it wins
    // outright (the dedupe mirror is never consulted as a reverse authority).
    #[test]
    fn registry_is_authoritative_for_owner_channel_resolution() {
        let registry_channel = ChannelId::new(123_000_000_000_000);

        // Registry hit, no mirror.
        assert_eq!(
            resolve_owner_channel_authoritatively("tmux-a", Some(registry_channel), None),
            Some(registry_channel),
        );

        // Registry hit takes precedence even when the mirror disagrees.
        assert_eq!(
            resolve_owner_channel_authoritatively(
                "tmux-a",
                Some(registry_channel),
                Some(999_000_000_000_000),
            ),
            Some(registry_channel),
            "registry must win over a disagreeing dedupe mirror"
        );
    }

    // #3018: a registry miss while the dedupe mirror still holds a mapping is
    // observable drift. The resolver must NOT fall back to the mirror; it
    // returns None (the warn drift alert is emitted as a side effect).
    #[test]
    fn registry_miss_but_dedupe_hit_drops_and_does_not_use_mirror() {
        assert_eq!(
            resolve_owner_channel_authoritatively("tmux-drift", None, Some(456_000_000_000_000),),
            None,
            "dedupe mirror must never act as a reverse routing authority"
        );

        // Both miss → None.
        assert_eq!(
            resolve_owner_channel_authoritatively("tmux-empty", None, None),
            None,
        );
    }

    // #3656: the owner-resolution chokepoint must arm repair on the first drift drop.
    #[cfg(unix)]
    #[tokio::test]
    async fn owner_channel_chokepoint_triggers_drift_repair_on_drift_drop() {
        let shared = super::super::make_shared_data_for_tests();
        let tmux = "AgentDesk-claude-drift-chokepoint-fix3656";
        let owner = ChannelId::new(1_504_468_805_772_903_656);

        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux, owner.get());
        assert_eq!(
            owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
            None,
            "registry miss + mirror hit still drops rather than routing from the mirror"
        );
        assert!(
            super::super::idle_relay_drift::repair_attempt_recorded_for_tests(tmux),
            "the chokepoint must arm a repair attempt on the first drift drop"
        );

        crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(tmux);
    }

    // #3105: a LIVE TUI session where the dedupe mirror holds a channel but the
    // `tmux_watchers` registry is missing must NOT be permanently dropped. The
    // fix self-heals by promoting the authoritative (settings-derived) channel
    // into the registry — NOT by routing from the mirror. This end-to-end relay
    // test asserts: (1) before repair the resolver drops (registry single
    // authority); (2) the dedupe mirror alone is never used as the routing
    // owner; (3) after an authoritative registry restore the relay routes again.
    #[test]
    fn live_session_relay_self_heals_via_authoritative_registry_not_mirror() {
        let shared = super::super::make_shared_data_for_tests();
        let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
        let owner = ChannelId::new(1_504_468_805_772_902_471);

        // The dedupe mirror has a mapping (live TUI session), but the
        // authoritative registry misses (slot evicted by compact/restart/rebind).
        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux, owner.get());
        assert_eq!(
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux),
            Some(owner.get()),
            "precondition: dedupe mirror holds the live session's channel"
        );

        // (1)+(2): the mirror alone must never be used as the delivery owner —
        // the resolver drops (the #3018 single-authority rule stays intact).
        assert_eq!(
            owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
            None,
            "registry miss + dedupe mirror hit must drop, never route from the mirror"
        );

        // (3): an authoritative registry restore (what the rehydrate loop does
        // from the settings-derived channel) makes the live session route again.
        let repaired = shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(tmux, owner);
        assert!(
            repaired,
            "first restore reports a change (single bounded incident)"
        );
        assert_eq!(
            owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
            Some(owner),
            "after authoritative re-registration the live session must route again"
        );

        // Cleanup shared global dedupe state for cross-test isolation.
        crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(tmux);
    }

    // #3306: a drift-triggered self-heal (the new path for ROUTINE sessions that
    // have NO settings binding) must promote a durable channel via the SAME
    // authoritative `restore_owner_channel_for_tmux_session` registry path the
    // #3105 rehydrate uses — proving the registry stays the single authority and
    // the resolver routes again after the drift WARN drop. The decision-core
    // tests (`idle_relay_drift`) prove WHICH durable source is chosen and the
    // mis-delivery guards; this end-to-end test pins the registry promotion +
    // resolver hand-off.
    //
    // #3356: gated to unix to match the `test_watcher_handle` helper (def at
    // `#[cfg(unix)]` below) it consumes — every other caller of that helper is
    // already `#[cfg(unix)]`, so this restores def/usage cfg symmetry and lets
    // the windows leg compile (the helper does not exist on windows).
    #[cfg(unix)]
    #[test]
    fn drift_triggered_restore_makes_routine_session_route_again() {
        let shared = super::super::make_shared_data_for_tests();
        // A routine tmux name that matches no settings channel binding (the
        // exact class that drifts permanently before #3306).
        let tmux = "AgentDesk-claude-routine-token-daily-report---token-manager";
        let owner = ChannelId::new(1_512_635_194_124_013_681);

        // Drift precondition: mirror holds a mapping, registry misses ⇒ drop.
        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux, owner.get());
        assert_eq!(
            owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
            None,
            "registry miss + mirror hit must drop (drift), never route from mirror"
        );

        // The drift repair (durable source promotion) re-registers the owner via
        // the authoritative restore path — exactly what `attempt_drift_repair`
        // does on a passing `RepairDecision::Promote`.
        let repaired = shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(tmux, owner);
        assert!(repaired, "first drift-triggered restore reports a change");
        assert_eq!(
            owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
            Some(owner),
            "after the drift-triggered authoritative restore the session routes again"
        );

        // Live truth wins: a real watcher claim for the session must own it
        // authoritatively, so a subsequent restore no-ops (the restored entry
        // can never shadow a live watcher).
        shared.tmux_watchers.insert(
            owner,
            test_watcher_handle(tmux, Path::new("/tmp/nope.jsonl")),
        );
        assert!(
            !shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(tmux, owner),
            "restore must no-op while a live watcher owns the session (live truth wins)"
        );

        // Cleanup shared global state for cross-test isolation.
        shared.tmux_watchers.remove(&owner);
        crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(tmux);
    }

    // #3105 (codex P1 sub-case B): a DEAD/orphaned tmux session (pane gone, not
    // present on this host) whose dedupe mirror still holds a stale ClaudeTui
    // runtime binding + channel mapping must NOT spam the per-poll drift/skip
    // WARN forever. After the rehydrate pass evicts the mirror, the next idle
    // relay iteration finds NO runtime binding to iterate and NO channel mapping
    // to drift on — proving the 0.5s spam is stopped. A unique, never-created
    // session name guarantees `tmux_session_has_live_pane` is false.
    #[cfg(unix)]
    #[test]
    fn dead_orphaned_session_mirror_is_evicted_and_stops_drift_spam() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let shared = super::super::make_shared_data_for_tests();
        // A session that does not exist on this host (pane gone / orphaned).
        let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471-DEAD-ORPHAN-fix3105";
        let owner = 1_504_468_805_772_902_471u64;

        // Seed the stale dedupe mirror exactly as a dead/orphaned session leaves it:
        // a ClaudeTui runtime binding (what the relay loop iterates) and a
        // last-seen channel mapping (what the drift-alert resolver reads).
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/claude-transcript-dead-orphan.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux, owner);

        // Preconditions: the relay loop WOULD iterate this binding and drift.
        assert!(
            crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
                RuntimeHandoffKind::ClaudeTui
            )
            .iter()
            .any(|(name, _)| name == tmux),
            "precondition: dead session's binding is in the relay loop's iteration set"
        );
        assert_eq!(
            owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
            None,
            "precondition: registry misses + mirror hit == the drift the relay loop hits"
        );
        // The session is genuinely dead/orphaned (no live pane, no live watcher).
        assert!(
            claude_tui_session_is_dead_orphaned(&shared, tmux),
            "precondition: a never-created session is dead/orphaned"
        );

        // The rehydrate pass runs the eviction. (rehydrate_existing_claude_tui_bindings
        // calls evict_dead_orphaned_claude_tui_mirrors first; we call it directly
        // so the assertion does not depend on a live tmux binary for list-sessions.)
        evict_dead_orphaned_claude_tui_mirrors(&shared);

        // After eviction: the relay loop iterates an EMPTY set for this session,
        // and the drift-alert resolver finds NO mapping → no drift/skip WARN.
        assert!(
            !crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
                RuntimeHandoffKind::ClaudeTui
            )
            .iter()
            .any(|(name, _)| name == tmux),
            "the stale runtime binding must be evicted so the relay loop no longer iterates it"
        );
        assert_eq!(
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux),
            None,
            "the stale channel mirror must be evicted so no drift WARN can fire"
        );

        // Idempotent on the next pass (the ~0.5s repeat): no binding, no work,
        // no second incident — proving the spam is bounded to one line.
        evict_dead_orphaned_claude_tui_mirrors(&shared);
        assert!(
            !crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
                RuntimeHandoffKind::ClaudeTui
            )
            .iter()
            .any(|(name, _)| name == tmux),
            "subsequent iterations stay clean (0.5s spam stopped)"
        );
    }

    // #3105 (codex P1 sub-case A guard): a LIVE thread-suffixed session whose
    // authoritative registry entry was evicted must NOT be treated as
    // dead/orphaned — its mirror must survive so the live self-heal path can
    // re-register the authoritative owner. We assert the dead-orphaned predicate
    // is gated on pane-liveness: with a live watcher handle present the predicate
    // is false even though the registry owner map is otherwise empty.
    #[cfg(unix)]
    #[test]
    fn live_session_with_watcher_handle_is_not_dead_orphaned() {
        let shared = super::super::make_shared_data_for_tests();
        let tmux = "AgentDesk-claude-adk-cc-LIVE-fix3105";
        let owner = ChannelId::new(1_504_468_805_772_902_471);

        // A live watcher handle owns the session → it is NOT dead/orphaned even
        // though the host has no real tmux pane for this synthetic name.
        let dir = std::env::temp_dir();
        let output_path = dir.join("claude-live-fix3105.jsonl");
        shared
            .tmux_watchers
            .insert(owner, test_watcher_handle(tmux, &output_path));
        assert!(
            shared.tmux_watchers.has_live_watcher_handle(tmux),
            "precondition: a live watcher handle owns the session"
        );
        assert!(
            !claude_tui_session_is_dead_orphaned(&shared, tmux),
            "a session with a live watcher handle must never be tombstoned as dead/orphaned"
        );
    }

    // #3105 (codex P2): a LIVE session with no watcher handle whose FIRST pane
    // probe flakes (reads not-live) but whose subsequent probes report live must
    // NOT be classified dead/orphaned. A single transient negative read can never
    // trigger the destructive eviction, so the live session keeps its mirror and
    // self-heal path. We drive the pure predicate with a scripted probe sequence
    // [false, true] so the flake is deterministic (no real tmux needed).
    #[cfg(unix)]
    #[test]
    fn transient_pane_flake_on_live_session_is_not_dead_orphaned() {
        use std::cell::RefCell;

        // First probe flakes (not live), second probe reports live.
        let live_reads = RefCell::new(vec![false, true].into_iter());
        let is_dead = pane_is_confirmed_dead_orphaned(
            || live_reads.borrow_mut().next().unwrap_or(true),
            // session_exists must NOT even be consulted once a live pane is seen.
            || panic!("session_exists must not be probed once a live pane is observed"),
            DEAD_ORPHANED_PANE_PROBE_SAMPLES,
            None,
        );
        assert!(
            !is_dead,
            "a single flaky negative pane read followed by a live read must NOT be dead/orphaned"
        );
    }

    // #3105 (codex P2 / sub-case B regression): a genuinely-gone session reads no
    // live pane on EVERY sample AND the hard has-session check confirms it does
    // not exist → it is still classified dead/orphaned, so the per-poll WARN spam
    // is still stopped. The retries must not make the real dead session immortal.
    #[cfg(unix)]
    #[test]
    fn genuinely_gone_session_is_still_dead_orphaned_after_retries() {
        use std::cell::Cell;

        let probe_count = Cell::new(0usize);
        let is_dead = pane_is_confirmed_dead_orphaned(
            || {
                probe_count.set(probe_count.get() + 1);
                false // never a live pane
            },
            || false, // hard has-session: session truly gone
            DEAD_ORPHANED_PANE_PROBE_SAMPLES,
            None,
        );
        assert!(
            is_dead,
            "a session with no live pane across all samples AND no has-session must still evict"
        );
        assert_eq!(
            probe_count.get(),
            DEAD_ORPHANED_PANE_PROBE_SAMPLES,
            "all configured samples must be taken before declaring a session dead"
        );
    }

    // #3105 (codex P2): the weakest-signal guard. Even when every soft pane probe
    // reports dead, if the hard `tmux has-session` check still finds the session
    // present on this host (a transient pane read with the session very much
    // alive), it must NOT be evicted — "no live pane" alone is never sufficient
    // when there is no watcher handle.
    #[cfg(unix)]
    #[test]
    fn confirmed_existing_session_is_not_dead_even_if_pane_probes_flake() {
        let is_dead = pane_is_confirmed_dead_orphaned(
            || false, // soft pane probe: reads dead on every sample
            || true,  // hard has-session: the session IS present on this host
            DEAD_ORPHANED_PANE_PROBE_SAMPLES,
            None,
        );
        assert!(
            !is_dead,
            "a session still present per has-session must not be evicted on soft pane reads alone"
        );
    }

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

    // #3100: a human typing directly into the TUI is a real active turn.
    #[test]
    fn classify_injected_prompt_human_direct_input() {
        assert_eq!(
            classify_injected_prompt("please review PR #1234"),
            InjectedPromptClass::HumanTuiDirect,
        );
        assert!(classify_injected_prompt("hi").is_human_active_turn());
    }

    // #3099: a `<task-notification>` auto-turn is a real provider turn (it earns
    // a `⏳`) but is not human-driven; its completion cleanup is anchored on the
    // injected message id, so it is classified distinctly from human input.
    #[test]
    fn classify_injected_prompt_task_notification_event() {
        assert_eq!(
            classify_injected_prompt(
                "<task-notification><status>completed</status><task_id>codex-background-event</task_id></task-notification>"
            ),
            InjectedPromptClass::TaskNotificationEvent,
        );
        // Tolerates a leading terminal-control prefix some injectors prepend.
        assert_eq!(
            classify_injected_prompt(
                "\u{1b}[0m<task-notification><status>completed</status></task-notification>"
            ),
            InjectedPromptClass::TaskNotificationEvent,
        );
        // An attribute-form opening tag is still recognised.
        assert_eq!(
            classify_injected_prompt("<task-notification kind=\"background\"></task-notification>"),
            InjectedPromptClass::TaskNotificationEvent,
        );
        assert!(
            !classify_injected_prompt(
                "<task-notification><status>completed</status></task-notification>"
            )
            .is_human_active_turn()
        );
    }

    // #3393 finding 2: the live-panel terminal BRIDGE is gated on a START-ANCHORED
    // check, distinct from the contains-based CARD classifier. A human direct
    // prompt that QUOTES a notification (embedding a LIVE tool-use-id) still earns
    // its card but must NOT push terminal StatusEvents — so a quoted id cannot
    // false-close a real running slot. A bare real-shape record (incl. a leading
    // injection-wrapper round-trip) still bridges.
    #[test]
    fn bridge_guard_is_start_anchored_not_contains() {
        // Human prompt quoting a notification mid-message, with a LIVE tool-use-id
        // that matches a real running slot: NOT start-anchored → no bridge.
        let quoted = "please re-run this, it printed:\n\
            <task-notification><tool-use-id>toolu_live_slot</tool-use-id>\
            <status>completed</status>\
            <summary>Agent \"x\" completed</summary></task-notification>";
        assert!(
            !is_start_anchored_task_notification(quoted),
            "a mid-message quoted notification must NOT pass the bridge guard"
        );
        // …yet the contains-based classifier still routes it to the CARD (the
        // card behavior is preserved; only the terminal bridge is suppressed).
        assert_eq!(
            classify_injected_prompt(quoted),
            InjectedPromptClass::TaskNotificationEvent,
        );

        // A bare, start-anchored real-shape record bridges.
        let bare = "<task-notification><tool-use-id>toolu_live_slot</tool-use-id>\
            <status>completed</status>\
            <summary>Agent \"x\" completed</summary></task-notification>";
        assert!(
            is_start_anchored_task_notification(bare),
            "a bare start-anchored notification must pass the bridge guard"
        );

        // Leading terminal-control prefix is tolerated (stripped by the pipeline).
        let ansi_prefixed = "\u{1b}[0m<task-notification><status>completed</status>\
            </task-notification>";
        assert!(
            is_start_anchored_task_notification(ansi_prefixed),
            "an ANSI-prefixed notification must still pass the bridge guard"
        );

        // SSH-direct injection-wrapper round-trip variant: the wrapper line + code
        // fence are peeled by `strip_leading_injection_wrapper`, leaving the tag
        // start-anchored → still bridges (mirrors the #3153 wrapper coverage).
        let wrapped = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\
            <task-notification><tool-use-id>toolu_live_slot</tool-use-id>\
            <status>completed</status>\
            <summary>Agent \"x\" completed</summary></task-notification>\n```";
        assert!(
            is_start_anchored_task_notification(wrapped),
            "an injection-wrapper round-trip notification must pass the bridge guard"
        );
    }

    // #3100: a compact/system continuation prologue is NOT a human request and
    // must classify away from the active-turn lifecycle.
    #[test]
    fn classify_injected_prompt_system_continuation() {
        assert_eq!(
            classify_injected_prompt(
                "This session is being continued from a previous conversation that ran out of context... Summary:"
            ),
            InjectedPromptClass::SystemContinuation,
        );
        assert_eq!(
            classify_injected_prompt("Please continue the conversation from where we left it off"),
            InjectedPromptClass::SystemContinuation,
        );
        assert!(
            !classify_injected_prompt(
                "This session is being continued from a previous conversation"
            )
            .is_human_active_turn()
        );
    }

    // #3153: a MACHINE slash-command control echo must classify as
    // SlashCommandControl — the raw `/loop …` ScheduleWakeup echo (HALF A of the
    // double-post), the Claude Code expanded `<command-*>` wrapper (HALF B), the
    // raw `/compact` echo (whole-token, incl. bare no-arg), and the
    // `<local-command-stdout>Compacted` stdout line. #3178 (codex fix): a machine
    // slash turn is now a FULL active turn (NOT suppressed) so concurrent input
    // queues; it gets a kind-only anchor + ⏳ + synthetic inflight + ✅, and the
    // near-simultaneous duplicate half is collapsed by the 2s dedupe gate.
    #[test]
    fn classify_injected_prompt_slash_command_control() {
        // HALF A — raw /loop echo.
        assert_eq!(
            classify_injected_prompt("/loop 5m /foo"),
            InjectedPromptClass::SlashCommandControl,
        );
        // HALF B — Claude Code expanded <command-*> wrapper.
        let wrapper = "<command-message>loop is running…</command-message>\
                       <command-name>/loop</command-name><command-args>5m /foo</command-args>";
        assert_eq!(
            classify_injected_prompt(wrapper),
            InjectedPromptClass::SlashCommandControl,
        );
        // Raw /compact echo with args.
        assert_eq!(
            classify_injected_prompt("/compact focus on the relay"),
            InjectedPromptClass::SlashCommandControl,
        );
        // Bare no-arg /compact (whole-token, EOS).
        assert_eq!(
            classify_injected_prompt("/compact"),
            InjectedPromptClass::SlashCommandControl,
        );
        // /compact command stdout line.
        assert_eq!(
            classify_injected_prompt("<local-command-stdout>Compacted (12.3k tokens)"),
            InjectedPromptClass::SlashCommandControl,
        );
        let command_name_first = compact_command_name_first_stub();
        assert_eq!(command_name_first.chars().count(), 134);
        assert_eq!(
            classify_injected_prompt(command_name_first),
            InjectedPromptClass::SlashCommandControl,
        );
        assert_eq!(slash_command_control_kind(command_name_first), "/compact");
        assert!(is_local_only_slash_command_prompt(command_name_first));

        // #3178 (codex fix): a machine slash turn is a FULL active turn — NOT
        // suppressed from the user-turn lifecycle (so concurrent input queues),
        // yet it is not a HUMAN active turn (no raw render), and it still delivers
        // assistant output via the bridge tail.
        let ctrl = InjectedPromptClass::SlashCommandControl;
        assert!(
            !ctrl.suppresses_user_turn_lifecycle(),
            "a machine slash turn must NOT suppress the active-turn lifecycle (it claims a full active turn so concurrent input queues)"
        );
        assert!(!ctrl.is_human_active_turn());
        assert!(ctrl.still_delivers_assistant_output());
    }

    // #3153 double-echo + envelope coverage: a /loop or wrapper echo that
    // round-trips through the SSH-direct injection envelope is still anchored
    // (strip_leading_injection_wrapper peels the leading wrapper before the
    // starts_with anchors) and still classifies as SlashCommandControl.
    #[test]
    fn classify_injected_prompt_wrapped_slash_command_control() {
        let wrapped_loop = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n/loop 5m /foo\n```";
        assert_eq!(
            classify_injected_prompt(wrapped_loop),
            InjectedPromptClass::SlashCommandControl,
        );
        let wrapped_wrapper = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\
                               <command-message>/loop</command-message>\n```";
        assert_eq!(
            classify_injected_prompt(wrapped_wrapper),
            InjectedPromptClass::SlashCommandControl,
        );
        let caveat_wrapped = format!(
            "<local-command-caveat>local commands are synthetic</local-command-caveat>\n{}",
            compact_command_name_first_stub(),
        );
        assert_eq!(
            classify_injected_prompt(&caveat_wrapped),
            InjectedPromptClass::SlashCommandControl,
        );
        assert_eq!(slash_command_control_kind(&caveat_wrapped), "/compact");
        assert!(is_local_only_slash_command_prompt(&caveat_wrapped));

        let caveat_only =
            "<local-command-caveat>local commands are synthetic</local-command-caveat>";
        assert_eq!(
            classify_injected_prompt(caveat_only),
            InjectedPromptClass::SlashCommandControl,
        );
        assert_eq!(slash_command_control_kind(caveat_only), "slash");
        assert!(is_local_only_slash_command_prompt(caveat_only));
    }

    // #3153 FALSE-POSITIVE GUARD: a human merely quoting "/loop" / "/compact"
    // mid-message must NOT be misclassified — detection is START-ANCHORED, and
    // "/compactX" (no whole-token boundary) must also stay a human turn.
    #[test]
    fn classify_injected_prompt_human_quote_of_slash_is_not_control() {
        let human = "Why does /loop keep appearing in my logs?";
        assert_eq!(
            classify_injected_prompt(human),
            InjectedPromptClass::HumanTuiDirect,
            "a human quoting /loop mid-message must stay a human turn",
        );
        assert!(classify_injected_prompt(human).is_human_active_turn());

        // "/compactfoo" is not the whole `/compact` token → human turn.
        assert_eq!(
            classify_injected_prompt("/compactfoo do the thing"),
            InjectedPromptClass::HumanTuiDirect,
        );

        // A human leading line that merely opens with the wrapped envelope but
        // whose body is a plain request stays a human turn.
        let wrapped_human =
            "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\nplease /loop later maybe\n```";
        assert_eq!(
            classify_injected_prompt(wrapped_human),
            InjectedPromptClass::HumanTuiDirect,
        );
        let quoted_command_name =
            "Why did the transcript include <command-name>/compact</command-name>?";
        assert_eq!(
            classify_injected_prompt(quoted_command_name),
            InjectedPromptClass::HumanTuiDirect,
        );
        let quoted_caveat =
            "The log contains <local-command-caveat>x</local-command-caveat> before XML.";
        assert_eq!(
            classify_injected_prompt(quoted_caveat),
            InjectedPromptClass::HumanTuiDirect,
        );
    }

    // #3305: a local-completing pass-through command's `<command-*>` echo (or its
    // raw `/compact` echo) must be detected as local-only so the idle relay skips
    // the synthetic-turn lifecycle (no ⏳ anchor, no inflight) while still posting
    // the kind-only guidance note. Covers all four pass-throughs across the
    // wrapper, the SSH-direct envelope round-trip, the leading terminal-control
    // prefix, the `/compact` raw echo, and the `Compacted` stdout.
    #[test]
    fn local_only_slash_prompt_detects_passthrough_command_xml() {
        for name in ["/effort", "/compact", "/cost", "/context"] {
            let wrapper = format!(
                "<command-message>{name} is running…</command-message>\n\
                 <command-name>{name}</command-name>\n<command-args></command-args>"
            );
            assert!(
                is_local_only_slash_command_prompt(&wrapper),
                "expanded wrapper for {name} must be local-only",
            );
            // SSH-direct envelope round-trip of the wrapper.
            let wrapped =
                format!("터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n{wrapper}\n```");
            assert!(
                is_local_only_slash_command_prompt(&wrapped),
                "envelope-wrapped {name} wrapper must still be local-only",
            );
            // Leading terminal-control prefix before the wrapper.
            let with_controls = format!("\u{1b}[2K\r{wrapper}");
            assert!(
                is_local_only_slash_command_prompt(&with_controls),
                "terminal-control-prefixed {name} wrapper must still be local-only",
            );
        }
        // `<command-name>` with an argument body — the first token (`/effort`) is
        // the canonical kind, so it still matches the allow-list.
        let effort_with_args =
            "<command-message>x</command-message>\n<command-name>/effort high</command-name>";
        assert!(is_local_only_slash_command_prompt(effort_with_args));
        // Raw `/compact` echo (no wrapper) and the `Compacted` stdout line.
        assert!(is_local_only_slash_command_prompt("/compact"));
        assert!(is_local_only_slash_command_prompt(
            "/compact focus on the relay"
        ));
        assert!(is_local_only_slash_command_prompt(
            "<local-command-stdout>Compacted (12.3k tokens)"
        ));
    }

    // #3305 (REQUIRED REGRESSION GUARD): `/loop` STARTS a real model turn, so it
    // must remain a full active turn (#3178) — classified as SlashCommandControl
    // yet NOT local-only. This double-assertion pins /loop on the anchor+⏳+
    // synthetic-inflight path at the classification level so the local-only skip
    // can never over-suppress it.
    #[test]
    fn local_only_slash_prompt_preserves_loop_wakeup_lifecycle() {
        let raw = "/loop 5m /foo";
        let wrapper = "<command-message>loop is running…</command-message>\n\
                       <command-name>/loop</command-name>\n<command-args>5m /foo</command-args>";
        for form in [raw, wrapper] {
            assert_eq!(
                classify_injected_prompt(form),
                InjectedPromptClass::SlashCommandControl,
                "/loop stays a SlashCommandControl active turn",
            );
            assert!(
                !is_local_only_slash_command_prompt(form),
                "/loop must NOT be local-only (it starts a model turn) — over-suppression guard",
            );
        }
    }

    // #3305: non-command text, the system-continuation banner, task notifications,
    // a token-boundary near-miss, and an UNLISTED command must all be rejected so
    // the local-only skip never fires for a real turn (fail-safe = lifecycle kept).
    #[test]
    fn local_only_slash_prompt_rejects_non_command_text() {
        // A plain human prompt.
        assert!(!is_local_only_slash_command_prompt(
            "please summarize the relay design"
        ));
        // SystemContinuation banner wins (handled by its own neutral-note path).
        assert!(!is_local_only_slash_command_prompt(
            "This session is being continued from a previous conversation…"
        ));
        // Task-notification tag is not a slash control echo.
        assert!(!is_local_only_slash_command_prompt(
            "<task-notification>done</task-notification>"
        ));
        // Token-boundary near-miss: `/compactX` is not the whole `/compact` token.
        assert!(!is_local_only_slash_command_prompt(
            "/compactX do the thing"
        ));
        // An UNLISTED command's wrapper — `/loop` is a SlashCommandControl but is
        // NOT on the allow-list (it starts a model turn), so lifecycle is preserved
        // (fail-safe default).
        let loop_wrapper =
            "<command-message>x</command-message>\n<command-name>/loop</command-name>";
        assert!(matches!(
            classify_injected_prompt(loop_wrapper),
            InjectedPromptClass::SlashCommandControl
        ));
        assert!(!is_local_only_slash_command_prompt(loop_wrapper));
        // #3500: `/model` IS a SlashCommandControl AND local-only (Claude-native,
        // changes the model with no model turn) — lifecycle is SKIPPED so it does
        // not strand a synthetic inflight that queues the next real message.
        let model_wrapper =
            "<command-message>x</command-message>\n<command-name>/model</command-name>";
        assert!(matches!(
            classify_injected_prompt(model_wrapper),
            InjectedPromptClass::SlashCommandControl
        ));
        assert!(is_local_only_slash_command_prompt(model_wrapper));
    }

    // #3178: the machine slash-command control trigger now resolves to a stable
    // command KIND that BOTH the raw `/loop` echo and the expanded `<command-*>`
    // wrapper for the SAME command map to (so the #3153 double-post collapses to
    // one dedupe entry), and the note shows ONLY that kind — never the raw body.
    #[test]
    fn slash_command_control_kind_is_stable_across_double_post_halves() {
        // HALF A (raw echo) and HALF B (expanded wrapper) for /loop share a kind.
        assert_eq!(slash_command_control_kind("/loop 5m /foo"), "/loop");
        let wrapper = "<command-message>loop is running…</command-message>\
                       <command-name>/loop</command-name><command-args>5m /foo</command-args>";
        assert_eq!(slash_command_control_kind(wrapper), "/loop");

        // /compact forms (raw echo, bare no-arg, Compacted stdout) share a kind.
        assert_eq!(
            slash_command_control_kind("/compact focus on the relay"),
            "/compact",
        );
        assert_eq!(slash_command_control_kind("/compact"), "/compact");
        assert_eq!(
            slash_command_control_kind("<local-command-stdout>Compacted (12.3k tokens)"),
            "/compact",
        );

        // A round-tripped SSH-direct envelope still resolves to the same kind.
        let wrapped_loop = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n/loop 5m /foo\n```";
        assert_eq!(slash_command_control_kind(wrapped_loop), "/loop");
    }

    // The note always names the command KIND + tmux session and marks the
    // injection non-active. `/loop` ALSO carries its directive body (operator
    // wants the recurring loop content visible). Every OTHER machine command
    // (`/compact`, the `Compacted …` stdout) stays kind-only and never leaks its
    // payload.
    #[test]
    fn slash_command_control_note_loop_shows_body_others_kind_only() {
        let loop_note = format_slash_command_control_note(
            "sess-a",
            "/loop",
            "/loop 290s relay check directive",
        );
        assert!(
            loop_note.contains("/loop"),
            "note must name the command kind"
        );
        assert!(loop_note.contains("sess-a"));
        assert!(loop_note.contains("활성 턴 아님"), "must mark non-active");
        assert!(
            loop_note.contains("290s relay check directive"),
            "the /loop note MUST carry the directive body",
        );

        // The expanded wrapper half exposes only the <command-args> block, never
        // the trailing skill markdown the wrapper appends.
        let wrapped = format_slash_command_control_note(
            "sess-a",
            "/loop",
            "<command-name>/loop</command-name>\n<command-args>watch the relay</command-args>\n# /loop — schedule\nSKILL BODY LEAK",
        );
        assert!(
            wrapped.contains("watch the relay"),
            "the /loop note MUST carry the wrapped directive body",
        );
        assert!(
            !wrapped.contains("SKILL BODY LEAK"),
            "the /loop note must NOT leak the trailing skill markdown",
        );

        // An UNTERMINATED wrapper (no closing </command-args>) must NOT spill the
        // trailing skill markdown — the closing tag is required, so it falls back
        // to kind-only rather than rendering the whole tail.
        let unterminated = format_slash_command_control_note(
            "sess-a",
            "/loop",
            "<command-name>/loop</command-name>\n<command-args>watch the relay\n# /loop — schedule\nSKILL BODY LEAK",
        );
        assert!(
            !unterminated.contains("SKILL BODY LEAK"),
            "unterminated wrapper must NOT leak the trailing skill markdown",
        );
        assert!(
            !unterminated.contains("```"),
            "unterminated wrapper falls back to the kind-only header",
        );

        // A bodyless /loop gracefully degrades to the kind-only header.
        let bare = format_slash_command_control_note("sess-a", "/loop", "/loop");
        assert!(bare.contains("/loop") && bare.contains("활성 턴 아님"));
        assert!(!bare.contains("```"), "bodyless /loop has no preview block");

        let compact_note = format_slash_command_control_note(
            "sess-a",
            "/compact",
            "<local-command-stdout>Compacted 12 messages</local-command-stdout>",
        );
        assert!(compact_note.contains("/compact"));
        assert!(
            !compact_note.contains("Compacted"),
            "note must NOT leak the compact stdout body",
        );
    }

    // #3178 CORE (codex fix): the same trigger (a /loop double-post: raw echo +
    // expanded wrapper, both mapping to kind "/loop" for the same tmux session)
    // creates the active turn exactly ONCE — the first sighting proceeds, the
    // immediate second (within the 2s window) is dropped BEFORE any lease/anchor.
    // A DIFFERENT command kind (/loop vs /compact) in the same session is NOT
    // collapsed (the kind is the real command name), and a different session is
    // never deduped.
    #[test]
    fn slash_command_control_turn_dedupes_double_post_but_not_distinct_commands() {
        // Unique session names so this test cannot collide with the shared
        // process-global dedupe map across parallel test runs.
        let sess = format!("dedupe-sess-{:p}", &0u8 as *const u8);

        // HALF A — first sighting proceeds to claim the active turn.
        assert!(slash_command_control_turn_is_first_sighting(&sess, "/loop"));
        // HALF B — same (session, kind) within the 2s window → NOT a first
        // sighting (dropped before any lease/anchor; the first turn is preserved).
        assert!(!slash_command_control_turn_is_first_sighting(
            &sess, "/loop"
        ));
        // And again — still deduped.
        assert!(!slash_command_control_turn_is_first_sighting(
            &sess, "/loop"
        ));

        // A DIFFERENT command kind in the same session is a DISTINCT turn —
        // /compact must NOT collapse into the in-window /loop entry.
        assert!(slash_command_control_turn_is_first_sighting(
            &sess, "/compact"
        ));
        // But its own repeat within the window is deduped.
        assert!(!slash_command_control_turn_is_first_sighting(
            &sess, "/compact"
        ));

        // Two DIFFERENT unknown commands in the same session within the window
        // are distinct turns (no single "slash" collapse) — the codex P2 fix.
        assert!(slash_command_control_turn_is_first_sighting(&sess, "/foo"));
        assert!(slash_command_control_turn_is_first_sighting(&sess, "/bar"));

        // A DIFFERENT session with the same kind is a distinct turn.
        let other = format!("{sess}-other");
        assert!(slash_command_control_turn_is_first_sighting(
            &other, "/loop"
        ));
    }

    #[test]
    fn compact_replay_kind_note_suppression_is_session_scoped_and_expires() {
        let now = std::time::Instant::now();
        let recent = now - Duration::from_secs(29);
        let expired = now - Duration::from_secs(31);

        assert!(should_suppress_local_only_kind_note_after_continuation(
            "/compact",
            Some(recent),
            now,
        ));
        assert!(should_suppress_local_only_kind_note_after_continuation(
            "slash",
            Some(recent),
            now,
        ));
        assert!(
            !should_suppress_local_only_kind_note_after_continuation("/compact", None, now),
            "a different session with no continuation timestamp must not suppress",
        );
        assert!(!should_suppress_local_only_kind_note_after_continuation(
            "/compact",
            Some(expired),
            now,
        ));
        assert!(!should_suppress_local_only_kind_note_after_continuation(
            "/cost",
            Some(recent),
            now,
        ));
    }

    // #3178 (codex P2 fix): the kind is the REAL command name, so two distinct
    // unknown `<command-message>` wrappers do NOT collapse into a single "slash"
    // kind (which would wrongly dedupe genuinely different commands).
    #[test]
    fn slash_command_control_kind_distinguishes_distinct_unknown_commands() {
        let foo = "<command-message>foo running</command-message>\
                   <command-name>/foo</command-name>";
        let bar = "<command-message>bar running</command-message>\
                   <command-name>/bar</command-name>";
        assert_eq!(slash_command_control_kind(foo), "/foo");
        assert_eq!(slash_command_control_kind(bar), "/bar");
        assert_ne!(
            slash_command_control_kind(foo),
            slash_command_control_kind(bar),
            "distinct unknown commands must NOT collapse to one kind"
        );
    }

    // #3153 regression guard: the compact CONTINUATION banner must STILL classify
    // as SystemContinuation (precedence — the continuation check runs before the
    // slash-command-control check, and the banner opening is textually disjoint
    // from the /compact echo / Compacted stdout anchors).
    #[test]
    fn classify_injected_prompt_continuation_still_wins_over_slash_control() {
        assert_eq!(
            classify_injected_prompt(
                "This session is being continued from a previous conversation that ran out of context... Summary:"
            ),
            InjectedPromptClass::SystemContinuation,
        );
    }

    // #3100: the system-continuation predicate is the most specific signal and
    // must win even if the continuation summary embeds a `<task-notification>`.
    #[test]
    fn classify_injected_prompt_continuation_wins_over_embedded_task_tag() {
        let mixed = "This session is being continued from a previous conversation.\nSummary: \
                     the agent ran <task-notification><status>completed</status></task-notification>";
        assert_eq!(
            classify_injected_prompt(mixed),
            InjectedPromptClass::SystemContinuation,
        );
    }

    // #3099 codex re-review (P1): a SystemContinuation must suppress ONLY the
    // user-turn lifecycle (⏳ + anchor + synthetic ownership) — it must STILL
    // deliver the provider's assistant output via the bridge tail. The original
    // early-return ran before the bridge tail spawn, orphaning Claude's output.
    // This guards the contract that drives the restructured relay flow.
    #[test]
    fn system_continuation_suppresses_user_turn_but_still_delivers_output() {
        let cont = InjectedPromptClass::SystemContinuation;
        assert!(
            cont.suppresses_user_turn_lifecycle(),
            "SystemContinuation must drop the ⏳/user-turn lifecycle"
        );
        assert!(
            cont.still_delivers_assistant_output(),
            "SystemContinuation must still relay Claude's assistant output (no orphaning)"
        );
        assert!(!cont.is_human_active_turn());

        // Human + task-notification turns keep their user-turn lifecycle AND
        // deliver output.
        for active in [
            InjectedPromptClass::HumanTuiDirect,
            InjectedPromptClass::TaskNotificationEvent,
        ] {
            assert!(
                !active.suppresses_user_turn_lifecycle(),
                "{active:?} must keep its user-turn lifecycle"
            );
            assert!(active.still_delivers_assistant_output());
        }
    }

    // #3100 codex re-review (P2): a human message that merely *quotes* the
    // continuation banner inside a normal request must NOT be mis-classified as
    // SystemContinuation — otherwise the human silently loses their `⏳`/turn.
    // Detection is anchored to start-of-prompt, so an embedded quote never trips.
    #[test]
    fn classify_injected_prompt_human_quote_of_banner_is_not_continuation() {
        let human = "Can you check why \"This session is being continued from a previous \
                     conversation\" keeps showing up in my logs? Please continue the \
                     conversation from where we left it off was also printed.";
        assert_eq!(
            classify_injected_prompt(human),
            InjectedPromptClass::HumanTuiDirect,
            "a human quoting the banner mid-message must stay a human turn",
        );
        assert!(classify_injected_prompt(human).is_human_active_turn());
    }

    // #3100 codex re-review (P2): a real machine-injected continuation banner is
    // the WHOLE prompt body and starts with the canonical opening — even with a
    // leading terminal-control prefix or leading whitespace the injector may
    // prepend, it must still classify as SystemContinuation (no false negative).
    #[test]
    fn classify_injected_prompt_real_injection_with_leading_controls_is_continuation() {
        let injected = "\u{1b}[2K\u{1b}[0m  \n\tThis session is being continued from a previous \
                        conversation that ran out of context.\nAnalysis:\n... summary body ...";
        assert_eq!(
            classify_injected_prompt(injected),
            InjectedPromptClass::SystemContinuation,
            "a real banner with leading controls/whitespace must classify as continuation",
        );
    }

    // #3100 codex P2: a real machine-injected continuation banner can arrive
    // WRAPPED with the SSH-direct injection envelope (the
    // `터미널에 직접 주입된 입력 (tmux : <session>):` line + a ```text fence) when a
    // previously-rendered notification round-trips back into the terminal and is
    // re-observed. After stripping the wrapper the banner body still starts with
    // the canonical opening, so it MUST classify as SystemContinuation — otherwise
    // it falls into the active-turn handler and wrongly gains a ⏳/anchor/synthetic
    // turn (the exact #3100 path this PR claims to fix).
    #[test]
    fn classify_injected_prompt_wrapped_continuation_is_continuation() {
        // Wrapper + ```text fence, exactly as `format_ssh_direct_prompt_notification`
        // renders it.
        let wrapped = "터미널에 직접 주입된 입력 (tmux : `AgentDesk-claude-adk-cc`):\n```text\n\
                       This session is being continued from a previous conversation that ran out \
                       of context.\nAnalysis: ... summary body ...\n```";
        assert_eq!(
            classify_injected_prompt(wrapped),
            InjectedPromptClass::SystemContinuation,
            "a wrapped continuation banner must classify as SystemContinuation",
        );
        assert!(!classify_injected_prompt(wrapped).is_human_active_turn());

        // Wrapper without a ```text fence (banner body directly on the next line).
        let wrapped_no_fence = "터미널에 직접 주입된 입력 (tmux : `s`):\n\
                                Please continue the conversation from where we left it off";
        assert_eq!(
            classify_injected_prompt(wrapped_no_fence),
            InjectedPromptClass::SystemContinuation,
        );

        // Wrapper + leading control codes the injector may prepend before the body.
        let wrapped_with_controls = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\u{1b}[2K  \
                                     This session is being continued from a previous conversation.";
        assert_eq!(
            classify_injected_prompt(wrapped_with_controls),
            InjectedPromptClass::SystemContinuation,
        );
    }

    // #3100 codex P2: stripping the wrapper is anchored to the START. A human
    // message whose body merely contains/quotes the wrapper marker (not as the
    // leading line) must NOT be unwrapped and must stay a human turn.
    #[test]
    fn classify_injected_prompt_wrapper_quoted_mid_body_is_not_continuation() {
        let human = "Why does \"터미널에 직접 주입된 입력 (tmux : `s`):\" appear, then \
                     This session is being continued from a previous conversation in my logs?";
        assert_eq!(
            classify_injected_prompt(human),
            InjectedPromptClass::HumanTuiDirect,
            "a human quoting the wrapper mid-body must stay a human turn",
        );
        assert!(classify_injected_prompt(human).is_human_active_turn());

        // A leading wrapper line whose body is NOT a continuation banner stays a
        // human turn (the wrapper alone must not force a continuation verdict).
        let wrapped_human =
            "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\nplease review PR #1234\n```";
        assert_eq!(
            classify_injected_prompt(wrapped_human),
            InjectedPromptClass::HumanTuiDirect,
        );
    }

    // #3100: the neutral session note must not present the system continuation
    // as "터미널에 직접 주입된 입력" (an active-turn marker).
    #[test]
    fn system_continuation_note_is_neutral_not_active_turn() {
        let prompt = "This session is being continued from a previous conversation. Summary: ...";
        let note = format_system_continuation_note("AgentDesk-claude-adk-cc", prompt);
        assert!(!note.contains("터미널에 직접 주입된 입력"));
        assert!(note.contains("세션 컨텍스트 이어가기"));
        assert!(note.contains("활성 턴 아님"));
        assert!(note.contains("(tmux : `AgentDesk-claude-adk-cc`)"));
        assert!(!note.contains("```text"));
        assert!(!note.contains("Summary:"));
        assert!(note.contains(&format!("요약 {}자 생략", prompt.chars().count())));
        assert!(note.contains("채널 기록과 동일 내용"));
    }

    #[cfg(unix)]
    fn test_watcher_handle(
        tmux_session_name: &str,
        output_path: &Path,
    ) -> super::super::TmuxWatcherHandle {
        super::super::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.display().to_string(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                super::super::tmux_watcher_now_ms(),
            )),
        }
    }

    #[cfg(unix)]
    #[test]
    fn external_input_owner_uses_resolved_claude_transcript_before_session_bound_owner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let stale_binding_path = dir.path().join("stale-binding.jsonl");
        let resolved_fresh_path = dir.path().join("resolved-fresh.jsonl");
        let tmux_session_name = "AgentDesk-claude-stale-binding-owner";
        let watchers = super::super::TmuxWatcherRegistry::new();
        watchers.insert(
            ChannelId::new(940_000_000_000_001),
            test_watcher_handle(tmux_session_name, &stale_binding_path),
        );

        assert_eq!(
            external_input_relay_owner_for_watchers(
                &watchers,
                tmux_session_name,
                Some(&stale_binding_path),
                true,
            ),
            ExternalInputRelayOwner::BridgeAdapter,
            "TUI-direct external turns have no synthetic inflight, so bridge tail owns response delivery"
        );
        assert_eq!(
            external_input_relay_owner_for_watchers(
                &watchers,
                tmux_session_name,
                Some(&resolved_fresh_path),
                true,
            ),
            ExternalInputRelayOwner::BridgeAdapter,
            "a heartbeat-fresh watcher may not own output for a different resolved transcript"
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_input_owner_selects_one_relay_path_per_observed_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let output_path = dir.path().join("output.jsonl");
        let other_path = dir.path().join("other.jsonl");
        let tmux_session_name = "AgentDesk-codex-owner-split";
        let watchers = super::super::TmuxWatcherRegistry::new();

        assert_eq!(
            external_input_relay_owner_for_watchers(
                &watchers,
                tmux_session_name,
                Some(&output_path),
                true,
            ),
            ExternalInputRelayOwner::BridgeAdapter
        );

        watchers.insert(
            ChannelId::new(940_000_000_000_002),
            test_watcher_handle(tmux_session_name, &output_path),
        );
        assert_eq!(
            external_input_relay_owner_for_watchers(
                &watchers,
                tmux_session_name,
                Some(&output_path),
                true,
            ),
            ExternalInputRelayOwner::BridgeAdapter
        );
        assert_eq!(
            external_input_relay_owner_for_watchers(
                &watchers,
                tmux_session_name,
                Some(&output_path),
                false,
            ),
            ExternalInputRelayOwner::TmuxWatcher
        );
        assert_eq!(
            external_input_relay_owner_for_watchers(
                &watchers,
                tmux_session_name,
                Some(&other_path),
                true,
            ),
            ExternalInputRelayOwner::BridgeAdapter
        );
    }

    #[cfg(unix)]
    #[test]
    fn bridge_tail_resolution_bypasses_watcher_suppression_for_session_bound_external_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript_path = dir.path().join("claude-transcript.jsonl");
        std::fs::write(&transcript_path, "").expect("write transcript");
        let tmux_session_name = "AgentDesk-claude-session-bound-direct-input";
        let channel_id = ChannelId::new(940_000_000_000_006);
        let shared = super::super::make_shared_data_for_tests();
        shared.tmux_watchers.insert(
            channel_id,
            test_watcher_handle(tmux_session_name, &transcript_path),
        );
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript_path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("claude-transcript".to_string()),
            last_offset: 0,
            relay_last_offset: None,
        };

        assert_eq!(
            resolve_idle_relay_transcript(&shared, tmux_session_name, channel_id, &binding, false,),
            Some(transcript_path.clone()),
            "BridgeAdapter-owned direct input must tail even when the watcher covers the transcript"
        );
        assert_eq!(
            resolve_idle_relay_transcript(&shared, tmux_session_name, channel_id, &binding, true,),
            None,
            "legacy watcher-owned mode may still suppress the bridge tail to avoid duplicates"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bridge_adapter_tails_only_bridge_owned_external_turns() {
        assert!(bridge_adapter_owns_external_turn(
            ExternalInputRelayOwner::BridgeAdapter
        ));
        assert!(!bridge_adapter_owns_external_turn(
            ExternalInputRelayOwner::SessionBoundRelay
        ));
        assert!(!bridge_adapter_owns_external_turn(
            ExternalInputRelayOwner::TmuxWatcher
        ));
        assert!(!bridge_adapter_owns_external_turn(
            ExternalInputRelayOwner::TuiPromptRelay
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_inflight_drain_wait_allows_transient_previous_turn() {
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_ref = probes.clone();

        assert!(
            wait_for_transient_state_to_clear(
                Duration::from_millis(50),
                Duration::from_millis(1),
                move || probe_ref.fetch_add(1, Ordering::SeqCst) < 2,
            )
            .await,
            "a short-lived previous inflight should not make the direct-input bridge tail give up"
        );
        assert!(
            probes.load(Ordering::SeqCst) >= 3,
            "the helper should re-check until the transient state clears"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_inflight_drain_wait_times_out_when_previous_turn_stays_active() {
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_ref = probes.clone();

        assert!(
            !wait_for_transient_state_to_clear(
                Duration::from_millis(5),
                Duration::from_millis(1),
                move || {
                    probe_ref.fetch_add(1, Ordering::SeqCst);
                    true
                },
            )
            .await,
            "a persistent previous inflight should keep the guarded skip behavior"
        );
        assert!(
            probes.load(Ordering::SeqCst) >= 2,
            "timeout branch should poll instead of making a single stale decision"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bridge_adapter_tail_guard_clears_only_current_external_lease() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-codex-bridge-guard";
        let channel_id = ChannelId::new(940_000_000_000_003);
        let original = ExternalInputRelayLease {
            channel_id: Some(channel_id.get()),
            turn_id: Some("external:codex:940000000000003:bridge-guard:1".to_string()),
            session_key: Some("host:AgentDesk-codex-bridge-guard".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::CodexTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        // Capture the RECORDED lease (with its stamped generation) — the guard must
        // hold the exact stored identity to clear it on drop.
        let recorded_original =
            crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                ProviderKind::Codex.as_str(),
                tmux,
                original.clone(),
            );

        {
            let _guard = TuiDirectExternalInputLeaseGuard::new(
                ProviderKind::Codex,
                tmux,
                channel_id,
                &recorded_original,
            );
        }
        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Codex.as_str(),
                tmux,
                channel_id.get(),
            )
            .is_none()
        );

        let newer = ExternalInputRelayLease {
            turn_id: Some("external:codex:940000000000003:bridge-guard:2".to_string()),
            ..original.clone()
        };
        let recorded_original =
            crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                ProviderKind::Codex.as_str(),
                tmux,
                original.clone(),
            );
        let recorded_newer;
        {
            let _guard = TuiDirectExternalInputLeaseGuard::new(
                ProviderKind::Codex,
                tmux,
                channel_id,
                &recorded_original,
            );
            recorded_newer = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                ProviderKind::Codex.as_str(),
                tmux,
                newer.clone(),
            );
        }
        assert_eq!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Codex.as_str(),
                tmux,
                channel_id.get(),
            ),
            Some(recorded_newer.clone()),
            "the old guard's drop must NOT clobber the newer lease (clear-by-identity)"
        );
        assert!(
            crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_matches(
                ProviderKind::Codex.as_str(),
                tmux,
                channel_id.get(),
                &recorded_newer,
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_bridge_lease_clears_when_tail_dedup_skips_spawn() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-claude-bridge-dedup-skip";
        let channel_id = ChannelId::new(940_000_000_000_004);
        let lease = ExternalInputRelayLease {
            channel_id: Some(channel_id.get()),
            turn_id: Some("external:claude:940000000000004:dedup-skip:2".to_string()),
            session_key: Some("host:AgentDesk-claude-bridge-dedup-skip".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        {
            let mut active = CLAUDE_IDLE_RESPONSE_TAILS
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            active.remove(tmux);
            active.insert(tmux.to_string());
        }
        let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            lease,
        );

        let spawned = spawn_claude_idle_response_tail_once(
            super::super::make_shared_data_for_tests(),
            tmux.to_string(),
            channel_id,
            PathBuf::from("/tmp/unused-claude-bridge-dedup-skip.jsonl"),
            0,
            "direct input while another tail is active".to_string(),
            lease.clone(),
        );
        assert!(
            !spawned,
            "active tail dedup should reject the second Claude tail"
        );
        assert!(clear_external_input_bridge_lease_if_current(
            &ProviderKind::Claude,
            tmux,
            channel_id,
            &lease,
        ));
        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            )
            .is_none(),
            "a dedup-skipped Claude BridgeAdapter lease must not block session-bound delivery until TTL"
        );
        CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(tmux);
    }

    // SAFETY (await_holding_lock): `tui_prompt_dedupe::TEST_LOCK` is a std Mutex
    // held across awaits to serialize tests that share the prompt-dedupe global
    // state; the hold is required for serialization. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[cfg(unix)]
    #[tokio::test]
    async fn claude_bridge_lease_guard_cleans_no_binding_precondition_skip() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-claude-bridge-no-binding";
        let channel_id = ChannelId::new(940_000_000_000_005);
        let prompt = ObservedTuiPrompt {
            provider: ProviderKind::Claude.as_str().to_string(),
            tmux_session_name: tmux.to_string(),
            prompt: "direct input without runtime binding".to_string(),
            observed_at: chrono::Utc::now(),
        };
        let lease = ExternalInputRelayLease {
            channel_id: Some(channel_id.get()),
            turn_id: Some("external:claude:940000000000005:no-binding:1".to_string()),
            session_key: Some("host:AgentDesk-claude-bridge-no-binding".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            lease,
        );

        let spawned;
        {
            let mut guard = TuiDirectExternalInputLeaseGuard::new(
                ProviderKind::Claude,
                tmux,
                channel_id,
                &lease,
            );
            spawned = maybe_spawn_claude_idle_response_tail(
                super::super::make_shared_data_for_tests(),
                channel_id,
                &prompt,
                &lease,
                None,
                None,
            )
            .await;
            if spawned {
                guard.disarm();
            }
        }

        assert!(
            !spawned,
            "missing runtime binding is a pre-tail precondition skip"
        );
        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            )
            .is_none(),
            "precondition skips before a tail guard exists must clear the recorded BridgeAdapter lease"
        );
    }

    // #3075 codex P1 #2: a `<task-notification>` edit-repeat records a fresh
    // external-input turn lease (record_observed_external_turn_lease) but then
    // early-returns before the normal bridge-tail / lease-guard cleanup. The
    // repeat path must clear exactly the lease it recorded so a dangling
    // non-Unassigned lease cannot make session-bound delivery skip a legitimate
    // bridge-tail delivery.
    #[test]
    fn task_notification_repeat_clears_its_recorded_external_lease() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-task-card-repeat-lease";
        let channel_id = ChannelId::new(950_000_000_000_001);
        let prompt = ObservedTuiPrompt {
            provider: ProviderKind::Claude.as_str().to_string(),
            tmux_session_name: tmux.to_string(),
            prompt: "<task-notification><task-id>repeat-x</task-id><status>completed</status></task-notification>".to_string(),
            observed_at: chrono::Utc::now(),
        };
        let lease = ExternalInputRelayLease {
            channel_id: Some(channel_id.get()),
            turn_id: Some("external:claude:950000000000001:repeat:1".to_string()),
            session_key: Some("host:AgentDesk-task-card-repeat-lease".to_string()),
            // A BridgeAdapter (non-Unassigned) lease is exactly what would block
            // session-bound delivery if left dangling.
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            lease,
        );
        // Sanity: the lease is present and would block delivery.
        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            )
        );

        // The repeat early-return clears exactly its recorded lease.
        assert!(clear_observed_external_turn_lease_if_current(
            &prompt, channel_id, &lease,
        ));
        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            )
            .is_none(),
            "a task-notification edit-repeat must not leave a stale lease that blocks bridge-tail delivery"
        );
    }

    // #3075 codex P1 #2: the exact-match guard must NOT clobber a newer turn's
    // lease that reused the same provider/session/channel after the repeat
    // recorded its lease.
    #[test]
    fn task_notification_repeat_lease_clear_preserves_newer_turn() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-task-card-repeat-newer";
        let channel_id = ChannelId::new(950_000_000_000_002);
        let prompt = ObservedTuiPrompt {
            provider: ProviderKind::Claude.as_str().to_string(),
            tmux_session_name: tmux.to_string(),
            prompt: "<task-notification><task-id>repeat-y</task-id></task-notification>"
                .to_string(),
            observed_at: chrono::Utc::now(),
        };
        let repeat_lease = ExternalInputRelayLease {
            channel_id: Some(channel_id.get()),
            turn_id: Some("external:claude:950000000000002:repeat:1".to_string()),
            session_key: Some("host:AgentDesk-task-card-repeat-newer".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let newer_lease = ExternalInputRelayLease {
            turn_id: Some("external:claude:950000000000002:repeat:2".to_string()),
            ..repeat_lease.clone()
        };
        let recorded_repeat = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            repeat_lease.clone(),
        );
        // A newer turn overwrites the lease before the repeat's cleanup runs.
        let recorded_newer = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            newer_lease.clone(),
        );
        assert_ne!(
            recorded_repeat.generation, recorded_newer.generation,
            "each recorded lease must get a distinct generation",
        );

        // The repeat's exact-match clear is a no-op against the newer lease.
        assert!(!clear_observed_external_turn_lease_if_current(
            &prompt,
            channel_id,
            &recorded_repeat,
        ));
        assert_eq!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            ),
            Some(recorded_newer),
            "exact-match clear must preserve a newer turn's lease",
        );
    }

    // #3089 A6b r2 [High]: the codex external-input bridge frame builder moved to
    // `tui_prompt_relay_controller_cutover::codex_external_input_bridge_stream_messages`
    // (flag-gated `OutputOffset` plumbing). Its OFF (`[Text, Done]`, byte-identical
    // legacy) and ON (`[Text, OutputOffset, Done]`, reaches the controller) shapes are
    // pinned in that sibling's test module under the shared env lock.

    // ====================================================================
    // #3256: stream-through of operator external-input prose. These tests pin
    // the SINGLE-bridge-turn invariant (one terminal Done = one finalize) and
    // prove that a LONG/multi-block response relays PROGRESSIVELY (more than one
    // Text frame forwarded before the terminal Done) while a SHORT response
    // still yields one finalized card — all WITHIN one bridge turn.
    //
    // These tests FAIL on the old code path: `bridge_adapter_stream_messages`
    // always collapsed the whole response into a single `[Text{full}, Done]`,
    // so the bridge only ever saw ONE Text frame regardless of how many prose
    // blocks the turn produced (the bug). The stream-through forwards each
    // reader frame, so multiple Text frames reach the bridge before Done.
    // ====================================================================

    #[cfg(unix)]
    fn drain_forwarded_idle_stream(
        prefix: Vec<StreamMessage>,
        rest: Vec<StreamMessage>,
    ) -> (Vec<StreamMessage>, usize) {
        let (reader_tx, reader_rx) = mpsc::channel();
        for message in rest {
            reader_tx.send(message).unwrap();
        }
        drop(reader_tx);
        let (bridge_tx, bridge_rx) = mpsc::channel();
        let text_frames = forward_idle_stream_into_bridge(prefix, reader_rx, bridge_tx);
        let forwarded: Vec<StreamMessage> = bridge_rx.into_iter().collect();
        (forwarded, text_frames)
    }

    #[cfg(unix)]
    #[test]
    fn idle_stream_long_response_relays_progressively_within_one_bridge_turn() {
        // A long autonomous turn produces multiple prose blocks interleaved with
        // tool use, then a terminal result. The stream-through must forward each
        // prose block as its own Text frame (progressive relay) and finalize on
        // EXACTLY ONE Done.
        let prefix = vec![StreamMessage::Text {
            content: "first prose block\n".to_string(),
        }];
        let rest = vec![
            StreamMessage::ToolUse {
                name: "Bash".to_string(),
                input: "ls".to_string(),
                tool_use_id: Some("t1".to_string()),
            },
            StreamMessage::OutputOffset { offset: 128 },
            StreamMessage::Text {
                content: "second prose block\n".to_string(),
            },
            StreamMessage::Text {
                content: "third prose block\n".to_string(),
            },
            // Real result line, then the reader's synthetic empty completion.
            StreamMessage::Done {
                result: "first prose block\nsecond prose block\nthird prose block".to_string(),
                session_id: Some("sess-9".to_string()),
            },
            StreamMessage::Done {
                result: String::new(),
                session_id: Some("sess-9".to_string()),
            },
        ];
        let (forwarded, text_frames) = drain_forwarded_idle_stream(prefix, rest);

        assert!(
            text_frames > 1,
            "long response must relay MORE THAN ONE Text frame before turn-done (got {text_frames})"
        );
        let done_count = forwarded
            .iter()
            .filter(|m| matches!(m, StreamMessage::Done { .. }))
            .count();
        assert_eq!(
            done_count, 1,
            "exactly one terminal Done must reach the bridge (single finalize)"
        );
        // The terminal Done must be the LAST frame and carry the authoritative
        // result; the trailing synthetic empty Done was dropped.
        assert!(matches!(
            forwarded.last(),
            Some(StreamMessage::Done { result, .. }) if result.contains("third prose block")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn idle_stream_short_response_produces_one_finalized_card() {
        // A short turn: one prose block then the result. Equivalent to the old
        // collect-then-send path — one card, one finalize.
        let prefix = vec![StreamMessage::Text {
            content: "quick answer".to_string(),
        }];
        let rest = vec![
            StreamMessage::Done {
                result: "quick answer".to_string(),
                session_id: Some("sess-1".to_string()),
            },
            StreamMessage::Done {
                result: String::new(),
                session_id: Some("sess-1".to_string()),
            },
        ];
        let (forwarded, text_frames) = drain_forwarded_idle_stream(prefix, rest);

        assert_eq!(
            text_frames, 1,
            "short response forwards exactly one Text frame"
        );
        assert_eq!(
            forwarded
                .iter()
                .filter(|m| matches!(m, StreamMessage::Done { .. }))
                .count(),
            1,
            "short response finalizes exactly once"
        );
    }

    #[cfg(unix)]
    #[test]
    fn idle_stream_finalizes_exactly_once_even_without_reader_done() {
        // Defensive: if the reader stream closes WITHOUT a terminal Done (e.g.
        // session died mid-stream), a synthetic Done is appended so the bridge
        // still finalizes — exactly once, never zero, never twice.
        let prefix = vec![StreamMessage::Text {
            content: "partial work".to_string(),
        }];
        let (forwarded, _) = drain_forwarded_idle_stream(prefix, Vec::new());

        let done_count = forwarded
            .iter()
            .filter(|m| matches!(m, StreamMessage::Done { .. }))
            .count();
        assert_eq!(
            done_count, 1,
            "missing reader Done must yield exactly one synthetic Done"
        );
        assert!(matches!(forwarded.last(), Some(StreamMessage::Done { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn idle_stream_strips_leading_chrome_from_first_text_only() {
        // The old compose path stripped leading TUI chrome; the stream-through
        // must strip it from the FIRST Text frame so the live card never flashes
        // it, while leaving later prose untouched.
        let prefix = vec![StreamMessage::Text {
            content: "No response requested.\nreal prose".to_string(),
        }];
        let rest = vec![
            StreamMessage::Text {
                content: "\nNo response requested. (literal later)".to_string(),
            },
            StreamMessage::Done {
                result: String::new(),
                session_id: None,
            },
        ];
        let (forwarded, _) = drain_forwarded_idle_stream(prefix, rest);

        assert!(matches!(
            &forwarded[0],
            StreamMessage::Text { content } if content.trim() == "real prose"
        ));
        // Later Text frames are NOT chrome-stripped.
        assert!(matches!(
            &forwarded[1],
            StreamMessage::Text { content } if content.contains("(literal later)")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn idle_stream_content_classifier_ignores_pure_control_and_empty_done() {
        // Empty / control-only frames are NOT content: a turn yielding only
        // these takes the no-card empty path (preserving today's behavior).
        assert!(!idle_stream_message_is_content(
            &StreamMessage::OutputOffset { offset: 10 }
        ));
        assert!(!idle_stream_message_is_content(&StreamMessage::Done {
            result: String::new(),
            session_id: None,
        }));
        assert!(!idle_stream_message_is_content(&StreamMessage::Text {
            content: "   \n".to_string(),
        }));
        // Real prose, an authoritative terminal body, and a transport error all
        // count as content.
        assert!(idle_stream_message_is_content(&StreamMessage::Text {
            content: "prose".to_string(),
        }));
        assert!(idle_stream_message_is_content(&StreamMessage::Done {
            result: "final body".to_string(),
            session_id: None,
        }));
        assert!(idle_stream_message_is_content(&StreamMessage::Error {
            message: "boom".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        }));
        // #3256 parity: a Text/Done body that is ONLY leading TUI chrome must NOT
        // count as content — otherwise a "No response requested." turn would now
        // spawn a placeholder card the old path never produced.
        assert!(!idle_stream_message_is_content(&StreamMessage::Text {
            content: "No response requested.".to_string(),
        }));
        assert!(!idle_stream_message_is_content(&StreamMessage::Text {
            content: "Continue from where you left off.".to_string(),
        }));
        assert!(!idle_stream_message_is_content(&StreamMessage::Done {
            result: "No response requested.".to_string(),
            session_id: None,
        }));
        // Chrome FOLLOWED by real prose is still content (only leading chrome is
        // stripped).
        assert!(idle_stream_message_is_content(&StreamMessage::Text {
            content: "No response requested.\nactual prose".to_string(),
        }));
    }

    #[cfg(unix)]
    #[test]
    fn idle_stream_commit_offset_only_on_successful_delivery() {
        // The stream-through commits the runtime-binding offset only when the
        // single bridge turn delivered successfully — matching the dedupe
        // contract vs. committed_relay_offset (the start-offset clamp in
        // `spawn_claude_idle_response_tail_once` handles the read side).
        assert!(tui_idle_tail_stream_should_commit_runtime_binding_offset(
            true
        ));
        assert!(!tui_idle_tail_stream_should_commit_runtime_binding_offset(
            false
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bridge_adapter_inflight_marks_external_input_as_bridge_owned() {
        let output_path = PathBuf::from("/tmp/adk-bridge-adapter.jsonl");
        let lease = ExternalInputRelayLease {
            channel_id: Some(42),
            turn_id: Some("external:codex:42:tmux:1".to_string()),
            session_key: Some("token:AgentDesk-codex-owner-split".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::CodexTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let state = build_tui_direct_bridge_inflight_state(
            ProviderKind::Codex,
            ChannelId::new(42),
            MessageId::new(101),
            MessageId::new(202),
            "typed in TUI",
            "AgentDesk-codex-owner-split",
            &output_path,
            333,
            &lease,
        );

        assert_eq!(state.turn_source, TurnSource::ExternalInput);
        assert_eq!(state.effective_relay_owner_kind(), RelayOwnerKind::None);
        assert_eq!(state.user_msg_id, 101);
        assert_eq!(state.current_msg_id, 202);
        assert_eq!(state.user_text, "typed in TUI");
        assert_eq!(state.session_key.as_deref(), lease.session_key.as_deref());
        assert_eq!(state.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(state.turn_start_offset, Some(333));
    }

    #[cfg(unix)]
    #[test]
    fn drain_wait_does_not_block_on_own_synthetic_inflight() {
        // #3176: the idle-tail drain-wait must treat THIS turn's own TUI-direct
        // synthetic inflight as non-blocking. If it waited on it, it would
        // self-deadlock (we created it; it never drains) and permanently skip the
        // relay. The discrimination is `tui_direct_synthetic_inflight_matches`:
        // ExternalInput + same tmux session => our own => non-blocking.
        let output_path = PathBuf::from("/tmp/adk-selfblock.jsonl");
        let lease = ExternalInputRelayLease {
            channel_id: Some(7),
            turn_id: Some("external:claude:7:tmux:1".to_string()),
            session_key: Some("token:AgentDesk-claude-self".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let state = build_tui_direct_bridge_inflight_state(
            ProviderKind::Claude,
            ChannelId::new(7),
            MessageId::new(11),
            MessageId::new(22),
            "typed in TUI",
            "AgentDesk-claude-self",
            &output_path,
            0,
            &lease,
        );

        // state.user_msg_id == 11 (the anchor id for this turn).
        // Our own synthetic for THIS turn (matching anchor id) => non-blocking.
        assert!(
            inflight_is_current_turn_synthetic(Some(&state), "AgentDesk-claude-self", Some(11)),
            "own synthetic (same session + matching anchor id) must be non-blocking"
        );
        // A PREVIOUS same-session TUI turn (different anchor id) => still blocks,
        // even though it is also ExternalInput on the same session. This is the
        // precision codex required: do not skip a genuinely distinct previous turn.
        assert!(
            !inflight_is_current_turn_synthetic(Some(&state), "AgentDesk-claude-self", Some(999)),
            "a different turn's inflight (anchor id mismatch) must stay blocking"
        );
        // This turn created no synthetic (system-continuation / slash) => anchor None
        // => any present inflight is a previous turn and still blocks.
        assert!(
            !inflight_is_current_turn_synthetic(Some(&state), "AgentDesk-claude-self", None),
            "no current synthetic (anchor None) must keep any inflight blocking"
        );
        // A different tmux session is never ours.
        assert!(
            !inflight_is_current_turn_synthetic(Some(&state), "AgentDesk-claude-other", Some(11)),
            "an inflight for a different tmux session must stay blocking"
        );
        // No inflight at all => nothing to wait on (not ours either).
        assert!(!inflight_is_current_turn_synthetic(
            None,
            "AgentDesk-claude-self",
            Some(11)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn synthetic_watcher_inflight_marks_existing_tui_turn_without_prompt_resubmit() {
        let output_path = PathBuf::from("/tmp/adk-tui-direct-watcher.jsonl");
        let lease = ExternalInputRelayLease {
            channel_id: Some(42),
            turn_id: Some("external:codex:42:tmux:2".to_string()),
            session_key: Some("token:AgentDesk-codex-owner-split".to_string()),
            relay_owner: ExternalInputRelayOwner::TmuxWatcher,
            runtime_kind: Some(RuntimeHandoffKind::CodexTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let state = build_tui_direct_synthetic_inflight_state(
            ProviderKind::Codex,
            ChannelId::new(42),
            MessageId::new(101),
            None,
            "typed in TUI",
            "AgentDesk-codex-owner-split",
            Some(&output_path),
            333,
            &lease,
            RelayOwnerKind::Watcher,
        );

        assert_eq!(state.turn_source, TurnSource::ExternalInput);
        assert_eq!(state.effective_relay_owner_kind(), RelayOwnerKind::Watcher);
        assert_eq!(
            state.request_owner_user_id,
            TUI_DIRECT_SYNTHETIC_OWNER_USER_ID
        );
        assert_eq!(state.user_msg_id, 101);
        assert_eq!(state.current_msg_id, 0);
        assert_eq!(state.user_text, "typed in TUI");
        assert_eq!(state.output_path.as_deref(), output_path.to_str());
        assert_eq!(state.input_fifo_path, None);
    }

    #[cfg(unix)]
    #[test]
    fn codex_external_input_relay_output_path_uses_rollout_not_wrapper() {
        let shared = super::super::make_shared_data_for_tests();
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/tmp/live-codex-rollout.jsonl".to_string(),
            relay_output_path: Some("/tmp/stale-wrapper-output.jsonl".to_string()),
            input_fifo_path: None,
            session_id: Some("codex-session".to_string()),
            last_offset: 123,
            relay_last_offset: Some(0),
        };

        assert_eq!(
            external_input_relay_output_path(
                &shared,
                ProviderKind::Codex.as_str(),
                "AgentDesk-codex-rollout-path",
                ChannelId::new(42),
                Some(&binding),
            ),
            Some(PathBuf::from("/tmp/live-codex-rollout.jsonl")),
            "Codex TUI response relay must tail the rollout file, not the wrapper jsonl"
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_external_input_binding_refreshes_from_live_rollout_marker() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let temp = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvRootGuard::set(temp.path());
        let tmux_session_name = "AgentDesk-codex-marker-refresh";
        let rollout_path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-marker-session\"}}\n",
        )
        .expect("write rollout");
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            tmux_session_name,
            &rollout_path,
            Some("codex-marker-session"),
        )
        .expect("write marker");
        let stale_binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: temp
                .path()
                .join("missing-wrapper.jsonl")
                .display()
                .to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };

        let refreshed = external_input_relay_binding(
            ProviderKind::Codex.as_str(),
            tmux_session_name,
            ChannelId::new(43),
            Some(stale_binding),
        )
        .expect("binding refresh");

        assert_eq!(refreshed.output_path, rollout_path.display().to_string());
        assert_eq!(
            refreshed.session_id.as_deref(),
            Some("codex-marker-session")
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_ownerless_external_input_missing_output_needs_rollout_repair() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _env = EnvRootGuard::set(dir.path());
        let rollout_path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            "{\"type\":\"session_meta\",\"payload\":{}}\n",
        )
        .expect("write rollout");
        let missing_output_path = dir.path().join("missing-wrapper.jsonl");
        let lease = ExternalInputRelayLease::unassigned(Some(44));
        let tmux_session_name = "AgentDesk-codex-repair-predicate";
        let mut state = build_tui_direct_synthetic_inflight_state(
            ProviderKind::Codex,
            ChannelId::new(44),
            MessageId::new(444),
            None,
            "typed in TUI",
            tmux_session_name,
            Some(&missing_output_path),
            0,
            &lease,
            RelayOwnerKind::None,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);

        assert!(
            codex_ownerless_external_input_inflight_needs_rollout_repair(
                &state,
                tmux_session_name,
                &rollout_path,
            )
        );

        state.output_path = Some(rollout_path.display().to_string());
        assert!(
            !codex_ownerless_external_input_inflight_needs_rollout_repair(
                &state,
                tmux_session_name,
                &rollout_path,
            )
        );

        state.output_path = Some(missing_output_path.display().to_string());
        state.current_msg_id = 123;
        assert!(
            !codex_ownerless_external_input_inflight_needs_rollout_repair(
                &state,
                tmux_session_name,
                &rollout_path,
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn synthetic_watcher_claim_requires_live_watcher_covering_output() {
        let dir = tempfile::tempdir().expect("temp dir");
        let output_path = dir.path().join("output.jsonl");
        let other_path = dir.path().join("other.jsonl");
        let tmux_session_name = "AgentDesk-codex-synthetic-owner";
        let watchers = super::super::TmuxWatcherRegistry::new();

        assert!(!tui_direct_watcher_can_own_output(
            &watchers,
            tmux_session_name,
            Some(&output_path),
        ));

        watchers.insert(
            ChannelId::new(940_000_000_000_007),
            test_watcher_handle(tmux_session_name, &output_path),
        );
        assert!(tui_direct_watcher_can_own_output(
            &watchers,
            tmux_session_name,
            Some(&output_path),
        ));
        assert!(!tui_direct_watcher_can_own_output(
            &watchers,
            tmux_session_name,
            Some(&other_path),
        ));
    }

    #[tokio::test]
    async fn tui_direct_pre_save_cleanup_does_not_decrement_global_active() {
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(940_000_000_000_008);
        let user_message_id = MessageId::new(940_000_000_000_108);
        let started = super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            Arc::new(CancelToken::new()),
            serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
            user_message_id,
        )
        .await;
        assert!(started, "test precondition: synthetic mailbox turn starts");

        shared.restart.global_active.store(3, Ordering::Relaxed);
        finish_tui_direct_synthetic_pre_save_failure(&shared, &provider, channel_id).await;

        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            3,
            "pre-save cleanup must not decrement a counter it has not incremented"
        );
        let snapshot = super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.active_user_message_id, None);
    }

    fn save_ownerless_tui_direct_inflight_for_mailbox_release_test(
        provider: ProviderKind,
        channel_id: ChannelId,
        user_message_id: MessageId,
        tmux_session_name: &str,
        output_path: &Path,
        stale_started_at: bool,
    ) {
        let lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));
        let mut state = build_tui_direct_synthetic_inflight_state(
            provider,
            channel_id,
            user_message_id,
            None,
            "typed in TUI",
            tmux_session_name,
            Some(output_path),
            0,
            &lease,
            RelayOwnerKind::None,
        );
        state.set_restart_mode(super::super::InflightRestartMode::DrainRestart);
        if stale_started_at {
            let stale_started_at = chrono::Local::now()
                - chrono::Duration::seconds(
                    (super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS + 1) as i64,
                );
            state.started_at = stale_started_at.format("%Y-%m-%d %H:%M:%S").to_string();
        }
        state.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        super::super::inflight::save_inflight_state(&state).expect("save inflight state");
    }

    #[tokio::test]
    async fn stale_ownerless_tui_direct_mailbox_release_allows_new_synthetic_claim() {
        let temp = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvRootGuard::set(temp.path());
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(940_000_000_000_009);
        let stale_message_id = MessageId::new(940_000_000_000_109);
        let anchor_message_id = MessageId::new(940_000_000_000_209);
        let tmux_session_name = "AgentDesk-codex-stale-mailbox-release";
        let output_path = temp.path().join("codex-rollout.jsonl");
        let stale_token = Arc::new(CancelToken::new());

        assert!(
            super::super::mailbox_try_start_turn(
                shared.as_ref(),
                channel_id,
                stale_token.clone(),
                serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                stale_message_id,
            )
            .await,
            "test precondition: stale mailbox turn starts"
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        save_ownerless_tui_direct_inflight_for_mailbox_release_test(
            provider.clone(),
            channel_id,
            stale_message_id,
            tmux_session_name,
            &output_path,
            true,
        );

        assert!(
            release_stale_ownerless_tui_direct_mailbox_if_current(
                &shared,
                &provider,
                channel_id,
                tmux_session_name,
                stale_message_id,
                anchor_message_id,
            )
            .await,
            "stale ownerless ExternalInput mailbox should be released"
        );
        assert!(stale_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        let snapshot = super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.active_user_message_id, None);

        assert!(
            super::super::mailbox_try_start_turn(
                shared.as_ref(),
                channel_id,
                Arc::new(CancelToken::new()),
                serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                anchor_message_id,
            )
            .await,
            "new synthetic claim must be able to occupy the released mailbox"
        );
    }

    #[tokio::test]
    async fn stale_ownerless_tui_direct_mailbox_release_preserves_fresh_owner() {
        let temp = tempfile::tempdir().expect("temp runtime root");
        let _env = EnvRootGuard::set(temp.path());
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(940_000_000_000_010);
        let active_message_id = MessageId::new(940_000_000_000_110);
        let anchor_message_id = MessageId::new(940_000_000_000_210);
        let tmux_session_name = "AgentDesk-codex-fresh-mailbox-preserve";
        let output_path = temp.path().join("codex-rollout.jsonl");
        let active_token = Arc::new(CancelToken::new());

        assert!(
            super::super::mailbox_try_start_turn(
                shared.as_ref(),
                channel_id,
                active_token.clone(),
                serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                active_message_id,
            )
            .await,
            "test precondition: active mailbox turn starts"
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        save_ownerless_tui_direct_inflight_for_mailbox_release_test(
            provider.clone(),
            channel_id,
            active_message_id,
            tmux_session_name,
            &output_path,
            false,
        );

        assert!(
            !release_stale_ownerless_tui_direct_mailbox_if_current(
                &shared,
                &provider,
                channel_id,
                tmux_session_name,
                active_message_id,
                anchor_message_id,
            )
            .await,
            "fresh ownerless ExternalInput mailbox must not be released"
        );
        assert!(!active_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        let snapshot = super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        assert_eq!(snapshot.active_user_message_id, Some(active_message_id));
        assert!(snapshot.cancel_token.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn tui_direct_gateway_has_no_live_bot_owner_for_local_queue_dispatch() {
        let gateway = TuiDirectBridgeGateway {
            http: Arc::new(serenity::Http::new("test-token")),
            shared: super::super::make_shared_data_for_tests(),
            provider: ProviderKind::Codex,
        };

        assert_eq!(gateway.bot_owner_provider(), None);
        assert!(
            gateway.can_chain_locally(),
            "bridge adapter still owns Discord delivery for the already-submitted turn"
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
    fn codex_rehydrate_thread_session_resolves_thread_channel_id() {
        let parent_channel_id = 1479671301387059200;
        let thread_id = 1504455726595051591_u64;
        let tmux_session_name =
            ProviderKind::Codex.build_tmux_session_name(&format!("adk-cdx-t{thread_id}"));

        assert_eq!(
            rehydrated_channel_id_for_segment(
                &ProviderKind::Codex,
                &tmux_session_name,
                "adk-cdx",
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
                ..
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

    // #3154 P1 (timestamp-anchor output loss): the worker-spawned BridgeAdapter
    // tail must anchor to the claim's post-drain EOF `turn_start_offset`, NOT a
    // `Utc::now()` timestamp scan. This proves the divergence on a transcript that
    // models the deferred-claim wait window: prior-turn bytes occupy `[0, X)`;
    // X is the post-drain EOF (the claim's `turn_start_offset`); THIS synthetic
    // turn then writes its response bytes at `[X, EOF)` DURING the wait, all with
    // timestamps that predate the worker's `Utc::now()` spawn (the worker spawns
    // the tail only AFTER the deferred claim resolves).
    //
    // RED (old `Utc::now()` timestamp anchoring): the scan looks for the first
    // line at/after `Utc::now()`. Every byte written during the wait predates it,
    // so the scan returns None and the start offset lands at the fallback (the
    // prior cursor) or — when the fallback is the stale binding cursor at X but
    // the scan would have to advance PAST the turn's lines — the turn's bytes in
    // `[X, EOF)` are skipped: output loss.
    //
    // GREEN (explicit `turn_start_offset` anchoring): the start offset is exactly
    // X. The tail relays `[X, EOF)` — every byte of this turn, no skip — and never
    // re-reads `[0, X)` (no prior-turn re-relay). The EOF offset is the boundary.
    #[cfg(unix)]
    #[test]
    fn worker_bridge_tail_anchors_to_turn_start_offset_not_utc_now_timestamp_scan() {
        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");

        // Prior turn's bytes: `[0, X)`. These are NOT part of this synthetic turn.
        let prior_a = r#"{"timestamp":"2026-05-28T00:00:00Z","type":"assistant"}"#;
        let prior_b = r#"{"timestamp":"2026-05-28T00:00:01Z","type":"assistant"}"#;
        let prior = format!("{prior_a}\n{prior_b}\n");
        let turn_start_offset = prior.len() as u64; // post-drain EOF == X (claim's turn_start_offset)

        // THIS synthetic turn's response bytes, written at `[X, EOF)` DURING the
        // deferred-claim wait. Their timestamps predate the worker's spawn instant.
        let turn_a = r#"{"timestamp":"2026-05-28T00:00:05Z","type":"assistant","text":"part-1"}"#;
        let turn_b = r#"{"timestamp":"2026-05-28T00:00:06Z","type":"assistant","text":"part-2"}"#;
        let turn = format!("{turn_a}\n{turn_b}\n");
        std::fs::write(&transcript, format!("{prior}{turn}")).expect("write transcript");
        let eof = (prior.len() + turn.len()) as u64;

        // The worker synthesizes `observed_at = Utc::now()` only AFTER the claim
        // wait — strictly after every byte above was written.
        let worker_spawn_now = chrono::DateTime::parse_from_rfc3339("2026-05-28T00:01:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        // The worker's fallback is the STALE binding cursor — a real pre-reseed
        // value that points PAST this turn (here: EOF). The explicit-anchor path
        // MUST override it; if the explicit offset were ignored and the timestamp
        // scan ran with this fallback, the turn's bytes would be skipped. Using a
        // stale-high fallback (not == X) is what makes the GREEN assertion FAIL if
        // the fix is reverted (explicit anchor ignored) — i.e. a true RED→GREEN.
        let fallback_offset = eof;

        // RED — the old `Utc::now()` timestamp anchoring (what the worker did
        // before this fix): `resolve_idle_tail_start_offset(.., explicit=None, ..)`
        // runs the timestamp scan. Every byte of this turn predates `worker_spawn_now`,
        // so the scan finds no boundary line and returns the fallback. The relay
        // window then starts at the fallback. Demonstrate the skip directly: when
        // the fallback is the stale-high prior cursor (a real pre-reseed value),
        // the timestamp path lands PAST this turn and skips ALL of its bytes.
        let red_offset = resolve_idle_tail_start_offset(
            &transcript,
            None, // old worker behaviour: no explicit anchor → Utc::now() scan
            worker_spawn_now,
            eof, // stale-high fallback (== EOF) the scan falls back to
        );
        assert_eq!(
            red_offset, eof,
            "RED: Utc::now() timestamp anchoring finds no boundary line (all bytes predate \
             the spawn instant) and falls back PAST this turn — the relay window [eof, eof) \
             skips every byte of this synthetic turn"
        );
        assert!(
            eof - red_offset < turn.len() as u64,
            "RED: bytes of this turn are skipped (relayed window is smaller than the turn)"
        );

        // GREEN — explicit anchoring on the claim's post-drain EOF `turn_start_offset`
        // (what the fixed worker passes: `explicit_start_offset = Some(turn_start_offset)`).
        // `observed_at`/`fallback` are IGNORED on this path.
        let green_offset = resolve_idle_tail_start_offset(
            &transcript,
            Some(turn_start_offset),
            worker_spawn_now, // must be ignored
            fallback_offset,  // must be ignored
        );
        assert_eq!(
            green_offset, turn_start_offset,
            "GREEN: explicit turn_start_offset anchoring relays from X — NO byte skip"
        );
        assert!(
            green_offset >= prior.len() as u64,
            "GREEN: the anchor never re-reads prior-turn bytes [0, X) (no re-relay)"
        );
        assert_eq!(
            eof - green_offset,
            turn.len() as u64,
            "GREEN: the relayed window [X, EOF) is EXACTLY this synthetic turn's bytes"
        );
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
                entry_id: None,
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
                entry_id: None,
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
                entry_id: None,
            }
        );
        // Last-prompt scan returns the just-typed prompt instead.
        assert_eq!(
            scan_claude_idle_transcript_for_last_prompt(&transcript, 0).expect("last scan"),
            ClaudeIdleTranscriptScan::Prompt {
                prompt: "just typed prompt".to_string(),
                prompt_start_offset: (old_prompt.len() + old_answer.len()) as u64,
                line_end_offset: (old_prompt.len() + old_answer.len() + new_prompt.len()) as u64,
                entry_id: None,
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
                entry_id: None,
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
                entry_id: None,
            }
        );
    }

    /// #3540: the watermark-reset / jsonl-head-rotation re-scan must NOT mint a
    /// phantom synthetic inflight. End-to-end at the scan→observe seam:
    ///   1. scan a transcript whose `user` entry carries a stable `uuid`;
    ///   2. feed (prompt, entry_id) to the relay observer → it relays
    ///      (`PublishedSshDirect`) and records the uuid;
    ///   3. simulate a head rotation: rewrite the transcript so the SAME entry
    ///      survives at a SHIFTED offset (head clipped) — its uuid is unchanged;
    ///   4. re-scan from offset 0 (watermark reset) and feed the same entry_id to
    ///      the observer → it is suppressed by IDENTITY
    ///      (`SuppressedReplayedEntry`), so the scanner's
    ///      `claude_idle_prompt_observation_should_tail_response` is FALSE and no
    ///      external-turn lease / synthetic claim is taken.
    #[test]
    fn watermark_reset_rescan_of_relayed_entry_does_not_resynthesize_turn() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let dir = tempfile::tempdir().expect("temp dir");
        let transcript = dir.path().join("transcript.jsonl");
        let uuid = "1516634295270117460-uuid";
        let head = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"earlier answer\"}]},\"sessionId\":\"s1\"}\n";
        let prompt_line = format!(
            "{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"deploy to make=live\"}}]}},\"sessionId\":\"s1\"}}\n"
        );
        std::fs::write(&transcript, format!("{head}{prompt_line}")).expect("write transcript");

        // ---- (1)/(2): first scan + observe → relays, records uuid. ----
        let scan = scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("first scan");
        let (prompt, entry_id) = match scan {
            ClaudeIdleTranscriptScan::Prompt {
                prompt, entry_id, ..
            } => (prompt, entry_id),
            other => panic!("expected Prompt, got {other:?}"),
        };
        assert_eq!(prompt, "deploy to make=live");
        assert_eq!(entry_id.as_deref(), Some(uuid));
        let first = crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
            ProviderKind::Claude.as_str(),
            "tmux-3540",
            &prompt,
            entry_id.as_deref(),
            chrono::Utc::now(),
        );
        assert_eq!(
            first,
            crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect,
            "the freshly-typed prompt relays on first sighting"
        );
        assert!(
            claude_idle_prompt_observation_should_tail_response(first),
            "first sighting tails the response (mints the external turn)"
        );

        // ---- (3): head rotation — same entry survives at a SHIFTED offset. ----
        let new_head = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n{\"type\":\"system\",\"subtype\":\"compact\",\"sessionId\":\"s1\"}\n";
        std::fs::write(&transcript, format!("{new_head}{prompt_line}"))
            .expect("rewrite transcript (rotation)");

        // ---- (4): watermark reset → re-scan from 0, same uuid → suppressed. ----
        let rescan = scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("re-scan");
        let (re_prompt, re_entry_id) = match rescan {
            ClaudeIdleTranscriptScan::Prompt {
                prompt,
                entry_id,
                prompt_start_offset,
                ..
            } => {
                assert_eq!(
                    prompt_start_offset,
                    new_head.len() as u64,
                    "the surviving entry is at a SHIFTED offset after head rotation"
                );
                (prompt, entry_id)
            }
            other => panic!("expected Prompt, got {other:?}"),
        };
        assert_eq!(
            re_entry_id.as_deref(),
            Some(uuid),
            "uuid survives the rotation"
        );
        let second = crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
            ProviderKind::Claude.as_str(),
            "tmux-3540",
            &re_prompt,
            re_entry_id.as_deref(),
            chrono::Utc::now(),
        );
        assert_eq!(
            second,
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedReplayedEntry,
            "#3540: the already-relayed entry re-encountered after a watermark \
             reset / head rotation is suppressed by identity"
        );
        assert!(
            !claude_idle_prompt_observation_should_tail_response(second),
            "#3540 (the fix): a suppressed-replayed entry does NOT tail a \
             response, so no external-turn lease / synthetic claim is taken — the \
             phantom synthetic inflight is never created"
        );
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
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

    // #3041 P1-4 codex: the early-return guard armed right after
    // `record_observed_external_turn_lease` must clear EXACTLY its recorded lease on
    // drop, so a FAILURE early-return (registry None / notify resolve Err-503 /
    // anchor POST failure) does not leave a dangling (BridgeAdapter-owned) lease
    // blocking the legitimate watcher/sink delivery for the full TTL.
    #[test]
    fn observed_lease_early_return_guard_clears_recorded_lease_on_drop() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-early-return-guard-clear";
        let channel_id = ChannelId::new(960_000_000_000_001);
        let lease = ExternalInputRelayLease {
            channel_id: Some(channel_id.get()),
            turn_id: Some("external:claude:960000000000001:early:1".to_string()),
            session_key: Some("host:AgentDesk-early-return-guard-clear".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let recorded = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            lease,
        );
        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            ),
            "lease must be present after record (would block delivery)"
        );

        // Simulate a failure early-return: the guard is armed and never disarmed, so
        // dropping it (function returns) clears the recorded lease.
        {
            let _guard = TuiDirectObservedLeaseEarlyReturnGuard::arm(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id,
                recorded.generation,
            );
        }

        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            )
            .is_none(),
            "an armed early-return guard drop must release the recorded lease so the watcher/sink can deliver"
        );
    }

    // #3041 P1-4 codex: on the SUCCESS path the caller DISARMs the guard before the
    // bridge-tail ownership block, so the lease PERSISTS for the in-flight turn (the
    // watcher/sink must not double-deliver). A disarmed guard's drop is a no-op.
    #[test]
    fn observed_lease_early_return_guard_disarm_preserves_lease() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-early-return-guard-disarm";
        let channel_id = ChannelId::new(960_000_000_000_002);
        let lease = ExternalInputRelayLease {
            channel_id: Some(channel_id.get()),
            turn_id: Some("external:claude:960000000000002:early:1".to_string()),
            session_key: Some("host:AgentDesk-early-return-guard-disarm".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let recorded = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            lease,
        );

        {
            let mut guard = TuiDirectObservedLeaseEarlyReturnGuard::arm(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id,
                recorded.generation,
            );
            // SUCCESS path: bridge legitimately takes ownership → disarm.
            guard.disarm();
        }

        assert_eq!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            ),
            Some(recorded),
            "a disarmed early-return guard must leave the lease intact for the in-flight turn"
        );
    }

    // #3041 P1-4 codex: the early-return guard clears BY GENERATION, so an OLD guard
    // armed with turn-1's generation must NOT clobber a NEWER same-key lease recorded
    // by turn-2 while turn-1 was awaiting the notify resolve / POST.
    #[test]
    fn observed_lease_early_return_guard_does_not_clobber_newer_lease() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        let tmux = "AgentDesk-early-return-guard-noclobber";
        let channel_id = ChannelId::new(960_000_000_000_003);
        let base = ExternalInputRelayLease {
            channel_id: Some(channel_id.get()),
            turn_id: Some("external:claude:960000000000003:early:1".to_string()),
            session_key: Some("host:AgentDesk-early-return-guard-noclobber".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
            generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        // turn-1 records and the relay arms a guard capturing G1.
        let recorded_turn1 = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            base.clone(),
        );
        let guard = TuiDirectObservedLeaseEarlyReturnGuard::arm(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id,
            recorded_turn1.generation,
        );
        // turn-2 records a NEWER same-key lease (G2) while turn-1 is in flight.
        let recorded_turn2 = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            ExternalInputRelayLease {
                turn_id: Some("external:claude:960000000000003:early:2".to_string()),
                ..base
            },
        );
        assert_ne!(recorded_turn1.generation, recorded_turn2.generation);

        // turn-1's guard drops (failure early-return) — by G1 it must NOT touch G2.
        drop(guard);

        assert_eq!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                ProviderKind::Claude.as_str(),
                tmux,
                channel_id.get(),
            ),
            Some(recorded_turn2),
            "an old early-return guard (G1) must leave turn-2's newer lease (G2) intact"
        );
    }

    // #3183: the idle-tail start offset must never fall below the watcher's
    // committed delivery offset, so the tail cannot re-relay a byte range the
    // tmux watcher already delivered (the double-relay regression).
    #[cfg(unix)]
    #[test]
    fn idle_tail_start_offset_clamps_up_to_watcher_committed_offset() {
        // Watcher already committed delivery up to byte 500. A prompt-timestamp
        // derived start offset of 200 sits BELOW the committed end, so the tail
        // would re-relay [200, 500) — exactly the duplicate. The clamp lifts the
        // start to the committed end so nothing the watcher delivered is re-sent.
        assert_eq!(
            clamp_idle_tail_start_offset_to_committed(200, 500),
            500,
            "start offset below the watcher committed offset must clamp up to it"
        );
        // When the watcher covered the whole turn (committed == EOF-ish), the
        // tail starts at the committed end and finds nothing new to relay.
        assert_eq!(
            clamp_idle_tail_start_offset_to_committed(500, 500),
            500,
            "equal start offset is unchanged (no re-relay, no over-skip)"
        );
    }

    // #3183 outage fallback (#3176): when the watcher stopped / never covered the
    // turn, `committed_relay_offset` is 0 (no confirmed delivery this process),
    // so the clamp is a no-op and the tail still relays from the timestamp
    // offset — no relay-loss regression.
    #[cfg(unix)]
    #[test]
    fn idle_tail_start_offset_clamp_is_noop_when_watcher_not_covering() {
        // committed == 0: watcher delivered nothing → the tail keeps its
        // timestamp-derived start offset and relays the full turn.
        assert_eq!(
            clamp_idle_tail_start_offset_to_committed(200, 0),
            200,
            "no committed delivery must leave the timestamp start offset intact (outage fallback)"
        );
        // A committed offset that lags the timestamp offset (watcher delivered an
        // OLDER region only) also must not pull the start backwards.
        assert_eq!(
            clamp_idle_tail_start_offset_to_committed(800, 300),
            800,
            "a lagging committed offset must not drag the start offset backwards"
        );
    }

    // #3358: a new synthetic inflight whose `relay_last_offset()` LAGS the
    // watcher's committed frontier must be born at/above that frontier so a
    // later same-identity re-claim cannot regress `turn_start_offset` /
    // `last_offset` below already-delivered bytes (the monotonicity ERROR triple).
    // The committed frontier is `Some(..)` here because the caller validated it
    // against the CURRENT wrapper generation (see the generation-mismatch test).
    #[test]
    fn synthetic_start_offset_carries_committed_frontier_forward() {
        // relay_last_offset lags (2821677) the watcher committed end (2838484):
        // born at the committed frontier so no backward re-seed is possible.
        assert_eq!(
            synthetic_start_offset_carry_forward(2_821_677, Some(2_838_484)),
            2_838_484,
            "lagging relay_last_offset must carry the committed frontier forward"
        );
        // Equal frontier → unchanged (born exactly at the committed end).
        assert_eq!(
            synthetic_start_offset_carry_forward(2_838_484, Some(2_838_484)),
            2_838_484
        );
    }

    // #3358 round 2 — Finding 1 guard: a STALE committed watermark from a
    // PREVIOUS wrapper generation must NOT clamp the synthetic forward. The
    // caller proves same-generation identity and passes `None` on mismatch, so
    // the helper falls back to `relay_last_offset` only. This is the content-skip
    // prevention: after a wrapper restart the stream resets to 0 and the new
    // synthetic must be born at its own (lagging) relay cursor, NOT lifted over a
    // stale frontier that would mark future bytes as already delivered.
    #[test]
    fn synthetic_start_offset_no_clamp_on_generation_mismatch() {
        // Generation mismatch → caller passes `None`: pre-fix seeding
        // (`relay_last_offset` only), even though a stale watermark (2838484) was
        // numerically higher. The rare monotonicity ERROR here is preferable to a
        // content skip (see helper doc).
        assert_eq!(
            synthetic_start_offset_carry_forward(2_821_677, None),
            2_821_677,
            "a generation-mismatched (stale) watermark must NOT clamp the synthetic forward"
        );
        // Fresh stream reset to 0 after restart, stale watermark unproven → birth
        // stays at 0, so the watcher walks the new generation from the head.
        assert_eq!(
            synthetic_start_offset_carry_forward(0, None),
            0,
            "a fresh post-restart stream must not be lifted over a stale frontier (content skip)"
        );
    }

    // #3358 genuine-regression guard: the carry-forward is BOUNDED to the
    // synthetic-creation handover — it never DRAGS a healthy start offset
    // backwards, and a missing/lagging committed frontier (outage / no confirmed
    // delivery) leaves the relay_last_offset intact so the invariants still catch
    // real backward writes elsewhere.
    #[test]
    fn synthetic_start_offset_carry_forward_never_regresses() {
        // committed unprovable/absent (`None`) → no-op.
        assert_eq!(
            synthetic_start_offset_carry_forward(2_821_677, None),
            2_821_677
        );
        // committed lags relay_last_offset → must NOT pull the start backwards.
        assert_eq!(
            synthetic_start_offset_carry_forward(900, Some(300)),
            900,
            "a lagging committed frontier must never drag the synthetic start backwards"
        );
    }
}
