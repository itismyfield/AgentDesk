//! The headless-turn port: how a KE caller reserves and starts a headless agent turn on the
//! provider runtimes the Discord health registry holds.

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{HeadlessTurnStartError, HeadlessTurnStartOutcome};

/// What a reserved headless turn starts with: the owner's runtime, the turn's channel and input.
pub(crate) struct HeadlessTurnStart {
    pub owner_channel_id: ChannelId,
    pub turn_channel_id: ChannelId,
    pub owner_provider: ProviderKind,
    pub prompt: String,
    pub source: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub channel_name_hint: Option<String>,
    pub tmux_session_label: Option<String>,
}

/// What a reserved headless DM turn starts with: the owner's runtime, the DM channel and input.
pub(crate) struct HeadlessDmTurnStart {
    pub owner_channel_id: ChannelId,
    pub dm_channel_id: ChannelId,
    pub dm_user_id: u64,
    pub owner_provider: ProviderKind,
    pub prompt: String,
    pub source: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub tmux_session_label: Option<String>,
}

/// The one thing a caller reads of a reservation before it starts the turn.
pub(crate) trait HeadlessTurnReservationView {
    fn turn_id(&self) -> &str;
}

pub(crate) trait HeadlessTurnPort: Send + Sync {
    type Reservation: HeadlessTurnReservationView + Send;

    /// Reserves a turn id on `channel_id` before anything records it.
    fn reserve_headless_agent_turn(&self, channel_id: ChannelId) -> Self::Reservation;

    /// Starts the reserved turn on the owner channel's runtime.
    fn start_reserved_headless_agent_turn_with_owner_channel<'a>(
        &'a self,
        start: HeadlessTurnStart,
        reservation: Self::Reservation,
    ) -> BoxFuture<'a, Result<HeadlessTurnStartOutcome, HeadlessTurnStartError>>;

    /// Opens the DM channel with `dm_user_id` on the owner channel's runtime and reserves a turn
    /// id on it.
    fn reserve_headless_agent_turn_in_dm<'a>(
        &'a self,
        owner_channel_id: ChannelId,
        dm_user_id: u64,
        owner_provider: &'a ProviderKind,
    ) -> BoxFuture<'a, Result<(ChannelId, Self::Reservation), HeadlessTurnStartError>>;

    /// Starts the reserved DM turn on the owner channel's runtime.
    fn start_reserved_headless_agent_turn_in_dm<'a>(
        &'a self,
        start: HeadlessDmTurnStart,
        reservation: Self::Reservation,
    ) -> BoxFuture<'a, Result<HeadlessTurnStartOutcome, HeadlessTurnStartError>>;
}
