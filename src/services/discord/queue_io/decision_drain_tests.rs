//! A request's own claim of the Decision source is refused at every entry that takes a source.
use super::*;
use serde_json::json;

#[test]
fn decision_source_named_by_a_request_is_self_declared() {
    let declared = [
        (Some("decision"), None),
        (Some(" Decision "), None),
        (None, Some(json!({"human_input": {"source": "decision"}}))),
        (
            Some("external"),
            Some(json!({"human_input": {"origin_id": "decision:anything"}})),
        ),
    ];
    for (source, metadata) in declared {
        assert!(self_declared(source, metadata.as_ref()), "{source:?}");
        assert_eq!(
            reject_handoff_source(source, metadata.as_ref()),
            Err(SOURCE_REFUSED)
        );
    }
    let plain = [
        (None, None),
        (Some("imessage"), None),
        (Some("decisions"), None),
        (
            Some("external"),
            Some(json!({"human_input": {"origin_id": "guid-1"}})),
        ),
    ];
    for (source, metadata) in plain {
        assert_eq!(reject_handoff_source(source, metadata.as_ref()), Ok(()));
    }
}

/// The service entry refuses before it reads the agent, so a caller that skips the HTTP route
/// reaches no backend either: the lazy pool below would fail any query.
#[tokio::test]
async fn decision_handoff_service_entry_refuses_before_any_backend() {
    use crate::services::discord::agent_handoff::{
        AgentHandoffChannelKind, start_agent_handoff_turn,
    };
    let registry = crate::services::discord::health::HealthRegistry::new();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://decision-handoff@127.0.0.1:1/none")
        .unwrap();
    let cases = [
        (Some("decision".to_string()), None),
        (None, Some(json!({"human_input": {"source": "decision"}}))),
    ];
    for (source, metadata) in cases {
        let started = start_agent_handoff_turn(
            &registry,
            &pool,
            "from-agent",
            "to-agent",
            "prompt",
            AgentHandoffChannelKind::Cc,
            true,
            None,
            source,
            metadata,
        )
        .await;
        let Err(refused) = started else {
            panic!("a decision handoff started a turn");
        };
        assert_eq!(refused.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(refused.body()["error"], SOURCE_REFUSED);
    }
}
