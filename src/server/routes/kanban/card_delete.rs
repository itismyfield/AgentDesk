use super::*;

/// DELETE /api/kanban-cards/:id
pub async fn delete_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_pool_required_error());
    };
    match kanban_db::soft_delete_card_pg(pool, &id).await {
        Ok(kanban_db::CardDeleteOutcome::NotFound) => Err(AppError::not_found("card not found")),
        Ok(kanban_db::CardDeleteOutcome::LiveDispatch(dispatch_id)) => Err(AppError::conflict(
            format!("card has live dispatch {dispatch_id}; cancel it before deleting the card"),
        )),
        Ok(kanban_db::CardDeleteOutcome::Deleted) => {
            crate::server::ws::emit_event(
                &state.broadcast_tx,
                "kanban_card_deleted",
                json!({"id": id}),
            );
            Ok((StatusCode::OK, Json(json!({"ok": true}))))
        }
        Err(error) => Err(database_error(error)),
    }
}
