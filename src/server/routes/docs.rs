use axum::{
    Json,
    extract::{Path, Query},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};

mod guides;
mod inventory;
mod taxonomy;

use guides::{api_friction_markers_body, card_lifecycle_ops_body, guide_index};
use inventory::{EndpointDoc, all_endpoints};
use taxonomy::{
    CANONICAL_CATEGORIES, GROUP_NAMES, canonical_category, categories_for_group,
    category_description, category_summaries, category_to_group, effective_category,
    group_description, is_canonical_category, subcategory_summaries,
};

// Keep coverage constants visible at `crate::server::routes::docs::*` while
// their bodies live in focused child modules.
#[allow(unused_imports)]
pub(crate) use guides::{API_FRICTION_MARKERS_LAST_REFRESHED, CARD_LIFECYCLE_OPS_LAST_REFRESHED};
#[allow(unused_imports)]
pub(crate) use inventory::TOP_40_PAIRED_PATHS;

// Category: ops

#[derive(Debug, Default, Deserialize)]
pub struct ApiDocsQuery {
    pub format: Option<String>,
    pub category: Option<String>,
}

/// GET /api/help — combined category summary plus detailed endpoint inventory.
pub async fn api_help() -> (StatusCode, Json<Value>) {
    let endpoints = all_endpoints();
    (
        StatusCode::OK,
        Json(json!({
            "categories": category_summaries(&endpoints),
            "endpoints": endpoints,
        })),
    )
}

/// GET /api/docs — #1063 hierarchical response.
///
/// Default response is the new 8-group hierarchy:
/// ```json
/// { "groups": [ { "name": "runtime", "description": "...",
///                 "categories": ["dispatches", "sessions", ...] }, ... ] }
/// ```
///
/// When `?format=flat` is passed, returns the full flat endpoint list
/// (preserved for backward-compatible tooling and the endpoint-coverage
/// contract tests).
///
/// #1443 also surfaces a `guides` array so callers discover the
/// long-form decision-tree pages (e.g. card-lifecycle-ops) without having
/// to read source.
pub async fn api_docs(Query(query): Query<ApiDocsQuery>) -> (StatusCode, Json<Value>) {
    let endpoints = all_endpoints();
    if query
        .format
        .as_deref()
        .is_some_and(|format| format.eq_ignore_ascii_case("flat"))
    {
        return (StatusCode::OK, Json(json!({ "endpoints": endpoints })));
    }

    if let Some(category) = query
        .category
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let category = canonical_category(category);
        let matching: Vec<EndpointDoc> = endpoints
            .into_iter()
            .filter(|endpoint| effective_category(endpoint) == category)
            .collect();
        if matching.is_empty() {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown docs category: {category}") })),
            );
        }
        return (
            StatusCode::OK,
            Json(json!({
                "group": category_to_group(category),
                "category": category,
                "description": category_description(category),
                "count": matching.len(),
                "endpoints": matching,
            })),
        );
    }

    let groups: Vec<Value> = GROUP_NAMES
        .iter()
        .map(|group| {
            let categories: Vec<&'static str> = categories_for_group(&endpoints, group)
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            json!({
                "name": group,
                "description": group_description(group),
                "categories": categories,
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "groups": groups,
            "guides": guide_index(),
        })),
    )
}

