use axum::{Json, http::StatusCode};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct ServiceError {
    status: StatusCode,
    message: String,
}

pub type ServiceResult<T> = Result<T, ServiceError>;

impl ServiceError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub fn into_json_response(self) -> (StatusCode, Json<Value>) {
        (self.status, Json(json!({"error": self.message})))
    }
}
