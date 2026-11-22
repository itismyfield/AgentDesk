//! Router entry points the Discord runtime reaches through `SharedData`; the bootstrap installs
//! the router's own functions so this side never names the router module.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId, UserId};

use crate::services::cluster::attachment_transfer::uploads::PendingUploads;
use crate::services::cluster::intake_turn::{IntakeExecution, IntakeRequest};
use crate::services::discord::queue_io::transport::IntakeDeps;
use crate::services::discord::replay_policy::{ExactHandoff, ExactRetry};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{DispatchLease, Intervention};

use super::super::{Error, SharedData};

/// The hosted-TUI promote gate's future, borrowed from its arguments.
pub(in crate::services::discord) type PromoteBlockedFuture<'a> =
    Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

/// A router future borrowed from its arguments.
pub(in crate::services::discord) type PortFuture<'a, T> =
    Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One dequeued intervention's queued admission request, as the router's admission takes it.
pub(in crate::services::discord) struct QueuedAdmission<'a> {
    pub provider: ProviderKind,
    pub channel_id: ChannelId,
    pub intervention: &'a Intervention,
    pub request_owner: UserId,
    pub request_owner_name: String,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub backstop_reason: &'static str,
    pub dispatch_lease: Option<Arc<DispatchLease>>,
}

/// How a queued admission ended; only an admitted intake runs the caller's step and finishes.
pub(in crate::services::discord) enum QueuedRun {
    /// Deferred, refused a non-portable attachment, or consumed into a held request.
    NotAdmitted,
    /// Admission could not restore the dequeued head.
    RejectedRestore,
    /// The value the caller's post-admission step returned, then the finished intake's result.
    Finished {
        after_admission: bool,
        result: Result<IntakeExecution, Error>,
    },
}

#[derive(Clone, Copy)]
pub(in crate::services::discord) struct RouterPorts {
    /// Wakes the channel's owner drive.
    #[cfg(unix)]
    pub owner_wake: fn(&Arc<SharedData>, ChannelId),
    /// Whether a late admission settles the channel's queued markers after dispatch.
    pub checkpoint_waits: fn(ChannelId) -> bool,
    /// Whether the hosted TUI must defer a queued turn's promotion.
    pub promote_readiness_blocked:
        for<'a> fn(&'a Arc<SharedData>, &'a ProviderKind, ChannelId) -> PromoteBlockedFuture<'a>,
    /// Admits a queued intervention; when admitted, awaits `after_admission` and then finishes it.
    pub admit_and_finish_queued: for<'a> fn(
        &'a IntakeDeps<'a>,
        QueuedAdmission<'a>,
        PortFuture<'a, bool>,
    ) -> PortFuture<'a, QueuedRun>,
    /// Starts an exact retry once the channel's mailbox is free.
    pub start_exact_retry: for<'a> fn(
        &'a Arc<serenity::http::Http>,
        &'a Arc<SharedData>,
        ChannelId,
        ExactRetry,
    ) -> PortFuture<'a, ExactHandoff>,
    /// Defers a promoted queued dispatch while the hosted TUI is busy.
    pub defer_promoted_dispatch: for<'a> fn(
        &'a Arc<SharedData>,
        &'a ProviderKind,
        ChannelId,
        &'a Intervention,
        Arc<DispatchLease>,
    ) -> PortFuture<'a, bool>,
    /// Whether the queued head may be offered to the running pane.
    pub queued_head_inject_covers: fn(ChannelId, Option<ChannelId>) -> bool,
    /// The intervention an intake request becomes when the input owner admits it.
    pub owned_original: fn(&IntakeRequest, bool, PendingUploads, bool) -> Intervention,
    /// The session key a kept start's headless turn runs under.
    #[cfg(unix)]
    pub owner_start_session_key: for<'a> fn(
        &'a Arc<SharedData>,
        ChannelId,
        &'a serde_json::Value,
    ) -> PortFuture<'a, Option<String>>,
}
