use axum::{
    Json,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::{
    db::server_campaigns::{
        CommandOutcome, CommandReply, ReceiptResponse, StoreError, StoreResult,
    },
    error::{AppError, AppResult, ErrorCode},
    services::campaign_execution::{
        definition,
        model::{Rejection, RejectionCode},
    },
};

pub(super) fn id(id: &str) -> AppResult<()> {
    if definition::valid_id(id) {
        Ok(())
    } else {
        Err(AppError::bad_request("invalid campaign ID"))
    }
}

pub(super) fn revision(value: u64) -> AppResult<()> {
    if value > 0 && value <= i64::MAX as u64 {
        Ok(())
    } else {
        Err(AppError::bad_request(
            "expected_definition_revision must be a positive signed 64-bit integer",
        ))
    }
}

pub(super) fn key(value: &str) -> AppResult<()> {
    if value.trim().is_empty() {
        Err(AppError::bad_request("idempotency_key must be nonempty"))
    } else {
        Ok(())
    }
}

pub(super) fn body<T>(input: Result<Json<T>, JsonRejection>) -> AppResult<T> {
    input.map(|Json(body)| body).map_err(|error| {
        let status = match error.status() {
            StatusCode::PAYLOAD_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
            StatusCode::UNSUPPORTED_MEDIA_TYPE => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            _ => StatusCode::BAD_REQUEST,
        };
        AppError::new(status, ErrorCode::Validation, error.body_text())
    })
}

pub(super) fn store_error(error: StoreError) -> AppError {
    match error {
        StoreError::InvalidIdentity => {
            AppError::bad_request("invalid campaign ID or empty idempotency_key")
        }
        StoreError::IdempotencyConflict => {
            AppError::conflict("idempotency_key already used for a different request")
        }
        StoreError::Sql(error) => {
            tracing::error!(%error, "server campaign database operation failed");
            if matches!(
                error,
                sqlx::Error::PoolClosed | sqlx::Error::PoolTimedOut | sqlx::Error::Io(_)
            ) {
                return AppError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::Database,
                    "canonical PostgreSQL pool unavailable",
                );
            }
            AppError::internal("server campaign database operation failed")
                .with_code(ErrorCode::Database)
        }
        StoreError::Json(_) | StoreError::InconsistentState => {
            AppError::internal("server campaign stored data is inconsistent")
        }
    }
}

fn rejection(rejection: &Rejection) -> AppError {
    let error = match rejection.code {
        RejectionCode::InvalidDefinition | RejectionCode::MissingChangeReason => {
            AppError::bad_request("server campaign command rejected")
        }
        _ => AppError::conflict("server campaign command rejected"),
    };
    let error = error
        .with_context("rejection", rejection.code)
        .with_context("target", &rejection.target);
    if let Some(detail) = &rejection.definition_error {
        error.with_context("definition_error", detail)
    } else {
        error
    }
}

fn render(outcome: &CommandOutcome, status: StatusCode) -> StoreResult<ReceiptResponse> {
    let (status, body) = match outcome {
        CommandOutcome::Decided(Ok(result)) => {
            (status, json!({"result": serde_json::to_value(result)?}))
        }
        CommandOutcome::Decided(Err(error)) => {
            let error = rejection(error);
            (error.status(), error.to_json_value())
        }
        CommandOutcome::NotFound => {
            let error = AppError::not_found("server campaign not found");
            (error.status(), error.to_json_value())
        }
    };
    Ok(ReceiptResponse {
        status: status.as_u16(),
        body,
    })
}

pub(super) fn created(outcome: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    render(outcome, StatusCode::CREATED)
}
pub(super) fn edited(outcome: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    render(outcome, StatusCode::OK)
}
pub(super) fn commanded(outcome: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    render(outcome, StatusCode::ACCEPTED)
}

pub(super) fn saved(reply: CommandReply) -> AppResult<Response> {
    let status = StatusCode::from_u16(reply.response.status)
        .map_err(|_| AppError::internal("invalid stored response status"))?;
    Ok((status, Json(reply.response.body)).into_response())
}
