use super::super::*;

pub(super) fn automation_recommender_context(
    checkpoint: Option<serde_json::Value>,
    observations: Vec<serde_json::Value>,
    automation_inventory: Vec<serde_json::Value>,
    now: chrono::DateTime<chrono::Utc>,
) -> RoutineTickContext {
    RoutineTickContext {
        routine: RoutineTickRoutine {
            id: "routine-automation".to_string(),
            agent_id: Some("maker".to_string()),
            script_ref: "monitoring/automation-candidate-recommender.js".to_string(),
            name: "automation-candidate-recommender".to_string(),
            execution_strategy: "fresh".to_string(),
            fresh_context_guaranteed: false,
        },
        run: RoutineTickRun {
            id: "run-automation".to_string(),
            lease_expires_at: now,
        },
        agent: None,
        checkpoint,
        now,
        observations: Some(observations),
        automation_inventory: Some(automation_inventory),
        limits: ObservationLimits::default(),
    }
}

pub(super) fn automation_recommender_loader() -> RoutineScriptLoader {
    let root = fixture_routines_root();
    let loader = RoutineScriptLoader::new().unwrap();
    loader
        .load_script(
            &root,
            &root.join("monitoring/automation-candidate-recommender.js"),
        )
        .unwrap();
    loader
}

pub(super) fn routine_observation(
    signature: &str,
    weight: u8,
    timestamp: &str,
) -> serde_json::Value {
    serde_json::json!({
        "timestamp": timestamp,
        "source": "routine_result",
        "category": "routine-candidate",
        "signature": signature,
        "summary": "routine completed with repeated evidence",
        "weight": weight,
        "evidence_ref": format!("routine_run:{signature}:{timestamp}"),
    })
}

pub(super) fn routine_observations(
    signature: &str,
    weight: u8,
    count: usize,
) -> Vec<serde_json::Value> {
    (0..count)
        .map(|index| {
            routine_observation(signature, weight, &format!("2026-04-30T06:59:{index:02}Z"))
        })
        .collect()
}

pub(super) fn categorized_observation(
    signature: &str,
    category: &str,
    source: &str,
    occurrences: u8,
    timestamp: &str,
) -> serde_json::Value {
    serde_json::json!({
        "timestamp": timestamp,
        "source": source,
        "category": category,
        "signature": signature,
        "summary": format!("{category} repeated evidence"),
        "weight": 2,
        "occurrences": occurrences,
        "evidence_ref": format!("{source}:{signature}:{timestamp}"),
    })
}
