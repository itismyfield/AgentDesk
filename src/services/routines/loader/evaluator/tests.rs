use super::super::{
    ObservationLimits, RoutineScriptLoader, RoutineTickContext, RoutineTickRoutine, RoutineTickRun,
};
use super::{
    JsonContainerPolicy, RetainedOutputBudget, capture_json_container_class_ids,
    create_plain_object_classifier, evaluate_tick_action_with_retained_output_limit,
    js_value_to_json_with_budgets, js_value_to_json_with_byte_budget, load_single_routine_script,
    validate_routine_script_source, validate_routine_script_source_with_budget,
    with_bounded_quickjs_context,
};
use std::path::Path;

fn validate_test_source(source: &str) -> anyhow::Result<super::ValidatedRoutineSource> {
    validate_routine_script_source(
        source,
        "adversarial",
        "adversarial.js",
        Path::new("adversarial.js"),
    )
}

fn assert_source_rejected(source: &str, expected: &str) {
    let error = validate_test_source(source).unwrap_err();
    let message = error.to_string();
    assert!(message.contains(expected), "{message}");
}

fn validate_test_source_with_retained_limit(
    source: &str,
    fallback_name: &str,
    maximum_retained_output_bytes: usize,
) -> (
    anyhow::Result<super::ValidatedRoutineSource>,
    RetainedOutputBudget,
) {
    let retained_output_budget = RetainedOutputBudget::new(maximum_retained_output_bytes);
    let result = validate_routine_script_source_with_budget(
        source,
        fallback_name,
        "adversarial.js",
        Path::new("adversarial.js"),
        &retained_output_budget,
    );
    (result, retained_output_budget)
}

fn convert_test_json_with_byte_budget(
    source: &str,
    maximum_converted_bytes: usize,
) -> anyhow::Result<serde_json::Value> {
    with_bounded_quickjs_context(|ctx| {
        let json_container_policy = JsonContainerPolicy {
            class_ids: capture_json_container_class_ids(ctx.clone())?,
            plain_object_classifier: create_plain_object_classifier(ctx.clone())?,
        };
        let value: rquickjs::Value = ctx
            .eval(source.as_bytes().to_vec())
            .map_err(|e| anyhow::anyhow!("test JSON eval failed: {e}"))?;
        js_value_to_json_with_byte_budget(
            value,
            "test JSON",
            &json_container_policy,
            maximum_converted_bytes,
        )
    })
}

fn tick_context(script_ref: &str, name: &str) -> RoutineTickContext {
    RoutineTickContext {
        routine: RoutineTickRoutine {
            id: "routine-1".to_string(),
            agent_id: None,
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
    }
}

#[test]
fn zero_call_capture_assignment_cannot_forge_registration() {
    assert_source_rejected(
        r#"
        globalThis.__routineCapture = {
          captured: { name: "Forged", tick() { return { action: "skip" }; } }
        };
        "#,
        "did not call agentdesk.routines.register()",
    );
}

#[test]
fn rejects_multiple_register_invocations() {
    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "First",
          tick() { return { action: "skip" }; }
        });
        agentdesk.routines.register({
          name: "Second",
          tick() { return { action: "skip" }; }
        });
        "#,
        "must call agentdesk.routines.register() exactly once (got 2)",
    );
}

#[test]
fn registration_getter_reentry_is_counted_without_refcell_panic() {
    assert_source_rejected(
        r#"
        const outer = {
          tick() { return { action: "skip" }; }
        };
        Object.defineProperty(outer, "name", {
          enumerable: true,
          get() {
            agentdesk.routines.register({
              name: "Nested",
              tick() { return { action: "skip" }; }
            });
            return "Outer";
          }
        });
        agentdesk.routines.register(outer);
        "#,
        "must call agentdesk.routines.register() exactly once (got 2)",
    );
}

#[test]
fn fake_register_assignment_cannot_replace_capture() {
    let validated = validate_test_source(
        r#"
        agentdesk.routines.register = function fakeRegister() {};
        agentdesk.routines.register({
          name: "Protected",
          tick() { return { action: "skip" }; }
        });
        "#,
    )
    .unwrap();

    assert_eq!(validated.name, "Protected");
}

