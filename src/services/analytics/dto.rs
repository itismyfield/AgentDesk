use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct AnalyticsResponse {
    pub generated_at: String,
    pub counters: Vec<Value>,
    pub events: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct QualityEventsResponse {
    pub events: Vec<Value>,
    pub generated_at_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct InvariantsResponse {
    pub generated_at: String,
    pub total_violations: i64,
    pub counts: Vec<Value>,
    pub recent: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct ObservabilityResponse {
    pub counters: Value,
    pub recent_events: Value,
    pub watcher_first_relay: Value,
    pub generated_at_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct PolicyHooksResponse {
    pub events: Vec<Value>,
    pub generated_at_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct AuditLogsResponse {
    pub logs: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct SkillsTrendResponse {
    pub trend: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct RateLimitsResponse {
    pub providers: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct StreaksResponse {
    pub streaks: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AchievementsResponse {
    pub achievements: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct ActivityHeatmapResponse {
    pub hours: Vec<Value>,
    pub date: String,
}

#[derive(Debug, Serialize)]
pub struct MachineStatusResponse {
    pub machines: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct PolicyHooksParams {
    pub policy_name: Option<String>,
    pub hook_name: Option<String>,
    pub last_minutes: Option<i64>,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct AuditLogsParams<'a> {
    pub limit: i64,
    pub entity_type: Option<&'a str>,
    pub entity_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_dtos_serialize_stable_fields() {
        let analytics = AnalyticsResponse {
            generated_at: "2026-05-06T00:00:00Z".to_string(),
            counters: vec![json!({"name": "counter"})],
            events: vec![json!({"event_type": "queue_event"})],
        };
        let machine_status = MachineStatusResponse {
            machines: vec![json!({"name": "local", "online": true})],
        };

        assert_eq!(
            serde_json::to_value(analytics).unwrap()["generated_at"],
            json!("2026-05-06T00:00:00Z")
        );
        assert_eq!(
            serde_json::to_value(machine_status).unwrap()["machines"][0]["online"],
            json!(true)
        );
    }
}
