use super::super::{EndpointDoc, body_param, ep, path_param, query_param};
use serde_json::json;

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    let definition = json!({"repository":"adk","title":"Release","goal":"Verify release","profile_version":"v1","nodes":[{"id":"a","title":"A","issue_url":"https://github.com/itismyfield/AgentDesk/issues/7438","acceptance":[{"id":"ac-a","description":"Verify A","verification":"test"}],"dependencies":[],"group":null}]});
    let result = json!({"definition_revision":1,"run_id":null,"control_version":null,"event_sequence":1,"control":"prepared"});
    let binding = json!({
        "run_id": "run", "node_id": "a", "gate": "start", "definition_revision": 1,
        "target": {"kind": "candidate", "base_sha": "a".repeat(40), "head_sha": "b".repeat(40)},
        "policy_hash": "c".repeat(64)
    });
    let campaign = json!({
        "definition": {
            "repository": definition["repository"], "title": definition["title"],
            "goal": definition["goal"], "profile_version": definition["profile_version"],
            "nodes": definition["nodes"],
            "workflow_options": {"start":true,"implementation":false,"review":false,"landing":false,"merge":false,"deploy":false}
        },
        "view": {
            "execution_mode": "server", "campaign_id": "release",
            "as_of": "2026-11-19T00:00:01Z", "definition_revision": 1, "run_id": "run",
            "control_version": 1, "last_event_sequence": 3, "last_event_at": "2026-11-19T00:00:00Z",
            "control": "running", "stop_mode": null, "stop_confirmed": false, "display": "waiting",
            "execution_readiness": "unavailable", "unknown": false, "failed_count": 0,
            "blocked_count": 0, "reasons": [{"code": "execution_unavailable", "target": "release"}],
            "commands": [
                {"command": "edit_definition", "rejection": "invalid_state"},
                {"command": "start", "rejection": "execution_unavailable"},
                {"command": "stop_immediate", "rejection": null},
                {"command": "stop_finish_stage", "rejection": null},
                {"command": "resume", "rejection": "execution_unavailable"},
                {"command": "prepare_retry", "rejection": "execution_unavailable"}
            ],
            "nodes": [{
                "id": "a", "direct_successors": [], "ready_after_main": [],
                "runtime": {
                    "stage": "requirements", "disposition": "pending", "series": 1,
                    "quality_failures": 0, "repair_cycles": 0, "review_rounds": 0,
                    "current_attempt": null, "main_integrated": false, "acceptance_complete": false,
                    "evidence_current": false, "wait_reason": null,
                    "pending_gate": {"gate": "start", "binding": binding},
                    "wait_started_at": "2026-11-19T00:00:00Z", "evidence_url": null,
                    "reminder_interval_seconds": null, "human_status": "awaiting_approval"
                }
            }],
            "sessions": [], "unreconciled_actions": []
        }
    });
    let mut endpoints = vec![
        ep("GET", "/api/server-campaigns", "campaigns", "List server campaigns as {campaigns:[{definition,view}]}. Definition is the input document; view includes campaign_id, revision, server as_of, execution_readiness=unavailable and the same nodes[].runtime schema as the detail endpoint. Uses existing authentication; PostgreSQL absence is 503, never an empty fallback.")
            .with_params([("limit",query_param("integer",false,"1..100; default 100.")),("offset",query_param("integer",false,"Nonnegative; default 0. Unknown query fields return 400.")),("view",query_param("string",false,"full (default) or summary. summary drops definition.nodes and view.nodes/sessions/unreconciled_actions and adds view.node_total, view.node_counts (by disposition), view.session_count, view.unreconciled_action_count; read nodes from GET /api/server-campaigns/{id}."))])
            .with_example(json!({}),json!({"campaigns":[campaign]})),
        ep("POST", "/api/server-campaigns", "campaigns", "Create a prepared definition (201). Reuse the client stable id and idempotency_key in the JSON body for retries; the original status/body is returned before stale checks. Different body with the same key returns 409. Closed JSON input rejects execution fields with 400; body limit 2 MiB (413), JSON media type required (415).")
            .with_params([("id",body_param("string",true,"Stable client campaign ID, 1..128 ASCII letters/digits/._-.")),("idempotency_key",body_param("string",true,"Nonempty key, scoped to this campaign across all writes.")),("definition",body_param("object",true,"repository/title/goal/profile_version/nodes plus optional pipeline and workflow_options. Invalid pipeline semantics return 422; workflow_options omission on create defaults all six gates OFF. workflow_options, when present, requires booleans start/implementation/review/landing/merge/deploy. Nodes accept only id/title/issue_url/acceptance/dependencies/group."))])
            .with_example(json!({"id":"release","idempotency_key":"create-1","definition":definition}),json!({"result":result})),
        ep("GET", "/api/server-campaigns/{id}", "campaigns", "Read {campaign:{definition,view}} from one stored state. Definition has repository/title/goal/profile_version/nodes and optional pipeline and workflow_options, without an input/hashes wrapper. View supplies identity/revision/control/events/as_of. Each nodes[].runtime adds nullable pending_gate {gate,binding} (binding is null until opened), wait_started_at (latest matching opening event through last_event_sequence), evidence_url and reminder_interval_seconds (currently null), and human_status (decision_required, awaiting_approval, awaiting_deploy, awaiting_start). Invalid ID is 400; missing campaign is 404.")
            .with_params([("id",path_param("Stable server campaign ID."))])
            .with_example(json!({}),json!({"campaign":campaign})),
        ep("PUT", "/api/server-campaigns/{id}/definition", "campaigns", "Edit the definition in prepared/stopped only (200). Related live/unknown sessions or unreconciled work keep editing locked. Unknown or nested execution fields return 400. Invalid pipeline semantics return 422. An omitted or null pipeline cannot erase a saved graph (409 definition_schema_upgrade_required); stale revision, state, occupied-state and key conflicts return 409; missing 404; PostgreSQL absence 503. Retries return the original receipt response.")
            .with_params([("id",path_param("Stable server campaign ID.")),("idempotency_key",body_param("string",true,"Reuse this body key on retry.")),("expected_definition_revision",body_param("integer",true,"Definition revision just read.")),("definition",body_param("object",true,"Complete definition replacement with all six workflow_options booleans explicitly present on fresh edits; omission is rejected to preserve configured gates. An original successful legacy receipt still replays before this guard.")),("change",body_param("object",false,"reference and explanation required when requirements/acceptance/profile meaning changes."))]),
        ep("POST", "/api/server-campaigns/{id}/commands", "campaigns", "Body is {idempotency_key,command:{command,...}}. Public commands are start, stop, resume, retry, approve. All require expected_definition_revision; stop/resume/retry/approve also require expected_run, retry requires node_id. Start/resume/retry with a valid target return 409 execution_unavailable and create no work. Approve binds node_id, gate, full target (candidate base_sha/head_sha, merge pull_request number/head_sha, deploy main sha), policy_hash and evidence (decision_ref, HTTPS response_ref, response_at) to an already opened gate. Use the pending_gate binding from the current view, including its definition revision. The dashboard copies the stable forum decision ID or slug into decision_ref, the specific approving response HTTPS permalink into response_ref, and its actual RFC 3339 timestamp into response_at. Submit once all three are available. Same gate/evidence approval is idempotent; changed binding or evidence is rejected. Provenance is coordinator_recorded_forum: the coordinator records a checked forum response; the server does not authenticate the forum user. Approvals do not enable execution. Stop accepts mode immediate|finish_stage (202); event advancement does not invalidate its run/definition target. Internal observation/result/completion commands and unknown fields are 400. Conflicts are 409, missing target 404, PostgreSQL absence 503.")
            .with_params([("id",path_param("Stable server campaign ID.")),("idempotency_key",body_param("string",true,"Reuse this body key and command on retry.")),("command",body_param("object",true,"Closed start/stop/resume/retry/approve command object; internal gate opening is not public."))])
            .with_example(json!({
                "idempotency_key": "release-start-approval",
                "command": {
                    "command":"approve", "expected_definition_revision":1, "expected_run":"run",
                    "node_id":"a", "gate":"start", "target":binding["target"],
                    "policy_hash":binding["policy_hash"], "evidence":{
                        "decision_ref":"release-start",
                        "response_ref":"https://forum.example/decisions/release-start#response-2",
                        "response_at":"2026-11-19T00:00:02Z"
                    }
                }
            }),json!({"result":{
                "definition_revision":1,"run_id":"run","control_version":1,"event_sequence":4,"control":"running"
            }}))
            .with_error_example(409,json!({"idempotency_key":"start-1","command":{"command":"start","expected_definition_revision":1}}),json!({"error":"server campaign command rejected","code":"conflict","context":{"rejection":"execution_unavailable","target":null}})),
        ep("GET", "/api/server-campaigns/{id}/events", "campaigns", "Read {events:[...]} in campaign sequence order. A missing campaign is 404; an existing campaign with no matching events is an empty page. Unknown query fields or out-of-bounds pagination return 400.")
            .with_params([("id",path_param("Stable server campaign ID.")),("run_id",query_param("string",false,"Optional run filter; no matches gives an empty page.")),("after",query_param("integer",false,"Nonnegative sequence, exclusive; default 0.")),("limit",query_param("integer",false,"1..500; default 100."))])
            .with_example(json!({"query":{"after":0,"limit":100}}),json!({"events":[]})),
    ];
    if let Some(example) = &mut endpoints[1].example {
        example.status = Some(201);
    }
    if let Some(example) = &mut endpoints[4].example {
        example.status = Some(202);
    }
    endpoints.extend(pipeline_endpoints());
    endpoints
}

fn pipeline_endpoints() -> Vec<EndpointDoc> {
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