#[test]
fn global_this_agentdesk_assignment_cannot_replace_capture() {
    let validated = validate_test_source(
        r#"
        this["agentdesk"] = {
          routines: { register() {} }
        };
        agentdesk.routines.register({
          name: "Still Protected",
          tick() { return { action: "skip" }; }
        });
        "#,
    )
    .unwrap();

    assert_eq!(validated.name, "Still Protected");
}

#[test]
fn define_property_cannot_replace_register_capture() {
    let validated = validate_test_source(
        r#"
        let overwriteRejected = false;
        try {
          Object.defineProperty(agentdesk.routines, "register", {
            value() {}
          });
        } catch (_) {
          overwriteRejected = true;
        }
        if (!overwriteRejected) throw new Error("register overwrite unexpectedly succeeded");
        agentdesk.routines.register({
          name: "Define Protected",
          tick() { return { action: "skip" }; }
        });
        "#,
    )
    .unwrap();

    assert_eq!(validated.name, "Define Protected");
}

#[test]
fn registration_argument_is_snapshotted_at_invocation() {
    let validated = validate_test_source(
        r#"
        const routine = {
          name: "Before Mutation",
          metadata: { phase: "before" },
          tick() { return { action: "skip" }; }
        };
        agentdesk.routines.register(routine);
        routine.name = "After Mutation";
        routine.metadata.phase = "after";
        routine.tick = async function () { return { action: "skip" }; };
        "#,
    )
    .unwrap();

    assert_eq!(validated.name, "Before Mutation");
    assert_eq!(validated.metadata["phase"], "before");
}

#[test]
fn register_after_throw_is_unreachable() {
    assert_source_rejected(
        r#"
        throw new Error("stopped before registration");
        agentdesk.routines.register({
          name: "Unreachable",
          tick() { return { action: "skip" }; }
        });
        "#,
        "stopped before registration",
    );
}

#[test]
fn rejects_syntax_invalid_balanced_source() {
    assert_source_rejected(
        r#"
        const invalid = ;
        agentdesk.routines.register({
          name: "Balanced But Invalid",
          tick() { return { action: "skip" }; }
        });
        "#,
        "JS eval error in routine script adversarial.js",
    );
}

#[test]
fn rejects_async_tick_method_at_registration() {
    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "Async Method",
          async tick() { return { action: "skip" }; }
        });
        "#,
        "tick must be synchronous",
    );
}

#[test]
fn rejects_async_tick_function_value_at_registration() {
    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "Async Function Value",
          tick: async function (ctx) { return { action: "skip" }; }
        });
        "#,
        "tick must be synchronous",
    );
}

#[test]
fn rejects_async_tick_function_value_across_newline() {
    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "Async Across Newline",
          tick:
            async function (ctx) { return { action: "skip" }; }
        });
        "#,
        "tick must be synchronous",
    );
}

#[test]
fn rejects_line_terminated_async_method_syntax() {
    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "Invalid Async Line Terminator",
          async
          tick() { return { action: "skip" }; }
        });
        "#,
        "JS eval error in routine script adversarial.js",
    );
}

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
fn bounded_runtime_rejects_oversized_validation_allocation() {
    assert_source_rejected(
        r#"
        const oversized = new Uint8Array(64 * 1024 * 1024);
        agentdesk.routines.register({
          name: "Oversized Validation Allocation",
          metadata: { oversized },
          tick() { return { action: "skip" }; }
        });
        "#,
        "JS eval error",
    );
}

#[test]
fn bounded_runtime_rejects_oversized_tick_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized-tick-allocation.js");
    std::fs::write(
        &path,
        r#"
        agentdesk.routines.register({
          name: "Oversized Tick Allocation",
          tick() {
            const oversized = new Uint8Array(64 * 1024 * 1024);
            return { action: "complete", result: { oversized } };
          }
        });
        "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let error = loader
        .execute_tick(
            "oversized-tick-allocation.js",
            tick_context("oversized-tick-allocation.js", "Oversized Tick Allocation"),
        )
        .unwrap_err();

    assert!(error.to_string().contains("tick(ctx) failed"));
}

