//! Voice connection lifecycle: driver Connect/Reconnect/Disconnect observability
//! plus a per-provider exponential-backoff rejoin supervisor, and a single
//! zombie-call playback gate. Covers #4234 (dormant/observability),
//! #4235 (DriverDisconnect subscription + rejoin), #4236 (zombie-call gate).
//!
//! Design notes:
//! * Songbird 0.6 fires `DriverDisconnect` only *after* its internal reconnect
//!   budget is exhausted (see crate docs on `DisconnectData`), so this is the
//!   exact point at which we should take over with our own backoff loop.
//! * `voice_occupancy()` (commands/voice.rs) is the desired-state registry:
//!   `/vc leave` removes the entry, so the supervisor treats a missing entry as
//!   "cancel the rejoin". No separate suppression channel is needed.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId};
use songbird::events::context_data::{DisconnectKind, DisconnectReason};
use songbird::model::CloseCode as VoiceCloseCode;
use songbird::{Event, EventContext, EventHandler};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// App-level outcome for Discord voice close codes that indicate DAVE/E2EE
/// negotiation failed. The alert metadata lives beside the classifier so the
/// close-code -> operator-visible outcome mapping cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct VoiceSecurityDisconnect {
    pub close_code: u16,
    pub outcome: &'static str,
    pub alert_kind: &'static str,
    pub alert_reason: &'static str,
}

/// Classify Discord voice gateway close codes that require a distinct
/// DAVE/E2EE operator alert. 4016 is the legacy encryption-mode negotiation
/// failure; 4017 is Discord's forced-DAVE failure.
pub(in crate::services::discord) fn classify_voice_security_close_code(
    close_code: VoiceCloseCode,
) -> Option<VoiceSecurityDisconnect> {
    match close_code {
        VoiceCloseCode::UnknownEncryptionMode => Some(VoiceSecurityDisconnect {
            close_code: 4016,
            outcome: "unknown_encryption_mode",
            alert_kind: "voice-e2ee-4016",
            alert_reason: "지원되는 음성 암호화 모드를 협상하지 못했습니다.",
        }),
        VoiceCloseCode::DaveProtocolRequired => Some(VoiceSecurityDisconnect {
            close_code: 4017,
            outcome: "dave_protocol_required",
            alert_kind: "voice-dave-4017",
            alert_reason: "Discord가 요구하는 DAVE 종단간 암호화를 협상하지 못했습니다.",
        }),
        _ => None,
    }
}

fn classify_voice_security_disconnect(
    reason: Option<DisconnectReason>,
) -> Option<VoiceSecurityDisconnect> {
    match reason {
        Some(DisconnectReason::WsClosed(Some(close_code))) => {
            classify_voice_security_close_code(close_code)
        }
        _ => None,
    }
}

fn record_voice_security_disconnect(
    provider: &str,
    guild_id: GuildId,
    channel_id: ChannelId,
    classified: VoiceSecurityDisconnect,
) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static TOTAL: AtomicU64 = AtomicU64::new(0);
    let metric_total = TOTAL.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    crate::services::observability::events::record_simple(
        "voice_security_disconnect",
        Some(channel_id.get()),
        Some(provider),
        serde_json::json!({
            "guild_id": guild_id.get(),
            "close_code": classified.close_code,
            "outcome": classified.outcome,
            "metric": "voice_security_disconnect_total",
            "metric_total": metric_total,
        }),
    );
    metric_total
}

fn voice_security_alert_content(
    provider: &str,
    guild_id: GuildId,
    channel_id: ChannelId,
    classified: VoiceSecurityDisconnect,
) -> String {
    format!(
        "🚨 음성 보안 프로토콜 협상 실패로 연결이 종료되었습니다 (provider `{provider}`, guild `{}`, channel `{}`, close code `{}`). {} DAVE/E2EE 지원 상태를 확인해 주세요.",
        guild_id.get(),
        channel_id.get(),
        classified.close_code,
        classified.alert_reason,
    )
}

/// A request to re-establish a dropped voice connection, routed from a
/// `VoiceLifecycleHandler` (which fires inside songbird's driver task) to the
/// per-provider rejoin supervisor task.
pub(in crate::services::discord) struct ReconnectRequest {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub control_channel_id: ChannelId,
    pub provider: String,
}

/// Process-static router keyed by provider. Mirrors the `voice_occupancy()`
/// process-singleton pattern (commands/voice.rs): every provider's `run_bot()`
/// registers exactly one supervisor sender here. A `DriverDisconnect` handler
/// looks up the sender by provider at fire time — if none is registered the
/// event is logged but no rejoin is scheduled.
fn lifecycle_router() -> &'static DashMap<String, UnboundedSender<ReconnectRequest>> {
    static ROUTER: std::sync::OnceLock<DashMap<String, UnboundedSender<ReconnectRequest>>> =
        std::sync::OnceLock::new();
    ROUTER.get_or_init(DashMap::new)
}

/// Per-`(provider, guild)` in-flight guard so a disconnect storm cannot spawn
/// multiple overlapping rejoin loops for the same guild.
fn rejoin_inflight() -> &'static DashSet<(String, u64)> {
    static INFLIGHT: std::sync::OnceLock<DashSet<(String, u64)>> = std::sync::OnceLock::new();
    INFLIGHT.get_or_init(DashSet::new)
}

