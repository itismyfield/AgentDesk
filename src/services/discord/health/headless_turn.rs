//! Extracted from `services::discord::health` (#3038 Phase A) — verbatim
//! move; behavior unchanged. Headless agent-turn reserve/start API
//! (reservation channel/turn-id invariants) and the direct-meeting starter.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::HealthRegistry;
use super::runtime_resolve::{resolve_direct_meeting_runtime, resolve_direct_meeting_shared};
use crate::services::discord::SharedData;
use crate::services::discord::{meeting, router};
use crate::services::provider::ProviderKind;

pub async fn start_headless_agent_turn(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    let reservation = reserve_headless_agent_turn(channel_id);
    start_reserved_headless_agent_turn(
        registry,
        channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        reservation,
    )
    .await
}

#[derive(Debug, Clone)]
pub struct HeadlessAgentTurnReservation {
    channel_id: ChannelId,
    turn_id: String,
    inner: router::HeadlessTurnReservation,
}

impl HeadlessAgentTurnReservation {
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }
}

pub fn reserve_headless_agent_turn(channel_id: ChannelId) -> HeadlessAgentTurnReservation {
    let inner = router::reserve_headless_turn();
    HeadlessAgentTurnReservation {
        channel_id,
        turn_id: inner.turn_id(channel_id),
        inner,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn start_reserved_headless_agent_turn(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    reservation: HeadlessAgentTurnReservation,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    if reservation.channel_id != channel_id {
        return Err(router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            channel_id.get()
        )));
    }

    let shared = resolve_direct_meeting_shared(registry, channel_id, &owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;

    start_reserved_headless_agent_turn_with_shared(
        shared,
        channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        None,
        None,
        reservation,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn start_reserved_headless_agent_turn_with_owner_channel(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    turn_channel_id: ChannelId,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    // #5: When set, this synthetic label drives the routine's DISTINCT tmux
    // session while `channel_name_hint` carries the agent's REAL primary
    // channel/alias so workspace resolution succeeds. Non-routine callers pass
    // `None` for identical behavior.
    tmux_session_label: Option<String>,
    reservation: HeadlessAgentTurnReservation,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    if reservation.channel_id != turn_channel_id {
        return Err(router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            turn_channel_id.get()
        )));
    }

    let shared = resolve_direct_meeting_shared(registry, owner_channel_id, &owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;

    start_reserved_headless_agent_turn_with_shared(
        shared,
        turn_channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        tmux_session_label,
        Some(false),
        reservation,
    )
    .await
}

pub async fn start_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, &owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;
    let ctx = shared
        .http
        .cached_serenity_ctx
        .get()
        .cloned()
        .ok_or_else(|| {
            router::HeadlessTurnStartError::Internal(format!(
                "provider runtime is not ready for channel {}",
                owner_channel_id.get()
            ))
        })?;
    let dm_channel = serenity::UserId::new(dm_user_id)
        .create_dm_channel(&ctx.http)
        .await
        .map_err(|error| {
            router::HeadlessTurnStartError::Internal(format!(
                "DM channel creation failed for user {dm_user_id}: {error}"
            ))
        })?;
    let dm_channel_id = dm_channel.id;
    let reservation = reserve_headless_agent_turn(dm_channel_id);
    let channel_name_hint = Some(format!("dm-{dm_user_id}"));

    start_reserved_headless_agent_turn_with_shared(
        shared,
        dm_channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        None,
        Some(true),
        reservation,
    )
    .await
}

pub async fn reserve_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: &ProviderKind,
) -> Result<(ChannelId, HeadlessAgentTurnReservation), router::HeadlessTurnStartError> {
    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;
    let ctx = shared
        .http
        .cached_serenity_ctx
        .get()
        .cloned()
        .ok_or_else(|| {
            router::HeadlessTurnStartError::Internal(format!(
                "provider runtime is not ready for channel {}",
                owner_channel_id.get()
            ))
        })?;
    let dm_channel = serenity::UserId::new(dm_user_id)
        .create_dm_channel(&ctx.http)
        .await
        .map_err(|error| {
            router::HeadlessTurnStartError::Internal(format!(
                "DM channel creation failed for user {dm_user_id}: {error}"
            ))
        })?;
    let dm_channel_id = dm_channel.id;
    Ok((dm_channel_id, reserve_headless_agent_turn(dm_channel_id)))
}

