use super::super::{
    ObservationLimits, RoutineScriptLoader, RoutineTickContext, RoutineTickRoutine, RoutineTickRun,
};
use super::load_single_routine_script;

#[test]
fn loads_registered_routine_script() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daily-summary.js");
    std::fs::write(
        &path,
        r#"
            agentdesk.routines.register({
              name: "Daily Summary",
              tick(ctx) {
                return { action: "complete", result: { ok: true } };
              }
            });
            "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    let script_ref = loader.load_script(dir.path(), &path).unwrap();
    assert_eq!(script_ref, "daily-summary.js");
    assert!(loader.has_script("daily-summary.js").unwrap());
    assert_eq!(loader.script_refs().unwrap(), vec!["daily-summary.js"]);
}

#[test]
fn captures_registered_routine_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("portable.js");
    std::fs::write(
        &path,
        r#"
            agentdesk.routines.register({
              name: "Portable",
              metadata: {
                migrated_launchd: {
                  entrypoint: "scripts/launchd-migrated/portable.sh",
                  required_connectors: ["obsidian_skill_root"]
                }
              },
              tick(ctx) {
                return { action: "complete", result: { ok: true } };
              }
            });
            "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let script = loader.get_script("portable.js").unwrap().unwrap();

    assert_eq!(
        script.metadata["migrated_launchd"]["entrypoint"],
        "scripts/launchd-migrated/portable.sh"
    );
    assert_eq!(
        script.metadata["migrated_launchd"]["required_connectors"][0],
        "obsidian_skill_root"
    );
}

#[test]
fn rejects_cyclic_registered_routine_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata-cycle.js");
    std::fs::write(
        &path,
        r#"
            const metadata = { migrated_launchd: { entrypoint: "scripts/launchd-migrated/test.sh" } };
            metadata.self = metadata;
            agentdesk.routines.register({
              name: "Metadata Cycle",
              metadata,
              tick(ctx) {
                return { action: "complete", result: { ok: true } };
              }
            });
            "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    let error = loader.load_script(dir.path(), &path).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("routine metadata cycle check failed")
            || message.contains("cyclic object graph"),
        "{message}"
    );
}

#[test]
fn isolates_global_bindings_between_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.js");
    let second = dir.path().join("second.js");
    let source = |name: &str| {
        format!(
            "const config = {{ name: '{name}' }}; agentdesk.routines.register({{ name: config.name, tick() {{ return {{ action: 'skip' }}; }} }});"
        )
    };
    std::fs::write(&first, source("First")).unwrap();
    std::fs::write(&second, source("Second")).unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 2);
    assert_eq!(
        loader.script_refs().unwrap(),
        vec!["first.js".to_string(), "second.js".to_string()]
    );
}

#[test]
fn quickjs_eval_error_includes_exception_message_and_stack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node-only.js");
    std::fs::write(&path, "require('node:fs');").unwrap();

    let error = load_single_routine_script(dir.path(), &path).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("require is not defined"), "{message}");
    assert!(message.contains("at <eval>"), "{message}");
}

#[test]
fn quickjs_eval_error_with_empty_message_starts_with_stack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty-message.js");
    std::fs::write(&path, "const error = new Error(''); throw error;").unwrap();

    let error = load_single_routine_script(dir.path(), &path).unwrap_err();
    let message = error.to_string();
    let detail = message
        .strip_prefix(&format!(
            "JS eval error in routine script {}: ",
            path.display()
        ))
        .unwrap();
    assert!(!detail.trim().is_empty(), "{detail:?}");
    assert_eq!(detail, detail.trim_start(), "{detail:?}");
    assert!(
        detail.lines().any(|line| !line.trim().is_empty()),
        "{detail:?}"
    );
}

#[test]
fn quickjs_eval_error_includes_primitive_throw_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("primitive-throw.js");
    std::fs::write(&path, "throw 'bad config';").unwrap();

    let error = load_single_routine_script(dir.path(), &path).unwrap_err();
    assert!(error.to_string().contains("bad config"));
}

#[test]
fn tick_error_includes_primitive_throw_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("primitive-tick.js");
    std::fs::write(
        &path,
        "agentdesk.routines.register({ name: 'Primitive Tick', tick() { throw 'tick unavailable'; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let error = loader
        .execute_tick(
            "primitive-tick.js",
            RoutineTickContext {
                routine: RoutineTickRoutine {
                    id: "routine-1".to_string(),
                    agent_id: None,
                    script_ref: "primitive-tick.js".to_string(),
                    name: "Primitive Tick".to_string(),
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
        .unwrap_err();

    let message = error.to_string();
    assert!(message.contains("tick unavailable"), "{message}");
    assert!(
        !message.contains("Exception generated by QuickJS"),
        "{message}"
    );
}

#[test]
fn rejects_cyclic_action_result_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cycle.js");
    std::fs::write(
        &path,
        r#"
            agentdesk.routines.register({
              name: "Cycle",
              tick() {
                const result = { ok: true };
                result.self = result;
                return { action: "complete", result };
              }
            });
            "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let error = loader
        .execute_tick(
            "cycle.js",
            RoutineTickContext {
                routine: RoutineTickRoutine {
                    id: "routine-1".to_string(),
                    agent_id: None,
                    script_ref: "cycle.js".to_string(),
                    name: "Cycle".to_string(),
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
        .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("cycle check failed") || message.contains("cyclic object graph"),
        "{message}"
    );
}
