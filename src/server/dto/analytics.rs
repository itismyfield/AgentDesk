use serde::Serialize;

#[allow(unused_imports)]
pub use crate::services::analytics::{
    AchievementsResponse, ActivityHeatmapResponse, AnalyticsResponse, AuditLogsResponse,
    InvariantsResponse, MachineStatusResponse, ObservabilityResponse, PolicyHooksResponse,
    QualityEventsResponse, RateLimitsResponse, SkillsTrendResponse, StreaksResponse,
};

#[derive(Debug, Serialize)]
pub struct AnalyticsErrorResponse {
    pub error: String,
}