/// #5708 S1: the shared-starter session inputs a reserved DM turn forwards —
/// `(channel_name_hint, tmux_session_label, is_dm_hint)`.
///
/// The DM hint stays the real `dm-<user>` channel so workspace/dispatch/role
/// resolution keeps resolving against the user's DM, while an optional
/// synthetic label carries the routine's DISTINCT tmux session and ADK session
/// key — the same split the thread routine path already gets (#3463 label,
/// #5685 `session_key_basis_override`). Split out of the starter so the
/// forwarding contract is unit-testable without a live Discord runtime.
pub(crate) fn dm_reserved_turn_session_inputs(
    dm_user_id: u64,
    tmux_session_label: Option<String>,
) -> (Option<String>, Option<String>, Option<bool>) {
    (
        Some(format!("dm-{dm_user_id}")),
        tmux_session_label,
        Some(true),
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn start_reserved_headless_agent_turn_in_dm(
    registry: &HealthRegistry,
    owner_channel_id: ChannelId,
    dm_channel_id: ChannelId,
    dm_user_id: u64,
    owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    // #5708 S1: when set, this synthetic label gives the turn its own tmux
    // session and ADK session key instead of the DM channel's canonical ones.
    // `None` reproduces the pre-#5708 behavior byte for byte, and every caller
    // still passes `None` — the routine caller only switches to
    // `Some(routine_agent_session_name(..))` once the DM completion receipt
    // (S2) and exact owned teardown (S3) exist to retire the routine's own
    // session. Activating it before then would strand routine panes.
    tmux_session_label: Option<String>,
    reservation: HeadlessAgentTurnReservation,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    if reservation.channel_id != dm_channel_id {
        return Err(router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            dm_channel_id.get()
        )));
    }

    let (_, shared) = resolve_direct_meeting_runtime(registry, owner_channel_id, &owner_provider)
        .await
        .map_err(router::HeadlessTurnStartError::Internal)?;
    let (channel_name_hint, tmux_session_label, is_dm_hint) =
        dm_reserved_turn_session_inputs(dm_user_id, tmux_session_label);

    start_reserved_headless_agent_turn_with_shared(
        shared,
        dm_channel_id,
        owner_provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
        tmux_session_label,
        is_dm_hint,
        reservation,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_reserved_headless_agent_turn_with_shared(
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    _owner_provider: ProviderKind,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    // #5: synthetic tmux-session label for routine turns; forwarded to the
    // router so the routine keeps a distinct tmux session while
    // `channel_name_hint` stays the real channel for workspace resolution.
    // `None` for non-routine callers.
    tmux_session_label: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessAgentTurnReservation,
) -> Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError> {
    // #5708 S1 seam: pin the two slots THIS call was handed. Test-only.
    #[cfg(test)]
    dm_reserved_label_tests::record_shared_starter_slots(&channel_name_hint, &tmux_session_label);

    if reservation.channel_id != channel_id {
        return Err(router::HeadlessTurnStartError::Internal(format!(
            "headless turn reservation channel mismatch: reserved {} but starting {}",
            reservation.channel_id.get(),
            channel_id.get()
        )));
    }

    let ctx = shared
        .http
        .cached_serenity_ctx
        .get()
        .cloned()
        .ok_or_else(|| {
            router::HeadlessTurnStartError::Internal(format!(
                "provider runtime is not ready for channel {}",
                channel_id.get()
            ))
        })?;
    let token = shared
        .http
        .cached_bot_token
        .get()
        .cloned()
        .or_else(|| crate::services::discord::resolve_discord_token_by_hash(&shared.token_hash))
        .ok_or_else(|| {
            router::HeadlessTurnStartError::Internal(format!(
                "provider token unavailable for channel {}",
                channel_id.get()
            ))
        })?;

    // The router derives its outcome id from this same opaque reservation.
    // Keep mismatches as a debug invariant instead of a post-spawn error: an
    // error after `Started` would invite callers to launch a duplicate retry.
    let expected_turn_id = reservation.turn_id.clone();
    let outcome = router::start_reserved_headless_turn(
        &ctx,
        channel_id,
        &prompt,
        source.as_deref().unwrap_or("system"),
        &shared,
        &token,
        source.as_deref(),
        metadata,
        channel_name_hint,
        tmux_session_label,
        is_dm_hint,
        reservation.inner,
    )
    .await?;

    if outcome.turn_id != expected_turn_id {
        tracing::error!(
            expected_turn_id = %expected_turn_id,
            actual_turn_id = %outcome.turn_id,
            "reserved headless turn returned an unexpected id after start; caller must fail closed"
        );
    }

    Ok(outcome)
}

pub async fn start_direct_meeting(
    registry: &HealthRegistry,
    channel_id: ChannelId,
    owner_provider: ProviderKind,
    primary_provider: ProviderKind,
    reviewer_provider: ProviderKind,
    agenda: String,
    fixed_participants: Vec<String>,
) -> Result<(), String> {
    let (http, shared) =
        resolve_direct_meeting_runtime(registry, channel_id, &owner_provider).await?;

    meeting::spawn_direct_start(
        http,
        channel_id,
        agenda,
        primary_provider,
        reviewer_provider,
        fixed_participants,
        shared,
    )
    .await
}

#[cfg(test)]
mod dm_reserved_label_tests {
    // #5708 S1 — DM reserved-turn label plumbing. The DM starter used to
    // hard-code `None` into the shared starter's `tmux_session_label` slot, so a
    // `fresh` routine running in a DM bound to the user's canonical DM tmux
    // session and ADK session key: every tick recreated (killed) the user's live
    // pane or warm-followed-up into it. The thread routine path has carried a
    // synthetic label since #3463; this slice gives the DM path the same
    // parameter, still passed `None` by every caller until the DM completion
    // receipt (S2) and exact owned teardown (S3) retire the routine's session.
    use std::cell::RefCell;

    use super::ChannelId;
    use super::{
        Arc, HealthRegistry, ProviderKind, dm_reserved_turn_session_inputs,
        reserve_headless_agent_turn, serenity, start_reserved_headless_agent_turn_in_dm,
    };

    const DM_USER_ID: u64 = 343_742_347_365_974_026;
    const DM_CHANNEL_HINT: &str = "dm-343742347365974026";
    const ROUTINE_LABEL: &str = "routine family-profile-probe-obujang - obujang";

    // The label must land in the tmux/session-key slot WITHOUT displacing the
    // `dm-<user>` workspace hint or the DM flag: those two drive workspace
    // resolution and DM delivery, which the routine still needs.
    #[test]
    fn dm_label_reaches_the_session_slot_without_moving_the_channel_hint() {
        let (channel_name_hint, tmux_session_label, is_dm_hint) =
            dm_reserved_turn_session_inputs(DM_USER_ID, Some(ROUTINE_LABEL.to_string()));

        assert_eq!(
            tmux_session_label.as_deref(),
            Some(ROUTINE_LABEL),
            "the routine label must reach the shared starter's tmux/session-key slot verbatim"
        );
        assert_eq!(
            channel_name_hint.as_deref(),
            Some(DM_CHANNEL_HINT),
            "the workspace hint must stay the REAL DM channel, not the routine label"
        );
        assert_eq!(
            is_dm_hint,
            Some(true),
            "a labelled DM turn is still a DM turn for delivery purposes"
        );
    }

    // Behavior-regression pin for this slice: with `None` the starter must
    // reproduce exactly what it forwarded before #5708 S1.
    #[test]
    fn dm_label_absent_reproduces_the_legacy_shared_starter_inputs() {
        let (channel_name_hint, tmux_session_label, is_dm_hint) =
            dm_reserved_turn_session_inputs(DM_USER_ID, None);

        assert_eq!(channel_name_hint.as_deref(), Some(DM_CHANNEL_HINT));
        assert_eq!(
            tmux_session_label, None,
            "no label means the canonical DM channel name still selects tmux and the session key"
        );
        assert_eq!(is_dm_hint, Some(true));
    }

    // The helper tests above cannot see the shared-starter call itself: dropping
    // the label there, or swapping it with the `channel_name_hint` slot next to
    // it, leaves them green. Drive the REAL starter and assert both slots the
    // call was handed. The stub runtime has no cached serenity ctx, so the start
    // fails only AFTER the forwarding under test.
    #[tokio::test]
    async fn dm_starter_hands_its_own_label_to_the_shared_starter_call() {
        let registry = HealthRegistry::new();
        registry
            .register(
                "claude".to_string(),
                crate::services::discord::make_shared_data_for_tests(),
            )
            .await;
        registry
            .register_http("claude".to_string(), Arc::new(serenity::Http::new("Bot t")))
            .await;
        let dm_channel = ChannelId::new(1_479_662_682_909_966_490);

        start_reserved_headless_agent_turn_in_dm(
            &registry,
            ChannelId::new(1_479_791_992_778_264_577),
            dm_channel,
            DM_USER_ID,
            ProviderKind::Claude,
            "probe".to_string(),
            None,
            None,
            Some(ROUTINE_LABEL.to_string()),
            reserve_headless_agent_turn(dm_channel),
        )
        .await
        .expect_err("a runtime without a serenity ctx cannot finish the start");

        assert_eq!(
            SHARED_STARTER_SLOTS.with(|slots| slots.borrow().clone()),
            Some((
                Some(DM_CHANNEL_HINT.to_string()),
                Some(ROUTINE_LABEL.to_string())
            )),
            "the shared starter's hint/label slots must arrive filled and unswapped"
        );
    }

    // `#[tokio::test]` runs its future on the test's own thread, so a
    // thread-local slot cannot be clobbered by a concurrently running test.
    thread_local! {
        static SHARED_STARTER_SLOTS: RefCell<Option<(Option<String>, Option<String>)>> =
            const { RefCell::new(None) };
    }

    pub(super) fn record_shared_starter_slots(
        channel_name_hint: &Option<String>,
        tmux_session_label: &Option<String>,
    ) {
        SHARED_STARTER_SLOTS.with(|slots| {
            *slots.borrow_mut() = Some((channel_name_hint.clone(), tmux_session_label.clone()));
        });
    }
}