#[test]
fn rejects_promise_nested_in_registered_metadata() {
    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "Promise Metadata",
          metadata: { nested: { value: Promise.resolve(7) } },
          tick() { return { action: "skip" }; }
        });
        "#,
        "routine metadata contains unsupported Promise",
    );
}

#[test]
fn rejects_exotic_registered_metadata_objects_before_enumeration() {
    for (expression, expected) in [
        ("new Date(0)", "unsupported non-plain JavaScript object"),
        (
            "new Map([['key', 1]])",
            "unsupported non-plain JavaScript object",
        ),
        (
            "new Uint8Array([1, 2, 3])",
            "unsupported non-plain JavaScript object",
        ),
        (
            "Object.create({ custom: true })",
            "unsupported non-plain JavaScript object",
        ),
        (
            "new Proxy({ key: 1 }, {})",
            "unsupported non-plain JavaScript object",
        ),
        (
            "new Proxy([1, 2, 3], {})",
            "unsupported non-plain JavaScript object",
        ),
        (
            "(() => { const proxy = Proxy.revocable({ key: 1 }, {}); proxy.revoke(); return proxy.proxy; })()",
            "unsupported non-plain JavaScript object",
        ),
    ] {
        let source = format!(
            r#"
            agentdesk.routines.register({{
              name: "Exotic Metadata",
              metadata: {{ nested: {expression} }},
              tick() {{ return {{ action: "skip" }}; }}
            }});
            "#
        );
        assert_source_rejected(&source, expected);
    }
}

#[test]
fn accepts_valid_nested_json_metadata_and_action_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested-json.js");
    std::fs::write(
        &path,
        r#"
        const nullPrototype = Object.create(null);
        nullPrototype.enabled = true;
        agentdesk.routines.register({
          name: "Nested JSON",
          metadata: {
            nested: [{ count: 3, values: [true, null, "ok"] }],
            nullPrototype
          },
          tick() {
            return {
              action: "complete",
              result: { nested: [{ count: 3, values: [true, null, "ok"] }] }
            };
          }
        });
        "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let script = loader.get_script("nested-json.js").unwrap().unwrap();
    assert_eq!(script.metadata["nested"][0]["count"], 3);
    assert_eq!(script.metadata["nullPrototype"]["enabled"], true);

    let action = loader
        .execute_tick(
            "nested-json.js",
            tick_context("nested-json.js", "Nested JSON"),
        )
        .unwrap();
    match action {
        crate::services::routines::RoutineAction::Complete { result_json, .. } => {
            let result = result_json.unwrap();
            assert_eq!(result["nested"][0]["values"][2], "ok");
        }
        other => panic!("unexpected action: {other:?}"),
    }
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
fn snapshots_stateful_metadata_getter_exactly_once() {
    let validated = validate_test_source(
        r#"
        const metadata = {};
        let reads = 0;
        Object.defineProperty(metadata, "value", {
          enumerable: true,
          get() {
            reads += 1;
            return reads === 1 ? 7 : metadata;
          }
        });
        agentdesk.routines.register({
          name: "Single Read Metadata",
          metadata,
          tick() { return { action: "skip" }; }
        });
        "#,
    )
    .unwrap();

    assert_eq!(validated.metadata["value"], 7);
}

#[test]
fn rejects_throwing_plain_object_getter_without_runtime_abort() {
    assert_source_rejected(
        r#"
        const metadata = {};
        Object.defineProperty(metadata, "explosive", {
          enumerable: true,
          get() { throw new Error("getter exploded"); }
        });
        agentdesk.routines.register({
          name: "Throwing Metadata Getter",
          metadata,
          tick() { return { action: "skip" }; }
        });
        "#,
        "routine metadata field explosive conversion failed: getter exploded",
    );
}

#[test]
fn rejects_sparse_metadata_array_before_large_host_allocation() {
    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "Sparse Metadata",
          metadata: new Array(0x3fffffff),
          tick() { return { action: "skip" }; }
        });
        "#,
        "array length 1073741823 exceeds maximum 16384",
    );
}

