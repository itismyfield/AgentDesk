//! Immutable request context crosses formatting; each HTTP future owns its own PG guard.
use crate::services::cluster::intake_router_hook::owner_record::output::{self, Artifact, Scope};
use crate::services::legacy_body_census::transport;
use poise::serenity_prelude::{self as serenity, ChannelId};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(in crate::services::discord) enum Operation {
    Post,
    Edit,
    Delete,
}

#[derive(Clone, Default)]
struct Context {
    scope: Option<Scope>,
    artifact: Option<Artifact>,
    body: Arc<str>,
    census: Option<Value>,
}

tokio::task_local! {
    static CONTEXT: Context;
}

pub(crate) fn current_census() -> Option<Value> {
    CONTEXT
        .try_with(|context| context.census.clone())
        .ok()
        .flatten()
}

/// Observation metadata cannot install or replace PG authority.
pub(crate) fn census_scope<T>(
    tag: Option<Value>,
    operation: impl Future<Output = T>,
) -> Pin<Box<impl Future<Output = T>>> {
    Box::pin(async move {
        let mut context = CONTEXT.try_with(Clone::clone).unwrap_or_default();
        context.census = tag;
        CONTEXT.scope(context, operation).await
    })
}

/// Contains provenance only, never an open transaction or a composite request deadline.
pub(in crate::services::discord) fn scope<T>(
    scope: Option<Scope>,
    artifact: Option<&Artifact>,
    body: &str,
    operation: impl Future<Output = T>,
) -> Pin<Box<impl Future<Output = T>>> {
    let artifact = artifact.cloned();
    let body: Arc<str> = body.into();
    Box::pin(async move {
        let mut context = CONTEXT.try_with(Clone::clone).unwrap_or_default();
        context.scope = scope;
        context.artifact = artifact;
        context.body = body;
        CONTEXT.scope(context, operation).await
    })
}

struct Observation {
    tag: Value,
    channel: ChannelId,
    op: Operation,
    started: Instant,
    begun: bool,
    finished: bool,
}

impl Observation {
    fn event(&self, phase: &str, outcome: Option<&str>) -> Value {
        json!({"kind":"target", "phase":phase, "outcome":outcome,
            "op":self.op, "channel":self.channel.get(), "census":self.tag,
            "at_ms":chrono::Utc::now().timestamp_millis(),
            "elapsed_ms":self.started.elapsed().as_millis()})
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        if !self.finished {
            let (phase, outcome) = if self.begun {
                ("result", Some("uncertain"))
            } else {
                ("not_sent", None)
            };
            transport::try_record(self.event(phase, outcome));
        }
    }
}

pub(in crate::services::discord) trait RequestError {
    fn rejected(&self) -> bool;
    fn authority(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self;
}

impl RequestError for serenity::Error {
    fn rejected(&self) -> bool {
        matches!(self, serenity::Error::Http(error) if error.status_code().is_some_and(|s| rejected_status(s.as_u16())))
    }
    fn authority(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        serenity::Error::Io(std::io::Error::other(error))
    }
}

impl RequestError for crate::services::dispatches::discord_delivery::DispatchMessagePostError {
    fn rejected(&self) -> bool {
        self.http_status()
            .is_some_and(|s| rejected_status(s.as_u16()))
    }
    fn authority(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::new(
            crate::services::dispatches::discord_delivery::DispatchMessagePostErrorKind::Other,
            error.into().to_string(),
        )
    }
}

fn rejected_status(status: u16) -> bool {
    matches!(status, 400 | 401 | 403 | 404 | 405 | 413 | 422 | 429)
}

fn outcome<T, E: RequestError>(result: &Result<T, E>) -> &'static str {
    match result {
        Ok(_) => "confirmed",
        Err(error) if error.rejected() => "rejected",
        Err(_) => "uncertain",
    }
}

pub(in crate::services::discord) async fn request_for_channel<T, E: RequestError>(
    channel: &str,
    op: Operation,
    raw: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    if let Ok(id) = channel.parse::<std::num::NonZeroU64>() {
        request(ChannelId::new(id.get()), op, raw).await
    } else {
        if current_census().is_some_and(|tag| tag["output_class"] != "NonBody") {
            crate::services::legacy_body_census::observation_unknown(
                "OUTBOUND_DESTINATION_UNKNOWN",
            );
        }
        raw.await
    }
}

pub(in crate::services::discord) fn request<T, E: RequestError>(
    channel: ChannelId,
    op: Operation,
    raw: impl Future<Output = Result<T, E>>,
) -> Pin<Box<impl Future<Output = Result<T, E>>>> {
    Box::pin(async move {
        let Some(context) = CONTEXT.try_with(Clone::clone).ok() else {
            return raw.await;
        };
        let mut observation = context
            .census
            .clone()
            .filter(|tag| op != Operation::Delete && tag["output_class"] != "NonBody")
            .map(|tag| Observation {
                tag,
                channel,
                op,
                started: Instant::now(),
                begun: false,
                finished: false,
            });
        let guard = match output::acquire(
            context.scope.as_ref(),
            &channel.get().to_string(),
            &context.body,
            context.artifact.as_ref(),
        )
        .await
        {
            Ok(guard) => guard,
            Err(error) => {
                if let Some(observation) = observation.as_mut() {
                    transport::record(observation.event("not_sent", None));
                    observation.finished = true;
                }
                return Err(E::authority(error));
            }
        };
        if let Some(observation) = observation.as_mut() {
            observation.begun = true;
            transport::record(observation.event("begin", None));
        }
        let result = guard
            .run(async {
                let result = raw.await;
                if let Some(observation) = observation.as_mut() {
                    // Keep the known raw outcome before rollback, without waiting for observation I/O.
                    transport::record(observation.event("result", Some(outcome(&result))));
                    observation.finished = true;
                }
                result
            })
            .await;
        if let Some(observation) = observation.as_mut()
            && !observation.finished
        {
            transport::record(observation.event("result", Some("uncertain")));
            observation.finished = true;
        }
        result.map_err(E::authority)?
    })
}

#[cfg(test)]
#[path = "transport_observation_tests.rs"]
pub(in crate::services::discord) mod tests;
