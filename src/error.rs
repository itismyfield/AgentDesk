use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AutoQueue,
    Config,
    Conflict,
    Database,
    Dispatch,
    Discord,
    Internal,
    Kanban,
    NotFound,
    Policy,
    Queue,
    Settings,
    Validation,
}

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct AppError {
    status: StatusCode,
    code: ErrorCode,
    message: String,
    context: Map<String, Value>,
}

impl AppError {
    pub fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            context: Map::new(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::Validation, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, ErrorCode::Conflict, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            message,
        )
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
    }

    pub fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = code;
        self
    }

    pub fn with_context(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).unwrap_or_else(|error| {
            json!({
                "serialization_error": format!("{error}")
            })
        });
        self.context.insert(key.into(), value);
        self
    }

    pub fn with_operation(self, operation: impl Into<String>) -> Self {
        self.with_context("operation", operation.into())
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn context(&self) -> &Map<String, Value> {
        &self.context
    }

    pub fn to_json_value(&self) -> Value {
        json!({
            "error": self.message,
            "code": self.code,
            "context": self.context,
        })
    }

    pub fn into_json_response(self) -> (StatusCode, Json<Value>) {
        let status = self.status;
        let body = self.to_json_value();
        (status, Json(body))
    }

    pub fn to_policy_json_value(&self) -> Value {
        let mut value = self.to_json_value();
        if let Some(obj) = value.as_object_mut() {
            obj.insert("ok".to_string(), Value::Bool(false));
        }
        value
    }

    pub fn into_policy_json_string(self) -> String {
        self.to_policy_json_value().to_string()
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, body) = self.into_json_response();
        (status, body).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(format!("IO error: {error}")).with_code(ErrorCode::Config)
    }
}

impl From<String> for AppError {
    fn from(message: String) -> Self {
        Self::internal(message)
    }
}

impl From<&str> for AppError {
    fn from(message: &str) -> Self {
        Self::internal(message.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_json_includes_code_and_context() {
        let (status, Json(body)) = AppError::bad_request("invalid input")
            .with_code(ErrorCode::Dispatch)
            .with_context("dispatch_id", "dispatch-123")
            .into_json_response();

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid input");
        assert_eq!(body["code"], "dispatch");
        assert_eq!(body["context"]["dispatch_id"], "dispatch-123");
    }

    #[test]
    fn policy_error_json_sets_ok_false() {
        let body = AppError::internal("policy bridge unavailable")
            .with_code(ErrorCode::Policy)
            .with_operation("emit_signal_json.runtime_supervisor")
            .to_policy_json_value();

        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "policy bridge unavailable");
        assert_eq!(body["code"], "policy");
        assert_eq!(
            body["context"]["operation"],
            "emit_signal_json.runtime_supervisor"
        );
    }
}
