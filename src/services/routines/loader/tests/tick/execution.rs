use super::super::*;

#[test]
fn executes_tick_and_validates_action() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("complete.js");
    std::fs::write(
        &path,
        r#"
        agentdesk.routines.register({
          name: "Complete",
          tick(ctx) {
            return {
              action: "complete",
              result: { routineId: ctx.routine.id, runId: ctx.run.id },
              lastResult: "ok"
            };
          }
        });
        "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let action = loader
        .execute_tick(
            "complete.js",
            RoutineTickContext {
                routine: RoutineTickRoutine {
                    id: "routine-1".to_string(),
                    agent_id: None,
                    script_ref: "complete.js".to_string(),
                    name: "Complete".to_string(),
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
            },
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Complete {
            result_json,
            last_result,
            ..
        } => {
            assert_eq!(last_result.as_deref(), Some("ok"));
            assert_eq!(
                result_json.unwrap(),
                serde_json::json!({"routineId": "routine-1", "runId": "run-1"})
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn legacy_automation_executor_v2_ref_resolves_to_canonical_script() {
    let dir = tempfile::tempdir().unwrap();
    let monitoring_dir = dir.path().join("monitoring");
    std::fs::create_dir_all(&monitoring_dir).unwrap();
    let path = monitoring_dir.join("automation-candidate-executor.js");
    std::fs::write(
        &path,
        r#"
        agentdesk.routines.register({
          name: "Automation Candidate Executor",
          tick(ctx) {
            return {
              action: "complete",
              result: { scriptRef: ctx.routine.script_ref },
              lastResult: "legacy-compatible"
            };
          }
        });
        "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 1);
    assert!(
        loader
            .get_script(LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF)
            .unwrap()
            .is_some()
    );

    let action = loader
        .execute_tick(
            LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF,
            RoutineTickContext {
                routine: RoutineTickRoutine {
                    id: "routine-legacy".to_string(),
                    agent_id: None,
                    script_ref: LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF.to_string(),
                    name: "Legacy Automation Executor".to_string(),
                    execution_strategy: "fresh".to_string(),
                    fresh_context_guaranteed: false,
                },
                run: RoutineTickRun {
                    id: "run-legacy".to_string(),
                    lease_expires_at: chrono::Utc::now(),
                },
                agent: None,
                checkpoint: None,
                now: chrono::Utc::now(),
                observations: None,
                automation_inventory: None,
                limits: ObservationLimits::default(),
            },
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Complete {
            result_json,
            last_result,
            ..
        } => {
            assert_eq!(last_result.as_deref(), Some("legacy-compatible"));
            assert_eq!(
                result_json.unwrap(),
                serde_json::json!({"scriptRef": LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF})
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn exposes_tick_agent_idle_state_to_js() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-idle.js");
    std::fs::write(
        &path,
        r#"
        agentdesk.routines.register({
          name: "Agent Idle",
          tick(ctx) {
            if (!ctx.agent.is_idle) {
              return {
                action: "skip",
                reason: "agent not idle",
                result: { isIdle: ctx.agent.is_idle },
                lastResult: "skipped"
              };
            }

            return {
              action: "complete",
              result: { isIdle: ctx.agent.is_idle },
              lastResult: "idle"
            };
          }
        });
        "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();

    let context_for = |is_idle: bool| RoutineTickContext {
        routine: RoutineTickRoutine {
            id: "routine-1".to_string(),
            agent_id: Some("monitoring".to_string()),
            script_ref: "agent-idle.js".to_string(),
            name: "Agent Idle".to_string(),
            execution_strategy: "fresh".to_string(),
            fresh_context_guaranteed: false,
        },
        run: RoutineTickRun {
            id: "run-1".to_string(),
            lease_expires_at: chrono::Utc::now(),
        },
        agent: Some(RoutineTickAgent {
            id: "monitoring".to_string(),
            status: if is_idle { "idle" } else { "working" }.to_string(),
            is_idle,
            current_task_id: None,
            current_thread_channel_id: None,
        }),
        checkpoint: None,
        now: chrono::Utc::now(),
        observations: None,
        automation_inventory: None,
        limits: ObservationLimits::default(),
    };

    let idle_action = loader
        .execute_tick("agent-idle.js", context_for(true))
        .unwrap();
    match idle_action {
        crate::services::routines::RoutineAction::Complete {
            result_json,
            last_result,
            ..
        } => {
            assert_eq!(last_result.as_deref(), Some("idle"));
            assert_eq!(result_json.unwrap(), serde_json::json!({"isIdle": true}));
        }
        other => panic!("unexpected idle action: {other:?}"),
    }

    let working_action = loader
        .execute_tick("agent-idle.js", context_for(false))
        .unwrap();
    match working_action {
        crate::services::routines::RoutineAction::Skip {
            reason,
            result_json,
            last_result,
            ..
        } => {
            assert_eq!(reason.as_deref(), Some("agent not idle"));
            assert_eq!(last_result.as_deref(), Some("skipped"));
            assert_eq!(result_json.unwrap(), serde_json::json!({"isIdle": false}));
        }
        other => panic!("unexpected working action: {other:?}"),
    }
}
