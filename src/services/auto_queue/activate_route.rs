/// POST /api/auto-queue/dispatch-next (formerly /api/auto-queue/activate, removed in #1064)
/// Dispatches the next pending entry in the active run.
pub async fn activate(
    State(state): State<AppState>,
    Json(body): Json<ActivateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let deps = AutoQueueActivateDeps::from_state(&state);
    let body = match activate_preflight_with_pg(pool, body).await {
        ActivatePgPreflight::Return(response) => return response,
        ActivatePgPreflight::Continue(body) => body,
    };

    activate_with_deps_pg(&deps, body).await
}
