use super::*;
use CheckStatus::{Fail, Pass, Warn};
use stale_mailbox_repair::apply_stale_mailbox_fixes_with_post;

fn fixture() -> Value {
    json!({"status":"healthy", "degraded_reasons":[], "global_active":0,
        "providers":[{"name":"discord", "connected":true}], "mailboxes":[{
        "channel_id":42, "has_cancel_token":true, "queue_depth":0,
        "watcher_attached":false, "inflight_state_present":true,
        "tmux_present":false, "process_present":false, "active_dispatch_present":false,
        "session_record_present":false, "session_status":null, "session_active_dispatch_id":null}]})
}

fn snapshot(body: Value) -> HealthSnapshot {
    HealthSnapshot {
        base: "injected".into(),
        body: Some(body),
        error: None,
    }
}

fn replace(body: &mut Value, key: &str, value: Option<Value>) {
    let object = body.as_object_mut().unwrap();
    if let Some(value) = value {
        object.insert(key.into(), value);
    } else {
        object.remove(key);
    }
}

fn unavailable_cases() -> Vec<(Value, String)> {
    let mut cases = Vec::new();
    for key in [
        "has_cancel_token",
        "watcher_attached",
        "inflight_state_present",
        "tmux_present",
        "process_present",
        "active_dispatch_present",
        "queue_depth",
        "channel_id",
    ] {
        for (cancel, queue, tmux, inflight, record) in [
            (true, 0, false, true, false),
            (false, 0, false, true, false),
            (false, 0, false, false, true),
            (true, 3, true, true, false),
        ] {
            for value in [None, Some(Value::Null), Some(json!("wrong"))] {
                let mut body = fixture();
                for (key, value) in [
                    ("has_cancel_token", json!(cancel)),
                    ("queue_depth", json!(queue)),
                    ("tmux_present", json!(tmux)),
                    ("inflight_state_present", json!(inflight)),
                    ("session_record_present", json!(record)),
                    ("session_status", json!("working")),
                ] {
                    body["mailboxes"][0][key] = value;
                }
                let code = match value.as_ref() {
                    None => "missing",
                    Some(Value::Null) => "null",
                    _ => "wrong_type",
                };
                replace(&mut body["mailboxes"][0], key, value);
                cases.push((body, format!("{key}/{code}")));
            }
        }
    }
    for value in [json!(-1), json!(0.5)] {
        let mut body = fixture();
        body["mailboxes"][0]["queue_depth"] = value;
        cases.push((body, "queue_depth".into()));
    }
    for (key, value) in [
        ("channel_id", json!(0)),
        ("session_active_dispatch_id", json!(false)),
    ] {
        let mut body = fixture();
        body["mailboxes"][0][key] = value;
        cases.push((body, key.into()));
    }
    for key in ["session_record_present", "session_status"] {
        let mut body = fixture();
        body["mailboxes"][0]["has_cancel_token"] = json!(false);
        body["mailboxes"][0]["inflight_state_present"] = json!(false);
        body["mailboxes"][0]["session_record_present"] = json!(true);
        body["mailboxes"][0].as_object_mut().unwrap().remove(key);
        cases.push((body, key.into()));
    }
    let mut body = fixture();
    body["mailboxes"][0]["process_present"] = json!(true);
    body["mailboxes"][0]
        .as_object_mut()
        .unwrap()
        .remove("tmux_present");
    cases.push((body, "tmux_present".into()));
    cases
}

#[test]
fn observations_preserve_unmeasured_diagnostics() {
    for (value, expected, field) in [
        (None, Fail, "degraded_reasons/missing"),
        (Some(Value::Null), Fail, "degraded_reasons/null"),
        (Some(json!(7)), Fail, "degraded_reasons/wrong_type"),
        (Some(json!({})), Fail, "degraded_reasons/wrong_type"),
        (
            Some(json!([{}])),
            Fail,
            "degraded_reasons[0]/invalid_element",
        ),
        (
            Some(json!(["db_unavailable", null])),
            Fail,
            "degraded_reasons[1]/invalid_element",
        ),
        (Some(json!([])), Pass, ""),
        (Some(json!(["future_reason"])), Warn, ""),
        (Some(json!(["db_unavailable"])), Fail, ""),
    ] {
        let mut body = fixture();
        replace(&mut body, "degraded_reasons", value);
        let check = check_degraded_reasons(&snapshot(body.clone()));
        assert_eq!(check.status, expected, "{body}");
        if !field.is_empty() {
            assert_eq!(check.fix_safety, FixSafety::NotFixable);
            assert!(check.actual.as_deref().unwrap().contains(field));
            assert!(check.detail.contains("unmeasured"));
            let mut degraded = body.clone();
            degraded["status"] = json!("degraded");
            let server = check_server_running(&snapshot(degraded.clone()));
            assert!(server.detail.contains("unmeasured"));
            assert!(
                server
                    .evidence
                    .as_ref()
                    .unwrap()
                    .get("degraded_reasons")
                    .is_none()
            );
            assert!(
                discord_bot_check_from_health("injected", &degraded)
                    .detail
                    .contains("unmeasured")
            );
        }
        assert_eq!(check_server_running(&snapshot(body.clone())).status, Pass);
        assert_eq!(
            discord_bot_check_from_health("injected", &body).status,
            Pass
        );
    }
    for (body, field) in unavailable_cases() {
        let checks = check_mailbox_consistency(&snapshot(body));
        let check = checks
            .iter()
            .find(|check| check.detail.contains(&field))
            .expect(&field);
        assert_eq!(check.status, Fail);
        assert_eq!(check.fix_safety, FixSafety::NotFixable);
        assert!(
            check
                .next_steps
                .iter()
                .all(|step| !step.contains("--fix") && !step.contains("POST"))
        );
    }
    for (value, count) in [
        (None, 0),
        (Some(json!([])), 0),
        (Some(Value::Null), 1),
        (Some(json!({})), 1),
    ] {
        let mut body = fixture();
        replace(&mut body, "mailboxes", value);
        let checks = check_mailbox_consistency(&snapshot(body));
        assert_eq!(checks.len(), count);
        if count == 1 {
            assert_eq!(checks[0].status, Warn);
            assert_eq!(checks[0].fix_safety, FixSafety::NotFixable);
        }
    }
    for value in [None, Some(Value::Null), Some(json!(-1))] {
        let mut body = fixture();
        replace(&mut body, "global_active", value);
        assert!(
            check_mailbox_consistency(&snapshot(body))
                .iter()
                .any(|check| check.detail.contains("global_active")
                    && check.fix_safety == FixSafety::NotFixable)
        );
    }
}