#[test]
fn rejects_metadata_exceeding_total_value_budget() {
    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "Wide Metadata",
          metadata: [
            new Array(10000),
            new Array(10000),
            new Array(10000),
            new Array(10000)
          ],
          tick() { return { action: "skip" }; }
        });
        "#,
        "exceeds maximum value count 32768",
    );
}

#[test]
fn rejects_repeated_string_references_exceeding_converted_byte_budget() {
    let error = convert_test_json_with_byte_budget(
        "(() => { const shared = '12345678'; return Array(8).fill(shared); })()",
        63,
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("exceeds maximum converted JSON size 63 bytes"),
        "{error}"
    );
}

#[test]
fn converted_byte_budget_accepts_exact_utf8_key_and_value_boundary() {
    let value = convert_test_json_with_byte_budget("({ 'é': '💣' })", 6).unwrap();
    assert_eq!(value, serde_json::json!({ "é": "💣" }));

    let error = convert_test_json_with_byte_budget("({ 'é': '💣' })", 5).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("exceeds maximum converted JSON size 5 bytes"),
        "{error}"
    );
}

#[test]
fn per_value_byte_budget_remains_authoritative_before_aggregate_budget() {
    let retained_output_budget = RetainedOutputBudget::new(2);
    let error = with_bounded_quickjs_context(|ctx| {
        let policy = JsonContainerPolicy {
            class_ids: capture_json_container_class_ids(ctx.clone())?,
            plain_object_classifier: create_plain_object_classifier(ctx.clone())?,
        };
        let value: rquickjs::Value = ctx.eval(r#""abc""#)?;
        js_value_to_json_with_budgets(
            value,
            "test JSON",
            &policy,
            2,
            Some(&retained_output_budget),
        )
    })
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("exceeds maximum converted JSON size 2 bytes"),
        "{error}"
    );
    assert!(!retained_output_budget.is_exhausted());
}

#[test]
fn retained_budget_charges_explicit_name_before_copy_at_exact_utf8_boundary() {
    let source = r#"
        agentdesk.routines.register({
          name: "é💣",
          tick() { return { action: "skip" }; }
        });
    "#;

    let (accepted, accepted_budget) = validate_test_source_with_retained_limit(source, "unused", 6);
    assert_eq!(accepted.unwrap().name, "é💣");
    assert!(!accepted_budget.is_exhausted());

    let (rejected, rejected_budget) = validate_test_source_with_retained_limit(source, "unused", 5);
    assert_eq!(
        rejected.unwrap_err().to_string(),
        rejected_budget.limit_error().to_string()
    );
    assert!(rejected_budget.is_exhausted());
}

#[test]
fn retained_budget_charges_fallback_name_before_copy_at_exact_utf8_boundary() {
    let source = r#"
        agentdesk.routines.register({
          tick() { return { action: "skip" }; }
        });
    "#;

    let (accepted, accepted_budget) = validate_test_source_with_retained_limit(source, "é💣", 6);
    assert_eq!(accepted.unwrap().name, "é💣");
    assert!(!accepted_budget.is_exhausted());

    let (rejected, rejected_budget) = validate_test_source_with_retained_limit(source, "é💣", 5);
    assert_eq!(
        rejected.unwrap_err().to_string(),
        rejected_budget.limit_error().to_string()
    );
    assert!(rejected_budget.is_exhausted());
}