/// Core logic for the single-segment docs route, shared between the HTTP
/// handler and the in-process CLI helper. Returns `(status, headers, body)`.
fn resolve_docs_segment(segment: &str, flat: bool) -> (StatusCode, HeaderMap, Value) {
    // #1443: long-form guide pages take precedence over group/category
    // resolution so callers can reach `/api/docs/card-lifecycle-ops` (etc.)
    // through the same single-segment route used by the rest of the docs
    // tree. Guides ignore the `?format=flat` switch — there is no endpoint
    // list to flatten.
    if segment == "card-lifecycle-ops" {
        let _ = flat;
        return (StatusCode::OK, HeaderMap::new(), card_lifecycle_ops_body());
    }
    if segment == "api-friction-markers" {
        let _ = flat;
        return (
            StatusCode::OK,
            HeaderMap::new(),
            api_friction_markers_body(),
        );
    }

    let endpoints = all_endpoints();

    // Primary: treat as group name.
    if GROUP_NAMES.contains(&segment) {
        if flat {
            let matching: Vec<EndpointDoc> = endpoints
                .into_iter()
                .filter(|endpoint| category_to_group(effective_category(endpoint)) == segment)
                .collect();
            return (
                StatusCode::OK,
                HeaderMap::new(),
                json!({ "endpoints": matching }),
            );
        }

        let categories: Vec<Value> = categories_for_group(&endpoints, segment)
            .into_iter()
            .map(|(name, count)| {
                json!({
                    "name": name,
                    "description": category_description(name),
                    "endpoint_count": count,
                })
            })
            .collect();
        return (
            StatusCode::OK,
            HeaderMap::new(),
            json!({
                "group": segment,
                "description": group_description(segment),
                "categories": categories,
            }),
        );
    }

    // Backward compat: legacy flat category route.
    let canonical: Option<&'static str> = if is_canonical_category(segment) {
        // The CANONICAL_CATEGORIES array stores &'static str literals so we can
        // recover a 'static lifetime by matching against the constant list.
        CANONICAL_CATEGORIES
            .iter()
            .copied()
            .find(|candidate| *candidate == segment)
    } else {
        endpoints
            .iter()
            .find(|endpoint| endpoint.subcategory.is_some_and(|sub| sub == segment))
            .map(|endpoint| endpoint.category)
    };

    let Some(canonical) = canonical else {
        return (
            StatusCode::NOT_FOUND,
            HeaderMap::new(),
            json!({ "error": format!("unknown docs group or category: {segment}") }),
        );
    };

    let legacy_drilldown = segment != canonical;
    let matching: Vec<EndpointDoc> = if legacy_drilldown {
        endpoints
            .into_iter()
            .filter(|endpoint| endpoint.subcategory.is_some_and(|sub| sub == segment))
            .collect()
    } else {
        endpoints
            .into_iter()
            .filter(|endpoint| endpoint.category == canonical)
            .collect()
    };

    if matching.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            HeaderMap::new(),
            json!({ "error": format!("unknown docs group or category: {segment}") }),
        );
    }

    // Emit deprecation hint pointing at the new /group/category route.
    let mut headers = HeaderMap::new();
    let group_for_category = category_to_group(segment);
    let canonical_path = format!("/api/docs/{group_for_category}/{segment}");
    if let Ok(value) = HeaderValue::from_str(&canonical_path) {
        headers.insert("X-Deprecated", value);
    }

    (
        StatusCode::OK,
        headers,
        json!({
            "category": segment,
            "canonical_category": canonical,
            "description": category_description(segment),
            "count": matching.len(),
            "deprecated": true,
            "canonical_path": canonical_path,
            "subcategories": subcategory_summaries(&matching, canonical),
            "endpoints": matching,
        }),
    )
}

/// GET /api/docs/{group_or_category}
///
/// Preferred behavior (#1063): `group` is one of the 8 top-level group names,
/// response is `{ group, categories: [{name, description, endpoint_count}] }`.
///
/// Backward-compatible fallback: if the segment matches a legacy category name
/// (e.g. `admin`, `queue`, `dispatches`, `ops`, or a fine-grained sub-category
/// like `reviews`), returns the old category-detail shape with an
/// `X-Deprecated` header pointing at the new route. Callers should migrate to
/// `GET /api/docs/{group}/{category}`.
pub async fn api_docs_group_or_category(
    Path(segment): Path<String>,
    Query(query): Query<ApiDocsQuery>,
) -> impl IntoResponse {
    let flat = query
        .format
        .as_deref()
        .is_some_and(|format| format.eq_ignore_ascii_case("flat"));
    let (status, headers, body) = resolve_docs_segment(&segment, flat);
    (status, headers, Json(body))
}

