use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct AgentOfficesResponse {
    pub offices: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentSkillsResponse {
    pub skills: Vec<Value>,
    #[serde(rename = "sharedSkills")]
    pub shared_skills: Vec<Value>,
    #[serde(rename = "totalCount")]
    pub total_count: usize,
}

#[derive(Debug, Serialize)]
pub struct AgentDispatchedSessionsResponse {
    pub sessions: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentTimelineResponse {
    pub events: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentTranscriptsResponse {
    pub agent_id: String,
    pub transcripts: Vec<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_container_response_shapes_are_stable() {
        let skills = serde_json::to_value(AgentSkillsResponse {
            skills: vec![json!({"id": "skill-1"})],
            shared_skills: Vec::new(),
            total_count: 1,
        })
        .unwrap();
        assert_eq!(
            skills,
            json!({
                "skills": [{"id": "skill-1"}],
                "sharedSkills": [],
                "totalCount": 1,
            })
        );

        let transcripts = serde_json::to_value(AgentTranscriptsResponse {
            agent_id: "agent-1".to_string(),
            transcripts: vec![json!({"turn_id": "turn-1"})],
        })
        .unwrap();
        assert_eq!(
            transcripts,
            json!({
                "agent_id": "agent-1",
                "transcripts": [{"turn_id": "turn-1"}],
            })
        );
    }
}
