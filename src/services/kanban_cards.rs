use sqlx::PgPool;

use crate::db::kanban::{self, ListCardsFilter};
use crate::server::dto::kanban::{KanbanCardView, ListCardsResponse};
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

#[derive(Clone)]
pub struct KanbanService {
    pg_pool: Option<PgPool>,
}

#[derive(Debug, Clone, Default)]
pub struct ListCardsInput {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

impl KanbanService {
    pub fn new(pg_pool: Option<PgPool>) -> Self {
        Self { pg_pool }
    }

    pub async fn list_cards(&self, input: ListCardsInput) -> ServiceResult<ListCardsResponse> {
        let pool = self.pg_pool.as_ref().ok_or_else(|| {
            ServiceError::internal("postgres pool unavailable for list_cards")
                .with_code(ErrorCode::Database)
                .with_operation("list_cards.pg_pool")
        })?;
        let registered_repo_ids =
            kanban::list_registered_repo_ids_pg(pool)
                .await
                .map_err(|error| {
                    ServiceError::internal(error)
                        .with_code(ErrorCode::Database)
                        .with_operation("list_cards.list_registered_repo_ids_pg")
                })?;
        let records = kanban::list_cards_pg(
            pool,
            &ListCardsFilter {
                status: input.status,
                repo_id: input.repo_id,
                assigned_agent_id: input.assigned_agent_id,
            },
            &registered_repo_ids,
        )
        .await
        .map_err(|error| {
            ServiceError::internal(error)
                .with_code(ErrorCode::Database)
                .with_operation("list_cards.query_pg")
        })?;

        Ok(ListCardsResponse {
            cards: records.into_iter().map(KanbanCardView::from).collect(),
        })
    }
}
