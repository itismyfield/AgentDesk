//! What the worker needs from the runtime it polls for and from the runtime that owns a channel;
//! the Discord layer implements the host and hands the port in when it starts the loop.

use super::IntakeWorkerLifecycle;
use crate::services::cluster::attachment_transfer::uploads::PendingUploads;
use crate::services::cluster::intake_turn::{IntakeExecution, IntakeRequest};
use poise::serenity_prelude as serenity;
use serenity::ChannelId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A live read of one runtime's shutdown flag; it cannot set the flag.
pub(crate) type ShutdownProbe = Box<dyn Fn() -> bool + Send + Sync>;

/// One runtime as the worker sees it: its tick admission gate and its shutdown flag.
pub(crate) trait IntakeHost: Send + Sync + 'static {
    fn intake_lifecycle(&self) -> &IntakeWorkerLifecycle;
    fn shutdown_probe(&self) -> ShutdownProbe;
}

/// The channel owner's runtime with the REST client and credentials of that same runtime.
pub(crate) struct IntakeWorkerRuntime<S> {
    pub(crate) http: Arc<serenity::Http>,
    pub(crate) shared: Arc<S>,
    pub(crate) token: String,
}

/// The router's admission check for a channel, consulted before replay sources are trusted.
pub(crate) type CheckpointWaits = fn(ChannelId) -> bool;
pub(crate) type IntakeTurnFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<IntakeExecution, Box<dyn std::error::Error + Send + Sync>>>
            + Send
            + 'a,
    >,
>;
/// Runs one claimed request as a Discord turn.
pub(crate) type ExecuteIntakeTurn<S> = for<'a> fn(
    &'a Arc<serenity::http::Http>,
    &'a Arc<S>,
    &'a str,
    IntakeRequest,
    PendingUploads,
) -> IntakeTurnFuture<'a>;
pub(crate) type ResolveRuntimeFuture<'a, S> =
    Pin<Box<dyn Future<Output = Result<IntakeWorkerRuntime<S>, String>> + Send + 'a>>;
/// Finds the runtime that owns a channel, starting from the runtime that claimed the row.
pub(crate) type ResolveRuntime<S> = for<'a> fn(&'a S, ChannelId) -> ResolveRuntimeFuture<'a, S>;

/// Router decisions the worker needs; the Discord runtime hands them in when it starts the loop.
pub(crate) struct IntakeTurnPort<S> {
    pub(crate) checkpoint_waits: CheckpointWaits,
    pub(crate) execute: ExecuteIntakeTurn<S>,
    pub(crate) resolve_runtime: ResolveRuntime<S>,
}

// Function pointers copy whatever the host is, so the host needs neither `Clone` nor `Copy`.
impl<S> Clone for IntakeTurnPort<S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> Copy for IntakeTurnPort<S> {}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
