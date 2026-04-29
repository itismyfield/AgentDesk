use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

use super::AppState;
use crate::services::auto_queue::route;

#[allow(unused_imports)]
pub use route::{
    ActivateBody, AddRunEntryBody, CancelQuery, DispatchBody, DispatchGroupBody, GenerateBody,
    GenerateEntryBody, HistoryQuery, OrderBody, PauseBody, RebindSlotBody, ReorderBody, ResetBody,
    ResetGlobalBody, StatusQuery, UpdateEntryBody, UpdateRunBody,
};

#[allow(unused_imports)]
pub(crate) use route::{
    AutoQueueActivateDeps, activate_with_bridge_pg, activate_with_deps, activate_with_deps_pg,
};

pub async fn generate(
    state: State<AppState>,
    body: Json<GenerateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::generate(state, body).await
}

pub async fn activate(
    state: State<AppState>,
    body: Json<ActivateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::activate(state, body).await
}

pub async fn dispatch(
    state: State<AppState>,
    body: Json<DispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::dispatch(state, body).await
}

pub async fn status(
    state: State<AppState>,
    query: Query<StatusQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::status(state, query).await
}

pub async fn history(
    state: State<AppState>,
    query: Query<HistoryQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::history(state, query).await
}

pub async fn update_entry(
    state: State<AppState>,
    id: Path<String>,
    body: Json<UpdateEntryBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::update_entry(state, id, body).await
}

pub async fn add_run_entry(
    state: State<AppState>,
    id: Path<String>,
    body: Json<AddRunEntryBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::add_run_entry(state, id, body).await
}

pub async fn restore_run(
    state: State<AppState>,
    id: Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::restore_run(state, id).await
}

pub async fn rebind_slot(
    state: State<AppState>,
    slot: Path<(String, i64)>,
    body: Json<RebindSlotBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::rebind_slot(state, slot, body).await
}

pub async fn skip_entry(
    state: State<AppState>,
    id: Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::skip_entry(state, id).await
}

pub async fn update_run(
    state: State<AppState>,
    id: Path<String>,
    body: Json<UpdateRunBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::update_run(state, id, body).await
}

pub async fn reset_slot_thread(
    state: State<AppState>,
    slot: Path<(String, i64)>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::reset_slot_thread(state, slot).await
}

pub async fn reset(state: State<AppState>, body: Bytes) -> (StatusCode, Json<serde_json::Value>) {
    route::reset(state, body).await
}

pub async fn reset_global(
    state: State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    route::reset_global(state, body).await
}

pub async fn pause(state: State<AppState>, body: Bytes) -> (StatusCode, Json<serde_json::Value>) {
    route::pause(state, body).await
}

pub async fn resume_run(state: State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    route::resume_run(state).await
}

pub async fn cancel(
    state: State<AppState>,
    query: Query<CancelQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::cancel(state, query).await
}

pub async fn reorder(
    state: State<AppState>,
    body: Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::reorder(state, body).await
}

pub async fn submit_order(
    state: State<AppState>,
    id: Path<String>,
    headers: HeaderMap,
    body: Json<OrderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    route::submit_order(state, id, headers, body).await
}
