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

fn outcome<T>(result: &serenity::Result<T>) -> &'static str {
    match result {
        Ok(_) => "confirmed",
        Err(serenity::Error::Http(error))
            if error.status_code().is_some_and(|status| {
                matches!(
                    status.as_u16(),
                    400 | 401 | 403 | 404 | 405 | 413 | 422 | 429
                )
            }) =>
        {
            "rejected"
        }
        Err(_) => "uncertain",
    }
}

pub(in crate::services::discord) fn request<T>(
    channel: ChannelId,
    op: Operation,
    raw: impl Future<Output = serenity::Result<T>>,
) -> Pin<Box<impl Future<Output = serenity::Result<T>>>> {
    Box::pin(async move {
        let Some(context) = CONTEXT.try_with(Clone::clone).ok() else {
            return raw.await;
        };
        let mut observation = context
            .census
            .clone()
            .filter(|_| op != Operation::Delete)
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
                    let receipt = transport::record(observation.event("not_sent", None));
                    observation.finished = true;
                    receipt.await;
                }
                return Err(serenity::Error::Io(std::io::Error::other(error)));
            }
        };
        if let Some(observation) = observation.as_mut() {
            observation.begun = true;
            transport::record(observation.event("begin", None)).await;
        }
        let mut receipt = None;
        let result = guard
            .run(async {
                let result = raw.await;
                if let Some(observation) = observation.as_mut() {
                    // Enqueue the known raw outcome before rollback; wait outside the raw deadline.
                    receipt = Some(transport::record(
                        observation.event("result", Some(outcome(&result))),
                    ));
                    observation.finished = true;
                }
                result
            })
            .await;
        if let Some(receipt) = receipt {
            receipt.await;
        } else if let Some(observation) = observation.as_mut() {
            let receipt = transport::record(observation.event("result", Some("uncertain")));
            observation.finished = true;
            receipt.await;
        }
        result.map_err(|error| serenity::Error::Io(std::io::Error::other(error)))?
    })
}

#[cfg(test)]
#[path = "transport_observation_tests.rs"]
mod tests;