/// Register a supervisor sender for `provider` and return the paired receiver.
/// Called once per provider at bootstrap; the returned receiver drives the
/// rejoin supervisor loop.
pub(in crate::services::discord) fn register_lifecycle_router(
    provider: &str,
) -> UnboundedReceiver<ReconnectRequest> {
    let (tx, rx) = unbounded_channel();
    lifecycle_router().insert(provider.to_string(), tx);
    rx
}

/// Test-support: unregister a provider's router sender so process-static state
/// does not leak across unit tests in other modules (mirrors the private
/// `lifecycle_router().remove` cleanup this module's own tests use).
#[cfg(test)]
pub(in crate::services::discord) fn remove_lifecycle_router_for_tests(provider: &str) {
    lifecycle_router().remove(provider);
}

/// Try to claim the in-flight slot for `(provider, guild_id)`. Returns `true`
/// when the caller acquired it (must release via `release_rejoin_inflight`).
pub(in crate::services::discord) fn try_acquire_rejoin_inflight(
    provider: &str,
    guild_id: u64,
) -> bool {
    rejoin_inflight().insert((provider.to_string(), guild_id))
}

/// Release a previously-claimed in-flight slot.
pub(in crate::services::discord) fn release_rejoin_inflight(provider: &str, guild_id: u64) {
    rejoin_inflight().remove(&(provider.to_string(), guild_id));
}

/// Per-`(provider, guild)` rejoin cancel flags. `/vc leave` sets the flag so an
/// in-flight rejoin loop aborts promptly — during backoff, before starting a
/// join, or (critically) right after a join succeeds but before it re-asserts
/// occupancy — instead of clobbering the user's leave (#4234 leave/rejoin
/// TOCTOU). Occupancy remains the authoritative desired-state; this flag is the
/// low-latency edge signal that lets a *specific* in-flight loop notice the
/// leave without waiting out a 300s backoff. Mirrors the `Arc<AtomicBool>`
/// shutdown-flag convention already used for `shutting_down`.
fn rejoin_cancel() -> &'static DashMap<(String, u64), Arc<std::sync::atomic::AtomicBool>> {
    static CANCEL: std::sync::OnceLock<DashMap<(String, u64), Arc<std::sync::atomic::AtomicBool>>> =
        std::sync::OnceLock::new();
    CANCEL.get_or_init(DashMap::new)
}

/// Register a fresh (un-cancelled) cancel flag for an about-to-start rejoin loop
/// and return a clone the loop polls. Replaces any stale entry — the in-flight
/// guard guarantees at most one loop per `(provider, guild)`, so no live loop is
/// ever displaced. Paired with `clear_rejoin_cancel` on loop exit.
pub(in crate::services::discord) fn register_rejoin_cancel(
    provider: &str,
    guild_id: u64,
) -> Arc<std::sync::atomic::AtomicBool> {
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    rejoin_cancel().insert((provider.to_string(), guild_id), Arc::clone(&flag));
    flag
}

/// Signal any in-flight rejoin loop for `(provider, guild)` to abort. A no-op
/// when no loop is currently registered (nothing to cancel). Called by
/// `/vc leave`.
pub(in crate::services::discord) fn signal_rejoin_cancel(provider: &str, guild_id: u64) {
    if let Some(flag) = rejoin_cancel().get(&(provider.to_string(), guild_id)) {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Drop the cancel flag for `(provider, guild)` once the loop has exited.
pub(in crate::services::discord) fn clear_rejoin_cancel(provider: &str, guild_id: u64) {
    rejoin_cancel().remove(&(provider.to_string(), guild_id));
}

/// Finalize decision for a *successful* rejoin join, kept pure for unit testing.
/// A join can land inside the window in which `/vc leave` cleared occupancy
/// and/or set the cancel flag; in that case the connection must be torn down
/// and `record_join_success` skipped so the bot honours the leave (#4234).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum PostJoinDecision {
    /// Occupancy still desires the connection and no leave raced us — keep it.
    Keep,
    /// `/vc leave` won the race (occupancy gone or cancel signalled) — abort:
    /// leave the channel and do not record the join.
    AbortAndLeave,
}

/// Pure post-join finalize transition: keep the freshly-established connection
/// only if occupancy still desires it *and* no leave signalled a cancel.
pub(in crate::services::discord) fn post_join_decision(
    occupancy_has_entry: bool,
    canceled: bool,
) -> PostJoinDecision {
    if canceled || !occupancy_has_entry {
        PostJoinDecision::AbortAndLeave
    } else {
        PostJoinDecision::Keep
    }
}

/// Route a reconnect request to the provider's supervisor, if one is running.
/// Returns `true` when the request was enqueued.
pub(in crate::services::discord) fn dispatch_reconnect(request: ReconnectRequest) -> bool {
    if let Some(sender) = lifecycle_router().get(&request.provider) {
        sender.send(request).is_ok()
    } else {
        false
    }
}

/// Pure classifier: should a `DriverDisconnect` trigger our own rejoin loop?
///
/// Songbird 0.6 semantics (verified against driver/tasks/mod.rs and the
/// `DisconnectData` docs):
/// * `reason == None` — gateway-driven user action (leave / channel move).
/// * `Requested` — explicit `Driver::leave` (what `/vc leave` triggers): the
///   driver fires `kind = Runtime, reason = Some(Requested)`, NOT `None`. This
///   corrects the design assumption that `/vc leave` yields `reason == None`.
/// * `AttemptDiscarded` — this attempt was superseded by a newer request.
/// * everything else (`Io`/`TimedOut`/`Internal`/`ProtocolViolation`/`WsClosed`
///   across `Connect`/`Reconnect`/`Runtime`) — a real fault we should recover.
pub(in crate::services::discord) fn should_schedule_rejoin(
    kind: DisconnectKind,
    reason: Option<DisconnectReason>,
) -> bool {
    let _ = kind; // kind is informational for logging; classification is reason-driven.
    match reason {
        None => false,
        Some(DisconnectReason::Requested) => false,
        Some(DisconnectReason::AttemptDiscarded) => false,
        Some(_) => true,
    }
}

/// Exponential backoff for rejoin attempts: 2, 4, 8, 16, 32, 64, 128, 256, then
/// capped at 300s. No jitter — a single process with a handful of channels
/// favours deterministic timing over thundering-herd avoidance.
pub(in crate::services::discord) fn reconnect_backoff(attempt: u32) -> Duration {
    const BASE_SECS: u64 = 2;
    const CAP_SECS: u64 = 300;
    let shift = attempt.min(8);
    let secs = BASE_SECS.saturating_mul(1u64 << shift).min(CAP_SECS);
    Duration::from_secs(secs)
}

/// Desired-state decision for one rejoin iteration, kept pure for unit testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RejoinDecision {
    /// Occupancy entry gone — `/vc leave` (or a takeover) released this guild;
    /// abandon the rejoin loop.
    Canceled,
    /// A live connection already exists (songbird internal recovery or a manual
    /// `/vc join` won the race); nothing to do.
    AlreadyConnected,
    /// No connection and occupancy still desires one — attempt the rejoin.
    Proceed,
}