#[test]
fn rejects_lone_surrogates_in_json_values_and_keys_before_rust_copy() {
    for (source, expected) in [
        (r#"({ value: "\ud800" })"#, "string conversion failed"),
        (
            r#"({ ["\ud800"]: "value" })"#,
            "object key conversion failed",
        ),
    ] {
        let error = convert_test_json_with_byte_budget(source, 64).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(expected), "{message}");
        assert!(message.contains("invalid UTF-8"), "{message}");
    }

    assert_eq!(
        convert_test_json_with_byte_budget(r#"({ key: "still alive" })"#, 14).unwrap(),
        serde_json::json!({ "key": "still alive" })
    );

    assert_source_rejected(
        r#"
        agentdesk.routines.register({
          name: "\ud800",
          tick() { return { action: "skip" }; }
        });
        "#,
        "routine name string conversion failed: invalid UTF-8",
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
            tick_context("primitive-tick.js", "Primitive Tick"),
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
fn action_retained_budget_is_fresh_after_registration_recapture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh-action-budget.js");
    std::fs::write(
        &path,
        r#"
        agentdesk.routines.register({
          name: "Fresh Action Budget",
          metadata: { long: "123456789" },
          tick() { return { "é": "💣" }; }
        });
        "#,
    )
    .unwrap();
    let script = load_single_routine_script(dir.path(), &path).unwrap();
    let context = tick_context("fresh-action-budget.js", "Fresh Action Budget");

    let action = evaluate_tick_action_with_retained_output_limit(&script, &context, 6).unwrap();
    assert_eq!(action, serde_json::json!({ "é": "💣" }));

    let error = evaluate_tick_action_with_retained_output_limit(&script, &context, 5).unwrap_err();
    assert_eq!(
        error.to_string(),
        "routine retained output exceeds maximum 5 bytes"
    );
}

#[test]
fn rejects_promise_returning_tick_without_awaiting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("promise-tick.js");
    std::fs::write(
        &path,
        "agentdesk.routines.register({ name: 'Promise Tick', tick() { return Promise.resolve({ action: 'skip' }); } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let error = loader
        .execute_tick(
            "promise-tick.js",
            tick_context("promise-tick.js", "Promise Tick"),
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("returned a Promise; async tick is not supported")
    );
}

#[test]
fn rejects_promise_nested_in_action_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested-promise-result.js");
    std::fs::write(
        &path,
        r#"
        agentdesk.routines.register({
          name: "Nested Promise Result",
          tick() {
            return {
              action: "complete",
              result: { nested: { value: Promise.resolve(7) } }
            };
          }
        });
        "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let error = loader
        .execute_tick(
            "nested-promise-result.js",
            tick_context("nested-promise-result.js", "Nested Promise Result"),
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("routine action contains unsupported Promise")
    );
}

#[test]
fn rejects_function_with_enumerable_action_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("function-action.js");
    std::fs::write(
        &path,
        r#"
        agentdesk.routines.register({
          name: "Function Action",
          tick() {
            function forgedAction() {}
            forgedAction.action = "skip";
            return forgedAction;
          }
        });
        "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let error = loader
        .execute_tick(
            "function-action.js",
            tick_context("function-action.js", "Function Action"),
        )
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("routine action contains unsupported function")
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
        .execute_tick("cycle.js", tick_context("cycle.js", "Cycle"))
        .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("cycle check failed") || message.contains("cyclic object graph"),
        "{message}"
    );
}

#[test]
fn snapshots_stateful_action_getter_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("single-read-action.js");
    std::fs::write(
        &path,
        r#"
            agentdesk.routines.register({
              name: "Single Read Action",
              tick() {
                const action = { action: "complete" };
                let reads = 0;
                Object.defineProperty(action, "result", {
                  enumerable: true,
                  get() {
                    reads += 1;
                    return reads === 1 ? { ok: true } : action;
                  }
                });
                return action;
              }
            });
            "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let action = loader
        .execute_tick(
            "single-read-action.js",
            tick_context("single-read-action.js", "Single Read Action"),
        )
        .unwrap();

    match action {
        crate::services::routines::RoutineAction::Complete { result_json, .. } => {
            assert_eq!(result_json.unwrap()["ok"], true);
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn rejects_action_exceeding_json_depth_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deep-action.js");
    std::fs::write(
        &path,
        r#"
            agentdesk.routines.register({
              name: "Deep Action",
              tick() {
                const root = {};
                let cursor = root;
                for (let index = 0; index < 140; index += 1) {
                  cursor.next = {};
                  cursor = cursor.next;
                }
                return root;
              }
            });
            "#,
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &path).unwrap();
    let error = loader
        .execute_tick(
            "deep-action.js",
            tick_context("deep-action.js", "Deep Action"),
        )
        .unwrap_err();

    assert!(error.to_string().contains("maximum nesting depth"));
}
