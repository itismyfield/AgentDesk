use axum::{
    Json,
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
};

use super::AppState;
use crate::services::auto_queue::route;
use crate::{api_caller_observability::RequestPrincipal, error::AppResult};

#[allow(unused_imports)]
pub use route::{
    ActivateBody, AddRunEntryBody, CancelQuery, GenerateBody, GenerateEntryBody, HistoryQuery,
    OrderBody, PauseBody, RebindSlotBody, ReorderBody, RepairPhaseGateBody, ResetBody,
    ResetGlobalBody, StatusQuery, UpdateEntryBody, UpdateRunBody,
};

#[allow(unused_imports)]
pub(crate) use route::{AutoQueueActivateDeps, activate_with_bridge_pg, activate_with_deps_pg};

/// POST /api/queue/generate
///
/// Bulk push of multiple issue numbers into a queue run. Single-call
/// complete: do NOT chain /redispatch, /retry, or /transition for the
/// same card after it (#1442). Cards with an active dispatch are silently
/// skipped and surfaced via `skipped_due_to_active_dispatch` (#1444). See
/// `/api/docs/card-lifecycle-ops` for the full decision tree (#1443).
pub async fn generate(
    state: State<AppState>,
    body: Json<GenerateBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::generate(state, body).await
}

pub async fn activate(
    state: State<AppState>,
    body: Json<ActivateBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::activate(state, body).await
}

pub async fn status(
    state: State<AppState>,
    query: Query<StatusQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::status(state, query).await
}

pub async fn history(
    state: State<AppState>,
    query: Query<HistoryQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::history(state, query).await
}

pub async fn update_entry(
    state: State<AppState>,
    id: Path<String>,
    body: Json<UpdateEntryBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::update_entry(state, id, body).await
}

pub async fn add_run_entry(
    state: State<AppState>,
    id: Path<String>,
    body: Json<AddRunEntryBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::add_run_entry(state, id, body).await
}

pub async fn restore_run(
    state: State<AppState>,
    id: Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::restore_run(state, id).await
}

pub async fn rebind_slot(
    state: State<AppState>,
    slot: Path<(String, i64)>,
    body: Json<RebindSlotBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::rebind_slot(state, slot, body).await
}

pub async fn skip_entry(
    state: State<AppState>,
    id: Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::skip_entry(state, id).await
}

pub async fn update_run(
    state: State<AppState>,
    id: Path<String>,
    body: Json<UpdateRunBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::update_run(state, id, body).await
}

pub async fn reset_slot_thread(
    state: State<AppState>,
    slot: Path<(String, i64)>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::reset_slot_thread(state, slot).await
}

pub async fn reset(
    state: State<AppState>,
    body: Bytes,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::reset(state, body).await
}

pub async fn reset_global(
    state: State<AppState>,
    body: Bytes,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::reset_global(state, body).await
}

pub async fn pause(
    state: State<AppState>,
    body: Bytes,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::pause(state, body).await
}

pub async fn resume_run(
    state: State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::resume_run(state).await
}

pub async fn repair_phase_gates(
    state: State<AppState>,
    id: Path<String>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    route::repair_phase_gates(state, id, headers, principal, body).await
}

pub async fn cancel(
    state: State<AppState>,
    query: Query<CancelQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::cancel(state, query).await
}

pub async fn reorder(
    state: State<AppState>,
    body: Json<ReorderBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::reorder(state, body).await
}

pub async fn submit_order(
    state: State<AppState>,
    id: Path<String>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
    body: Json<OrderBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::submit_order(state, id, headers, principal, body).await
}

/// GET /api/queue/phase-gates/catalog (#2125)
///
/// Returns the user-facing phase-gate kind catalog so dashboard and agents
/// share a single vocabulary for `phase_gate_kind` values used in
/// `/api/queue/generate` entries.
pub async fn phase_gate_catalog(state: State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    route::phase_gate_catalog(state).await
}

/// GET /api/queue/phase-gates/violations (#2657)
///
/// Reports any auto-queue entry whose `batch_phase` exceeds the run's
/// current phase pointer while still being pending or actively dispatched.
/// Read-only — does not block dispatches. Surfaced via the `/adk-phase`
/// Discord slash command and `agentdesk phase status` CLI.
pub async fn phase_gate_violations(
    state: State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::violations_route(state).await
}

/// POST /api/queue/request-generate (#2126)
///
/// Dashboard-facing: send a standardized "build a queue from these issues"
/// instruction to an agent's Discord channel. The backend owns both the
/// instruction text and channel routing so the dashboard stays decoupled
/// from prompt evolution.
pub async fn request_generate(
    state: State<AppState>,
    body: Json<serde_json::Value>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    route::request_generate(state, body).await
}
