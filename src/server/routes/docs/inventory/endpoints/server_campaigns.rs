use super::super::{EndpointDoc, body_param, ep, path_param, query_param};
use serde_json::json;

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    let definition = json!({"repository":"adk","title":"Release","goal":"Verify release","profile_version":"v1","nodes":[{"id":"a","title":"A","issue_url":"https://github.com/itismyfield/AgentDesk/issues/7438","acceptance":[{"id":"ac-a","description":"Verify A","verification":"test"}],"dependencies":[],"group":null}]});
    let result = json!({"definition_revision":1,"run_id":null,"control_version":null,"event_sequence":1,"control":"prepared"});
    let mut endpoints = vec![
        ep("GET", "/api/server-campaigns", "campaigns", "List server campaigns as {campaigns:[{definition,view}]}. Definition is the input document; view includes campaign_id, revision, server as_of and execution_readiness=unavailable. Uses existing authentication; PostgreSQL absence is 503, never an empty fallback.")
            .with_params([("limit",query_param("integer",false,"1..100; default 100.")),("offset",query_param("integer",false,"Nonnegative; default 0. Unknown query fields return 400."))])
            .with_example(json!({}),json!({"campaigns":[]})),
        ep("POST", "/api/server-campaigns", "campaigns", "Create a prepared definition (201). Reuse the client stable id and idempotency_key in the JSON body for retries; the original status/body is returned before stale checks. Different body with the same key returns 409. Closed JSON input rejects execution fields with 400; body limit 2 MiB (413), JSON media type required (415).")
            .with_params([("id",body_param("string",true,"Stable client campaign ID, 1..128 ASCII letters/digits/._-.")),("idempotency_key",body_param("string",true,"Nonempty key, scoped to this campaign across all writes.")),("definition",body_param("object",true,"repository/title/goal/profile_version/nodes; nodes accept only id/title/issue_url/acceptance/dependencies/group."))])
            .with_example(json!({"id":"release","idempotency_key":"create-1","definition":definition}),json!({"result":result})),
        ep("GET", "/api/server-campaigns/{id}", "campaigns", "Read {campaign:{definition,view}} from one stored state. Definition has repository/title/goal/profile_version/nodes, without an input/hashes wrapper. View supplies identity/revision/control/events/as_of. Invalid ID is 400; missing campaign is 404.")
            .with_params([("id",path_param("Stable server campaign ID."))]),
        ep("PUT", "/api/server-campaigns/{id}/definition", "campaigns", "Edit the definition in prepared/stopped only (200). Related live/unknown sessions or unreconciled work keep editing locked. Unknown/nested execution fields or invalid definitions return 400; stale revision/state/key conflict 409; missing 404; PostgreSQL absence 503. Retries return the original receipt response.")
            .with_params([("id",path_param("Stable server campaign ID.")),("idempotency_key",body_param("string",true,"Reuse this body key on retry.")),("expected_definition_revision",body_param("integer",true,"Definition revision just read.")),("definition",body_param("object",true,"Complete definition replacement, with no execution state.")),("change",body_param("object",false,"reference and explanation required when requirements/acceptance/profile meaning changes."))]),
        ep("POST", "/api/server-campaigns/{id}/commands", "campaigns", "Body is {idempotency_key,command:{command,...}}. Public commands are start, stop, resume, retry only. All require expected_definition_revision; stop/resume/retry also require expected_run, retry requires node_id. Start/resume/retry with a valid target return 409 execution_unavailable and create no work. Stop accepts mode immediate|finish_stage (202); event advancement does not invalidate its run/definition target. Internal observation/result/completion commands and unknown fields are 400. Conflicts are 409, missing target 404, PostgreSQL absence 503.")
            .with_params([("id",path_param("Stable server campaign ID.")),("idempotency_key",body_param("string",true,"Reuse this body key and command on retry.")),("command",body_param("object",true,"Closed start/stop/resume/retry command object."))])
            .with_error_example(409,json!({"idempotency_key":"start-1","command":{"command":"start","expected_definition_revision":1}}),json!({"error":"server campaign command rejected","code":"conflict","context":{"rejection":"execution_unavailable","target":null}})),
        ep("GET", "/api/server-campaigns/{id}/events", "campaigns", "Read {events:[...]} in campaign sequence order. A missing campaign is 404; an existing campaign with no matching events is an empty page. Unknown query fields or out-of-bounds pagination return 400.")
            .with_params([("id",path_param("Stable server campaign ID.")),("run_id",query_param("string",false,"Optional run filter; no matches gives an empty page.")),("after",query_param("integer",false,"Nonnegative sequence, exclusive; default 0.")),("limit",query_param("integer",false,"1..500; default 100."))])
            .with_example(json!({"query":{"after":0,"limit":100}}),json!({"events":[]})),
    ];
    if let Some(example) = &mut endpoints[1].example {
        example.status = Some(201);
    }
    endpoints
}