#[test]
fn observations_gate_stale_mailbox_posts() {
    let mut cases: Vec<_> = unavailable_cases()
        .into_iter()
        .map(|(body, field)| (body, 0, field))
        .collect();
    for reasons in [
        None,
        Some(Value::Null),
        Some(json!(["db_unavailable", null])),
        Some(json!({})),
    ] {
        let mut body = fixture();
        if let Some(reasons) = reasons {
            body["degraded_reasons"] = reasons;
        } else {
            body.as_object_mut().unwrap().remove("degraded_reasons");
        }
        cases.push((body, 0, "degraded_reasons".into()));
    }
    for key in [
        "tmux_present",
        "process_present",
        "active_dispatch_present",
        "session_active_dispatch_id",
    ] {
        let mut body = fixture();
        body["mailboxes"][0][key] = if key == "session_active_dispatch_id" {
            json!("dispatch")
        } else {
            json!(true)
        };
        cases.push((body, 0, "live".into()));
    }
    for value in [None, Some(Value::Null), Some(json!(""))] {
        let mut body = fixture();
        replace(
            &mut body["mailboxes"][0],
            "session_active_dispatch_id",
            value,
        );
        cases.push((body, 1, String::new()));
    }
    let mut queued = fixture();
    queued["mailboxes"][0]["queue_depth"] = json!(3);
    queued["degraded_reasons"] = json!(["future_reason"]);
    cases.push((queued, 1, String::new()));
    let mut stale = fixture();
    stale["mailboxes"][0]["has_cancel_token"] = json!(false);
    cases.push((stale, 1, String::new()));
    for value in [None, Some(Value::Null), Some(json!({})), Some(json!([]))] {
        let mut body = fixture();
        replace(&mut body, "mailboxes", value);
        cases.push((body, 0, String::new()));
    }
    for context in [
        RunContext::ManualCli,
        RunContext::StartupOnce,
        RunContext::RestartFollowup,
    ] {
        let options = DoctorOptions {
            fix: true,
            json: true,
            allow_restart: true,
            repair_sqlite_cache: true,
            allow_remote: false,
            profile: None,
            run_context: context,
            artifact_path: None,
        };
        for (body, count, field) in &cases {
            let mut requests = Vec::new();
            let actions = apply_stale_mailbox_fixes_with_post(
                &snapshot(body.clone()),
                &options,
                |path, request| {
                    requests.push((path.to_string(), request));
                    Ok(json!({"status":"applied"}))
                },
            );
            assert_eq!(requests.len(), *count, "{context:?} {body}");
            if *count == 1 {
                assert_eq!(
                    requests[0],
                    (
                        "/api/doctor/stale-mailbox/repair".into(),
                        json!({"channel_id":42, "expected_has_cancel_token":body["mailboxes"][0]["has_cancel_token"]})
                    )
                );
            }
            if field == "live" {
                if context == RunContext::StartupOnce {
                    assert!(actions.is_empty());
                } else {
                    assert_eq!(
                        actions[0].skipped_reason.as_deref(),
                        Some("live tmux/process/dispatch evidence present")
                    );
                }
            } else if !field.is_empty() {
                let action = actions
                    .iter()
                    .find(|action| action.safety_gate == "measurement_unavailable")
                    .expect(field);
                assert!(action.skipped && !action.requires_explicit_consent);
                assert_eq!(action.fix_safety, FixSafety::NotFixable);
                assert!(action.skipped_reason.as_deref().unwrap().contains(field));
            }
        }
        assert!(
            apply_stale_mailbox_fixes_with_post(
                &HealthSnapshot {
                    base: "injected".into(),
                    body: None,
                    error: Some("offline".into())
                },
                &options,
                |_, _| panic!("unavailable snapshot must not POST")
            )
            .is_empty()
        );
    }
}
