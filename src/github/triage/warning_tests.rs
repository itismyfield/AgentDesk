use super::*;
use crate::github::test_support::{LogCapture, issue};

#[tokio::test]
async fn unknown_agent_caller_preserves_routing_identity_and_recovery_paths() {
    let capture = LogCapture::new();
    let _guard = tracing::dispatcher::set_default(&capture.dispatch);
    // min_connections=0: CLOSED issues exit before any database operation.
    // A nonexistent socket also prevents accidental access to a running DB.
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(
        sqlx::postgres::PgConnectOptions::new().socket("/nonexistent/5727-warning-test"),
    );
    for (label, expected_source) in [
        (Some("agent:missing-caller-agent"), "explicit label"),
        (None, "inferred routing"),
    ] {
        let labels: Vec<_> = label.into_iter().collect();
        let mut issue = issue("Dashboard frontend", None, &labels);
        let repo = format!("triage-caller/{expected_source}");
        let routing = resolve_agent_routing(&issue, true);
        let expected_agent = if label.is_some() {
            "missing-caller-agent"
        } else {
            "adk-dashboard"
        };
        let message = format!(
            "Ignoring unknown agent '{expected_agent}' from {expected_source} for {repo} issue #42"
        );
        for recovery in ["exists", "unrouted", "closed"] {
            for _ in 0..2 {
                let validated = validate_agent_routing(&repo, &issue, &routing, |id| async move {
                    assert_eq!(id, expected_agent);
                    Ok(None)
                })
                .await
                .unwrap();
                assert_eq!(validated.unknown_agent_id.as_deref(), Some(expected_agent));
                assert!(validated.assigned_agent_id.is_none());
            }
            capture.assert_levels(&message, &["WARN", "DEBUG"]);
            match recovery {
                "exists" => {
                    let validated = validate_agent_routing(&repo, &issue, &routing, |id| async {
                        Ok(Some(id))
                    })
                    .await
                    .unwrap();
                    assert_eq!(validated.assigned_agent_id.as_deref(), Some(expected_agent));
                    assert!(validated.unknown_agent_id.is_none());
                }
                "unrouted" => {
                    let unrouted = AgentRoutingResolution::Unrouted {
                        reason: UnroutedReason::NoMatch,
                        matches: vec![],
                    };
                    let validated = validate_agent_routing_pg(&pool, &repo, &issue, &unrouted)
                        .await
                        .unwrap();
                    assert!(validated.assigned_agent_id.is_none());
                }
                _ => {
                    issue.state = "CLOSED".into();
                    assert_eq!(
                        triage_new_issues_pg(&pool, &repo, &[issue.clone()])
                            .await
                            .unwrap(),
                        0
                    );
                    issue.state = "OPEN".into();
                }
            }
            capture.assert_levels(&message, &[]);
        }
        validate_agent_routing(&repo, &issue, &routing, |_| async { Ok(None) })
            .await
            .unwrap();
        capture.assert_levels(&message, &["WARN"]);
    }
    pool.close().await;
}
