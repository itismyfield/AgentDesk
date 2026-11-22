use super::super::{EndpointDoc, body_param, ep, path_param, query_param};
use serde_json::json;

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    let pipeline = json!({
        "schema_version":1,"vocabulary_version":1,"evaluator_version":1,"registry_version":1,
        "start":"requirements","states":[{"id":"requirements","kind":"auto_check"},{"id":"complete","kind":"end_success"}],
        "transitions":[{"id":"finish","from":"requirements","to":"complete","priority":20,
        "condition":{"field":"state.outcome","op":"eq","value":"succeeded"}}]
    });
    let request = json!({"pipeline":pipeline});
    let invalid = json!({"errors":[{"code":"START_NOT_FOUND","path":"/start","state_id":"missing","transition_id":null}],"warnings":[]});
    let mut invalid_request = request.clone();
    invalid_request["pipeline"]["start"] = json!("missing");
    let template = json!({"id":"campaign-development-9","version":1,"design_enabled":false,"execution_readiness":"unavailable"});
    let result = json!({"result":{"definition_revision":2,"run_id":null,"control_version":null,"event_sequence":2,"control":"prepared"}});
    let definition = (
        "pipeline",
        body_param(
            "object",
            true,
            "Complete typed definition with versions, start, states and transitions.",
        ),
    );
    let edit = [
        ("id", path_param("Stable server campaign ID.")),
        (
            "idempotency_key",
            body_param("string", true, "Stable key for exact receipt replay."),
        ),
        (
            "expected_definition_revision",
            body_param(
                "integer",
                true,
                "Current revision from the pipeline detail.",
            ),
        ),
    ];
    let [edit_id, edit_key, edit_revision] = edit.clone();
    vec![
        ep("GET", "/api/server-campaigns/pipeline/condition-fields", "campaigns", "List typed condition capabilities. Responses include version and fields with field, type, operators, enum_values, source, subject_binding, freshness and available.")
            .with_params([("kind",query_param("string",true,"State kind.")),("executor",query_param("string",false,"Executor registry ID.")),("executor_version",query_param("integer",false,"Executor version (default 1 with an executor)."))])
            .with_example(json!({"query":{"kind":"agent_task","executor":"r8.review"}}),json!({"vocabulary_version":1,"fields":[{"field":"review.verdict","type":"enum","operators":["eq","ne"],"enum_values":["clean","issues"],"source":"review","subject_binding":"candidate_producer","freshness":"current_candidate","available":true}]})),
        ep("GET", "/api/server-campaigns/pipeline/templates", "campaigns", "List immutable built-in pipeline template versions and execution capability.")
            .with_example(json!({}),json!({"templates":[template]})),
        ep("GET", "/api/server-campaigns/pipeline/templates/{template_id}/{version}", "campaigns", "Read {template, execution_hash, pipeline} with the full versioned graph. Missing templates return 404.")
            .with_params([("template_id",path_param("Template identifier.")),("version",path_param("Template version."))]),
        ep("POST", "/api/server-campaigns/pipeline/validate", "campaigns", "Validate a candidate without saving. A decoded graph returns 200 with {errors,warnings}. Each diagnostic has a code, JSON Pointer path relative to the graph, nullable state_id and nullable transition_id. Malformed wire input is 400.")
            .with_params([definition.clone()]).with_example(invalid_request,invalid),
        ep("GET", "/api/server-campaigns/{id}/pipeline", "campaigns", "Read pipeline, definition_revision, execution_hash, source (saved or legacy_projection), editable, migration_available and execution {mode,run_id,definition_revision,state_ids}. execution reports the legacy runtime; state_ids are template IDs keyed by node, null before run assignment.")
            .with_params([("id",path_param("Stable server campaign ID."))])
            .with_example(json!({}),json!({"pipeline":pipeline,"definition_revision":1,"execution_hash":"b".repeat(64),"source":"saved","editable":true,"migration_available":false,"execution":{"mode":"legacy_stage","run_id":null,"definition_revision":null,"state_ids":{"a":null}}})),
        ep("POST", "/api/server-campaigns/{id}/pipeline/validate", "campaigns", "Read-only validation with the current run's occupied-state checks. Response uses the same diagnostics as standalone validation. A subsequent save evaluates the latest state again.")
            .with_params([("id",path_param("Stable server campaign ID.")), definition.clone()])
            .with_example(request,json!({"errors":[],"warnings":[]})),
        ep("PUT", "/api/server-campaigns/{id}/pipeline", "campaigns", "Replace only the pipeline with a definition revision compare-and-swap. Prepared or stopped quiescent campaigns can save. Wire errors are 400, invalid graphs are 422 with context.definition_error.pipeline_errors, and stale revision, occupied states, busy control or key conflicts are 409. Returns the durable campaign command result.")
            .with_params([edit_id, edit_key, edit_revision, definition])
            .with_example(json!({"idempotency_key":"save-1","expected_definition_revision":1,"pipeline":pipeline}),result.clone()),
        ep("POST", "/api/server-campaigns/{id}/pipeline/migrate", "campaigns", "Attach campaign-development-9 version 1 to a prepared campaign with no run or a stopped, settled run. Uses the same revision CAS and receipt rules as PUT. Existing graphs are returned without a new revision. The legacy executor, runtime and run pin stay as recorded.")
            .with_params(edit)
            .with_example(json!({"idempotency_key":"migration-1","expected_definition_revision":1}),result),
    ]
}
