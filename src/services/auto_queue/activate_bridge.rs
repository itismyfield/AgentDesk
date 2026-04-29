pub(crate) fn activate_with_deps(
    deps: &AutoQueueActivateDeps,
    body: ActivateBody,
) -> (StatusCode, Json<serde_json::Value>) {
    if deps.pg_pool.is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool is not configured"})),
        );
    }
    let deps = deps.clone();
    match crate::utils::async_bridge::block_on_result(
        async move { Ok::<_, String>(activate_with_deps_pg(&deps, body).await) },
        |error| error,
    ) {
        Ok(response) => response,
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

