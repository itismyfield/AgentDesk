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
use songbird::{Event, EventContext, EventHandler};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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
                if data.reason.is_none() {
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
    barge_in.register_voice_context(channel_id, guild_id);
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
            )
            .await;
        }
    });
}

/// Claim the per-guild in-flight slot, run the rejoin loop, then release it —
/// so a disconnect storm cannot spawn overlapping loops for the same guild.
async fn handle_rejoin_request(
    ctx: &serenity::Context,
    receiver: &crate::voice::VoiceReceiver,
    barge_in: &Arc<super::voice_barge_in::VoiceBargeInRuntime>,
    provider: &str,
    shutting_down: &Arc<std::sync::atomic::AtomicBool>,
    request: ReconnectRequest,
) {
    let guild_id = request.guild_id;
    if !try_acquire_rejoin_inflight(provider, guild_id.get()) {
        // A rejoin loop for this guild is already running — drop the duplicate.
        return;
    }
    run_rejoin_loop(ctx, receiver, barge_in, provider, shutting_down, &request).await;
    release_rejoin_inflight(provider, guild_id.get());
}

async fn run_rejoin_loop(
    ctx: &serenity::Context,
    receiver: &crate::voice::VoiceReceiver,
    barge_in: &Arc<super::voice_barge_in::VoiceBargeInRuntime>,
    provider: &str,
    shutting_down: &Arc<std::sync::atomic::AtomicBool>,
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
        let backoff = reconnect_backoff(attempt);
        if !sleep_with_shutdown(backoff, shutting_down).await {
            tracing::info!(
                provider,
                guild_id = guild_id.get(),
                attempt,
                "voice rejoin canceled: process shutting down during backoff"
            );
            return;
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

/// Sleep for `duration`, waking every 5s to poll the shutdown flag so a long
/// backoff does not delay process teardown. Returns `false` if shutdown was
/// observed (the caller should abandon the loop).
async fn sleep_with_shutdown(
    duration: Duration,
    shutting_down: &Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    use std::sync::atomic::Ordering;
    let slice = Duration::from_secs(5);
    let mut remaining = duration;
    while remaining > Duration::ZERO {
        if shutting_down.load(Ordering::Relaxed) {
            return false;
        }
        let step = remaining.min(slice);
        tokio::time::sleep(step).await;
        remaining = remaining.saturating_sub(step);
    }
    !shutting_down.load(Ordering::Relaxed)
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

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
}
