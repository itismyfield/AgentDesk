//! One task-local carries a send's PG provenance and census tag to the raw request boundary.
use crate::services::cluster::intake_router_hook::owner_record::output::{Artifact, Scope};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};

#[derive(Clone, Default)]
pub struct Context {
    pub scope: Option<Scope>,
    pub artifact: Option<Artifact>,
    pub body: Arc<str>,
    pub census: Option<Value>,
}

tokio::task_local! {
    static CONTEXT: Context;
}

/// The raw request boundary's read; `None` outside every scope.
pub fn current() -> Option<Context> {
    CONTEXT.try_with(Clone::clone).ok()
}

pub fn current_census() -> Option<Value> {
    CONTEXT
        .try_with(|context| context.census.clone())
        .ok()
        .flatten()
}

/// Observation metadata cannot install or replace PG authority.
pub fn census_scope<T>(
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
pub fn scope<T>(
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
