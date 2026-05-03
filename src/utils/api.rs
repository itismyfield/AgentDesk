use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use std::fmt::Display;

pub(crate) type ApiJsonResponse = (StatusCode, Json<Value>);

pub(crate) fn clamp_limit(limit: Option<usize>, default: usize, max: usize) -> usize {
    limit.unwrap_or(default).clamp(1, max)
}

pub(crate) fn clamp_api_limit(limit: Option<usize>) -> usize {
    clamp_limit(limit, 50, 100)
}

pub(crate) fn error_response(status: StatusCode, error: impl Display) -> ApiJsonResponse {
    (status, Json(json!({ "error": error.to_string() })))
}

pub(crate) fn bad_request(error: impl Display) -> ApiJsonResponse {
    error_response(StatusCode::BAD_REQUEST, error)
}

pub(crate) fn internal_error(error: impl Display) -> ApiJsonResponse {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error)
}

pub(crate) fn not_found(message: &'static str) -> ApiJsonResponse {
    error_response(StatusCode::NOT_FOUND, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_api_limit_defaults_and_bounds() {
        assert_eq!(clamp_api_limit(None), 50);
        assert_eq!(clamp_api_limit(Some(0)), 1);
        assert_eq!(clamp_api_limit(Some(12)), 12);
        assert_eq!(clamp_api_limit(Some(250)), 100);
    }

    #[test]
    fn error_helpers_preserve_simple_error_shape() {
        let (status, Json(body)) = internal_error("database unavailable");

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({ "error": "database unavailable" }));
    }
}
