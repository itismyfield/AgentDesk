use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AnalyticsErrorResponse {
    pub error: String,
}