/// Back-compat shim for the in-process CLI `cmd_docs` path. Routes a single
/// category name through the legacy branch and returns the body with the
/// resolved status. Headers (including `X-Deprecated`) are dropped because
/// the CLI prints JSON only.
pub async fn api_docs_category(Path(category): Path<String>) -> (StatusCode, Json<Value>) {
    let (status, _headers, body) = resolve_docs_segment(&category, false);
    (status, Json(body))
}

/// GET /api/docs/{group}/{category} — endpoints for one fine-grained
/// category nested under a specific group.
pub async fn api_docs_group_category(
    Path((group, category)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    if !GROUP_NAMES.contains(&group.as_str()) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown docs group: {group}") })),
        );
    }

    if category_to_group(&category) != group.as_str() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("category {category} does not belong to group {group}")
            })),
        );
    }

    let endpoints = all_endpoints();
    let matching: Vec<EndpointDoc> = endpoints
        .into_iter()
        .filter(|endpoint| effective_category(endpoint) == category.as_str())
        .collect();

    if matching.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("no endpoints documented for {group}/{category}")
            })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "group": group,
            "category": category,
            "description": category_description(&category),
            "count": matching.len(),
            "endpoints": matching,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kanban_assign_docs_expose_assignment_and_partial_transition_results() {
        let endpoints = all_endpoints();

        for (method, path) in [
            ("POST", "/api/kanban-cards/assign-issue"),
            ("POST", "/api/kanban-cards/{id}/assign"),
        ] {
            let endpoint = endpoints
                .iter()
                .find(|endpoint| endpoint.method == method && endpoint.path == path)
                .unwrap_or_else(|| panic!("{method} {path} must be documented"));

            assert!(
                endpoint.description.contains("Assignment is guaranteed")
                    && endpoint.description.contains("response.transition"),
                "{method} {path} must describe assignment/transition partial-success semantics: {}",
                endpoint.description
            );

            let happy = endpoint
                .example
                .as_ref()
                .unwrap_or_else(|| panic!("{method} {path} must include a happy example"));
            assert_eq!(happy.response["assignment"]["ok"], true);
            assert_eq!(happy.response["transition"]["ok"], true);
            assert!(
                happy.response["transition"]["error"].is_null(),
                "{method} {path} success example must keep transition.error null"
            );

            let partial = endpoint
                .partial_success_example
                .as_ref()
                .unwrap_or_else(|| {
                    panic!("{method} {path} must include a partial-success example")
                });
            assert_eq!(partial.status, Some(200));
            assert_eq!(partial.scenario, Some("partial_success"));
            assert_eq!(partial.response["assignment"]["ok"], true);
            assert_eq!(partial.response["transition"]["ok"], false);
            assert_eq!(
                partial.response["transition"]["next_action"],
                "inspect_transition_error"
            );
            assert!(
                partial.response["transition"]["failed_step"].is_string(),
                "{method} {path} partial-success example must identify failed_step"
            );
            assert!(
                partial.response["transition"]["error"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty()),
                "{method} {path} partial-success example must include transition.error"
            );
        }
    }

    #[test]
    fn dispatch_docs_include_patch_lifecycle_and_cancel_contract() {
        let endpoints = all_endpoints();

        let patch = endpoints
            .iter()
            .find(|endpoint| endpoint.method == "PATCH" && endpoint.path == "/api/dispatches/{id}")
            .expect("PATCH /api/dispatches/{id} must be documented");
        assert!(
            patch.description.contains("Allowed status values")
                && patch
                    .description
                    .contains("already-terminal dispatches return 409")
                && patch
                    .description
                    .contains("allowed_from is a status precondition")
                && patch.description.contains("result_summary")
                && patch.description.contains("completed_at"),
            "PATCH dispatch docs must describe lifecycle response semantics: {}",
            patch.description
        );
        let status = patch
            .params
            .get("status")
            .expect("PATCH dispatch docs must include status body param");
        assert_eq!(status.location, "body");
        assert_eq!(
            status.enum_values.as_deref(),
            Some(&["pending", "dispatched", "completed", "cancelled", "failed"][..])
        );
        let response = &patch
            .example
            .as_ref()
            .expect("PATCH dispatch docs must include a response example")
            .response;
        assert_eq!(response["dispatch"]["result_summary"], "done");
        assert!(response["dispatch"]["updated_at"].is_string());
        assert!(response["dispatch"]["completed_at"].is_string());
        assert_eq!(
            patch
                .error_example
                .as_ref()
                .and_then(|example| example.status),
            Some(409)
        );
        let error_response = &patch
            .error_example
            .as_ref()
            .expect("PATCH dispatch docs must include an error response example")
            .response;
        assert_eq!(error_response["dispatch_id"], "dispatch-1");
        assert!(
            error_response["error"]
                .as_str()
                .is_some_and(|message| message.contains("cannot be completed")),
            "PATCH dispatch docs must show terminal completion conflict: {error_response}"
        );

        let cancel = endpoints
            .iter()
            .find(|endpoint| {
                endpoint.method == "POST" && endpoint.path == "/api/dispatches/{id}/cancel"
            })
            .expect("POST /api/dispatches/{id}/cancel must be documented");
        assert_eq!(effective_category(cancel), "dispatches");
        assert!(
            cancel.description.contains("pending or dispatched")
                && cancel.description.contains("active turn")
                && cancel
                    .description
                    .contains("Terminal dispatches return 409"),
            "cancel dispatch docs must describe active/terminal lifecycle semantics: {}",
            cancel.description
        );
        assert_eq!(
            cancel
                .example
                .as_ref()
                .expect("cancel dispatch docs must include example")
                .response["ok"],
            true
        );
        assert_eq!(
            cancel
                .error_example
                .as_ref()
                .and_then(|example| example.status),
            Some(409)
        );

        let delivery_events = endpoints
            .iter()
            .find(|endpoint| {
                endpoint.method == "GET" && endpoint.path == "/api/dispatches/{id}/events"
            })
            .expect("GET /api/dispatches/{id}/events must be documented");
        assert_eq!(effective_category(delivery_events), "dispatches");
        assert!(
            delivery_events.description.contains("newest-first")
                && delivery_events.description.contains("typed delivery table")
                && delivery_events.description.contains("never reads kv_meta"),
            "delivery event docs must describe typed read-only semantics: {}",
            delivery_events.description
        );
        assert_eq!(
            delivery_events
                .example
                .as_ref()
                .expect("delivery event docs must include example")
                .response["events"][0]["status"],
            "sent"
        );
    }

    #[test]
    fn api_friction_marker_guide_is_indexed_and_describes_collection() {
        let guides = guide_index();
        assert!(guides.iter().any(|guide| {
            guide["name"] == "api-friction-markers"
                && guide["path"] == "/api/docs/api-friction-markers"
        }));

        let body = api_friction_markers_body();
        assert_eq!(body["marker_prefix"], "API_FRICTION:");
        assert_eq!(
            body["schema"]["required"]["endpoint"],
            "HTTP endpoint or API surface, for example PATCH /api/dispatches/{id}"
        );
        let body_text = body.to_string();
        assert!(body_text.contains("api_friction_events"));
        assert!(body_text.contains("api_friction_issues"));
        assert!(body_text.contains("Memento"));
        assert!(body_text.contains("API_FRICTION:"));

        let (status, _headers, routed) = resolve_docs_segment("api-friction-markers", false);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(routed["path"], "/api/docs/api-friction-markers");
    }

    #[test]
    fn github_issue_create_docs_surface_block_on_and_auto_dispatch_capabilities() {
        let endpoints = all_endpoints();
        let endpoint = endpoints
            .iter()
            .find(|endpoint| {
                endpoint.method == "POST" && endpoint.path == "/api/github/issues/create"
            })
            .expect("POST /api/github/issues/create must be documented");

        assert!(
            !endpoint.params.contains_key("auto_dispatch"),
            "unsupported auto_dispatch must not be advertised as a public request param"
        );
        assert!(
            endpoint.params.contains_key("block_on"),
            "supported block_on dependency contract must be documented"
        );
        assert!(
            endpoint.description.contains("capabilities")
                && endpoint.description.contains("auto_dispatch")
                && endpoint.description.contains("block_on records positive"),
            "docs must explain block_on support and dry_run capability discovery: {}",
            endpoint.description
        );

        let dry_run = endpoint
            .dry_run_example
            .as_ref()
            .expect("issue create docs must include dry_run discovery example");
        assert_eq!(dry_run.status, Some(200));
        assert_eq!(dry_run.response["capabilities"]["auto_dispatch"], false);
        assert_eq!(dry_run.response["capabilities"]["block_on"], true);
        assert_eq!(
            dry_run.response["capabilities"]["unsupported_features"],
            json!(["auto_dispatch"])
        );
        assert_eq!(dry_run.response["block_on"], json!([3718]));
        assert_eq!(dry_run.response["unsupported_features"], json!([]));
    }

    #[test]
    fn auto_queue_docs_surface_slot_recovery_identifiers() {
        let endpoints = all_endpoints();
        let find = |method: &str, path: &str| {
            endpoints
                .iter()
                .find(|endpoint| endpoint.method == method && endpoint.path == path)
                .unwrap_or_else(|| panic!("{method} {path} must be documented"))
        };

        let status = find("GET", "/api/queue/status");
        assert!(
            status
                .description
                .contains("diagnostics.slot_invariant_violations"),
            "status docs must describe slot invariant recovery diagnostics"
        );
        assert!(
            status
                .description
                .contains("diagnostics.entry_dispatch_delivery_mismatches")
                && status
                    .description
                    .contains("diagnostics.run_timeout_overruns"),
            "status docs must describe delivery split-brain and timeout diagnostics"
        );
        assert!(
            status.description.contains("entry_ids")
                && status.description.contains("dispatch_ids")
                && status
                    .description
                    .contains("/api/queue/slots/{agent_id}/{slot_index}/rebind"),
            "status docs must expose actionable identifiers and repair endpoints"
        );

        let rebind = find("POST", "/api/queue/slots/{agent_id}/{slot_index}/rebind");
        assert_eq!(
            rebind
                .params
                .get("run_id")
                .expect("rebind docs must include run_id")
                .required,
            true
        );
        assert_eq!(
            rebind
                .params
                .get("thread_group")
                .expect("rebind docs must include thread_group")
                .required,
            true
        );
        assert_eq!(
            rebind
                .example
                .as_ref()
                .expect("rebind docs must include an example")
                .response["rebound"],
            true
        );

        let sessions = find("GET", "/api/dispatched-sessions");
        let session = &sessions
            .example
            .as_ref()
            .expect("dispatched-sessions docs must include an example")
            .response["sessions"][0];
        assert_eq!(session["auto_queue_entry_id"], "entry-1");
        assert_eq!(session["auto_queue_run_id"], "run-1");
        assert_eq!(session["auto_queue_slot_index"], 0);
        assert_eq!(
            session["recovery_identifiers"]["auto_queue_thread_group"],
            0
        );
    }

    #[test]
    fn prompt_manifest_retention_docs_surface_boot_snapshot_semantics() {
        let endpoints = all_endpoints();
        let retention = endpoints
            .iter()
            .find(|endpoint| {
                endpoint.method == "GET" && endpoint.path == "/api/prompt-manifest/retention"
            })
            .expect("GET /api/prompt-manifest/retention must be documented");

        assert_eq!(effective_category(retention), "monitoring");
        assert!(
            retention
                .description
                .contains("boot-time retention config snapshot")
        );
        assert!(retention.description.contains("require process restart"));
        let response = &retention
            .example
            .as_ref()
            .expect("retention docs must include an example")
            .response;
        assert_eq!(response["restart_required_for_config_changes"], true);
        assert_eq!(response["config_applied_at"], "boot");
        assert_eq!(response["config_source"], "agentdesk.yaml boot snapshot");
        assert_eq!(response["hot_reload"], false);
    }

    #[test]
    fn kanban_response_contracts_document_stable_fields() {
        let endpoints = all_endpoints();
        let find = |method: &str, path: &str| {
            endpoints
                .iter()
                .find(|endpoint| endpoint.method == method && endpoint.path == path)
                .unwrap_or_else(|| panic!("{method} {path} must be documented"))
        };

        let assign = find("POST", "/api/kanban-cards/{id}/assign");
        let assign_response = &assign
            .example
            .as_ref()
            .expect("assign docs must include a response example")
            .response;
        assert_eq!(assign_response["assignment"]["ok"], true);
        assert_eq!(assign_response["assignment"]["agent_id"], "ch-td");
        assert_eq!(assign_response["transition"]["target_status"], "requested");
        assert_eq!(
            assign_response["transition"]["next_action"],
            "none_required"
        );
        assert!(assign_response["transition"]["error"].is_null());

        for path in [
            "/api/kanban-cards/{id}/retry",
            "/api/kanban-cards/{id}/redispatch",
        ] {
            let endpoint = find("POST", path);
            let response = &endpoint
                .example
                .as_ref()
                .unwrap_or_else(|| panic!("{path} docs must include a response example"))
                .response;
            assert!(response.get("card").is_some(), "{path} must document card");
            assert!(
                response.get("new_dispatch_id").is_some(),
                "{path} must document new_dispatch_id"
            );
            assert!(
                response.get("cancelled_dispatch_id").is_some(),
                "{path} must document cancelled_dispatch_id"
            );
            assert_eq!(
                response["next_action"], "none_required",
                "{path} must document next_action"
            );
        }
    }

    #[test]
    fn discord_send_docs_include_canonical_fields_and_legacy_aliases() {
        let endpoints = all_endpoints();
        let send = endpoints
            .iter()
            .find(|endpoint| endpoint.method == "POST" && endpoint.path == "/api/discord/send")
            .expect("POST /api/discord/send must be documented");

        for param in [
            "target",
            "content",
            "channel_id",
            "message",
            "source",
            "bot",
            "X-AgentDesk-Source",
        ] {
            assert!(
                send.params.contains_key(param),
                "send docs must include {param}"
            );
        }
        assert_eq!(
            send.params
                .get("channel_id")
                .expect("channel_id alias should be documented")
                .description,
            "Alias for target=channel:<id>"
        );
        assert_eq!(
            send.params
                .get("message")
                .expect("message alias should be documented")
                .description,
            "Alias for content"
        );
        let example_body = &send
            .example
            .as_ref()
            .expect("send docs must include an example")
            .request["body"];
        assert_eq!(example_body["target"], "channel:1473922824350601297");
        assert_eq!(example_body["content"], "hello");
        assert_eq!(example_body["source"], "operator");
        assert!(
            send.curl_example
                .expect("send docs must include a curl example")
                .contains("X-AgentDesk-Source: cli")
        );
    }

    #[test]
    fn session_termination_events_docs_include_filters_and_response_shape() {
        let endpoints = all_endpoints();
        let endpoint = endpoints
            .iter()
            .find(|endpoint| {
                endpoint.method == "GET" && endpoint.path == "/api/session-termination-events"
            })
            .expect("GET /api/session-termination-events must be documented");

        for param in ["dispatch_id", "card_id", "session_key", "limit"] {
            assert!(
                endpoint.params.contains_key(param),
                "termination event docs must include {param}"
            );
        }
        assert_eq!(
            endpoint
                .params
                .get("limit")
                .expect("limit should be documented")
                .default,
            Some(json!(50))
        );
        assert!(
            !endpoint.description.contains("TODO"),
            "termination event docs should not be a placeholder"
        );

        let event = &endpoint
            .example
            .as_ref()
            .expect("termination event docs must include an example")
            .response["events"][0];
        assert_eq!(event["tmux_alive"], false);
        assert_eq!(event["last_offset"], Value::Null);
        assert_eq!(event["created_at"], "2026-05-16T04:15:48.151Z");
    }
}
