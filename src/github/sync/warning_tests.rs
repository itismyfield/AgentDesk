use super::*;
use crate::github::test_support::{LogCapture, issue};

#[test]
fn terminal_open_caller_warns_repeats_and_recovers_from_loaded_state() {
    let repo = "sync-caller/terminal";
    let capture = LogCapture::new();
    let _guard = tracing::dispatcher::set_default(&capture.dispatch);
    let mut issue = issue("Terminal card", None, &[]);
    let mut card = PgCardRecord {
        id: "card-42".into(),
        status: "shipped".into(),
        review_status: None,
        latest_dispatch_id: None,
        assigned_agent_id: None,
    };
    let pipeline = serde_json::from_value(serde_json::json!({
        "name": "warning-test", "version": 1, "transitions": [],
        "states": [
            {"id": "backlog", "label": "Backlog"},
            {"id": "shipped", "label": "Shipped", "terminal": true}
        ]
    }))
    .unwrap();
    let message = "sync-caller/terminal#42: card card-42 is terminal but issue is OPEN";
    for recovery in ["CLOSED", "nonterminal"] {
        for _ in 0..2 {
            assert!(card_state::observe(repo, &issue, &card, &pipeline));
        }
        capture.assert_levels(message, &["WARN", "DEBUG"]);
        if recovery == "CLOSED" {
            issue.state = "CLOSED".into();
        } else {
            card.status = "backlog".into();
        }
        assert_eq!(
            card_state::observe(repo, &issue, &card, &pipeline),
            recovery == "CLOSED"
        );
        capture.assert_levels(message, &[]);
        issue.state = "OPEN".into();
        card.status = "shipped".into();
    }
    assert!(card_state::observe(repo, &issue, &card, &pipeline));
    capture.assert_levels(message, &["WARN"]);
}

#[test]
fn stale_reconcile_caller_keeps_repeats_debug_and_changed_errors_warn() {
    let repo = "sync-caller/stale";
    let capture = LogCapture::new();
    let _guard = tracing::dispatcher::set_default(&capture.dispatch);
    let mut result = SyncResult::default();
    let mut issues = vec![issue("Fetched issue", None, &[])];
    for error in [
        Some("GraphQL unavailable"),
        Some("GraphQL unavailable"),
        None,
        Some("GraphQL unavailable"),
        Some("issue 7 missing"),
    ] {
        apply_stale_reconcile_fetch_report(
            repo,
            &mut result,
            &mut issues,
            3,
            StaleIssueFetchReport {
                batch_count: 1,
                error_count: usize::from(error.is_some()),
                errors: error.map(str::to_owned).into_iter().collect(),
                ..Default::default()
            },
        );
    }
    capture.assert_levels(
        "sync-caller/stale: stale card reconcile",
        &["WARN", "DEBUG", "DEBUG", "WARN"],
    );
    assert_eq!(result.stale_card_issue_error_count, 4);
    assert_eq!(result.stale_card_issue_check_count, 15);
    assert_eq!(issues.len(), 1);
}