/// Pure state transition: occupancy is authoritative desired-state, connection
/// presence short-circuits redundant work.
pub(in crate::services::discord) fn rejoin_decision(
    occupancy_has_entry: bool,
    currently_connected: bool,
) -> RejoinDecision {
    if !occupancy_has_entry {
        RejoinDecision::Canceled
    } else if currently_connected {
        RejoinDecision::AlreadyConnected
    } else {
        RejoinDecision::Proceed
    }
}

/// Songbird global event handler attached per join. Emits structured lifecycle
/// logs and, on a recoverable disconnect, routes a `ReconnectRequest` to the
/// provider supervisor. Cloned three times at registration (Connect / Reconnect
/// / Disconnect), so it is cheap and `Clone`.
#[derive(Clone)]
pub(in crate::services::discord) struct VoiceLifecycleHandler {
    provider: String,
    guild_id: GuildId,
    channel_id: ChannelId,
    control_channel_id: ChannelId,
}

impl VoiceLifecycleHandler {
    pub(in crate::services::discord) fn new(
        provider: &str,
        guild_id: GuildId,
        channel_id: ChannelId,
        control_channel_id: ChannelId,
    ) -> Self {
        Self {
            provider: provider.to_string(),
            guild_id,
            channel_id,
            control_channel_id,
        }
    }

    fn schedule_rejoin(&self) {
        let scheduled = dispatch_reconnect(ReconnectRequest {
            guild_id: self.guild_id,
            channel_id: self.channel_id,
            control_channel_id: self.control_channel_id,
            provider: self.provider.clone(),
        });
        if !scheduled {
            tracing::debug!(
                provider = %self.provider,
                guild_id = self.guild_id.get(),
                channel_id = self.channel_id.get(),
                "voice rejoin not scheduled: no lifecycle supervisor registered for provider"
            );
        }
    }
}

#[async_trait]
impl EventHandler for VoiceLifecycleHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::DriverConnect(data) => {
                tracing::info!(
                    guild_id = self.guild_id.get(),
                    channel_id = self.channel_id.get(),
                    session_id = data.session_id,
                    provider = %self.provider,
                    "voice driver connected"
                );
            }
            EventContext::DriverReconnect(data) => {
                tracing::info!(
                    guild_id = self.guild_id.get(),
                    channel_id = self.channel_id.get(),
                    session_id = data.session_id,
                    provider = %self.provider,
                    "voice driver reconnected (songbird internal retry)"
                );
            }
            EventContext::DriverDisconnect(data) => {
                if let Some(classified) = classify_voice_security_disconnect(data.reason) {
                    let metric_total = record_voice_security_disconnect(
                        &self.provider,
                        self.guild_id,
                        self.channel_id,
                        classified,
                    );
                    tracing::warn!(
                        guild_id = self.guild_id.get(),
                        channel_id = self.channel_id.get(),
                        provider = %self.provider,
                        kind = ?data.kind,
                        reason = ?data.reason,
                        close_code = classified.close_code,
                        outcome = classified.outcome,
                        metric = "voice_security_disconnect_total",
                        metric_total,
                        "voice driver disconnected: DAVE/E2EE negotiation failure"
                    );
                    super::commands::notify_voice_alert(
                        self.control_channel_id,
                        voice_security_alert_content(
                            &self.provider,
                            self.guild_id,
                            self.channel_id,
                            classified,
                        ),
                        classified.alert_kind,
                    )
                    .await;
                } else if data.reason.is_none() {
                    tracing::info!(
                        guild_id = self.guild_id.get(),
                        channel_id = self.channel_id.get(),
                        provider = %self.provider,
                        "voice driver disconnected: user-requested"
                    );
                } else {
                    tracing::warn!(
                        guild_id = self.guild_id.get(),
                        channel_id = self.channel_id.get(),
                        provider = %self.provider,
                        kind = ?data.kind,
                        reason = ?data.reason,
                        "voice driver disconnected"
                    );
                }
                if should_schedule_rejoin(data.kind, data.reason) {
                    self.schedule_rejoin();
                }
            }
            _ => {}
        }
        None
    }
}

