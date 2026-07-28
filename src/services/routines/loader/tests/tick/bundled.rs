use super::super::*;

#[test]
fn bundled_sample_routines_load_and_validate() {
    // Operator routines live in gitignored `routines/` in real deployments.
    // Keep this battery hermetic by validating the tracked fixture contract.
    let root = fixture_routines_root();
    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(&root).unwrap(), 8);
    assert_eq!(
        loader.script_refs().unwrap(),
        vec![
            "agent-checkpoint-review.js".to_string(),
            "family-profile-probe-obujang.js".to_string(),
            "family-profile-probe-yohoejang.js".to_string(),
            "migrated-launchd/cookingheart-daily-briefing.js".to_string(),
            "migrated-launchd/queue-stability-batch.js".to_string(),
            "monitoring/automation-candidate-recommender.js".to_string(),
            "monitoring/working-watchdog.js".to_string(),
            "script-summary.js".to_string(),
        ]
    );

    let context_for = |script_ref: &str, name: &str| RoutineTickContext {
        routine: RoutineTickRoutine {
            id: "routine-1".to_string(),
            agent_id: Some("maker".to_string()),
            script_ref: script_ref.to_string(),
            name: name.to_string(),
            execution_strategy: "fresh".to_string(),
            fresh_context_guaranteed: false,
        },
        run: RoutineTickRun {
            id: "run-1".to_string(),
            lease_expires_at: chrono::Utc::now(),
        },
        agent: None,
        checkpoint: None,
        now: chrono::Utc::now(),
        observations: None,
        automation_inventory: None,
        limits: ObservationLimits::default(),
    };

    assert!(matches!(
        loader
            .execute_tick(
                "script-summary.js",
                context_for("script-summary.js", "script-only-summary")
            )
            .unwrap(),
        crate::services::routines::RoutineAction::Complete { .. }
    ));
    assert!(matches!(
        loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                context_for(
                    "monitoring/automation-candidate-recommender.js",
                    "automation-candidate-recommender"
                )
            )
            .unwrap(),
        crate::services::routines::RoutineAction::Complete { .. }
    ));
    assert!(matches!(
        loader
            .execute_tick(
                "monitoring/working-watchdog.js",
                context_for(
                    "monitoring/working-watchdog.js",
                    "monitoring-working-watchdog"
                )
            )
            .unwrap(),
        crate::services::routines::RoutineAction::Complete { .. }
    ));
    assert!(matches!(
        loader
            .execute_tick(
                "agent-checkpoint-review.js",
                context_for("agent-checkpoint-review.js", "agent-checkpoint-review")
            )
            .unwrap(),
        crate::services::routines::RoutineAction::Agent { .. }
    ));
    // Spot-check one of the migrated launchd routines: must return Agent.
    assert!(matches!(
        loader
            .execute_tick(
                "migrated-launchd/cookingheart-daily-briefing.js",
                context_for(
                    "migrated-launchd/cookingheart-daily-briefing.js",
                    "cookingheart-daily-briefing"
                )
            )
            .unwrap(),
        crate::services::routines::RoutineAction::Agent { .. }
    ));
    assert!(matches!(
        loader
            .execute_tick(
                "migrated-launchd/queue-stability-batch.js",
                context_for(
                    "migrated-launchd/queue-stability-batch.js",
                    "queue-stability-batch"
                )
            )
            .unwrap(),
        crate::services::routines::RoutineAction::Agent { .. }
    ));
}

#[test]
fn family_profile_probe_agent_action_defers_daily_marker_until_delivery() {
    let root = fixture_routines_root();
    let loader = RoutineScriptLoader::new().unwrap();
    loader
        .load_script(&root, &root.join("family-profile-probe-obujang.js"))
        .unwrap();

    let now = chrono::DateTime::parse_from_rfc3339("2026-05-30T03:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let action = loader
        .execute_tick(
            "family-profile-probe-obujang.js",
            RoutineTickContext {
                routine: RoutineTickRoutine {
                    id: "routine-family-profile".to_string(),
                    agent_id: Some("family-counsel".to_string()),
                    script_ref: "family-profile-probe-obujang.js".to_string(),
                    name: "family-profile-probe-obujang".to_string(),
                    execution_strategy: "fresh".to_string(),
                    fresh_context_guaranteed: false,
                },
                run: RoutineTickRun {
                    id: "run-family-profile".to_string(),
                    lease_expires_at: now,
                },
                agent: None,
                checkpoint: Some(serde_json::json!({
                    "plan": {"date": "2026-05-30", "hour": 12, "minute": 0}
                })),
                now,
                observations: None,
                automation_inventory: None,
                limits: ObservationLimits::default(),
            },
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Agent {
            dm_user_id,
            checkpoint,
            ..
        } => {
            assert_eq!(dm_user_id.as_deref(), Some("343742347365974026"));
            let checkpoint = checkpoint.expect("agent checkpoint");
            assert!(
                checkpoint.get("lastTriggeredDate").is_none(),
                "generated-but-undelivered DM must not consume today's marker"
            );
            assert_eq!(
                checkpoint
                    .pointer("/pendingDelivery/kind")
                    .and_then(serde_json::Value::as_str),
                Some("family-profile-probe")
            );
            assert_eq!(
                checkpoint
                    .pointer("/pendingDelivery/triggerDate")
                    .and_then(serde_json::Value::as_str),
                Some("2026-05-30")
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}
