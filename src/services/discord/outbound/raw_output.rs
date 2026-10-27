//! Immutable request context crosses formatting; each HTTP future owns its own PG guard.
use crate::services::cluster::intake_router_hook::owner_record::output::{self, Artifact, Scope};
use poise::serenity_prelude::{self as serenity, ChannelId};
use std::{future::Future, sync::Arc};

#[derive(Clone)]
struct Context {
    scope: Option<Scope>,
    artifact: Option<Artifact>,
    body: Arc<str>,
}

tokio::task_local! {
    static CONTEXT: Context;
}

/// Contains provenance only, never an open transaction or a composite request deadline.
pub(in crate::services::discord) async fn scope<T>(
    scope: Option<Scope>,
    artifact: Option<&Artifact>,
    body: &str,
    operation: impl Future<Output = T>,
) -> T {
    CONTEXT
        .scope(
            Context {
                scope,
                artifact: artifact.cloned(),
                body: body.into(),
            },
            operation,
        )
        .await
}

pub(in crate::services::discord) async fn request<T>(
    channel: ChannelId,
    raw: impl Future<Output = serenity::Result<T>>,
) -> serenity::Result<T> {
    let Some(context) = CONTEXT.try_with(Clone::clone).ok() else {
        return raw.await;
    };
    let guard = output::acquire(
        context.scope.as_ref(),
        &channel.get().to_string(),
        &context.body,
        context.artifact.as_ref(),
    )
    .await
    .map_err(|error| serenity::Error::Io(std::io::Error::other(error)))?;
    guard
        .run(raw)
        .await
        .map_err(|error| serenity::Error::Io(std::io::Error::other(error)))?
}