/// #4236 zombie-call gate — the single place that upgrades a `manager.get()`
/// handle to "actually connected". A `Some(Call)` handle can be a zombie left by
/// a failed join (no UDP socket bound → no audio ever plays). Playback call
/// sites route through this so a driver drop is a skipped-and-logged no-op
/// instead of a silent hang.
pub(in crate::services::discord) async fn connected_voice_call(
    manager: &Arc<songbird::Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    context: &'static str,
) -> Option<Arc<Mutex<songbird::Call>>> {
    let call_lock = manager.get(guild_id)?;
    let connected = call_lock.lock().await.current_connection().is_some();
    if connected {
        Some(call_lock)
    } else {
        tracing::warn!(
            guild_id = guild_id.get(),
            channel_id = channel_id.get(),
            context,
            "voice playback skipped: songbird call exists but driver not connected (#4236 zombie gate)"
        );
        None
    }
}

/// #4235: single-source the join-success postlude. Re-registers the barge-in
/// voice context for both the control and voice channels and records process
/// occupancy. Shared by the auto-join path (`commands::try_join_for_provider`)
/// and the rejoin supervisor so the two never drift on the state a successful
/// connection must leave behind. Deliberately log-free — each caller emits its
/// own INFO ("voice auto-join Ok" vs "voice rejoin Ok").
pub(in crate::services::discord) fn record_join_success(
    barge_in: &Arc<super::voice_barge_in::VoiceBargeInRuntime>,
    self_provider: &str,
    guild_id: GuildId,
    channel_id: ChannelId,
    control_channel_id: ChannelId,
) {
    barge_in.register_voice_context(control_channel_id, guild_id);
    barge_in.voice_connected(channel_id, guild_id);
    super::commands::voice_occupancy().insert(
        (self_provider.to_string(), guild_id.get()),
        channel_id.get(),
    );
}

/// #4235: spawn the per-provider rejoin supervisor. Registers this provider's
/// sender in the process-static lifecycle router and drives the reconnect loop
/// off the paired receiver. Called once per `run_bot()` from the voice
/// bootstrap. Follows the F21 shutdown-flag capture convention.
pub(in crate::services::discord) fn spawn_voice_rejoin_supervisor(
    ctx: serenity::Context,
    receiver: crate::voice::VoiceReceiver,
    barge_in: Arc<super::voice_barge_in::VoiceBargeInRuntime>,
    provider: crate::services::provider::ProviderKind,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
) {
    let provider_key = provider.as_str().to_string();
    let mut rx = register_lifecycle_router(&provider_key);
    tracing::info!(
        provider = provider_key.as_str(),
        "voice rejoin supervisor started"
    );
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            handle_rejoin_request(
                &ctx,
                &receiver,
                &barge_in,
                &provider_key,
                &shutting_down,
                request,
            );
        }
    });
}

/// Spawn the rejoin loop for one request as its own task (#4234 liveness). The
/// supervisor must return to `rx.recv()` immediately: the old design inline-
/// `await`ed the loop, so a permanently-unreachable guild — whose loop backs off
/// up to 300s *forever* — serialized behind it and starved every other guild's
/// rejoin on the same provider. Spawning makes the per-`(provider, guild)`
/// in-flight guard the real concurrency bound (one live loop per guild); under
/// the old inline await that guard was dead code.
fn handle_rejoin_request(
    ctx: &serenity::Context,
    receiver: &crate::voice::VoiceReceiver,
    barge_in: &Arc<super::voice_barge_in::VoiceBargeInRuntime>,
    provider: &str,
    shutting_down: &Arc<std::sync::atomic::AtomicBool>,
    request: ReconnectRequest,
) {
    let ctx = ctx.clone();
    let receiver = receiver.clone();
    let barge_in = Arc::clone(barge_in);
    let provider_owned = provider.to_string();
    let shutting_down = Arc::clone(shutting_down);
    let _spawned = spawn_rejoin_task(provider, request.guild_id.get(), move |cancel| async move {
        barge_in.voice_disconnected(request.channel_id);
        run_rejoin_loop(
            &ctx,
            &receiver,
            &barge_in,
            &provider_owned,
            &shutting_down,
            &cancel,
            &request,
        )
        .await;
    });
}

