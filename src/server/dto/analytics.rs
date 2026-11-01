use serde::Serialize;

#[allow(unused_imports)]
pub use crate::services::analytics::{
    ActivityHeatmapResponse, AnalyticsResponse, AuditLogsResponse, InvariantsResponse,
    MachineStatusResponse, ObservabilityResponse, PolicyHooksResponse, RateLimitsResponse,
    SkillsTrendResponse,
};

#[derive(Debug, Serialize)]
pub struct AnalyticsErrorResponse {
    pub error: String,
}