/// Guard + cancel-flag + task lifecycle for one rejoin loop, factored out so the
/// spawn-per-guild concurrency contract is unit-testable with a stubbed loop
/// body. Claims the in-flight slot (returning `false` and dropping the request
/// if a loop is already running for this guild), registers a fresh cancel flag,
/// spawns `run(cancel)` on its own task, and releases both on completion.
fn spawn_rejoin_task<F, Fut>(provider: &str, guild_id: u64, run: F) -> bool
where
    F: FnOnce(Arc<std::sync::atomic::AtomicBool>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    if !try_acquire_rejoin_inflight(provider, guild_id) {
        // A rejoin loop for this guild is already running — drop the duplicate.
        return false;
    }
    let cancel = register_rejoin_cancel(provider, guild_id);
    let provider_owned = provider.to_string();
    tokio::spawn(async move {
        run(Arc::clone(&cancel)).await;
        clear_rejoin_cancel(&provider_owned, guild_id);
        release_rejoin_inflight(&provider_owned, guild_id);
    });
    true
}

async fn run_rejoin_loop(
    ctx: &serenity::Context,
    receiver: &crate::voice::VoiceReceiver,
    barge_in: &Arc<super::voice_barge_in::VoiceBargeInRuntime>,
    provider: &str,
    shutting_down: &Arc<std::sync::atomic::AtomicBool>,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    request: &ReconnectRequest,
) {
    use std::sync::atomic::Ordering;

    let guild_id = request.guild_id;
    let channel_id = request.channel_id;
    let control_channel_id = request.control_channel_id;

    let Some(manager) = songbird::get(ctx).await else {
        tracing::warn!(
            provider,
            guild_id = guild_id.get(),
            "voice rejoin aborted: songbird manager missing"
        );
        return;
    };

    let mut attempt: u32 = 0;
    loop {
        if shutting_down.load(Ordering::Relaxed) {
            tracing::info!(
                provider,
                guild_id = guild_id.get(),
                attempt,
                "voice rejoin canceled: process shutting down"
            );
            return;
        }
        if cancel.load(Ordering::SeqCst) {
            tracing::info!(
                provider,
                guild_id = guild_id.get(),
                attempt,
                "voice rejoin canceled: /vc leave requested"
            );
            return;
        }
        let backoff = reconnect_backoff(attempt);
        match sleep_through_backoff(backoff, shutting_down, cancel).await {
            BackoffOutcome::Elapsed => {}
            BackoffOutcome::Shutdown => {
                tracing::info!(
                    provider,
                    guild_id = guild_id.get(),
                    attempt,
                    "voice rejoin canceled: process shutting down during backoff"
                );
                return;
            }
            BackoffOutcome::Canceled => {
                tracing::info!(
                    provider,
                    guild_id = guild_id.get(),
                    attempt,
                    "voice rejoin canceled: /vc leave requested during backoff"
                );
                return;
            }
        }

        // Desired-state (occupancy) + live-connection check.
        let occupancy_has_entry = super::commands::voice_occupancy()
            .contains_key(&(provider.to_string(), guild_id.get()));
        let currently_connected = match manager.get(guild_id) {
            Some(call_lock) => call_lock.lock().await.current_connection().is_some(),
            None => false,
        };
        match rejoin_decision(occupancy_has_entry, currently_connected) {
            RejoinDecision::Canceled => {
                tracing::info!(
                    provider,
                    guild_id = guild_id.get(),
                    attempt,
                    "voice rejoin canceled: occupancy released"
                );
                return;
            }
            RejoinDecision::AlreadyConnected => {
                barge_in.voice_connected(channel_id, guild_id);
                tracing::info!(
                    provider,
                    guild_id = guild_id.get(),
                    attempt,
                    "voice rejoin skipped: already connected"
                );
                return;
            }
            RejoinDecision::Proceed => {}
        }

        // #4235: cross the degraded threshold once (attempt 5). The notify dedup
        // registry keeps this to a single alert per process lifetime.
        if attempt == 5 {
            super::commands::notify_voice_alert(
                control_channel_id,
                format!(
                    "⚠️ 보이스 재연결이 반복 실패 중입니다 (provider `{provider}`, guild `{}`, attempt `{attempt}`). 네트워크/리전 상태를 확인해 주세요.",
                    guild_id.get()
                ),
                "reconnect-degraded",
            )
            .await;
        }

        tracing::warn!(
            provider,
            guild_id = guild_id.get(),
            channel_id = channel_id.get(),
            attempt,
            backoff_ms = backoff.as_millis() as u64,
            "voice rejoin attempt"
        );

        // Clear any zombie call before a fresh join (#2054 zombie-cleanup pattern).
        if manager.get(guild_id).is_some() {
            let _ = manager.remove(guild_id).await;
        }

        barge_in.voice_join_started(channel_id, guild_id);
        match super::commands::join_voice_channel(
            ctx,
            receiver.clone(),
            provider,
            guild_id,
            channel_id,
            control_channel_id,
        )
        .await
        {
            Ok(()) => {
                // #4234 TOCTOU: the occupancy sample above is stale by the time
                // the join completes — `/vc leave` can have cleared occupancy
                // and/or signalled cancel inside the join window. Re-check both
                // *before* record_join_success (which unconditionally re-asserts
                // occupancy + barge-in). If the leave won, tear the fresh
                // connection back down instead of clobbering the user's leave.
                let occupancy_has_entry = super::commands::voice_occupancy()
                    .contains_key(&(provider.to_string(), guild_id.get()));
                let canceled = cancel.load(Ordering::SeqCst);
                match post_join_decision(occupancy_has_entry, canceled) {
                    PostJoinDecision::AbortAndLeave => {
                        if let Err(error) = manager.leave(guild_id).await {
                            tracing::warn!(
                                provider,
                                guild_id = guild_id.get(),
                                channel_id = channel_id.get(),
                                attempt,
                                error = %error,
                                "voice rejoin abort-leave failed after leave raced join"
                            );
                        }
                        tracing::info!(
                            provider,
                            guild_id = guild_id.get(),
                            channel_id = channel_id.get(),
                            attempt,
                            canceled,
                            "voice rejoin discarded: /vc leave raced the join (occupancy released)"
                        );
                        barge_in.voice_disconnected(channel_id);
                        return;
                    }
                    PostJoinDecision::Keep => {}
                }
                record_join_success(barge_in, provider, guild_id, channel_id, control_channel_id);
                tracing::info!(
                    provider,
                    guild_id = guild_id.get(),
                    channel_id = channel_id.get(),
                    attempt,
                    "voice rejoin Ok"
                );
                return;
            }
            Err(error) => {
                barge_in.voice_disconnected(channel_id);
                // join_voice_channel already embeds the full songbird error_chain
                // in its context message, so a Display render is sufficient here.
                tracing::warn!(
                    provider,
                    guild_id = guild_id.get(),
                    channel_id = channel_id.get(),
                    attempt,
                    error = %error,
                    "voice rejoin attempt failed"
                );
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// Why a backoff sleep ended: it either ran to completion or was cut short by
/// process teardown (`Shutdown`) or a `/vc leave` cancel (`Canceled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackoffOutcome {
    Elapsed,
    Shutdown,
    Canceled,
}

/// Sleep for `duration`, waking every 5s to poll the shutdown *and* cancel flags
/// so neither a process teardown nor a `/vc leave` waits out a long (up to 300s)
/// backoff. Cancel is checked first so an explicit leave is reported as such.
async fn sleep_through_backoff(
    duration: Duration,
    shutting_down: &Arc<std::sync::atomic::AtomicBool>,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
) -> BackoffOutcome {
    use std::sync::atomic::Ordering;
    let slice = Duration::from_secs(5);
    let mut remaining = duration;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return BackoffOutcome::Canceled;
        }
        if shutting_down.load(Ordering::Relaxed) {
            return BackoffOutcome::Shutdown;
        }
        if remaining == Duration::ZERO {
            return BackoffOutcome::Elapsed;
        }
        let step = remaining.min(slice);
        tokio::time::sleep(step).await;
        remaining = remaining.saturating_sub(step);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn close_code_4017_maps_to_dave_failure_and_ops_alert() {
        let classified = classify_voice_security_close_code(VoiceCloseCode::DaveProtocolRequired)
            .expect("4017 must be classified as a DAVE/E2EE failure");

        assert_eq!(classified.close_code, 4017);
        assert_eq!(classified.outcome, "dave_protocol_required");
        assert_eq!(classified.alert_kind, "voice-dave-4017");
        let alert = voice_security_alert_content(
            "claude",
            GuildId::new(42),
            ChannelId::new(84),
            classified,
        );
        assert!(alert.contains("음성 보안 프로토콜 협상 실패"));
        assert!(alert.contains("close code `4017`"));
        assert!(alert.contains("DAVE/E2EE"));

        let related = classify_voice_security_close_code(VoiceCloseCode::UnknownEncryptionMode)
            .expect("4016 must share the E2EE classification path");
        assert_eq!(related.close_code, 4016);
        assert_eq!(related.outcome, "unknown_encryption_mode");
        assert_eq!(related.alert_kind, "voice-e2ee-4016");

        assert_eq!(
            classify_voice_security_close_code(VoiceCloseCode::VoiceServerCrash),
            None,
            "non-security close codes must stay on the generic disconnect path"
        );
    }

    #[test]
    fn reconnect_backoff_table_and_cap() {
        assert_eq!(reconnect_backoff(0), Duration::from_secs(2));
        assert_eq!(reconnect_backoff(1), Duration::from_secs(4));
        assert_eq!(reconnect_backoff(3), Duration::from_secs(16));
        assert_eq!(reconnect_backoff(7), Duration::from_secs(256));
        // 2 * 2^8 = 512 -> capped at 300.
        assert_eq!(reconnect_backoff(8), Duration::from_secs(300));
        assert_eq!(reconnect_backoff(20), Duration::from_secs(300));
        // No overflow at the extreme: shift is clamped to 8 before the shift.
        assert_eq!(reconnect_backoff(u32::MAX), Duration::from_secs(300));
    }

    #[test]
    fn backoff_is_monotonic_non_decreasing() {
        let mut previous = Duration::ZERO;
        for attempt in 0..40 {
            let current = reconnect_backoff(attempt);
            assert!(
                current >= previous,
                "backoff decreased at attempt {attempt}"
            );
            previous = current;
        }
    }

    #[test]
    fn should_schedule_rejoin_all_branches() {
        // reason == None across every kind -> user gateway action, no rejoin.
        for kind in [
            DisconnectKind::Connect,
            DisconnectKind::Reconnect,
            DisconnectKind::Runtime,
        ] {
            assert!(
                !should_schedule_rejoin(kind, None),
                "reason=None should never rejoin ({kind:?})"
            );
        }
        // Explicit /vc leave: songbird 0.6 fires Runtime + Requested.
        assert!(!should_schedule_rejoin(
            DisconnectKind::Runtime,
            Some(DisconnectReason::Requested)
        ));
        // Superseded attempt.
        assert!(!should_schedule_rejoin(
            DisconnectKind::Reconnect,
            Some(DisconnectReason::AttemptDiscarded)
        ));
        // Real faults -> rejoin.
        assert!(should_schedule_rejoin(
            DisconnectKind::Runtime,
            Some(DisconnectReason::Io)
        ));
        assert!(should_schedule_rejoin(
            DisconnectKind::Runtime,
            Some(DisconnectReason::TimedOut)
        ));
        assert!(should_schedule_rejoin(
            DisconnectKind::Connect,
            Some(DisconnectReason::TimedOut)
        ));
        assert!(should_schedule_rejoin(
            DisconnectKind::Reconnect,
            Some(DisconnectReason::Internal)
        ));
    }

    #[test]
    fn rejoin_decision_state_transitions() {
        // Occupancy released wins even if disconnected.
        assert_eq!(rejoin_decision(false, false), RejoinDecision::Canceled);
        assert_eq!(rejoin_decision(false, true), RejoinDecision::Canceled);
        // Desired + already connected -> skip.
        assert_eq!(
            rejoin_decision(true, true),
            RejoinDecision::AlreadyConnected
        );
        // Desired + disconnected -> proceed.
        assert_eq!(rejoin_decision(true, false), RejoinDecision::Proceed);
    }

    #[test]
    fn lifecycle_router_register_dispatch_and_isolation() {
        // Isolate on an unlikely provider key (process-static registry).
        let provider = "test-provider-0xC0FFEE";
        let mut rx = register_lifecycle_router(provider);

        let dispatched = dispatch_reconnect(ReconnectRequest {
            guild_id: GuildId::new(0xC0FFEE_0000_0001),
            channel_id: ChannelId::new(0xC0FFEE_0000_0002),
            control_channel_id: ChannelId::new(0xC0FFEE_0000_0003),
            provider: provider.to_string(),
        });
        assert!(
            dispatched,
            "dispatch to a registered provider should succeed"
        );

        let received = rx.try_recv().expect("supervisor should receive request");
        assert_eq!(received.guild_id.get(), 0xC0FFEE_0000_0001);
        assert_eq!(received.channel_id.get(), 0xC0FFEE_0000_0002);
        assert_eq!(received.control_channel_id.get(), 0xC0FFEE_0000_0003);

        // Unknown provider is not routed.
        assert!(!dispatch_reconnect(ReconnectRequest {
            guild_id: GuildId::new(0xC0FFEE_0000_0001),
            channel_id: ChannelId::new(0xC0FFEE_0000_0002),
            control_channel_id: ChannelId::new(0xC0FFEE_0000_0003),
            provider: "test-provider-unregistered-0xC0FFEE".to_string(),
        }));

        // Cleanup so the process-static registry does not leak into other tests.
        lifecycle_router().remove(provider);
    }

    #[test]
    fn record_join_success_registers_occupancy_and_voice_guilds() {
        let mut config = crate::voice::VoiceConfig::default();
        config.enabled = true;
        let barge_in = Arc::new(
            crate::services::discord::voice_barge_in::VoiceBargeInRuntime::from_voice_config(
                &config,
            ),
        );
        // Ids outside the production range so the process-static occupancy
        // registry does not collide with other tests / live runtime.
        let guild_id = GuildId::new(0xC0FFEE_0000_00B1);
        let channel_id = ChannelId::new(0xC0FFEE_0000_00B2);
        let control_channel_id = ChannelId::new(0xC0FFEE_0000_00B3);
        let provider = "record-join-test-0xC0FFEE";

        record_join_success(
            &barge_in,
            provider,
            guild_id,
            channel_id,
            control_channel_id,
        );

        assert_eq!(
            super::super::commands::voice_occupancy()
                .get(&(provider.to_string(), guild_id.get()))
                .map(|v| *v),
            Some(channel_id.get()),
            "occupancy registry should record (provider, guild) -> voice channel"
        );

        let mut channels = barge_in.control_channel_ids_for_guild(guild_id);
        channels.sort_unstable();
        let mut expected = vec![channel_id.get(), control_channel_id.get()];
        expected.sort_unstable();
        assert_eq!(
            channels, expected,
            "voice_guilds should map both control and voice channels to the guild"
        );

        super::super::commands::voice_occupancy().remove(&(provider.to_string(), guild_id.get()));
    }

    #[test]
    fn rejoin_inflight_guard_dedupes() {
        let provider = "test-inflight-0xC0FFEE";
        let guild: u64 = 0xC0FFEE_0000_00A1;
        assert!(try_acquire_rejoin_inflight(provider, guild));
        // Second acquire without release fails (loop already running).
        assert!(!try_acquire_rejoin_inflight(provider, guild));
        release_rejoin_inflight(provider, guild);
        // After release it can be acquired again.
        assert!(try_acquire_rejoin_inflight(provider, guild));
        release_rejoin_inflight(provider, guild);
    }

    // #4234 LIVENESS regression: a permanently-stuck guild's rejoin loop must
    // not starve rejoins for other guilds on the same provider. Exercises the
    // real `spawn_rejoin_task` seam (guard + cancel + task lifecycle) that the
    // supervisor uses per request, with a stubbed loop body.
    #[tokio::test]
    async fn supervisor_spawns_per_guild_so_stuck_guild_does_not_block_others() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let provider = "test-liveness-0xC0FFEE";
        let guild_stuck: u64 = 0xC0FFEE_0000_0D01;
        let guild_free: u64 = 0xC0FFEE_0000_0D02;

        // Guild A's loop is permanently stuck (models an unreachable guild whose
        // backoff loop never terminates).
        let stuck_started = spawn_rejoin_task(provider, guild_stuck, |_cancel| async move {
            std::future::pending::<()>().await;
        });
        assert!(
            stuck_started,
            "first request for a guild must start its loop"
        );

        // A duplicate request for the still-running guild is dropped by the
        // in-flight guard — proving the guard is now *live* (it was dead code
        // under the old serial inline-await supervisor).
        let stuck_dup = spawn_rejoin_task(provider, guild_stuck, |_cancel| async move {
            std::future::pending::<()>().await;
        });
        assert!(
            !stuck_dup,
            "duplicate in-flight guild must be rejected by the per-guild guard"
        );

        // Guild B runs to completion *concurrently* even though guild A never
        // returns — the liveness property the spawn-per-guild fix restores.
        let done = Arc::new(AtomicBool::new(false));
        let done_in_task = Arc::clone(&done);
        let free_started = spawn_rejoin_task(provider, guild_free, move |_cancel| async move {
            done_in_task.store(true, Ordering::SeqCst);
        });
        assert!(free_started, "second guild must start its own loop");

        let mut settled = false;
        for _ in 0..200 {
            if done.load(Ordering::SeqCst) {
                settled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            settled,
            "guild B rejoin must complete while guild A is permanently stuck"
        );

        // Guild A's task is pending forever and never runs its own release/clear,
        // so drop its process-static registry entries by hand. Guild B's task
        // self-cleans; the removes are idempotent.
        release_rejoin_inflight(provider, guild_stuck);
        clear_rejoin_cancel(provider, guild_stuck);
        release_rejoin_inflight(provider, guild_free);
        clear_rejoin_cancel(provider, guild_free);
    }

    // #4234 RACE regression (finalize half): after a join succeeds, the loop must
    // keep the connection only if occupancy still desires it *and* no leave
    // signalled cancel — otherwise record_join_success is skipped and the fresh
    // connection is torn back down, so a raced `/vc leave` is not clobbered.
    #[test]
    fn post_join_decision_keeps_only_when_desired_and_not_canceled() {
        assert_eq!(post_join_decision(true, false), PostJoinDecision::Keep);
        // Occupancy released by /vc leave -> abort (record_join_success skipped).
        assert_eq!(
            post_join_decision(false, false),
            PostJoinDecision::AbortAndLeave
        );
        // Cancel signalled by /vc leave -> abort even if the stale occupancy
        // sample still shows an entry.
        assert_eq!(
            post_join_decision(true, true),
            PostJoinDecision::AbortAndLeave
        );
        assert_eq!(
            post_join_decision(false, true),
            PostJoinDecision::AbortAndLeave
        );
    }

    // #4234 RACE regression (signal half): `/vc leave` must be able to abort an
    // in-flight rejoin loop, and a stale signal after the loop cleared its flag
    // must not pre-cancel a subsequently-registered loop.
    #[test]
    fn leave_signal_cancels_inflight_rejoin() {
        use std::sync::atomic::Ordering;
        let provider = "test-cancel-0xC0FFEE";
        let guild: u64 = 0xC0FFEE_0000_0C01;

        // A running rejoin loop registers a fresh, un-cancelled flag.
        let cancel = register_rejoin_cancel(provider, guild);
        assert!(
            !cancel.load(Ordering::SeqCst),
            "a fresh rejoin cancel flag starts un-cancelled"
        );

        // /vc leave signals it -> the in-flight loop observes the cancel.
        signal_rejoin_cancel(provider, guild);
        assert!(
            cancel.load(Ordering::SeqCst),
            "leave must set the in-flight rejoin cancel flag"
        );

        // After the loop clears its flag, a stale signal is a harmless no-op and
        // must not resurrect into a future loop's fresh flag.
        clear_rejoin_cancel(provider, guild);
        signal_rejoin_cancel(provider, guild);
        let next = register_rejoin_cancel(provider, guild);
        assert!(
            !next.load(Ordering::SeqCst),
            "a signal after clear must not pre-cancel a later-registered loop"
        );
        clear_rejoin_cancel(provider, guild);
    }
}
