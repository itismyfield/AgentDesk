use axum::{
    Json,
    extract::{Path, Query},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

// Category: ops

#[derive(Debug, Default, Deserialize)]
pub struct ApiDocsQuery {
    pub format: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ParamDoc {
    pub location: &'static str,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub required: bool,
    pub description: &'static str,
    #[serde(skip_serializing_if = "Option::is_none", rename = "enum")]
    pub enum_values: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

impl ParamDoc {
    fn with_enum(mut self, values: &[&'static str]) -> Self {
        self.enum_values = Some(values.iter().copied().collect());
        self
    }

    fn with_default(mut self, value: impl Into<Value>) -> Self {
        self.default = Some(value.into());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
struct ExampleDoc {
    pub request: Value,
    pub response: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
struct EndpointDoc {
    pub method: &'static str,
    pub path: &'static str,
    pub category: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subcategory: Option<&'static str>,
    pub description: &'static str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamDoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<ExampleDoc>,
    /// #1068 (904-6) paired-scenario companion: canonical 4xx failure example.
    /// When present, surfaces alongside the happy-path `example` so callers can
    /// see both the success shape and the most common error shape at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_example: Option<ExampleDoc>,
    /// Nested operation-level failure that can still be returned with a
    /// successful HTTP status. Used for partial-success contracts where callers
    /// must inspect nested result fields instead of trusting the status code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_success_example: Option<ExampleDoc>,
    /// #1068 (904-6) curl 1-liner reference. Intentionally a single physical
    /// line so it can be copy-pasted directly into a terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curl_example: Option<&'static str>,
    #[serde(skip_serializing_if = "is_false")]
    pub deprecated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<&'static str>,
}

impl EndpointDoc {
    fn with_params<const N: usize>(mut self, params: [(&'static str, ParamDoc); N]) -> Self {
        self.params = params
            .into_iter()
            .map(|(name, param)| (name.to_string(), param))
            .collect();
        self
    }

    fn with_example(mut self, request: Value, response: Value) -> Self {
        self.example = Some(ExampleDoc {
            request,
            response,
            status: Some(200),
            scenario: Some("happy"),
        });
        self
    }

    /// #1068 (904-6) paired-scenario companion. Records the canonical 4xx
    /// failure response (shape + status) so callers can see both the success
    /// shape from `with_example` and the most common mistake at once.
    fn with_error_example(mut self, status: u16, request: Value, response: Value) -> Self {
        self.error_example = Some(ExampleDoc {
            request,
            response,
            status: Some(status),
            scenario: Some("error"),
        });
        self
    }

    fn with_partial_success_example(mut self, request: Value, response: Value) -> Self {
        self.partial_success_example = Some(ExampleDoc {
            request,
            response,
            status: Some(200),
            scenario: Some("partial_success"),
        });
        self
    }

    /// #1068 (904-6) curl 1-liner. Must stay a single physical line so it can
    /// be copy-pasted into a terminal without escaping line breaks.
    fn with_curl(mut self, curl: &'static str) -> Self {
        self.curl_example = Some(curl);
        self
    }
}

#[derive(Debug, Clone, Serialize)]
struct CategorySummary {
    pub name: &'static str,
    pub count: usize,
    pub description: &'static str,
    pub subcategories: Vec<SubcategorySummary>,
}

#[derive(Debug, Clone, Serialize)]
struct SubcategorySummary {
    pub name: &'static str,
    pub count: usize,
    pub description: &'static str,
}

fn ep(
    method: &'static str,
    path: &'static str,
    category: &'static str,
    description: &'static str,
) -> EndpointDoc {
    let canonical_category = canonical_category(category);
    EndpointDoc {
        method,
        path,
        category: canonical_category,
        subcategory: (canonical_category != category).then_some(category),
        description,
        params: BTreeMap::new(),
        example: None,
        error_example: None,
        partial_success_example: None,
        curl_example: None,
        deprecated: false,
        canonical_path: None,
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn path_param(description: &'static str) -> ParamDoc {
    ParamDoc {
        location: "path",
        kind: "string",
        required: true,
        description,
        enum_values: None,
        default: None,
    }
}

fn query_param(kind: &'static str, required: bool, description: &'static str) -> ParamDoc {
    ParamDoc {
        location: "query",
        kind,
        required,
        description,
        enum_values: None,
        default: None,
    }
}

fn body_param(kind: &'static str, required: bool, description: &'static str) -> ParamDoc {
    ParamDoc {
        location: "body",
        kind,
        required,
        description,
        enum_values: None,
        default: None,
    }
}

// ---------------------------------------------------------------------------
// #1068 (904-6) — top-40 paired-scenario endpoints.
//
// These are the high-traffic runtime/kanban/agent/queue/integration endpoints
// that must ship with BOTH a happy-path example AND an error example plus a
// curl 1-liner so agents can learn the contract without reading source.
//
// The test `api_docs_exposes_paired_examples_for_top_40` iterates this slice
// and asserts `example`, `error_example`, and `curl_example` are all populated.
// Endpoints not in this list may rely on `// TODO: example` markers in their
// description text while paired coverage expands in follow-up issues.
// ---------------------------------------------------------------------------
pub(crate) const TOP_40_PAIRED_PATHS: &[(&str, &str)] = &[
    ("GET", "/api/health"),
    ("POST", "/api/discord/send"),
    ("POST", "/api/discord/send-to-agent"),
    ("POST", "/api/discord/send-dm"),
    ("GET", "/api/agents"),
    ("POST", "/api/agents"),
    ("GET", "/api/agents/{id}"),
    ("PATCH", "/api/agents/{id}"),
    ("POST", "/api/agents/setup"),
    ("POST", "/api/agents/{id}/turn/start"),
    ("POST", "/api/agents/{id}/turn/stop"),
    ("GET", "/api/agents/{id}/quality"),
    ("GET", "/api/agents/quality/ranking"),
    ("GET", "/api/kanban-cards"),
    ("POST", "/api/kanban-cards"),
    ("GET", "/api/kanban-cards/{id}"),
    ("PATCH", "/api/kanban-cards/{id}"),
    ("POST", "/api/kanban-cards/{id}/assign"),
    ("POST", "/api/kanban-cards/{id}/transition"),
    ("POST", "/api/kanban-cards/{id}/retry"),
    ("POST", "/api/kanban-cards/{id}/redispatch"),
    ("POST", "/api/kanban-cards/{id}/resume"),
    ("POST", "/api/kanban-cards/{id}/reopen"),
    ("POST", "/api/kanban-cards/assign-issue"),
    ("GET", "/api/dispatches"),
    ("POST", "/api/dispatches"),
    ("GET", "/api/dispatches/{id}"),
    ("GET", "/api/dispatches/{id}/events"),
    ("PATCH", "/api/dispatches/{id}"),
    ("POST", "/api/queue/generate"),
    ("POST", "/api/queue/dispatch-next"),
    ("GET", "/api/queue/status"),
    ("POST", "/api/queue/pause"),
    ("POST", "/api/queue/resume"),
    ("POST", "/api/queue/cancel"),
    ("PATCH", "/api/queue/reorder"),
    ("POST", "/api/github/issues/create"),
    ("GET", "/api/pipeline/cards/{card_id}"),
    ("GET", "/api/analytics/observability"),
    ("GET", "/api/analytics/invariants"),
    ("POST", "/api/reviews/verdict"),
    ("GET", "/api/docs"),
];

const CANONICAL_CATEGORIES: [&str; 8] = [
    "agents",
    "kanban",
    "dispatches",
    "queue",
    "routines",
    "ops",
    "integrations",
    "admin",
];

fn canonical_category(category: &str) -> &'static str {
    match category {
        "agents" => "agents",
        "kanban" | "kanban-repos" | "pipeline" | "pm" | "reviews" => "kanban",
        "dispatches" | "dispatched-sessions" | "internal" | "messages" | "sessions" => "dispatches",
        "auto-queue" | "cron" | "queue" => "queue",
        "routines" => "routines",
        "analytics" | "auth" | "cluster" | "docs" | "health" | "monitoring" | "stats"
        | "provider-cli" => "ops",
        "discord" | "github" | "github-dashboard" | "meetings" => "integrations",
        "departments" | "memory" | "offices" | "onboarding" | "policies" | "settings"
        | "skills" => "admin",
        _ => "ops",
    }
}

fn is_canonical_category(category: &str) -> bool {
    CANONICAL_CATEGORIES.contains(&category)
}

// ---------------------------------------------------------------------------
// #1063 — 8-group hierarchical docs taxonomy.
//
// The hierarchy is:
//   GET /api/docs                     -> { groups: [{name, description, categories: [names]}] }
//   GET /api/docs/{group}             -> { group, categories: [{name, description, endpoint_count}] }
//   GET /api/docs/{group}/{category}  -> { group, category, endpoints: [full endpoint docs] }
//
// `CANONICAL_CATEGORIES` (the legacy 7-group contract) is retained so that
// existing callers of `/api/docs/{category}` (and `api_help`) keep working,
// but the default root response now reflects the new GROUP_NAMES hierarchy.
// ---------------------------------------------------------------------------

const GROUP_NAMES: [&str; 8] = [
    "runtime",
    "kanban",
    "agents",
    "integrations",
    "automation",
    "config",
    "observability",
    "internal",
];

/// Maps a fine-grained endpoint category (the value stored on
/// `EndpointDoc.subcategory` when present, otherwise `EndpointDoc.category`)
/// to its owning top-level group.
fn category_to_group(category: &str) -> &'static str {
    match category {
        // runtime — turns, sessions, dispatches, server lifecycle, messages
        "dispatches" | "dispatched-sessions" | "sessions" | "messages" => "runtime",
        // kanban — cards, pipeline, reviews, pm, repo config
        "kanban" | "kanban-repos" | "pipeline" | "pm" | "reviews" => "kanban",
        // agents — agents, agent-setup, agent-quality, roles
        "agents" => "agents",
        // integrations — discord, github, meetings, provider, mcp
        "discord" | "github" | "github-dashboard" | "meetings" => "integrations",
        // automation — auto-queue, policies, scheduler, cron, maintenance
        "auto-queue" | "queue" | "cron" | "policies" | "routines" => "automation",
        // config — settings, onboarding, knowledge, source-of-truth, skills,
        // offices, departments, memory (#1066 /api/memory dual-mode)
        "settings" | "onboarding" | "skills" | "offices" | "departments" | "memory" => "config",
        // observability — analytics, metrics, events, slo, diagnostics,
        // monitoring, stats, health, auth
        "analytics" | "cluster" | "monitoring" | "stats" | "health" | "auth" => "observability",
        // internal — debug, testing, internal endpoints, docs discovery
        "internal" | "docs" => "internal",
        _ => "internal",
    }
}

/// Short human-readable description for one of the top-level groups.
fn group_description(group: &str) -> &'static str {
    match group {
        "runtime" => "Turns, sessions, dispatches, message log, and server lifecycle surfaces.",
        "kanban" => {
            "Kanban cards, pipeline state, reviews (kanban/reviews), PM decisions, and repo board config."
        }
        "agents" => {
            "Agent registry, turn control, setup wizard, quality telemetry, and role metadata."
        }
        "integrations" => {
            "Discord, GitHub, round-table meetings, and provider/MCP integration entrypoints."
        }
        "automation" => {
            "Auto-queue generation and dispatch, policies, cron, and scheduled maintenance jobs."
        }
        "config" => {
            "Settings, onboarding, skill catalog, knowledge, and source-of-truth config surfaces."
        }
        "observability" => {
            "Analytics, metrics, events, SLO signals, diagnostics, health, and monitoring surfaces."
        }
        "internal" => "Internal, debug, testing, and API-docs discovery endpoints.",
        _ => "Miscellaneous API endpoints.",
    }
}

/// Returns the effective fine-grained category for an endpoint (subcategory if
/// present, otherwise canonical category).
fn effective_category(endpoint: &EndpointDoc) -> &'static str {
    endpoint.subcategory.unwrap_or(endpoint.category)
}

/// For a given top-level group, return the list of distinct fine-grained
/// categories under it plus their endpoint counts, in a deterministic order.
fn categories_for_group(endpoints: &[EndpointDoc], group: &str) -> Vec<(&'static str, usize)> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for endpoint in endpoints {
        let category = effective_category(endpoint);
        if category_to_group(category) == group {
            *counts.entry(category).or_default() += 1;
        }
    }
    counts.into_iter().collect()
}

fn category_description(category: &str) -> &'static str {
    match category {
        "agents" => "Agent registry, turn control, setup, and activity timelines.",
        "kanban" => "Kanban cards, pipeline state, reviews, PM decisions, and repo board config.",
        "dispatches" => {
            "Dispatch CRUD, dispatched sessions, runtime sessions, and message records."
        }
        "queue" => {
            "Auto-queue generation, activation, ordering, live queue views, and turn control."
        }
        "ops" => "Health, auth, docs, analytics, stats, and monitoring surfaces.",
        "integrations" => "Discord, GitHub, and round-table meeting integration entrypoints.",
        "admin" => "Onboarding, settings, policies, skills, offices, and departments.",
        "api-friction" => {
            "Structured API-friction events, repeated-pattern aggregation, and auto issue creation."
        }
        "analytics" => "Operational analytics, receipts, machine status, and rate-limit views.",
        "auth" => "Current authentication session state.",
        "auto-queue" => {
            "Auto-queue generation, activation, slot repair, and queue execution control."
        }
        "cron" => "Registered cron jobs per agent.",
        "cluster" => "Multinode worker-node registry, heartbeat, and role diagnostics.",
        "departments" => "Department CRUD and ordering.",
        "discord" => "Discord delivery helpers, bindings, message reads, and DM reply hooks.",
        "dispatched-sessions" => "Persisted dispatched-session lifecycle and cleanup helpers.",
        "docs" => "API documentation discovery and category drill-down.",
        "github" => "GitHub repository integration, issue creation, and sync entrypoints.",
        "github-dashboard" => "Dashboard-oriented GitHub read models and issue actions.",
        "health" => "Health and liveness endpoints.",
        "internal" => "Internal-only thread reuse helpers used by the runtime.",
        "kanban-repos" => "Kanban repository settings and ownership metadata.",
        "meetings" => "Round-table meeting lifecycle and issue generation.",
        "memory" => {
            "Memory fragment CRUD (recall/remember/forget) with auto-selected memento-or-local backend."
        }
        "messages" => "Message log read/write APIs.",
        "monitoring" => "Channel monitoring status entries and rendered status updates.",
        "offices" => "Office CRUD, ordering, and agent membership.",
        "onboarding" => "Initial setup, provider validation, and prompt generation.",
        "pipeline" => "Pipeline stages, config overrides, graphs, and card history.",
        "pm" => "PM decision workflow for force-only pipeline states.",
        "policies" => "Loaded policy inventory.",
        "provider-cli" => {
            "Provider CLI safe migration: channel registry, upgrade orchestration, and operator promote/rollback."
        }
        "reviews" => "Review verdict submission, decisions, and tuning aggregation.",
        "routines" => "Durable script-backed routines, run history, and manual routine controls.",
        "sessions" => "Sessions, force-kill, and termination events.",
        "settings" => "Settings surfaces, live overrides, precedence, and onboarding contracts.",
        "skills" => "Skill catalog and usage ranking.",
        "stats" => "Aggregate system counters.",
        _ => "Miscellaneous API endpoints.",
    }
}

fn category_summaries(endpoints: &[EndpointDoc]) -> Vec<CategorySummary> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for endpoint in endpoints {
        *counts.entry(endpoint.category).or_default() += 1;
    }

    CANONICAL_CATEGORIES
        .into_iter()
        .map(|name| CategorySummary {
            name,
            count: counts.get(name).copied().unwrap_or_default(),
            description: category_description(name),
            subcategories: subcategory_summaries(endpoints, name),
        })
        .collect()
}

fn subcategory_summaries(endpoints: &[EndpointDoc], category: &str) -> Vec<SubcategorySummary> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for endpoint in endpoints {
        if endpoint.category == category {
            let subcategory = endpoint.subcategory.unwrap_or(endpoint.category);
            *counts.entry(subcategory).or_default() += 1;
        }
    }

    counts
        .into_iter()
        .map(|(name, count)| SubcategorySummary {
            name,
            count,
            description: category_description(name),
        })
        .collect()
}

fn all_endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "GET",
            "/api/health",
            "health",
            "Health check with `server_up` minimum readiness and `fully_recovered` startup recovery completion.",
        )
        .with_example(
            json!({}),
            json!({
                "status": "healthy",
                "server_up": true,
                "fully_recovered": true,
                "latest_startup_doctor": {
                    "available": true,
                    "status": "warned",
                    "artifact_path": "/Users/kunkun/.adk/release/runtime/doctor/startup/123-456.json",
                    "started_at": "2026-04-26T14:49:14+09:00",
                    "completed_at": "2026-04-26T14:49:17+09:00",
                    "boot_id": "123-456",
                    "summary": {"passed": 21, "warned": 3, "failed": 0, "total": 24},
                    "failed_count": 0,
                    "warned_count": 3,
                    "detail_endpoint": "/api/doctor/startup/latest"
                },
                "db": true,
                "dashboard": true,
                "deferred_hooks": 0,
                "queue_depth": 0,
                "watcher_count": 0,
                "outbox_age": 0,
                "recovery_duration": 0.12
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"status": "degraded", "server_up": true, "fully_recovered": false, "db": false, "error": "db connection failing"}),
        )
        .with_curl("curl http://localhost:8787/api/health"),
        ep(
            "GET",
            "/api/health/detail",
            "health",
            "Local/protected detailed health with provider diagnostics and latest startup doctor detail.",
        )
        .with_example(
            json!({}),
            json!({
                "status": "healthy",
                "server_up": true,
                "fully_recovered": true,
                "latest_startup_doctor": {
                    "available": true,
                    "status": "failed",
                    "artifact_path": "/Users/kunkun/.adk/release/runtime/doctor/startup/123-456.json",
                    "summary": {"passed": 21, "warned": 3, "failed": 1, "total": 25},
                    "failed_count": 1,
                    "warned_count": 3,
                    "detail_endpoint": "/api/doctor/startup/latest",
                    "run_context": "startup_once",
                    "non_fatal": true,
                    "failed_checks": [{"id": "dispatch_outbox", "status": "fail"}],
                    "warned_checks": [{"id": "disk_usage", "status": "warn"}],
                    "followup_context": "restart_followup"
                }
            }),
        )
        .with_error_example(
            403,
            json!({}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl http://localhost:8787/api/health/detail"),
        ep(
            "GET",
            "/api/prompt-manifest/retention",
            "monitoring",
            "Prompt-manifest storage stats and the boot-time retention config snapshot; retention config changes require process restart and are not hot-reloaded.",
        )
        .with_example(
            json!({}),
            json!({
                "total_stored_bytes": 1234,
                "total_original_bytes": 5678,
                "manifest_count": 42,
                "layer_count": 168,
                "truncated_count": 3,
                "oldest_full_content_at": "2026-04-04T01:23:45Z",
                "retention_horizon_at": "2026-04-04T01:23:45Z",
                "retention_days": 30,
                "per_layer_max_bytes_adk_provided": 65536,
                "per_layer_max_bytes_user_derived": 16384,
                "enabled": true,
                "restart_required_for_config_changes": true,
                "config_applied_at": "boot",
                "config_source": "agentdesk.yaml boot snapshot",
                "hot_reload": false
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres pool unavailable"}),
        )
        .with_curl("curl http://localhost:8787/api/prompt-manifest/retention"),
        ep(
            "GET",
            "/api/cluster/nodes",
            "cluster",
            "Protected multinode worker registry view with configured/effective role, heartbeat, labels, and capabilities.",
        )
        .with_example(
            json!({}),
            json!({
                "cluster": {
                    "enabled": true,
                    "configured_role": "auto",
                    "lease_ttl_secs": 30,
                    "heartbeat_interval_secs": 10
                },
                "nodes": [{
                    "instance_id": "mac-mini",
                    "hostname": "mac-mini",
                    "role": "auto",
                    "effective_role": "leader",
                    "status": "online",
                    "labels": ["mac-mini"],
                    "capabilities": {"providers": ["codex"]}
                }]
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres unavailable"}),
        )
        .with_curl("curl http://localhost:8787/api/cluster/nodes"),
        ep(
            "GET",
            "/api/cluster/routing-diagnostics",
            "cluster",
            "Explain which multinode workers satisfy a required capability set and why excluded workers do not match.",
        )
        .with_example(
            json!({"required": "{\"labels\":[\"mac-book\"],\"providers\":[\"codex\"],\"mcp\":{\"filesystem\":{\"healthy\":true}}}"}),
            json!({
                "required": {
                    "labels": ["mac-book"],
                    "providers": ["codex"],
                    "mcp": {"filesystem": {"healthy": true}}
                },
                "decisions": [{
                    "instance_id": "mac-book-release",
                    "eligible": true,
                    "reasons": []
                }, {
                    "instance_id": "mac-mini-release",
                    "eligible": false,
                    "reasons": ["missing label 'mac-book'"]
                }]
            }),
        )
        .with_error_example(
            400,
            json!({"required": "{not-json"}),
            json!({"error": "invalid required JSON: expected object key"}),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres unavailable"}),
        )
        .with_curl("curl --get http://localhost:8787/api/cluster/routing-diagnostics --data-urlencode 'required={\"labels\":[\"mac-book\"],\"providers\":[\"codex\"]}'"),
        ep(
            "GET",
            "/api/cluster/resource-locks",
            "cluster",
            "List active multinode resource locks used to serialize exclusive worker resources such as Unreal editor/test execution.",
        )
        .with_example(
            json!({"include_expired": false}),
            json!({
                "default_ttl_secs": 900,
                "locks": [{
                    "lock_key": "unreal:project:CookingHeart",
                    "holder_instance_id": "mac-book-release",
                    "holder_job_id": "phase-compile",
                    "metadata": {"phase": "compile"},
                    "expires_at": "2026-05-01T06:25:00Z",
                    "heartbeat_at": "2026-05-01T06:10:00Z",
                    "created_at": "2026-05-01T06:10:00Z",
                    "updated_at": "2026-05-01T06:10:00Z"
                }]
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres unavailable"}),
        )
        .with_curl("curl http://localhost:8787/api/cluster/resource-locks"),
        ep(
            "POST",
            "/api/cluster/resource-locks/acquire",
            "cluster",
            "Acquire or renew a PG-backed exclusive resource lock. Conflicting active holders return 409 with the current holder.",
        )
        .with_example(
            json!({
                "lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-compile",
                "ttl_secs": 900,
                "metadata": {"phase": "compile"}
            }),
            json!({
                "acquired": true,
                "lock": {
                    "lock_key": "unreal:project:CookingHeart",
                    "holder_instance_id": "mac-book-release",
                    "holder_job_id": "phase-compile"
                },
                "current": null
            }),
        )
        .with_error_example(
            409,
            json!({
                "lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-mini-release",
                "holder_job_id": "phase-compile"
            }),
            json!({"acquired": false, "lock": null, "current": {"holder_instance_id": "mac-book-release"}}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/resource-locks/acquire -H 'content-type: application/json' -d '{\"lock_key\":\"unreal:project:CookingHeart\",\"holder_instance_id\":\"mac-book-release\",\"holder_job_id\":\"phase-compile\"}'"),
        ep(
            "POST",
            "/api/cluster/resource-locks/heartbeat",
            "cluster",
            "Extend a resource lock only when the same holder still owns the lock.",
        )
        .with_example(
            json!({
                "lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-compile",
                "ttl_secs": 900
            }),
            json!({"ok": true, "lock": {"lock_key": "unreal:project:CookingHeart"}}),
        )
        .with_error_example(
            409,
            json!({}),
            json!({"ok": false, "error": "lock is not held by requester or has expired"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/resource-locks/heartbeat -H 'content-type: application/json' -d '{\"lock_key\":\"unreal:project:CookingHeart\",\"holder_instance_id\":\"mac-book-release\",\"holder_job_id\":\"phase-compile\"}'"),
        ep(
            "POST",
            "/api/cluster/resource-locks/release",
            "cluster",
            "Release a resource lock only when lock key, holder instance, and holder job all match.",
        )
        .with_example(
            json!({
                "lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-compile"
            }),
            json!({"released": true}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/resource-locks/release -H 'content-type: application/json' -d '{\"lock_key\":\"unreal:project:CookingHeart\",\"holder_instance_id\":\"mac-book-release\",\"holder_job_id\":\"phase-compile\"}'"),
        ep(
            "POST",
            "/api/cluster/resource-locks/reclaim-expired",
            "cluster",
            "Delete expired resource locks so crashed workers do not permanently hold exclusive resources.",
        )
        .with_example(json!({}), json!({"reclaimed": 1}))
        .with_curl("curl -X POST http://localhost:8787/api/cluster/resource-locks/reclaim-expired"),
        ep(
            "GET",
            "/api/cluster/test-phase-runs",
            "cluster",
            "List deterministic test phase evidence records by phase, head SHA, and status for multinode merge gates.",
        )
        .with_example(
            json!({"phase_key": "unreal-smoke", "head_sha": "abc123", "status": "passed"}),
            json!({
                "runs": [{
                    "id": "tpr-123",
                    "idempotency_key": "unreal-smoke:abc123",
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "passed",
                    "required_capabilities": {"labels": ["mac-book"], "unreal": true},
                    "resource_lock_key": "unreal:project:CookingHeart",
                    "evidence": {"log": "passed"},
                    "completed_at": "2026-05-01T06:30:00Z"
                }]
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres unavailable"}),
        )
        .with_curl("curl --get http://localhost:8787/api/cluster/test-phase-runs --data-urlencode phase_key=unreal-smoke --data-urlencode head_sha=abc123"),
        ep(
            "POST",
            "/api/cluster/test-phase-runs/upsert",
            "cluster",
            "Create or update the idempotent evidence row for one test phase and commit head SHA.",
        )
        .with_example(
            json!({
                "phase_key": "unreal-smoke",
                "head_sha": "abc123",
                "status": "passed",
                "issue_id": "881",
                "card_id": "card-881",
                "required_capabilities": {"labels": ["mac-book"], "unreal": true},
                "resource_lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-unreal-smoke-abc123",
                "evidence": {"runner": "deterministic-phase-runner", "result": "passed"}
            }),
            json!({
                "run": {
                    "idempotency_key": "unreal-smoke:abc123",
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "passed"
                }
            }),
        )
        .with_error_example(
            400,
            json!({"phase_key": "", "head_sha": "abc123"}),
            json!({"error": "phase_key is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/test-phase-runs/upsert -H 'content-type: application/json' -d '{\"phase_key\":\"unreal-smoke\",\"head_sha\":\"abc123\",\"status\":\"passed\"}'"),
        ep(
            "POST",
            "/api/cluster/test-phase-runs/start",
            "cluster",
            "Acquire the required resource lock and mark a deterministic test phase as running.",
        )
        .with_example(
            json!({
                "phase_key": "unreal-smoke",
                "head_sha": "abc123",
                "resource_lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-unreal-smoke-abc123",
                "ttl_secs": 900,
                "required_capabilities": {"labels": ["mac-book"], "unreal": true}
            }),
            json!({
                "started": true,
                "run": {
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "running"
                },
                "lock": {"lock_key": "unreal:project:CookingHeart"},
                "current_lock": null
            }),
        )
        .with_error_example(
            409,
            json!({}),
            json!({"started": false, "run": null, "lock": null, "current_lock": {"holder_instance_id": "mac-mini-release"}}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/test-phase-runs/start -H 'content-type: application/json' -d '{\"phase_key\":\"unreal-smoke\",\"head_sha\":\"abc123\",\"resource_lock_key\":\"unreal:project:CookingHeart\",\"holder_instance_id\":\"mac-book-release\",\"holder_job_id\":\"phase-unreal-smoke-abc123\"}'"),
        ep(
            "POST",
            "/api/cluster/test-phase-runs/complete",
            "cluster",
            "Record terminal phase evidence and optionally release the resource lock held by the runner.",
        )
        .with_example(
            json!({
                "phase_key": "unreal-smoke",
                "head_sha": "abc123",
                "status": "passed",
                "release_lock": true,
                "evidence": {"result": "passed", "log_path": "Saved/Logs/phase.log"}
            }),
            json!({
                "run": {
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "passed"
                },
                "lock_released": true
            }),
        )
        .with_error_example(
            400,
            json!({"phase_key": "unreal-smoke", "head_sha": "abc123", "status": "running"}),
            json!({"error": "complete requires status passed, failed, or canceled"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/test-phase-runs/complete -H 'content-type: application/json' -d '{\"phase_key\":\"unreal-smoke\",\"head_sha\":\"abc123\",\"status\":\"passed\",\"release_lock\":true}'"),
        ep(
            "GET",
            "/api/cluster/test-phase-runs/evidence",
            "cluster",
            "Fetch the latest passing evidence for a required phase/head SHA pair. Merge gates should use this shape before accepting phase evidence.",
        )
        .with_example(
            json!({"phase_key": "unreal-smoke", "head_sha": "abc123"}),
            json!({
                "ok": true,
                "run": {
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "passed",
                    "evidence": {"result": "passed"}
                }
            }),
        )
        .with_error_example(
            404,
            json!({"phase_key": "unreal-smoke", "head_sha": "missing"}),
            json!({"ok": false, "error": "passing evidence not found"}),
        )
        .with_curl("curl --get http://localhost:8787/api/cluster/test-phase-runs/evidence --data-urlencode phase_key=unreal-smoke --data-urlencode head_sha=abc123"),
        ep(
            "POST",
            "/api/cluster/task-dispatches/claim",
            "cluster",
            "Atomically claim pending task_dispatches for a worker with PG row locking and capability-match diagnostics.",
        )
        .with_example(
            json!({
                "claim_owner": "mac-book-release",
                "ttl_secs": 600,
                "limit": 10,
                "dispatch_type": "implementation"
            }),
            json!({
                "claimed": [{
                    "id": "dispatch-123",
                    "claim_owner": "mac-book-release",
                    "required_capabilities": {"labels": ["mac-book"]}
                }],
                "skipped": [{
                    "id": "dispatch-456",
                    "reasons": ["missing label 'mac-mini'"]
                }]
            }),
        )
        .with_error_example(
            400,
            json!({"claim_owner": ""}),
            json!({"error": "claim_owner is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/task-dispatches/claim -H 'content-type: application/json' -d '{\"claim_owner\":\"mac-book-release\",\"limit\":10}'"),
        ep(
            "GET",
            "/api/cluster/issue-specs",
            "cluster",
            "List parsed Issue-as-Spec contracts, including required phases consumed by merge gates.",
        )
        .with_example(
            json!({"card_id": "card-881"}),
            json!({
                "specs": [{
                    "issue_id": "881",
                    "card_id": "card-881",
                    "required_phases": ["unreal-smoke"],
                    "validation_errors": []
                }]
            }),
        )
        .with_curl("curl --get http://localhost:8787/api/cluster/issue-specs --data-urlencode card_id=card-881"),
        ep(
            "POST",
            "/api/cluster/issue-specs/upsert",
            "cluster",
            "Parse a GitHub issue body into acceptance criteria, test plan, DoD, and required phase keys.",
        )
        .with_example(
            json!({
                "issue_id": "881",
                "card_id": "card-881",
                "repo_id": "itismyfield/AgentDesk",
                "issue_number": 881,
                "head_sha": "abc123",
                "body": "## Acceptance Criteria\n- Evidence is persisted\n\n## Test Plan\n- Run regression\n\n## Definition of Done\n- Gate consumes evidence\n\n## Required Phases\n- Unreal Smoke"
            }),
            json!({
                "spec": {
                    "issue_id": "881",
                    "required_phases": ["unreal-smoke"],
                    "validation_errors": []
                }
            }),
        )
        .with_error_example(
            400,
            json!({"issue_id": "", "body": ""}),
            json!({"error": "issue_id is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/issue-specs/upsert -H 'content-type: application/json' -d '{\"issue_id\":\"881\",\"body\":\"## Acceptance Criteria\\n- Done\\n\\n## Test Plan\\n- Test\\n\\n## Definition of Done\\n- Ship\"}'"),
        ep(
            "GET",
            "/api/doctor/startup/latest",
            "health",
            "Local/protected latest startup doctor artifact envelope for agent rescue and diagnosis.",
        )
        .with_example(
            json!({}),
            json!({
                "ok": true,
                "available": true,
                "artifact_path": "/Users/kunkun/.adk/release/runtime/doctor/startup/123-456.json",
                "detail_source": "startup_doctor_artifact",
                "followup_context": "restart_followup",
                "summary": {"passed": 21, "warned": 3, "failed": 1, "total": 25},
                "artifact": {"schema_version": 1, "boot_id": "123-456", "checks": []}
            }),
        )
        .with_error_example(
            200,
            json!({}),
            json!({"ok": true, "available": false, "artifact_path": null, "reason": "startup_doctor_artifact_missing", "artifact": null}),
        )
        .with_curl("curl http://localhost:8787/api/doctor/startup/latest"),
        ep(
            "POST",
            "/api/doctor/stale-mailbox/repair",
            "health",
            "Local/protected stale mailbox repair endpoint used by doctor follow-up workflows.",
        )
        .with_params([
            ("channel_id", body_param("integer", true, "Discord channel snowflake")),
            (
                "expected_has_cancel_token",
                body_param("boolean", false, "Optional guard for the observed mailbox token state"),
            ),
        ])
        .with_example(
            json!({"body": {"channel_id": "1486017489027469493", "expected_has_cancel_token": true}}),
            json!({"ok": true, "applied": true}),
        )
        .with_error_example(
            403,
            json!({"body": {"channel_id": "1486017489027469493"}}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/doctor/stale-mailbox/repair -H 'Content-Type: application/json' -d '{\"channel_id\":1486017489027469493}'"),
        ep(
            "POST",
            "/api/channels/{id}/relay-recovery",
            "health",
            "Local/protected relay recovery dry-run endpoint with bounded apply for safe local auto-heal actions, including stale proof cleanup and detached watcher reattach.",
        )
        .with_params([
            ("id", path_param("Discord channel snowflake")),
            (
                "provider",
                body_param("string", false, "Optional provider filter such as codex"),
            ),
            (
                "apply",
                body_param("boolean", false, "Default false. When true, only eligible bounded local cleanup or watcher reattach may run"),
            ),
        ])
        .with_example(
            json!({"body": {"provider": "codex", "apply": false}}),
            json!({
                "ok": true,
                "mode": "dry_run",
                "applied": false,
                "skipped": false,
                "decision": {
                    "relay_stall_state": "orphan_pending_token",
                    "action": "clear_orphan_pending_token",
                    "reason": "mailbox holds a cancel token without bridge, watcher, or live tmux evidence",
                    "evidence": {"mailbox_has_cancel_token": true, "bridge_inflight_present": false, "watcher_attached": false},
                    "affected": {"channel_id": "1486017489027469493", "provider": "codex"},
                    "auto_heal": {"eligible": true, "bounded": true, "max_attempts_per_window": 1, "window_secs": 600}
                }
            }),
        )
        .with_error_example(
            403,
            json!({"body": {"provider": "codex"}}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/channels/1486017489027469493/relay-recovery -H 'Content-Type: application/json' -d '{\"provider\":\"codex\",\"apply\":false}'"),
        ep(
            "POST",
            "/api/discord/send",
            "discord",
            "Send a Discord channel message",
        )
        .with_params([
            (
                "target",
                body_param("string", false, "Target channel:<id>|channel:<name>|agent:<roleId>"),
            ),
            (
                "content",
                body_param("string", false, "Message body (markdown supported)"),
            ),
            (
                "channel_id",
                body_param("string", false, "Alias for target=channel:<id>"),
            ),
            ("message", body_param("string", false, "Alias for content")),
            (
                "source",
                body_param("string", false, "Known agent role_id or internal source; defaults to system"),
            ),
            (
                "bot",
                body_param("string", false, "Delivery bot: announce (default) or notify"),
            ),
        ])
        .with_example(
            json!({"body": {"target": "channel:1473922824350601297", "content": "hello", "source": "system", "bot": "notify"}}),
            json!({"ok": true, "message_id": "1500000000000000000"}),
        )
        .with_error_example(
            400,
            json!({"body": {"target": "channel:1473922824350601297"}}),
            json!({"error": "content is required", "ok": false}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/discord/send -H 'Content-Type: application/json' -d '{\"target\":\"channel:1473922824350601297\",\"content\":\"hello\",\"source\":\"system\",\"bot\":\"notify\"}'"),
        ep(
            "POST",
            "/api/discord/send-to-agent",
            "discord",
            "Send a Discord message by agent role_id",
        )
        .with_params([
            ("role_id", body_param("string", true, "Target agent role_id")),
            ("message", body_param("string", true, "Discord message content")),
            (
                "mode",
                body_param(
                    "string",
                    false,
                    "Delivery bot: announce (default) or notify",
                )
                .with_enum(&["announce", "notify"]),
            ),
        ])
        .with_example(
            json!({"body": {"role_id": "project-agentdesk", "message": "deploy done", "mode": "announce"}}),
            json!({"ok": true, "channel_id": "1473922824350601297", "message_id": "1500000000000000001"}),
        )
        .with_error_example(
            404,
            json!({"body": {"role_id": "ghost-agent", "message": "hi"}}),
            json!({"error": "agent not found: ghost-agent"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/discord/send-to-agent -H 'Content-Type: application/json' -d '{\"role_id\":\"project-agentdesk\",\"message\":\"deploy done\"}'"),
        ep(
            "POST",
            "/api/discord/send-dm",
            "discord",
            "Send a Discord direct message",
        )
        .with_params([
            ("user_id", body_param("string", true, "Target Discord user snowflake")),
            ("message", body_param("string", true, "DM body")),
        ])
        .with_example(
            json!({"body": {"user_id": "100000000000000000", "message": "heads-up"}}),
            json!({"ok": true, "message_id": "1500000000000000002"}),
        )
        .with_error_example(
            400,
            json!({"body": {"message": "heads-up"}}),
            json!({"error": "user_id is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/discord/send-dm -H 'Content-Type: application/json' -d '{\"user_id\":\"100000000000000000\",\"message\":\"heads-up\"}'"),
        ep("GET", "/api/agents", "agents", "List all agents")
            .with_example(
                json!({}),
                json!({"agents": [{"id": "project-agentdesk", "name": "AgentDesk", "discord_channel_id": "1473922824350601297"}]}),
            )
            .with_error_example(
                500,
                json!({}),
                json!({"error": "internal: failed to load agents"}),
            )
            .with_curl("curl http://localhost:8787/api/agents"),
        ep("POST", "/api/agents", "agents", "Create an agent")
            .with_params([
                ("id", body_param("string", true, "New agent id (role_id)")),
                ("name", body_param("string", true, "Display name")),
                ("discord_channel_id", body_param("string", false, "Primary Discord channel")),
            ])
            .with_example(
                json!({"body": {"id": "pm-planner", "name": "PM Planner", "discord_channel_id": "1473922824350601297"}}),
                json!({"agent": {"id": "pm-planner", "name": "PM Planner", "discord_channel_id": "1473922824350601297"}}),
            )
            .with_error_example(
                409,
                json!({"body": {"id": "project-agentdesk", "name": "dup"}}),
                json!({"error": "agent id already exists: project-agentdesk"}),
            )
            .with_curl("curl -X POST http://localhost:8787/api/agents -H 'Content-Type: application/json' -d '{\"id\":\"pm-planner\",\"name\":\"PM Planner\"}'"),
        ep("GET", "/api/agents/{id}", "agents", "Get agent by ID")
            .with_params([("id", path_param("Agent id"))])
            .with_example(
                json!({"path": {"id": "project-agentdesk"}}),
                json!({"agent": {"id": "project-agentdesk", "name": "AgentDesk", "discord_channel_id": "1473922824350601297"}}),
            )
            .with_error_example(
                404,
                json!({"path": {"id": "ghost"}}),
                json!({"error": "agent not found: ghost"}),
            )
            .with_curl("curl http://localhost:8787/api/agents/project-agentdesk"),
        ep("PATCH", "/api/agents/{id}", "agents", "Update agent metadata and prompt content")
            .with_params([
                ("id", path_param("Agent id")),
                ("name", body_param("string", false, "Display name")),
                ("department_id", body_param("string", false, "Department id")),
                ("prompt_content", body_param("string", false, "Full prompt markdown content to rewrite")),
                ("auto_commit", body_param("boolean", false, "Commit prompt rewrite with git when available").with_default(false)),
            ])
            .with_example(
                json!({"path": {"id": "project-agentdesk"}, "body": {"name": "AgentDesk", "prompt_content": "# role\n...", "auto_commit": false}}),
                json!({"agent": {"id": "project-agentdesk"}, "prompt": {"changed": true}}),
            )
            .with_error_example(
                404,
                json!({"path": {"id": "ghost"}, "body": {"name": "x"}}),
                json!({"error": "agent not found: ghost"}),
            )
            .with_curl("curl -X PATCH http://localhost:8787/api/agents/project-agentdesk -H 'Content-Type: application/json' -d '{\"name\":\"AgentDesk\"}'"),
        ep("DELETE", "/api/agents/{id}", "agents", "Delete agent // TODO: example"),
        ep(
            "POST",
            "/api/agents/{id}/archive",
            "agents",
            "Soft-archive an agent: blocks active turns, disables config binding, records agent_archive state, and optionally applies Discord readonly/move action.",
        )
        .with_params([
            ("id", path_param("Agent id")),
            ("reason", body_param("string", false, "Archive reason")),
            ("discord_action", body_param("string", false, "none, readonly, or move").with_enum(&["none", "readonly", "move"])),
            ("archive_category_id", body_param("string", false, "Discord category id for move action")),
        ])
        .with_example(
            json!({"path": {"id": "project-agentdesk"}, "body": {"reason": "temporary pause", "discord_action": "readonly"}}),
            json!({"ok": true, "archive_state": "archived"}),
        ),
        ep(
            "POST",
            "/api/agents/{id}/unarchive",
            "agents",
            "Restore an archived agent config binding and mark archive state unarchived. // TODO: example",
        ),
        ep(
            "POST",
            "/api/agents/{id}/duplicate",
            "agents",
            "Duplicate an agent by reusing /api/agents/setup with the source prompt as template. // TODO: example",
        )
        .with_params([
            ("id", path_param("Source agent id")),
            ("new_agent_id", body_param("string", true, "New role id")),
            ("channel_id", body_param("string", true, "Existing Discord channel snowflake")),
            ("provider", body_param("string", false, "Provider override")),
            ("dry_run", body_param("boolean", false, "Preview setup mutations only").with_default(false)),
        ]),
        ep(
            "GET",
            "/api/onboarding/status",
            "onboarding",
            "Get onboarding status // TODO: example",
        ),
        ep(
            "GET",
            "/api/onboarding/draft",
            "onboarding",
            "Get onboarding resume draft // TODO: example",
        ),
        ep(
            "PUT",
            "/api/onboarding/draft",
            "onboarding",
            "Persist onboarding resume draft // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/onboarding/draft",
            "onboarding",
            "Clear onboarding resume draft // TODO: example",
        ),
        ep(
            "POST",
            "/api/onboarding/validate-token",
            "onboarding",
            "Validate onboarding token // TODO: example",
        ),
        ep(
            "GET",
            "/api/onboarding/channels",
            "onboarding",
            "List onboarding candidate channels // TODO: example",
        ),
        ep(
            "POST",
            "/api/onboarding/channels",
            "onboarding",
            "Persist onboarding channel selection // TODO: example",
        ),
        ep(
            "POST",
            "/api/onboarding/complete",
            "onboarding",
            "Complete onboarding // TODO: example",
        ),
        ep(
            "POST",
            "/api/onboarding/check-provider",
            "onboarding",
            "Validate provider installation and credentials // TODO: example",
        ),
        ep(
            "POST",
            "/api/onboarding/generate-prompt",
            "onboarding",
            "Generate onboarding prompt // TODO: example",
        ),
        ep(
            "POST",
            "/api/agents/setup",
            "agents",
            "Atomically create an agent config binding, prompt file, workspace seed, DB row, and optional skill workspace mapping. Supports dry_run planning and rollback on partial failure.",
        )
        .with_params([
            ("agent_id", body_param("string", true, "New agent id")),
            (
                "channel_id",
                body_param("string", true, "Existing Discord channel snowflake"),
            ),
            (
                "provider",
                body_param("string", true, "Provider for the agent channel")
                    .with_enum(&["claude", "codex", "gemini", "opencode", "qwen"]),
            ),
            (
                "prompt_template_path",
                body_param(
                    "string",
                    true,
                    "Prompt template path, usually config/agents/_shared.prompt.md",
                ),
            ),
            (
                "skills",
                body_param("array", false, "Managed skill ids to map to the new workspace"),
            ),
            (
                "dry_run",
                body_param("boolean", false, "Validate and return planned mutations only")
                    .with_default(false),
            ),
        ])
        .with_example(
            json!({
                "body": {
                    "agent_id": "project-agentdesk",
                    "channel_id": "1473922824350601297",
                    "provider": "codex",
                    "prompt_template_path": "config/agents/_shared.prompt.md",
                    "skills": ["memory-read"],
                    "dry_run": true
                }
            }),
            json!({
                "ok": true,
                "dry_run": true,
                "created": [],
                "rolled_back": [],
                "errors": [],
            }),
        )
        .with_error_example(
            400,
            json!({"body": {"agent_id": "x", "channel_id": "1473922824350601297", "provider": "unknown", "prompt_template_path": "config/agents/_shared.prompt.md"}}),
            json!({"error": "provider must be one of claude|codex|gemini|opencode|qwen"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/setup -H 'Content-Type: application/json' -d '{\"agent_id\":\"project-agentdesk\",\"channel_id\":\"1473922824350601297\",\"provider\":\"codex\",\"prompt_template_path\":\"config/agents/_shared.prompt.md\",\"dry_run\":true}'"),
        ep(
            "GET",
            "/api/agents/{id}/offices",
            "agents",
            "List offices for agent // TODO: example",
        ),
        ep(
            "POST",
            "/api/agents/{id}/signal",
            "agents",
            "Send runtime signal to agent // TODO: example",
        ),
        ep(
            "POST",
            "/api/agents/{id}/message",
            "agents",
            "Send a trigger-capable agent handoff via the announce bot",
        )
        .with_params([
            ("id", path_param("Target agent id")),
            ("from_agent_id", body_param("string", true, "Source agent id")),
            ("message", body_param("string", true, "Message body")),
            (
                "channel_kind",
                body_param("string", false, "Target binding: cc (default) or cdx")
                    .with_enum(&["cc", "cdx"])
                    .with_default(json!("cc")),
            ),
            (
                "prefix",
                body_param("boolean", false, "Add the handoff prefix").with_default(true),
            ),
        ])
        .with_example(
            json!({"path": {"id": "adk-dashboard"}, "body": {"from_agent_id": "project-agentdesk", "message": "hello", "channel_kind": "cc", "prefix": true}}),
            json!({"to_agent_id": "adk-dashboard", "channel_id": "1473922824350601297", "channel_kind": "cc", "message_id": "1500000000000000002", "bot": "announce", "prefixed": true}),
        )
        .with_error_example(
            422,
            json!({"path": {"id": "adk-dashboard"}, "body": {"from_agent_id": "project-agentdesk", "message": "hello", "channel_kind": "cc"}}),
            json!({"error": "channel_kind unset", "to_agent_id": "adk-dashboard", "channel_kind": "cc", "available_kinds": ["cdx"]}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/adk-dashboard/message -H 'Content-Type: application/json' -d '{\"from_agent_id\":\"project-agentdesk\",\"message\":\"hello\",\"channel_kind\":\"cc\",\"prefix\":true}'"),
        ep(
            "GET",
            "/api/agents/{id}/cron",
            "agents",
            "List cron jobs for agent // TODO: example",
        ),
        ep(
            "GET",
            "/api/agents/{id}/skills",
            "agents",
            "List skills for agent // TODO: example",
        ),
        ep(
            "GET",
            "/api/agents/{id}/dispatched-sessions",
            "agents",
            "List dispatched sessions for agent. Rows are de-duplicated by \
             (channel_id, agent_id) so the same agent never appears twice for \
             the same Discord channel even when stale provider snapshots \
             linger. Each row carries Discord deeplink fields the dashboard \
             can drop straight into an anchor `href`: `channel_id`, \
             `deeplink_url` (web — https://discord.com/channels/{guild}/{channel}), \
             plus thread aliases `thread_id` and `thread_deeplink_url` \
             (Discord app — discord://discord.com/channels/{guild}/{channel}). \
             Legacy fields `thread_channel_id`, `channel_web_url`, \
             `channel_deeplink_url` are preserved for backwards compatibility \
             with existing dashboard code paths.",
        )
        .with_params([("id", path_param("Agent id"))])
        .with_example(
            json!({"path": {"id": "project-agentdesk"}}),
            json!({
                "sessions": [
                    {
                        "id": 42,
                        "session_key": "mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011",
                        "agent_id": "project-agentdesk",
                        "provider": "codex",
                        "status": "working",
                        "active_dispatch_id": "dispatch-1",
                        "model": null,
                        "tokens": 0,
                        "cwd": null,
                        "last_heartbeat": "2026-04-27T12:34:56+00:00",
                        "thread_channel_id": "1485506232256168011",
                        "channel_id": "1485506232256168011",
                        "thread_id": "1485506232256168011",
                        "guild_id": "1490141479707086938",
                        "channel_web_url": "https://discord.com/channels/1490141479707086938/1485506232256168011",
                        "channel_deeplink_url": "discord://discord.com/channels/1490141479707086938/1485506232256168011",
                        "deeplink_url": "https://discord.com/channels/1490141479707086938/1485506232256168011",
                        "thread_deeplink_url": "discord://discord.com/channels/1490141479707086938/1485506232256168011",
                        "kanban_card_id": null
                    }
                ]
            }),
        ),
        ep(
            "GET",
            "/api/agents/{id}/turn",
            "agents",
            "Get active turn status and recent output // TODO: example",
        ),
        ep(
            "POST",
            "/api/agents/{id}/turn/start",
            "agents",
            "Start a headless agent turn from the agent primary mailbox/session. Returns conflict when another turn is already active for that agent mailbox.",
        )
        .with_params([
            ("id", path_param("Agent id")),
            (
                "prompt",
                body_param("string", true, "Instruction to execute in the headless turn"),
            ),
            (
                "metadata",
                body_param(
                    "object",
                    false,
                    "Optional trigger metadata injected into the turn context",
                ),
            ),
            (
                "source",
                body_param(
                    "string",
                    false,
                    "Optional trigger source label (for example system or pipeline)",
                ),
            ),
            (
                "dm_user_id",
                body_param(
                    "string",
                    false,
                    "Optional Discord user id. When set, the turn is bound to that user's DM channel with the agent's primary bot.",
                ),
            ),
        ])
        .with_example(
            json!({
                "path": {"id": "family-counsel"},
                "body": {
                    "prompt": "오부장 probe slot 도달. memento recall 후 gap 탐지하고 DM 전송",
                    "metadata": {
                        "trigger_source": "launchd:family-profile-probe",
                        "target_key": "obujang"
                    },
                    "source": "system"
                }
            }),
            json!({
                "ok": true,
                "turn_id": "discord:1473922824350601297:9100000000000000000",
                "status": "started"
            }),
        )
        .with_error_example(
            409,
            json!({"path": {"id": "family-counsel"}, "body": {"prompt": "do it"}}),
            json!({"error": "turn already active for this agent mailbox", "active_turn_id": "discord:1473922824350601297:9000000000000000000"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/family-counsel/turn/start -H 'Content-Type: application/json' -d '{\"prompt\":\"hello\",\"source\":\"system\",\"dm_user_id\":\"343742347365974026\"}'"),
        ep(
            "POST",
            "/api/agents/{id}/turn/stop",
            "agents",
            "Stop the active turn for agent",
        )
        .with_params([("id", path_param("Agent id"))])
        .with_example(
            json!({"path": {"id": "family-counsel"}}),
            json!({"ok": true, "turn_id": "discord:1473922824350601297:9100000000000000000", "status": "stopped"}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "family-counsel"}}),
            json!({"error": "no active turn for agent"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/agents/family-counsel/turn/stop"),
        ep(
            "GET",
            "/api/agents/{id}/transcripts",
            "agents",
            "List recent completed turn transcripts for agent // TODO: example",
        ),
        ep(
            "GET",
            "/api/agents/{id}/timeline",
            "agents",
            "Agent activity timeline // TODO: example",
        ),
        ep(
            "GET",
            "/api/agents/{id}/quality",
            "agents",
            "Per-agent quality summary (#1102): current + trend_7d + trend_30d from agent_quality_daily with event-based mini-rollup fallback",
        )
        .with_params([
            ("id", path_param("Agent id")),
            (
                "days",
                query_param("integer", false, "Lookback window in days for the daily trend")
                    .with_default(30),
            ),
            (
                "limit",
                query_param("integer", false, "Max daily rows to return").with_default(60),
            ),
        ])
        .with_example(
            json!({"path": {"id": "project-agentdesk"}, "query": {"days": 30}}),
            json!({"agent_id": "project-agentdesk", "current": {"turn_success_rate": 0.92, "sample_size": 50}, "trend_7d": {"turn_success_rate": 0.9}, "trend_30d": {"turn_success_rate": 0.88}}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "ghost"}}),
            json!({"error": "agent not found: ghost"}),
        )
        .with_curl("curl 'http://localhost:8787/api/agents/project-agentdesk/quality?days=30'"),
        ep(
            "GET",
            "/api/agents/quality/ranking",
            "agents",
            "Cross-agent quality ranking (#1102): sorts by the requested metric×window with sample_size >= min_sample_size",
        )
        .with_params([
            (
                "limit",
                query_param("integer", false, "Max agents to return").with_default(50),
            ),
            (
                "metric",
                ParamDoc {
                    location: "query",
                    kind: "string",
                    required: false,
                    description: "Ranking metric",
                    enum_values: None,
                    default: None,
                }
                .with_enum(&["turn_success_rate", "review_pass_rate"])
                .with_default("turn_success_rate"),
            ),
            (
                "window",
                ParamDoc {
                    location: "query",
                    kind: "string",
                    required: false,
                    description: "Rolling window",
                    enum_values: None,
                    default: None,
                }
                .with_enum(&["7d", "30d"])
                .with_default("7d"),
            ),
            (
                "min_sample_size",
                query_param("integer", false, "Exclude agents with window sample_size below this threshold")
                    .with_default(5),
            ),
        ])
        .with_example(
            json!({"query": {"metric": "turn_success_rate", "window": "7d", "limit": 10}}),
            json!({"ranking": [{"agent_id": "project-agentdesk", "turn_success_rate": 0.94, "sample_size": 20}]}),
        )
        .with_error_example(
            400,
            json!({"query": {"metric": "unknown", "window": "7d"}}),
            json!({"error": "metric must be one of turn_success_rate|review_pass_rate"}),
        )
        .with_curl("curl 'http://localhost:8787/api/agents/quality/ranking?metric=turn_success_rate&window=7d&limit=10'"),
        ep("GET", "/api/sessions", "sessions", "List sessions // TODO: example"),
        ep("GET", "/api/policies", "policies", "List policies // TODO: example"),
        ep(
            "GET",
            "/api/auth/session",
            "auth",
            "Get current auth session // TODO: example",
        ),
        ep("GET", "/api/kanban-cards", "kanban", "List kanban cards")
            .with_params([
                (
                    "status",
                    query_param("string", false, "Filter cards by pipeline status"),
                ),
                (
                    "repo_id",
                    query_param("string", false, "Filter cards by repository id"),
                ),
                (
                    "assigned_agent_id",
                    query_param("string", false, "Filter cards by assigned agent id"),
                ),
            ])
            .with_example(
                json!({"query": {"status": "ready"}}),
                json!({"cards": [{"id": "card-1", "title": "Fix docs", "status": "ready", "priority": "high"}]}),
            )
            .with_error_example(
                400,
                json!({"query": {"status": "invalid_status"}}),
                json!({"error": "unknown pipeline status: invalid_status"}),
            )
            .with_curl("curl 'http://localhost:8787/api/kanban-cards?status=ready'"),
        ep("POST", "/api/kanban-cards", "kanban", "Create kanban card")
            .with_params([
                ("title", body_param("string", true, "Card title")),
                (
                    "repo_id",
                    body_param("string", false, "Repository id or full name"),
                ),
                (
                    "priority",
                    body_param("string", false, "Priority label")
                        .with_default("medium"),
                ),
                (
                    "github_issue_url",
                    body_param("string", false, "Linked GitHub issue URL"),
                ),
            ])
            .with_example(
                json!({"body": {"title": "Test Card", "priority": "high"}}),
                json!({"card": {"id": "uuid-card-1", "title": "Test Card", "status": "backlog", "priority": "high"}}),
            )
            .with_error_example(
                400,
                json!({"body": {"priority": "high"}}),
                json!({"error": "title is required"}),
            )
            .with_curl("curl -X POST http://localhost:8787/api/kanban-cards -H 'Content-Type: application/json' -d '{\"title\":\"Test Card\",\"priority\":\"high\"}'"),
        ep(
            "GET",
            "/api/kanban-cards/stalled",
            "kanban",
            "List stalled cards // TODO: example",
        ),
        ep(
            "POST",
            "/api/kanban-cards/assign-issue",
            "kanban",
            "Create or update a card from a GitHub issue. Assignment is guaranteed once the request succeeds; transition to a dispatchable state is best-effort and must be checked in response.transition.",
        )
        .with_params([
            (
                "github_repo",
                body_param("string", true, "Repository full name"),
            ),
            (
                "github_issue_number",
                body_param("number", true, "GitHub issue number"),
            ),
            (
                "github_issue_url",
                body_param("string", false, "Linked GitHub issue URL"),
            ),
            ("title", body_param("string", true, "Card title")),
            (
                "description",
                body_param("string", false, "Card description override"),
            ),
            (
                "assignee_agent_id",
                body_param("string", true, "Agent that should own the card"),
            ),
        ])
        .with_example(
            json!({"body": {"github_repo": "itismyfield/AgentDesk", "github_issue_number": 426, "title": "Improve docs", "assignee_agent_id": "project-agentdesk"}}),
            json!({
                "card": {"id": "card-426", "status": "requested", "github_issue_number": 426, "assigned_agent_id": "project-agentdesk"},
                "deduplicated": false,
                "assignment": {"ok": true, "agent_id": "project-agentdesk"},
                "transition": {"attempted": true, "ok": true, "from": "backlog", "to": "requested", "target": "requested", "target_status": "requested", "error": null, "next_action": "none_required"}
            }),
        )
        .with_partial_success_example(
            json!({"body": {"github_repo": "itismyfield/AgentDesk", "github_issue_number": 427, "title": "Already done issue", "assignee_agent_id": "project-agentdesk"}}),
            json!({
                "card": {"id": "card-427", "status": "done", "github_issue_number": 427, "assigned_agent_id": "project-agentdesk"},
                "deduplicated": true,
                "assignment": {"ok": true, "agent_id": "project-agentdesk"},
                "transition": {
                    "attempted": true,
                    "ok": false,
                    "from": "done",
                    "to": "done",
                    "target": "requested",
                    "target_status": "requested",
                    "steps": ["requested"],
                    "completed_steps": [],
                    "failed_step": "requested",
                    "error": "transition done -> requested is not allowed",
                    "next_action": "inspect_transition_error"
                }
            }),
        )
        .with_error_example(
            404,
            json!({"body": {"github_repo": "unknown/repo", "github_issue_number": 1, "title": "x", "assignee_agent_id": "ghost"}}),
            json!({"error": "assignee agent not found: ghost"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/assign-issue -H 'Content-Type: application/json' -d '{\"github_repo\":\"itismyfield/AgentDesk\",\"github_issue_number\":426,\"title\":\"Improve docs\",\"assignee_agent_id\":\"project-agentdesk\"}'"),
        ep("GET", "/api/kanban-cards/{id}", "kanban", "Get card by ID")
            .with_params([("id", path_param("Kanban card ID"))])
            .with_example(
                json!({"path": {"id": "card-1"}}),
                json!({"card": {"id": "card-1", "title": "Fix docs", "status": "ready"}}),
            )
            .with_error_example(
                404,
                json!({"path": {"id": "ghost-card"}}),
                json!({"error": "card not found: ghost-card"}),
            )
            .with_curl("curl http://localhost:8787/api/kanban-cards/card-1"),
        ep(
            "PATCH",
            "/api/kanban-cards/{id}",
            "kanban",
            "Update card fields, or perform one manual status edit. Status edits cannot be combined with metadata or other field updates. Status edits are limited to backlog -> ready and any -> backlog; use /transition for administrative force transitions and /rereview for review reruns.",
        )
            .with_params([
                ("id", path_param("Kanban card ID")),
                ("title", body_param("string", false, "Updated title")),
                (
                    "status",
                    body_param(
                        "string",
                        false,
                        "Limited manual status edit: backlog -> ready or any -> backlog only; do not combine with other fields",
                    ),
                ),
                (
                    "priority",
                    body_param("string", false, "Priority label"),
                ),
                (
                    "assigned_agent_id",
                    body_param("string", false, "Assigned agent id"),
                ),
                (
                    "assignee_agent_id",
                    body_param("string", false, "Alias for assigned_agent_id"),
                ),
                (
                    "repo_id",
                    body_param("string", false, "Repository id or full name"),
                ),
                (
                    "github_issue_url",
                    body_param("string", false, "Linked GitHub issue URL"),
                ),
                (
                    "metadata",
                    body_param("object", false, "Metadata object"),
                ),
                (
                    "description",
                    body_param("string", false, "Card description"),
                ),
                (
                    "metadata_json",
                    body_param("string", false, "Raw metadata JSON string"),
                ),
                (
                    "review_status",
                    body_param("string", false, "Review status override"),
                ),
                (
                    "review_notes",
                    body_param("string", false, "Review notes"),
                ),
            ])
            .with_example(
                json!({"path": {"id": "card-1"}, "body": {"priority": "high"}}),
                json!({"card": {"id": "card-1", "status": "backlog", "priority": "high"}}),
            )
            .with_error_example(
                400,
                json!({"path": {"id": "card-1"}, "body": {"status": "done"}}),
                json!({"error": "PATCH /api/kanban-cards/{id} only allows manual status transitions backlog -> ready and any -> backlog (requested: review -> done). Use POST /api/kanban-cards/{id}/transition for administrative force transitions, or POST /api/kanban-cards/{id}/rereview for review reruns."}),
            )
            .with_error_example(
                400,
                json!({"path": {"id": "card-1"}, "body": {"status": "ready", "metadata_json": "{\"x\":true}"}}),
                json!({"error": "PATCH /api/kanban-cards/{id} cannot combine status changes with metadata or other field updates. Send metadata/field updates in one request, then send a status-only PATCH request, or use POST /api/kanban-cards/{id}/transition for administrative force transitions."}),
            )
            .with_curl("curl -X PATCH http://localhost:8787/api/kanban-cards/card-1 -H 'Content-Type: application/json' -d '{\"priority\":\"high\"}'"),
        ep(
            "DELETE",
            "/api/kanban-cards/{id}",
            "kanban",
            "Delete card // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "POST",
            "/api/kanban-cards/{id}/assign",
            "kanban",
            "Assign card to agent. Assignment is guaranteed once the request succeeds; transition to a dispatchable state is best-effort and failures are reported in response.transition.",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            ("agent_id", body_param("string", true, "Agent ID")),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"agent_id": "ch-td"}}),
            json!({
                "card": {"id": "card-1", "assigned_agent_id": "ch-td", "status": "requested"},
                "assignment": {"ok": true, "agent_id": "ch-td"},
                "transition": {"attempted": true, "ok": true, "from": "backlog", "to": "requested", "target": "requested", "target_status": "requested", "error": null, "next_action": "none_required"}
            }),
        )
        .with_partial_success_example(
            json!({"path": {"id": "card-2"}, "body": {"agent_id": "ch-td"}}),
            json!({
                "card": {"id": "card-2", "assigned_agent_id": "ch-td", "status": "done"},
                "assignment": {"ok": true, "agent_id": "ch-td"},
                "transition": {
                    "attempted": true,
                    "ok": false,
                    "from": "done",
                    "to": "done",
                    "target": "requested",
                    "target_status": "requested",
                    "steps": ["requested"],
                    "completed_steps": [],
                    "failed_step": "requested",
                    "error": "transition done -> requested is not allowed",
                    "next_action": "inspect_transition_error"
                }
            }),
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/rereview",
            "kanban",
            "Force a card back through review and create or reuse a fresh review dispatch. Use this instead of PATCH status=review for review reruns; requires explicit Bearer auth.",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "reason",
                body_param("string", false, "Why the rereview is needed"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"reason": "repeated finding"}}),
            json!({"card": {"id": "card-1", "status": "review"}, "rereviewed": true, "review_dispatch_id": "dispatch-review-1", "reason": "repeated finding"}),
        ),
        ep(
            "POST",
            "/api/kanban-cards/batch-rereview",
            "kanban",
            "Batch rereview by GitHub issue number",
        )
        .with_params([
            (
                "issues",
                body_param("number[]", true, "GitHub issue numbers to rereview"),
            ),
            (
                "reason",
                body_param("string", false, "Shared rereview reason"),
            ),
        ])
        .with_example(
            json!({"body": {"issues": [423, 426], "reason": "counter-model retry"}}),
            json!({"results": [{"issue": 423, "ok": true, "dispatch_id": "dispatch-review-423"}, {"issue": 426, "ok": false, "error": "card not found for issue #426"}]}),
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/reopen",
            "kanban",
            "Reopen a terminal card: move a closed/done card back to an active pipeline state (ready). Distinct from /retry (same failed step), /redispatch (new dispatch id), and /resume (continue checkpoint); reopen re-admits a terminal card into the board.",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "review_status",
                body_param("string", false, "Optional review status to set after reopen"),
            ),
            (
                "dispatch_type",
                body_param("string", false, "Reserved dispatch type override"),
            ),
            ("reason", body_param("string", false, "Audit reason")),
            (
                "reset_full",
                body_param(
                    "boolean",
                    false,
                    "Clear recovery/review state and cancel stale work",
                )
                .with_default(false),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"reason": "manual reopen", "reset_full": true}}),
            json!({"card": {"id": "card-1", "status": "ready"}, "reopened": true, "reset_full": true, "cancelled_dispatches": 1, "skipped_auto_queue_entries": 1, "from": "done", "to": "ready", "reason": "manual reopen"}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "does-not-exist"}, "body": {}}),
            json!({"error": "card not found: does-not-exist"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/reopen -H 'Content-Type: application/json' -d '{\"reason\":\"manual reopen\",\"reset_full\":true}'"),
        ep(
            "POST",
            "/api/kanban-cards/{id}/transition",
            "kanban",
            "Transition a single card with administrative force-transition semantics. Canonical runtime path is /transition; the old /force-transition path is removed. Requires explicit Bearer auth. Single-call complete: do NOT chain /redispatch, /retry, or /queue/generate after it (#1442). Inspect cancelled_dispatch_ids, created_dispatch_id, and next_action_hint in the response. See /api/docs/card-lifecycle-ops for the full decision tree (#1443).",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "status",
                body_param(
                    "string",
                    true,
                    "Target pipeline status for the administrative force transition",
                ),
            ),
            (
                "cancel_dispatches",
                body_param(
                    "boolean",
                    false,
                    "When cleanup applies, cancel active dispatches and skip affected auto-queue entries",
                )
                .with_default(true),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"status": "ready", "cancel_dispatches": true}}),
            json!({
                "card": {"id": "card-1", "status": "ready"},
                "forced": true,
                "from": "in_progress",
                "to": "ready",
                "cancelled_dispatches": 1,
                "cancelled_dispatch_ids": ["dispatch-abc"],
                "created_dispatch_id": null,
                "next_action_hint": "call /api/queue/generate to dispatch newly-ready card",
                "skipped_auto_queue_entries": 1
            }),
        )
        .with_error_example(
            400,
            json!({"path": {"id": "card-1"}, "body": {}}),
            json!({"error": "status is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/transition -H 'Content-Type: application/json' -d '{\"status\":\"ready\",\"cancel_dispatches\":true}'"),
        ep("POST", "/api/kanban-cards/{id}/retry", "kanban", "Retry card: re-execute the same failed step with the same intent and context (optionally swapping assignee). Distinct from /redispatch (creates a NEW dispatch id), /resume (continues a checkpointed turn), and /reopen (re-admits a closed card). Single-call complete: do NOT chain /transition or /queue/generate after it (#1442). See /api/docs/card-lifecycle-ops for the full decision tree (#1443).")
            .with_params([
                ("id", path_param("Kanban card ID")),
                (
                    "assignee_agent_id",
                    body_param("string", false, "Override assignee before retry"),
                ),
                (
                    "request_now",
                    body_param("boolean", false, "Legacy compatibility flag"),
                ),
            ])
            .with_example(
                json!({"path": {"id": "card-1"}, "body": {"assignee_agent_id": "agent-review"}}),
                json!({
                    "card": {"id": "card-1", "assigned_agent_id": "agent-review", "latest_dispatch_id": "dispatch-retry-1"},
                    "new_dispatch_id": "dispatch-retry-1",
                    "cancelled_dispatch_id": "dispatch-old-1",
                    "next_action": "none_required"
                }),
            )
            .with_error_example(
                409,
                json!({"path": {"id": "card-1"}, "body": {}}),
                json!({"error": "card has no failed dispatch to retry"}),
            )
            .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/retry -H 'Content-Type: application/json' -d '{\"assignee_agent_id\":\"agent-review\"}'"),
        ep(
            "POST",
            "/api/kanban-cards/{id}/redispatch",
            "kanban",
            "Redispatch: cancel the current live dispatch and create a brand-new dispatch entry with a new dispatch_id for the same card intent. Distinct from /retry (re-executes the SAME step with the same params), /resume (continues a checkpoint), and /reopen (re-admits a closed card). Single-call complete: do NOT chain /transition or /queue/generate after it (#1442). See /api/docs/card-lifecycle-ops for the full decision tree (#1443).",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "reason",
                body_param("string", false, "Optional redispatch rationale"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"reason": "stale thread"}}),
            json!({
                "card": {"id": "card-1", "latest_dispatch_id": "dispatch-redispatch-1", "status": "requested"},
                "new_dispatch_id": "dispatch-redispatch-1",
                "cancelled_dispatch_id": "dispatch-old-1",
                "next_action": "none_required"
            }),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "card-unknown"}, "body": {"reason": "stale thread"}}),
            json!({"error": "card not found: card-unknown"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/redispatch -H 'Content-Type: application/json' -d '{\"reason\":\"stale thread\"}'"),
        ep(
            "POST",
            "/api/kanban-cards/{id}/resume",
            "kanban",
            "Resume: continue a stuck/paused card from its current checkpointed state by inspecting review/dispatch state and issuing the minimal next action. Distinct from /retry (re-run same failed step), /redispatch (new dispatch id), and /reopen (re-admit closed card).",
        )
        .with_params([
            (
                "id",
                path_param("Card ID or GitHub issue number for the most recent matching card"),
            ),
            (
                "force",
                body_param("boolean", false, "Bypass guards for manual-intervention review/in-progress states")
                    .with_default(false),
            ),
            (
                "reason",
                body_param("string", false, "Audit reason for the resume"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-resume"}, "body": {"reason": "manual resume"}}),
            json!({"card": {"id": "card-resume", "status": "in_progress"}, "action": {"type": "new_implementation_dispatch", "dispatch_id": "dispatch-resume-1"}}),
        )
        .with_error_example(
            409,
            json!({"path": {"id": "card-in-review"}, "body": {}}),
            json!({"error": "resume blocked: card is in manual-intervention review; retry with force=true"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-resume/resume -H 'Content-Type: application/json' -d '{\"reason\":\"manual resume\"}'"),
        ep(
            "PATCH",
            "/api/kanban-cards/{id}/defer-dod",
            "kanban",
            "Update deferred DoD items",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            (
                "items",
                body_param("string[]", false, "Replace the full deferred DoD item list"),
            ),
            (
                "verify",
                body_param("string[]", false, "Mark DoD items as verified"),
            ),
            (
                "unverify",
                body_param("string[]", false, "Remove DoD items from verified set"),
            ),
            (
                "remove",
                body_param("string[]", false, "Remove items from items and verified"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"items": ["ship tests"], "verify": ["ship tests"]}}),
            json!({"card": {"id": "card-1", "deferred_dod": {"items": ["ship tests"], "verified": ["ship tests"]}}}),
        ),
        ep(
            "GET",
            "/api/kanban-cards/{id}/reviews",
            "kanban",
            "List reviews for card // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/review-state",
            "kanban",
            "Get canonical review-state record for card // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/audit-log",
            "kanban",
            "Get audit log for card // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/comments",
            "kanban",
            "Get GitHub comments for linked card issue // TODO: example",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-repos",
            "kanban-repos",
            "List kanban repos // TODO: example",
        ),
        ep(
            "POST",
            "/api/kanban-repos",
            "kanban-repos",
            "Create kanban repo // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/kanban-repos/{owner}/{repo}",
            "kanban-repos",
            "Update kanban repo // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/kanban-repos/{owner}/{repo}",
            "kanban-repos",
            "Delete kanban repo // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/kanban-reviews/{id}/decisions",
            "reviews",
            "Update review decisions // TODO: example",
        ),
        ep(
            "POST",
            "/api/kanban-reviews/{id}/trigger-rework",
            "reviews",
            "Trigger rework for review // TODO: example",
        ),
        ep(
            "POST",
            "/api/reviews/recovery",
            "reviews",
            "Recover a review dispatch target commit/worktree after stale cwd or incorrect metadata.",
        )
        .with_params([
            ("dispatch_id", body_param("string", false, "Review dispatch ID")),
            (
                "card_id",
                body_param("string", false, "Kanban card ID; resolves the latest active or failed review dispatch when dispatch_id is omitted"),
            ),
            (
                "target_commit",
                body_param("string", false, "Reviewed commit SHA to pin; must reference or belong to the card issue"),
            ),
            (
                "worktree_path",
                body_param("string", false, "Worktree path whose HEAD must match target_commit; used to infer target_commit when omitted"),
            ),
            ("reason", body_param("string", false, "Operator reason recorded in audit payload")),
        ])
        .with_example(
            json!({"body": {"dispatch_id": "review-1874-r1", "target_commit": "abc1234", "worktree_path": "/Users/me/.adk/release/workspaces/agentdesk-issue-1874", "reason": "stale cwd correction"}}),
            json!({"ok": true, "dispatch_id": "review-1874-r1", "card_id": "card-1874", "from_status": "failed", "to_status": "pending", "target": {"reviewed_commit": "abc1234", "worktree_path": "/Users/me/.adk/release/workspaces/agentdesk-issue-1874", "branch": "fix/1874-review-recovery-endpoint"}, "cleared_failure_markers": 2}),
        )
        .with_error_example(
            422,
            json!({"body": {"dispatch_id": "review-1874-r1", "target_commit": "def9999"}}),
            json!({"error": "target_commit def9999 does not reference or belong to card card-1874"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/reviews/recovery -H 'Content-Type: application/json' -d '{\"dispatch_id\":\"review-1874-r1\",\"target_commit\":\"abc1234\",\"worktree_path\":\"/Users/me/.adk/release/workspaces/agentdesk-issue-1874\",\"reason\":\"stale cwd correction\"}'"),
        ep("GET", "/api/dispatches", "dispatches", "List dispatches")
            .with_params([
                (
                    "status",
                    query_param("string", false, "Filter by dispatch status"),
                ),
                (
                    "kanban_card_id",
                    query_param("string", false, "Filter by kanban card id"),
                ),
            ])
            .with_example(
                json!({"query": {"status": "pending"}}),
                json!({"dispatches": [{"id": "dispatch-1", "kanban_card_id": "card-1", "to_agent_id": "agent-1", "status": "pending", "title": "Implement feature"}]}),
            )
            .with_error_example(
                400,
                json!({"query": {"status": "invalid"}}),
                json!({"error": "unknown dispatch status: invalid"}),
            )
            .with_curl("curl 'http://localhost:8787/api/dispatches?status=pending'"),
        ep(
            "POST",
            "/api/dispatches",
            "dispatches",
            "Create dispatch (supports optional skip_outbox for bookkeeping-only dispatches)",
        )
        .with_params([
            (
                "kanban_card_id",
                body_param("string", true, "Card to dispatch"),
            ),
            (
                "to_agent_id",
                body_param("string", true, "Target agent ID"),
            ),
            (
                "dispatch_type",
                body_param("string", false, "Dispatch type such as review or implementation"),
            ),
            ("title", body_param("string", true, "Dispatch title")),
            (
                "context",
                body_param("object", false, "Structured context payload"),
            ),
            (
                "skip_outbox",
                body_param(
                    "boolean",
                    false,
                    "Suppress notify outbox persistence for bookkeeping-only dispatches",
                )
                .with_default(false),
            ),
        ])
        .with_example(
            json!({"body": {"kanban_card_id": "card-1", "to_agent_id": "ch-td", "title": "Do it", "skip_outbox": true}}),
            json!({"dispatch": {"id": "dispatch-1", "kanban_card_id": "card-1", "to_agent_id": "ch-td", "status": "pending", "title": "Do it"}}),
        )
        .with_error_example(
            404,
            json!({"body": {"kanban_card_id": "ghost", "to_agent_id": "ghost", "title": "x"}}),
            json!({"error": "agent not found: ghost"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/dispatches -H 'Content-Type: application/json' -d '{\"kanban_card_id\":\"card-1\",\"to_agent_id\":\"ch-td\",\"title\":\"Do it\"}'"),
        ep(
            "GET",
            "/api/dispatches/{id}",
            "dispatches",
            "Get dispatch by ID",
        )
        .with_params([("id", path_param("Dispatch ID"))])
        .with_example(
            json!({"path": {"id": "dispatch-1"}}),
            json!({"dispatch": {"id": "dispatch-1", "status": "pending", "kanban_card_id": "card-1"}}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "dispatch-ghost"}}),
            json!({"error": "dispatch not found: dispatch-ghost"}),
        )
        .with_curl("curl http://localhost:8787/api/dispatches/dispatch-1"),
        ep(
            "GET",
            "/api/dispatches/{id}/events",
            "dispatches",
            "List read-only dispatch delivery events recorded in the typed delivery table for one dispatch. Returns events newest-first and never reads kv_meta.",
        )
        .with_params([("id", path_param("Dispatch ID"))])
        .with_example(
            json!({"path": {"id": "dispatch-1"}}),
            json!({
                "dispatch_id": "dispatch-1",
                "events": [{
                    "id": 1,
                    "dispatch_id": "dispatch-1",
                    "correlation_id": "dispatch:dispatch-1",
                    "semantic_event_id": "dispatch:dispatch-1:notify",
                    "operation": "send",
                    "target_kind": "channel",
                    "target_channel_id": "1500000000000000000",
                    "target_thread_id": null,
                    "status": "sent",
                    "attempt": 1,
                    "message_id": "1500000000000000001",
                    "messages_json": [{"channel_id": "1500000000000000000", "message_id": "1500000000000000001"}],
                    "fallback_kind": null,
                    "error": null,
                    "result_json": {"status": "success"},
                    "reserved_until": null,
                    "created_at": "2026-05-06T08:00:00Z",
                    "updated_at": "2026-05-06T08:00:01Z"
                }]
            }),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "dispatch-ghost"}}),
            json!({"error": "dispatch not found"}),
        )
        .with_curl("curl http://localhost:8787/api/dispatches/dispatch-1/events"),
        ep(
            "PATCH",
            "/api/dispatches/{id}",
            "dispatches",
            "Update dispatch lifecycle state or result. Allowed status values are pending, dispatched, completed, cancelled, and failed. status=completed uses the dispatch completion finalizer; review dispatches require a verdict and callers should use POST /api/reviews/verdict. Non-completed status changes and result-only updates refresh updated_at. Completed responses include result_summary and completed_at; legacy completed rows without completed_at mirror updated_at in the response.",
        )
        .with_params([
            ("id", path_param("Dispatch ID")),
            (
                "status",
                body_param("string", false, "New dispatch status").with_enum(&[
                    "pending",
                    "dispatched",
                    "completed",
                    "cancelled",
                    "failed",
                ]),
            ),
            (
                "result",
                body_param(
                    "object",
                    false,
                    "Structured dispatch result payload; response derives result_summary from result/context",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "dispatch-1"}, "body": {"status": "completed", "result": {"summary": "done"}}}),
            json!({"dispatch": {"id": "dispatch-1", "status": "completed", "result": {"summary": "done"}, "result_summary": "done", "updated_at": "2026-05-03 01:23:45+00", "completed_at": "2026-05-03 01:23:45+00"}}),
        )
        .with_error_example(
            400,
            json!({"path": {"id": "dispatch-1"}, "body": {"status": "done"}}),
            json!({"error": "invalid dispatch status 'done' — allowed values: pending, dispatched, completed, cancelled, failed"}),
        )
        .with_curl("curl -X PATCH http://localhost:8787/api/dispatches/dispatch-1 -H 'Content-Type: application/json' -d '{\"status\":\"completed\"}'"),
        ep(
            "POST",
            "/api/internal/link-dispatch-thread",
            "internal",
            "Link dispatch to an existing Discord thread // TODO: example",
        ),
        ep(
            "GET",
            "/api/internal/card-thread",
            "internal",
            "Resolve thread metadata for a card // TODO: example",
        ),
        ep(
            "GET",
            "/api/internal/pending-dispatch-for-thread",
            "internal",
            "Find pending dispatch bound to a thread // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/stages",
            "pipeline",
            "List pipeline stages // TODO: example",
        ),
        ep(
            "PUT",
            "/api/pipeline/stages",
            "pipeline",
            "Replace all pipeline stages // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/pipeline/stages",
            "pipeline",
            "Delete pipeline stages // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/cards/{card_id}",
            "pipeline",
            "Get card pipeline state",
        )
        .with_params([("card_id", path_param("Kanban card ID"))])
        .with_example(
            json!({"path": {"card_id": "card-1"}}),
            json!({"card_id": "card-1", "status": "ready", "stage": "implementation", "review_status": null}),
        )
        .with_error_example(
            404,
            json!({"path": {"card_id": "ghost-card"}}),
            json!({"error": "card not found: ghost-card"}),
        )
        .with_curl("curl http://localhost:8787/api/pipeline/cards/card-1"),
        ep(
            "GET",
            "/api/pipeline/cards/{card_id}/history",
            "pipeline",
            "Get card transition history // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/cards/{card_id}/transcripts",
            "pipeline",
            "List completed turn transcripts linked to card dispatches // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/config/default",
            "pipeline",
            "Get default pipeline config // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/config/effective",
            "pipeline",
            "Get effective merged pipeline config // TODO: example",
        )
        .with_params([
            (
                "repo",
                query_param("string", false, "Repository full name for config resolution"),
            ),
            (
                "agent_id",
                query_param("string", false, "Agent id for config resolution"),
            ),
        ]),
        ep(
            "GET",
            "/api/pipeline/config/repo/{owner}/{repo}",
            "pipeline",
            "Get repo pipeline override // TODO: example",
        ),
        ep(
            "PUT",
            "/api/pipeline/config/repo/{owner}/{repo}",
            "pipeline",
            "Set repo pipeline override // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/config/agent/{agent_id}",
            "pipeline",
            "Get agent pipeline override // TODO: example",
        ),
        ep(
            "PUT",
            "/api/pipeline/config/agent/{agent_id}",
            "pipeline",
            "Set agent pipeline override // TODO: example",
        ),
        ep(
            "GET",
            "/api/pipeline/config/graph",
            "pipeline",
            "Get pipeline graph // TODO: example",
        )
        .with_params([
            (
                "repo",
                query_param("string", false, "Repository full name for config resolution"),
            ),
            (
                "agent_id",
                query_param("string", false, "Agent id for config resolution"),
            ),
        ]),
        ep("GET", "/api/github/repos", "github", "List GitHub repos // TODO: example"),
        ep(
            "POST",
            "/api/github/issues/create",
            "github",
            "Create a GitHub issue with server-enforced PMD markdown format",
        )
        .with_params([
            (
                "repo",
                body_param(
                    "string",
                    true,
                    "Repository alias (`ADK`, `CH`) or `owner/repo`",
                ),
            ),
            ("title", body_param("string", true, "Issue title")),
            (
                "background",
                body_param("string", true, "Required `## 배경` body text"),
            ),
            (
                "content",
                body_param("array[string]", true, "Required bullet items for `## 내용`"),
            ),
            (
                "dod",
                body_param(
                    "array[string]",
                    true,
                    "Required DoD checklist items (1-10 entries, emitted as `- [ ]`)",
                ),
            ),
            (
                "agent_id",
                body_param(
                    "string",
                    false,
                    "Optional agent id converted into the `agent:<id>` GitHub label",
                ),
            ),
            (
                "dependencies",
                body_param(
                    "array[number|string|object]",
                    false,
                    "Optional dependency references rendered into `## 의존성`",
                ),
            ),
            (
                "risks",
                body_param(
                    "array[string]",
                    false,
                    "Optional risk bullets rendered into `## 리스크`",
                ),
            ),
            (
                "hints",
                body_param(
                    "array[string]",
                    false,
                    "Optional kickoff hints rendered into `## 착수 힌트` with a warning banner",
                ),
            ),
            (
                "auto_dispatch",
                body_param(
                    "boolean",
                    false,
                    "Reserved for future dispatch automation; currently returns 501 when true",
                ),
            ),
            (
                "block_on",
                body_param(
                    "array[number]",
                    false,
                    "Reserved for dependency blocking; currently returns 501 when non-empty",
                ),
            ),
        ])
        .with_example(
            json!({
                "repo": "ADK",
                "title": "create-issue 스킬을 ADK API로 승격",
                "background": "AgentDesk 내부에서 PMD 포맷 이슈를 서버 API로 직접 생성해야 한다.",
                "content": [
                    "POST /api/github/issues/create 엔드포인트를 추가한다.",
                    "서버에서 PMD 마크다운 포맷을 강제한다."
                ],
                "dod": [
                    "성공 시 GitHub issue URL을 반환한다",
                    "DoD는 서버에서 - [ ] 체크리스트로 변환된다"
                ],
                "agent_id": "adk-backend"
            }),
            json!({
                "issue": {
                    "number": 819,
                    "url": "https://github.com/itismyfield/AgentDesk/issues/819",
                    "repo": "itismyfield/AgentDesk"
                },
                "applied_labels": ["agent:adk-backend"],
                "pmd_format_version": 1
            }),
        )
        .with_error_example(
            400,
            json!({"body": {"repo": "ADK", "title": "no DoD"}}),
            json!({"error": "dod is required and must contain 1-10 entries"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/github/issues/create -H 'Content-Type: application/json' -d '{\"repo\":\"ADK\",\"title\":\"Example\",\"background\":\"bg\",\"content\":[\"do thing\"],\"dod\":[\"it works\"]}'"),
        ep("POST", "/api/github/repos", "github", "Register GitHub repo // TODO: example"),
        ep(
            "POST",
            "/api/github/repos/{owner}/{repo}/sync",
            "github",
            "Sync GitHub repo // TODO: example",
        ),
        ep(
            "GET",
            "/api/github-repos",
            "github-dashboard",
            "List GitHub repos for dashboard // TODO: example",
        ),
        ep(
            "GET",
            "/api/github-issues",
            "github-dashboard",
            "List GitHub issues for dashboard // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/github-issues/{owner}/{repo}/{number}/close",
            "github-dashboard",
            "Close GitHub issue from dashboard // TODO: example",
        ),
        ep(
            "GET",
            "/api/github-closed-today",
            "github-dashboard",
            "List issues closed today // TODO: example",
        ),
        ep("GET", "/api/offices", "offices", "List offices // TODO: example"),
        ep("POST", "/api/offices", "offices", "Create office // TODO: example"),
        ep(
            "PATCH",
            "/api/offices/reorder",
            "offices",
            "Reorder offices // TODO: example",
        ),
        ep("PATCH", "/api/offices/{id}", "offices", "Update office // TODO: example"),
        ep("DELETE", "/api/offices/{id}", "offices", "Delete office // TODO: example"),
        ep(
            "POST",
            "/api/offices/{id}/agents",
            "offices",
            "Add agent to office // TODO: example",
        ),
        ep(
            "POST",
            "/api/offices/{id}/agents/batch",
            "offices",
            "Batch add agents to office // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/offices/{id}/agents/{agentId}",
            "offices",
            "Remove agent from office // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/offices/{id}/agents/{agentId}",
            "offices",
            "Update office agent // TODO: example",
        ),
        ep(
            "GET",
            "/api/departments",
            "departments",
            "List departments // TODO: example",
        ),
        ep(
            "POST",
            "/api/departments",
            "departments",
            "Create department // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/departments/reorder",
            "departments",
            "Reorder departments // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/departments/{id}",
            "departments",
            "Update department // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/departments/{id}",
            "departments",
            "Delete department // TODO: example",
        ),
        ep("GET", "/api/stats", "stats", "Get system stats // TODO: example"),
        ep(
            "GET",
            "/api/stats/memento",
            "stats",
            "Get hourly Memento logical call counts and dedup hit rates // TODO: example",
        )
        .with_params([(
            "hours",
            query_param("integer", false, "Trailing window size in hours (1-168)")
                .with_default(24),
        )]),
        ep(
            "GET",
            "/api/settings",
            "settings",
            "Get the canonical company settings JSON stored in `kv_meta['settings']`",
        )
        .with_example(
            json!({}),
            json!({
                "companyName": "AgentDesk",
                "language": "ko",
                "theme": "midnight"
            }),
        ),
        ep(
            "PUT",
            "/api/settings",
            "settings",
            "Full-replace company settings JSON. Callers must send a merged payload if hidden keys should survive.",
        )
        .with_example(
            json!({
                "companyName": "AgentDesk",
                "language": "ko",
                "theme": "midnight"
            }),
            json!({"ok": true}),
        ),
        ep(
            "GET",
            "/api/settings/config",
            "settings",
            "Get editable policy/config keys with effective value, baseline, and restart-behavior metadata",
        )
        .with_example(
            json!({}),
            json!({
                "entries": [
                    {
                        "key": "merge_strategy",
                        "value": "merge",
                        "default": "rebase",
                        "baseline": "rebase",
                        "baseline_source": "yaml",
                        "override_active": true,
                        "editable": true,
                        "restart_behavior": "reseed-from-yaml",
                        "category": "automation",
                        "label_ko": "자동 머지 전략",
                        "label_en": "Merge Strategy"
                    },
                    {
                        "key": "merge_strategy_mode",
                        "value": "pr-always",
                        "default": "direct-first",
                        "baseline": "direct-first",
                        "baseline_source": "hardcoded",
                        "override_active": true,
                        "editable": true,
                        "restart_behavior": "persist-live-override",
                        "category": "automation",
                        "label_ko": "자동 머지 경로",
                        "label_en": "Merge Strategy Mode"
                    },
                    {
                        "key": "server_port",
                        "value": "8791",
                        "default": "8791",
                        "baseline": "8791",
                        "baseline_source": "config",
                        "override_active": false,
                        "editable": false,
                        "restart_behavior": "config-only",
                        "category": "system",
                        "label_ko": "서버 포트",
                        "label_en": "Server Port"
                    }
                ]
            }),
        ),
        ep(
            "PATCH",
            "/api/settings/config",
            "settings",
            "Patch live overrides for editable whitelisted config keys. YAML-backed keys are re-seeded on restart.",
        )
        .with_example(
            json!({
                "merge_strategy": "merge",
                "merge_strategy_mode": "pr-always",
                "max_review_rounds": 5
            }),
            json!({"ok": true, "updated": 2, "rejected": []}),
        ),
        ep(
            "GET",
            "/api/settings/runtime-config",
            "settings",
            "Get runtime tuning as `current` merged over YAML-or-hardcoded `defaults`",
        )
        .with_example(
            json!({}),
            json!({
                "current": {
                    "dispatchPollSec": 15,
                    "maxRetries": 7,
                    "maxEntryRetries": 4
                },
                "defaults": {
                    "dispatchPollSec": 30,
                    "maxRetries": 3,
                    "maxEntryRetries": 3
                }
            }),
        ),
        ep(
            "PUT",
            "/api/settings/runtime-config",
            "settings",
            "Replace the stored runtime-config override object",
        )
        .with_example(
            json!({
                "dispatchPollSec": 15,
                "maxRetries": 7,
                "maxEntryRetries": 4
            }),
            json!({"ok": true}),
        ),
        ep(
            "GET",
            "/api/settings/escalation",
            "settings",
            "Get escalation routing defaults plus the current override-applied value",
        )
        .with_example(
            json!({}),
            json!({
                "current": {
                    "mode": "scheduled",
                    "owner_user_id": 343742347365974026u64,
                    "pm_channel_id": "kanban-manager",
                    "schedule": {
                        "pm_hours": "09:00-18:00",
                        "timezone": "Asia/Seoul"
                    }
                },
                "defaults": {
                    "mode": "pm",
                    "owner_user_id": 343742347365974026u64,
                    "pm_channel_id": "kanban-manager",
                    "schedule": {
                        "pm_hours": "00:00-08:00",
                        "timezone": "Asia/Seoul"
                    }
                }
            }),
        ),
        ep(
            "PUT",
            "/api/settings/escalation",
            "settings",
            "Replace the escalation override. Sending the default body clears the stored override.",
        )
        .with_example(
            json!({
                "mode": "scheduled",
                "owner_user_id": 343742347365974026u64,
                "pm_channel_id": "kanban-manager",
                "schedule": {
                    "pm_hours": "09:00-18:00",
                    "timezone": "Asia/Seoul"
                }
            }),
            json!({
                "ok": true,
                "current": {
                    "mode": "scheduled",
                    "owner_user_id": 343742347365974026u64,
                    "pm_channel_id": "kanban-manager",
                    "schedule": {
                        "pm_hours": "09:00-18:00",
                        "timezone": "Asia/Seoul"
                    }
                },
                "defaults": {
                    "mode": "pm",
                    "owner_user_id": 343742347365974026u64,
                    "pm_channel_id": "kanban-manager",
                    "schedule": {
                        "pm_hours": "00:00-08:00",
                        "timezone": "Asia/Seoul"
                    }
                }
            }),
        ),
        ep(
            "GET",
            "/api/dispatched-sessions",
            "dispatched-sessions",
            "List dispatched sessions with recovery identifiers for active dispatch, tmux session, thread channel, and linked auto-queue entry/run/slot when available.",
        )
        .with_example(
            json!({"query": {"all": false}}),
            json!({"sessions": [{
                "session_key": "mac-mini:AgentDesk-codex-adk-cdx",
                "status": "turn_active",
                "active_dispatch_id": "dispatch-1",
                "tmux_session": "AgentDesk-codex-adk-cdx",
                "resolved_thread_channel_id": "1501205715878936748",
                "auto_queue_entry_id": "entry-1",
                "auto_queue_run_id": "run-1",
                "auto_queue_slot_index": 0,
                "recovery_identifiers": {
                    "session_key": "mac-mini:AgentDesk-codex-adk-cdx",
                    "tmux_session": "AgentDesk-codex-adk-cdx",
                    "active_dispatch_id": "dispatch-1",
                    "thread_channel_id": "1501205715878936748",
                    "auto_queue_entry_id": "entry-1",
                    "auto_queue_run_id": "run-1",
                    "auto_queue_slot_index": 0,
                    "auto_queue_thread_group": 0
                }
            }]}),
        ),
        ep(
            "DELETE",
            "/api/dispatched-sessions/cleanup",
            "dispatched-sessions",
            "Delete stale dispatched sessions // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/dispatched-sessions/gc-threads",
            "dispatched-sessions",
            "Garbage-collect orphaned thread sessions // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/dispatched-sessions/{id}",
            "dispatched-sessions",
            "Update dispatched session // TODO: example",
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/webhook",
            "dispatched-sessions",
            "Session webhook // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/dispatched-sessions/webhook",
            "dispatched-sessions",
            "Delete session webhook state // TODO: example",
        ),
        ep(
            "GET",
            "/api/dispatched-sessions/claude-session-id",
            "dispatched-sessions",
            "Resolve Claude session id by session key // TODO: example",
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/clear-stale-session-id",
            "dispatched-sessions",
            "Clear stale Claude session id // TODO: example",
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/clear-session-id",
            "dispatched-sessions",
            "Clear Claude session id by session key // TODO: example",
        ),
        ep(
            "POST",
            "/api/sessions/{session_key}/force-kill",
            "sessions",
            "Force-kill session and optionally retry // TODO: example",
        ),
        ep(
            "GET",
            "/api/sessions/{id}/tmux-output",
            "sessions",
            "Capture recent tmux pane output for a session (watch-agent-turn skill promotion)",
        )
        .with_params([
            ("id", path_param("Session id (sessions.id)")),
            (
                "lines",
                query_param(
                    "integer",
                    false,
                    "Trailing tmux pane lines to capture (1..=2000)",
                )
                .with_default(80),
            ),
        ])
        .with_example(
            json!({"query": {"lines": 40}}),
            json!({
                "session_id": 42,
                "session_key": "mac-mini:remoteCC-claude-foo",
                "tmux_name": "remoteCC-claude-foo",
                "tmux_alive": true,
                "agent_id": "ch-dd",
                "provider": "claude",
                "status": "working",
                "lines_requested": 40,
                "lines_effective": 40,
                "recent_output": "...tail of tmux pane...",
                "captured_at_ms": 1_745_000_000_000_i64
            }),
        ),
        ep(
            "GET",
            "/api/session-termination-events",
            "sessions",
            "List recorded session termination events // TODO: example",
        ),
        ep("GET", "/api/messages", "messages", "List messages // TODO: example"),
        ep("POST", "/api/messages", "messages", "Create message // TODO: example"),
        ep(
            "GET",
            "/api/discord/bindings",
            "discord",
            "List Discord bindings // TODO: example",
        ),
        ep(
            "GET",
            "/api/discord/channels/{id}/messages",
            "discord",
            "Read channel or thread messages // TODO: example",
        ),
        ep(
            "GET",
            "/api/discord/channels/{id}",
            "discord",
            "Get channel or thread info // TODO: example",
        ),
        ep(
            "POST",
            "/api/dm-reply/register",
            "discord",
            "Register DM reply handler // TODO: example",
        ),
        ep(
            "GET",
            "/api/round-table-meetings",
            "meetings",
            "List meetings // TODO: example",
        ),
        ep(
            "POST",
            "/api/round-table-meetings",
            "meetings",
            "Create or update meeting // TODO: example",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/start",
            "meetings",
            "Start meeting // TODO: example",
        ),
        ep(
            "GET",
            "/api/round-table-meetings/{id}",
            "meetings",
            "Get meeting by ID // TODO: example",
        ),
        ep(
            "DELETE",
            "/api/round-table-meetings/{id}",
            "meetings",
            "Delete meeting // TODO: example",
        ),
        ep(
            "PATCH",
            "/api/round-table-meetings/{id}/issue-repo",
            "meetings",
            "Update meeting issue repository // TODO: example",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues",
            "meetings",
            "Create meeting issues // TODO: example",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues/discard",
            "meetings",
            "Discard one meeting issue // TODO: example",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues/discard-all",
            "meetings",
            "Discard all meeting issues // TODO: example",
        ),
        ep("GET", "/api/skills/catalog", "skills", "List skill catalog // TODO: example").with_params([(
            "include_stale",
            query_param(
                "boolean",
                false,
                "Include stale skill entries that no longer exist on disk",
            ),
        )]),
        ep(
            "GET",
            "/api/skills/ranking",
            "skills",
            "Skill usage ranking // TODO: example",
        )
        .with_params([
            (
                "window",
                query_param("string", false, "Ranking window: 7d, 30d, 90d, all"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum number of ranking entries"),
            ),
            (
                "include_stale",
                query_param(
                    "boolean",
                    false,
                    "Include stale skill entries that no longer exist on disk",
                ),
            ),
        ]),
        ep("POST", "/api/skills/prune", "skills", "Preview or prune stale skill metadata // TODO: example")
            .with_params([(
                "dry_run",
                query_param(
                    "boolean",
                    false,
                    "When true, report stale skill ids without deleting skills rows",
                ),
            )]),
        ep("GET", "/api/cron-jobs", "cron", "List cron jobs // TODO: example"),
        ep(
            "GET",
            "/api/routines",
            "routines",
            "List durable routines with optional agent/status filters.",
        )
        .with_params([
            (
                "agent_id",
                query_param("string", false, "Filter routines attached to one agent"),
            ),
            (
                "status",
                query_param("string", false, "Filter by enabled, paused, or detached"),
            ),
        ])
        .with_example(
            json!({"query": {"status": "enabled"}}),
            json!({"routines": [{"id": "routine-1", "script_ref": "daily-summary.js", "status": "enabled"}]}),
        ),
        ep(
            "GET",
            "/api/routines/metrics",
            "routines",
            "Aggregate routine status counts, run outcome/error counts, and average finished-run latency with optional agent and time-window filters.",
        )
        .with_params([
            (
                "agent_id",
                query_param("string", false, "Filter metrics to one attached agent"),
            ),
            (
                "since",
                query_param("string", false, "Optional RFC3339 lower bound for routine_runs.created_at"),
            ),
        ])
        .with_example(
            json!({"query": {"agent_id": "codex", "since": "2026-04-29T00:00:00Z"}}),
            json!({"metrics": {"routines_total": 3, "routines_enabled": 2, "routines_paused": 1, "routines_detached": 0, "runs_total": 12, "runs_running": 1, "runs_succeeded": 9, "runs_failed": 1, "runs_skipped": 0, "runs_paused": 0, "runs_interrupted": 1, "runs_error": 2, "avg_latency_ms": 1532.4}, "filters": {"agent_id": "codex", "since": "2026-04-29T00:00:00Z"}}),
        ),
        ep(
            "GET",
            "/api/routines/runs/search",
            "routines",
            "Search recent routine runs by `routine_runs.result_json` text with optional agent, status, time-window, and limit filters.",
        )
        .with_params([
            ("q", query_param("string", true, "Search text matched against routine_runs.result_json")),
            (
                "agent_id",
                query_param("string", false, "Filter matches to one attached agent"),
            ),
            (
                "status",
                query_param("string", false, "Filter by running, succeeded, failed, skipped, paused, or interrupted"),
            ),
            (
                "since",
                query_param("string", false, "Optional RFC3339 lower bound for routine_runs.created_at"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum rows to return, clamped to 1..100"),
            ),
        ])
        .with_example(
            json!({"query": {"q": "checkpoint", "agent_id": "codex", "status": "succeeded", "limit": 20}}),
            json!({"runs": [{"id": "run-1", "routine_id": "routine-1", "script_ref": "agent-checkpoint-review.js", "status": "succeeded", "result_json": {"summary": "checkpoint ok"}}], "filters": {"q": "checkpoint", "agent_id": "codex", "status": "succeeded", "since": null, "limit": 20}}),
        ),
        ep(
            "POST",
            "/api/routines",
            "routines",
            "Attach a file-backed routine row without starting an agent action.",
        )
        .with_params([
            (
                "script_ref",
                body_param(
                    "string",
                    true,
                    "Routine script path relative to routines.dir or routines.additional_dirs",
                ),
            ),
            ("name", body_param("string", false, "Human-readable routine name")),
            ("agent_id", body_param("string", false, "Optional attached agent id")),
            ("execution_strategy", body_param("string", false, "fresh or persistent")),
            (
                "schedule",
                body_param("string", false, "Optional @every duration or 5-field cron such as 30 9 * * 1-5"),
            ),
            ("next_due_at", body_param("string", false, "Optional RFC3339 due time")),
            ("discord_thread_id", body_param("string", false, "Optional existing Discord thread id")),
            ("timeout_secs", body_param("integer", false, "Optional per-routine agent timeout in seconds")),
        ])
        .with_example(
            json!({"body": {"script_ref": "daily-summary.js", "name": "Daily Summary", "execution_strategy": "fresh"}}),
            json!({"routine": {"id": "routine-1", "script_ref": "daily-summary.js", "status": "enabled"}, "discord_log": {"status": "skipped"}}),
        ),
        ep(
            "GET",
            "/api/routines/{id}",
            "routines",
            "Get one durable routine row.",
        )
        .with_params([("id", path_param("Routine id"))])
        .with_example(
            json!({"path": {"id": "routine-1"}}),
            json!({"routine": {"id": "routine-1", "script_ref": "daily-summary.js"}}),
        ),
        ep(
            "PATCH",
            "/api/routines/{id}",
            "routines",
            "Patch routine metadata, scheduling fields, or checkpoint.",
        )
        .with_params([
            ("id", path_param("Routine id")),
            ("name", body_param("string", false, "New routine name")),
            (
                "execution_strategy",
                body_param("string", false, "fresh or persistent"),
            ),
            (
                "schedule",
                body_param("string|null", false, "Set @every duration or 5-field cron, or pass null to clear it"),
            ),
            (
                "next_due_at",
                body_param("string|null", false, "RFC3339 due time or null to clear it"),
            ),
            (
                "checkpoint",
                body_param("object|null", false, "Replacement checkpoint JSON or null to clear it"),
            ),
            (
                "discord_thread_id",
                body_param("string|null", false, "Saved Discord thread id or null to clear it"),
            ),
            (
                "timeout_secs",
                body_param("integer|null", false, "Per-routine agent timeout in seconds or null for config default"),
            ),
        ]),
        ep(
            "GET",
            "/api/routines/{id}/runs",
            "routines",
            "List recent run history for one routine, including best-effort Discord log status and warning detail.",
        )
        .with_params([
            ("id", path_param("Routine id")),
            ("limit", query_param("integer", false, "Maximum runs to return, capped at 100")),
        ]),
        ep(
            "POST",
            "/api/routines/{id}/pause",
            "routines",
            "Pause an enabled routine, clear its next due time, and enqueue a best-effort Discord log when an attached agent has a channel.",
        )
        .with_params([("id", path_param("Routine id"))]),
        ep(
            "POST",
            "/api/routines/{id}/resume",
            "routines",
            "Resume a paused routine with an optional next due time and best-effort Discord log.",
        )
        .with_params([
            ("id", path_param("Routine id")),
            ("next_due_at", body_param("string", false, "Optional RFC3339 due time")),
        ]),
        ep(
            "POST",
            "/api/routines/{id}/detach",
            "routines",
            "Detach a non-running routine without deleting its run history; Discord log failure is returned only as discord_log.warning_code/warning.",
        )
        .with_params([("id", path_param("Routine id"))]),
        ep(
            "POST",
            "/api/routines/{id}/run-now",
            "routines",
            "Claim and execute one routine. Script actions close immediately; agent actions store turn_id and remain running until session_transcripts completion evidence is found.",
        )
        .with_params([("id", path_param("Routine id"))])
        .with_example(
            json!({"path": {"id": "routine-1"}}),
            json!({"outcome": {"run_id": "run-1", "routine_id": "routine-1", "action": "agent", "status": "running", "result_json": {"turn_id": "discord:1473922824350601297:9100000000000000000", "fresh_context_guaranteed": false}}, "discord_log": {"status": "ok"}}),
        ),
        ep(
            "POST",
            "/api/routines/{id}/session/reset",
            "routines",
            "Reset the provider session for a persistent agent-backed routine. Claude sends /clear; managed tmux providers reset the process session; providers without managed tmux clear runtime mailbox state only.",
        )
        .with_params([("id", path_param("Routine id"))])
        .with_example(
            json!({"path": {"id": "routine-1"}}),
            json!({"ok": true, "session": {"action": "reset", "provider": "codex", "provider_clear_behavior": "runtime clear plus managed process session reset for the provider tmux session", "runtime_cleared": true}, "interrupted_run_id": null}),
        ),
        ep(
            "POST",
            "/api/routines/{id}/session/kill",
            "routines",
            "Force-kill the provider session for a persistent agent-backed routine, disconnect matching session rows, and interrupt the routine's in-flight run when the session actually changes.",
        )
        .with_params([("id", path_param("Routine id"))])
        .with_example(
            json!({"path": {"id": "routine-1"}}),
            json!({"ok": true, "session": {"action": "kill", "provider": "codex", "tmux_killed": true, "lifecycle_path": "mailbox_canonical"}, "interrupted_run_id": "run-1"}),
        ),
        ep(
            "POST",
            "/api/queue/generate",
            "auto-queue",
            "Generate auto-queue entries. Single-call complete: do NOT chain /redispatch, /retry, or /transition for the same card after it (#1442). Inspect skipped_due_to_active_dispatch / skipped_due_to_dependency / skipped_due_to_filter in the response to see structured skip reasons. See /api/docs/card-lifecycle-ops for the full decision tree (#1443).",
        )
        .with_params([
            (
                "repo",
                body_param("string", false, "Filter cards by repository"),
            ),
            (
                "agent_id",
                body_param("string", false, "Filter cards by assigned agent"),
            ),
            (
                "auto_assign_agent",
                body_param(
                    "boolean",
                    false,
                    "Assign unowned explicit issue_numbers or entries to agent_id before queue generation",
                )
                .with_default(false),
            ),
            (
                "issue_numbers",
                body_param(
                    "number[]",
                    false,
                    "Explicit GitHub issue numbers to include in the run",
                ),
            ),
            (
                "entries",
                body_param(
                    "object[]",
                    false,
                    "Explicit entries with issue_number, batch_phase, and optional thread_group",
                ),
            ),
            (
                "unified_thread",
                body_param(
                    "boolean",
                    false,
                    "Accepted for compatibility but ignored; generate keeps slot pooling",
                )
                .with_default(false),
            ),
            (
                "max_concurrent_threads",
                body_param("number", false, "Upper bound for simultaneously active groups")
                    .with_default(1),
            ),
            (
                "review_mode",
                body_param(
                    "string",
                    false,
                    "Run review mode: 'enabled' keeps the normal review gate; 'disabled' skips review dispatch creation and waits for main-merge detection before moving to done",
                )
                .with_default("enabled"),
            ),
            (
                "max_concurrent_per_agent",
                body_param(
                    "number",
                    false,
                    "Legacy compatibility field; accepted but ignored",
                ),
            ),
        ])
        .with_example(
            json!({"body": {"repo": "test-repo", "issue_numbers": [423, 405, 407], "review_mode": "disabled", "unified_thread": true, "max_concurrent_threads": 2}}),
            json!({
                "run": {"id": "run-1", "status": "generated", "review_mode": "disabled", "thread_group_count": 2, "max_concurrent_threads": 2, "unified_thread": false},
                "entries": [{"id": "entry-1", "github_issue_number": 423, "thread_group": 0, "priority_rank": 0, "status": "pending"}],
                "skipped_due_to_active_dispatch": [{"issue_number": 405, "existing_dispatch_id": "dispatch-already-running"}],
                "skipped_due_to_dependency": [{"issue_number": 407, "unresolved_deps": ["#410:in_progress"]}],
                "skipped_due_to_filter": []
            }),
        )
        .with_error_example(
            400,
            json!({"body": {"repo": "test-repo", "issue_numbers": []}}),
            json!({"error": "issue_numbers or entries must be non-empty"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/generate -H 'Content-Type: application/json' -d '{\"repo\":\"test-repo\",\"issue_numbers\":[423,405]}'"),
        ep(
            "POST",
            "/api/queue/dispatch-next",
            "auto-queue",
            "Dispatch the next pending auto-queue entries. See /api/docs/card-lifecycle-ops for the full decision tree on when to call /generate vs /dispatch-next (#1443).",
        )
        .with_params([
            (
                "run_id",
                body_param("string", false, "Specific auto-queue run to activate"),
            ),
            (
                "repo",
                body_param("string", false, "Restrict activation to matching repo"),
            ),
            (
                "agent_id",
                body_param("string", false, "Restrict activation to matching agent"),
            ),
            (
                "thread_group",
                body_param("number", false, "Limit activation to a single thread group"),
            ),
            (
                "unified_thread",
                body_param(
                    "boolean",
                    false,
                    "Accepted for compatibility but ignored; slot pooling stays enabled",
                )
                .with_default(false),
            ),
            (
                "active_only",
                body_param(
                    "boolean",
                    false,
                    "Internal recovery mode; do not promote generated/pending runs",
                )
                .with_default(false),
            ),
        ])
        .with_example(
            json!({"body": {"repo": "test-repo", "unified_thread": false}}),
            json!({"dispatched": [{"id": "entry-1", "card_id": "card-423", "dispatch_id": "dispatch-1", "status": "dispatched"}], "count": 1, "active_groups": 1, "pending_groups": 1}),
        )
        .with_error_example(
            404,
            json!({"body": {"run_id": "run-ghost"}}),
            json!({"error": "auto-queue run not found: run-ghost"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/dispatch-next -H 'Content-Type: application/json' -d '{\"repo\":\"test-repo\"}'"),
        ep(
            "GET",
            "/api/queue/status",
            "auto-queue",
            "Get latest auto-queue run state. diagnostics.entry_dispatch_delivery_mismatches surfaces split-brain delivery where an entry is dispatched but the linked dispatch/session is not live; it includes run_id, entry_id, dispatch_id, card_id, github_issue_number, thread_group, slot_index, dispatch_status, entry_status, age_ms, and recovery endpoints. diagnostics.run_timeout_overruns reports active runs beyond timeout_minutes. When auto_queue_slot_single_active_entry is violated, diagnostics.slot_invariant_violations identifies run_id, agent_id, slot_index, conflicting entry_ids, related dispatch_ids, and recovery endpoints. Recommended recovery: reset stale entries to pending, release slot bindings with /api/queue/slots/{agent_id}/{slot_index}/reset-thread or /api/queue/slots/{agent_id}/{slot_index}/rebind, then dispatch again.",
        )
        .with_params([
            (
                "repo",
                query_param(
                    "string",
                    false,
                    "Restrict status view to a repository id; use agent_id for agent filters",
                ),
            ),
            (
                "agent_id",
                query_param("string", false, "Restrict status view to an agent"),
            ),
        ])
        .with_example(
            json!({"query": {"repo": "test-repo"}}),
            json!({"run": {"id": "run-1", "status": "active", "review_mode": "enabled"}, "entries": [{"id": "entry-1", "status": "pending", "github_issue_number": 423}], "agents": {"agent-1": {"pending": 1, "dispatched": 0, "done": 0, "skipped": 0}}, "thread_groups": {"0": {"status": "pending", "pending": 1, "dispatched": 0, "done": 0, "skipped": 0, "entries": [{"id": "entry-1", "card_id": "card-423", "status": "pending", "github_issue_number": 423, "batch_phase": 0}]}}}),
        )
        .with_error_example(
            404,
            json!({"query": {"repo": "no-such-repo"}}),
            json!({"error": "no auto-queue run for repo: no-such-repo"}),
        )
        .with_curl("curl 'http://localhost:8787/api/queue/status?repo=test-repo'"),
        ep(
            "GET",
            "/api/queue/history",
            "auto-queue",
            "List recent auto-queue runs with outcome metrics",
        )
        .with_params([
            (
                "repo",
                query_param("string", false, "Restrict history view to a repo"),
            ),
            (
                "agent_id",
                query_param("string", false, "Restrict history view to an agent"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum number of recent runs to return")
                    .with_default(8),
            ),
        ])
        .with_example(
            json!({"query": {"repo": "test-repo", "limit": 5}}),
            json!({
                "summary": {
                    "total_runs": 2,
                    "completed_runs": 1,
                    "success_rate": 0.375,
                    "failure_rate": 0.625
                },
                "runs": [{
                    "id": "run-2",
                    "repo": "test-repo",
                    "agent_id": "agent-1",
                    "status": "completed",
                    "timeout_minutes": 120,
                    "timeout_exceeded": false,
                    "timeout_overrun_ms": 0,
                    "created_at": 1712600000000_i64,
                    "completed_at": 1712600300000_i64,
                    "duration_ms": 300000_i64,
                    "entry_count": 4,
                    "done_count": 3,
                    "skipped_count": 1,
                    "pending_count": 0,
                    "dispatched_count": 0,
                    "success_rate": 0.75,
                    "failure_rate": 0.25
                }]
            }),
        ),
        ep(
            "PATCH",
            "/api/queue/entries/{id}",
            "auto-queue",
            "Update one auto-queue entry or reconcile a failed terminal entry",
        )
        .with_params([
            ("id", path_param("Auto-queue entry ID")),
            (
                "thread_group",
                body_param("number", false, "Move the entry to another thread group"),
            ),
            (
                "batch_phase",
                body_param("number", false, "Move the entry to another batch phase"),
            ),
            (
                "priority_rank",
                body_param("number", false, "Set the entry's rank within its group"),
            ),
            (
                "status",
                body_param(
                    "string",
                    false,
                    "Manual status update: pending, skipped, or done only when reconciling a failed entry whose card is done and completed_commit evidence exists",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "entry-1"}, "body": {"thread_group": 1, "batch_phase": 2, "priority_rank": 0}}),
            json!({"ok": true, "entry": {"id": "entry-1", "thread_group": 1, "batch_phase": 2, "priority_rank": 0, "status": "pending"}}),
        )
        .with_example(
            json!({"path": {"id": "entry-failed"}, "body": {"status": "done"}}),
            json!({"ok": true, "entry": {"id": "entry-failed", "status": "done", "card_id": "card-1794", "github_issue_number": 1794}}),
        ),
        ep(
            "PATCH",
            "/api/queue/entries/{id}/skip",
            "auto-queue",
            "Skip a pending auto-queue entry",
        )
        .with_params([("id", path_param("Auto-queue entry ID"))])
        .with_example(
            json!({"path": {"id": "entry-1"}}),
            json!({"ok": true}),
        ),
        ep(
            "PATCH",
            "/api/queue/runs/{id}",
            "auto-queue",
            "Update auto-queue run metadata",
        )
        .with_params([
            ("id", path_param("Auto-queue run ID")),
            (
                "status",
                body_param("string", false, "New run status"),
            ),
            (
                "max_concurrent_threads",
                body_param("number", false, "Set the run's concurrency limit"),
            ),
            (
                "unified_thread",
                body_param(
                    "boolean",
                    false,
                    "Accepted for compatibility but ignored",
                ),
            ),
        ])
        .with_example(
            json!({"path": {"id": "run-1"}, "body": {"status": "completed", "max_concurrent_threads": 4, "unified_thread": true}}),
            json!({"ok": true, "ignored": ["unified_thread"]}),
        ),
        ep(
            "PATCH",
            "/api/queue/reorder",
            "auto-queue",
            "Reorder pending auto-queue entries",
        )
        .with_params([
            (
                "ordered_ids",
                body_param("string[]", true, "Ordered entry ids in desired priority order"),
            ),
            (
                "agent_id",
                body_param("string", false, "Optional agent scope for reordering"),
            ),
        ])
        .with_example(
            json!({"body": {"ordered_ids": ["entry-2", "entry-1"], "agent_id": "agent-1"}}),
            json!({"ok": true}),
        )
        .with_error_example(
            400,
            json!({"body": {"agent_id": "agent-1"}}),
            json!({"error": "ordered_ids is required"}),
        )
        .with_curl("curl -X PATCH http://localhost:8787/api/queue/reorder -H 'Content-Type: application/json' -d '{\"ordered_ids\":[\"entry-2\",\"entry-1\"],\"agent_id\":\"agent-1\"}'"),
        ep(
            "POST",
            "/api/queue/slots/{agent_id}/{slot_index}/rebind",
            "auto-queue",
            "Rebind one auto-queue slot to a run/thread group after the active dispatch has been completed, cancelled, or skipped.",
        )
        .with_params([
            ("agent_id", path_param("Agent ID")),
            (
                "slot_index",
                ParamDoc {
                    location: "path",
                    kind: "number",
                    required: true,
                    description: "Slot pool index",
                    enum_values: None,
                    default: None,
                },
            ),
            ("run_id", body_param("string", true, "Active or paused auto-queue run ID")),
            (
                "thread_group",
                body_param("number", true, "Thread group that should own this slot"),
            ),
        ])
        .with_example(
            json!({"path": {"agent_id": "agent-1", "slot_index": 0}, "body": {"run_id": "run-1", "thread_group": 0}}),
            json!({"ok": true, "agent_id": "agent-1", "slot_index": 0, "run_id": "run-1", "thread_group": 0, "rebound": true, "updated_entries": 1}),
        )
        .with_error_example(
            409,
            json!({"path": {"agent_id": "agent-1", "slot_index": 0}, "body": {"run_id": "run-1", "thread_group": 0}}),
            json!({"error": "slot 0 for agent-1 has an active dispatch; reset or complete it before rebind"}),
        ),
        ep(
            "POST",
            "/api/queue/slots/{agent_id}/{slot_index}/reset-thread",
            "auto-queue",
            "Reset a slot-thread binding for an agent",
        )
        .with_params([
            ("agent_id", path_param("Agent ID")),
            (
                "slot_index",
                ParamDoc {
                    location: "path",
                    kind: "number",
                    required: true,
                    description: "Slot pool index",
                    enum_values: None,
                    default: None,
                },
            ),
        ])
        .with_example(
            json!({"path": {"agent_id": "agent-1", "slot_index": 0}}),
            json!({"ok": true, "agent_id": "agent-1", "slot_index": 0, "archived_threads": 1, "cleared_sessions": 1, "cleared_bindings": 1}),
        ),
        ep(
            "POST",
            "/api/queue/reset",
            "auto-queue",
            "Reset one agent queue and clear its queue entries",
        )
        .with_params([("agent_id", body_param("string", true, "Agent ID for the queue reset"))])
        .with_example(
            json!({"body": {"agent_id": "agent-1"}}),
            json!({"ok": true, "deleted_entries": 4, "completed_runs": 1, "protected_active_runs": 0}),
        ),
        ep(
            "POST",
            "/api/queue/reset-global",
            "auto-queue",
            "Reset all queues with an explicit confirmation token",
        )
        .with_params([(
            "confirmation_token",
            body_param(
                "string",
                true,
                "Confirmation token required for global reset",
            ),
        )])
        .with_example(
            json!({"body": {"confirmation_token": "confirm-global-reset"}}),
            json!({"ok": true, "deleted_entries": 4, "completed_runs": 1, "protected_active_runs": 0}),
        ),
        ep(
            "POST",
            "/api/queue/pause",
            "auto-queue",
            "Soft-pause active runs",
        )
        .with_params([(
            "force",
            body_param(
                "boolean",
                false,
                "Cancel live dispatches and release tmux slots before pausing",
            )
            .with_default(false),
        )])
        .with_example(
            json!({}),
            json!({
                "ok": true,
                "paused_runs": 1,
                "cancelled_dispatches": 0,
                "released_slots": 0,
                "cleared_slot_sessions": 0
            }),
        )
        .with_error_example(
            409,
            json!({}),
            json!({"error": "no active runs to pause"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/pause -H 'Content-Type: application/json' -d '{}'"),
        ep(
            "POST",
            "/api/queue/resume",
            "auto-queue",
            "Resume paused runs and dispatch next entries. Runs blocked by pending/failed phase-gate rows remain paused and return blocked_runs with message='No resumable runs'.",
        )
        .with_example(json!({}), json!({"ok": true, "resumed_runs": 1, "blocked_runs": 0, "dispatched": 1}))
        .with_error_example(
            200,
            json!({}),
            json!({"ok": true, "resumed_runs": 0, "blocked_runs": 1, "message": "No resumable runs"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/resume"),
        ep(
            "POST",
            "/api/queue/runs/{id}/restore",
            "auto-queue",
            "Restore a cancelled or restoring run by re-evaluating skipped entries. Paused runs are rejected; use /api/queue/resume unless the run is cancelled/restoring.",
        )
        .with_params([("id", path_param("Auto-queue run ID"))])
        .with_example(
            json!({"path": {"id": "run-1"}}),
            json!({
                "ok": true,
                "run_id": "run-1",
                "run_status": "active",
                "restored_pending": 1,
                "restored_done": 0,
                "restored_dispatched": 0,
                "rebound_slots": 0,
                "created_dispatches": 0,
                "unbound_dispatches": 0
            }),
        )
        .with_error_example(
            400,
            json!({"path": {"id": "run-1"}}),
            json!({"error": "only cancelled or restoring runs can be restored (status=paused)", "run_id": "run-1", "status": "paused"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/runs/run-1/restore"),
        ep(
            "POST",
            "/api/queue/cancel",
            "auto-queue",
            "Cancel active or paused runs and skip pending entries",
        )
        .with_example(
            json!({}),
            json!({"ok": true, "cancelled_entries": 3, "cancelled_runs": 1}),
        )
        .with_error_example(
            409,
            json!({}),
            json!({"error": "no cancellable runs"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/queue/cancel -H 'Content-Type: application/json' -d '{}'"),
        ep(
            "POST",
            "/api/queue/runs/{id}/order",
            "auto-queue",
            "Submit ordered cards for a pending run",
        )
        .with_params([
            ("id", path_param("Auto-queue run ID")),
            (
                "order",
                body_param(
                    "number[]|string[]",
                    true,
                    "Ordered GitHub issue numbers or card ids",
                ),
            ),
            (
                "rationale",
                body_param("string", false, "Ordering rationale for this run"),
            ),
            (
                "reasoning",
                body_param("string", false, "Alias for rationale"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "run-1"}, "body": {"order": [423, 405], "rationale": "dependency-first"}}),
            json!({"ok": true, "created": 2, "run_id": "run-1", "message": "Queue active. Call POST /api/queue/dispatch-next to start dispatching."}),
        ),
        ep(
            "GET",
            "/api/channels/{id}/queue",
            "queue",
            "List queue entries for a Discord channel // TODO: example",
        ),
        ep(
            "GET",
            "/api/channels/{id}/watcher-state",
            "monitoring",
            "Read-only snapshot of the tmux-watcher lifecycle state for a channel. \
             Core fields (#964): provider, attached, tmux_session, last_relay_offset, \
             inflight_state_present, last_relay_ts_ms, last_capture_offset, unread_bytes, \
             desynced (orphan/cross-owner/stale capture divergence, 30s threshold), \
             reconnect_count, has_pending_queue. \
             #1133 enriched diagnostics (omitted when source is absent): \
             inflight_started_at, inflight_updated_at, inflight_user_msg_id, \
             inflight_current_msg_id, watcher_owner_channel_id, tmux_session_alive \
             (PID check via `tmux has-session`), mailbox_active_user_msg_id. Returns \
             404 when no watcher / inflight / mailbox engagement exists for the channel.",
        )
        .with_params([("id", path_param("Discord channel ID (numeric)"))])
        .with_example(
            json!({"path": {"id": "523456789012345678"}}),
            json!({
                "provider": "codex",
                "attached": true,
                "tmux_session": "agentdesk-codex-channel-523456789012345678",
                "watcher_owner_channel_id": 523456789012345678_u64,
                "last_relay_offset": 2048,
                "inflight_state_present": true,
                "last_relay_ts_ms": 1_761_369_600_000_i64,
                "last_capture_offset": 4096,
                "unread_bytes": 2048,
                "desynced": false,
                "reconnect_count": 1,
                "inflight_started_at": "2026-04-25 03:00:00",
                "inflight_updated_at": "2026-04-25 03:00:42",
                "inflight_user_msg_id": 9001,
                "inflight_current_msg_id": 9002,
                "tmux_session_alive": true,
                "has_pending_queue": false,
                "mailbox_active_user_msg_id": 9001,
            }),
        ),
        ep(
            "GET",
            "/api/dispatches/pending",
            "queue",
            "List pending dispatches // TODO: example",
        ),
        ep(
            "POST",
            "/api/dispatches/{id}/cancel",
            "dispatches",
            "Cancel a pending or dispatched dispatch, reset linked auto-queue bookkeeping, cancel any matching active turn through the shared turn cancel finalizer, and remove the dispatch notify guard. Terminal dispatches return 409 Conflict.",
        )
        .with_params([("id", path_param("Dispatch ID"))])
        .with_example(
            json!({"path": {"id": "dispatch-1"}}),
            json!({
                "ok": true,
                "dispatch_id": "dispatch-1",
                "active_turn_cancelled": true,
                "turn_status": "cancelled",
                "turn_completed_at": "2026-05-03T01:23:45Z"
            }),
        )
        .with_error_example(
            409,
            json!({"path": {"id": "dispatch-1"}}),
            json!({"error": "dispatch already in terminal state: completed", "code": "dispatch"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/dispatches/dispatch-1/cancel"),
        ep(
            "POST",
            "/api/dispatches/cancel-all",
            "queue",
            "Cancel all queued dispatches // TODO: example",
        ),
        ep(
            "POST",
            "/api/turns/{channel_id}/cancel",
            "queue",
            "Cancel a live turn by channel // TODO: example",
        ),
        ep(
            "POST",
            "/api/turns/{channel_id}/extend-timeout",
            "queue",
            "Extend live turn timeout // TODO: example",
        ),
        ep(
            "POST",
            "/api/channels/{channel_id}/monitoring",
            "monitoring",
            "Create or update a channel monitoring status entry // TODO: example",
        )
        .with_params([
            ("channel_id", path_param("Discord channel ID")),
            ("key", body_param("string", true, "Stable monitoring entry key")),
            (
                "description",
                body_param("string", true, "Human-readable status description"),
            ),
        ]),
        ep(
            "GET",
            "/api/channels/{channel_id}/monitoring",
            "monitoring",
            "List channel monitoring status entries // TODO: example",
        )
        .with_params([("channel_id", path_param("Discord channel ID"))]),
        ep(
            "DELETE",
            "/api/channels/{channel_id}/monitoring/{key}",
            "monitoring",
            "Remove a channel monitoring status entry // TODO: example",
        )
        .with_params([
            ("channel_id", path_param("Discord channel ID")),
            ("key", path_param("Monitoring entry key")),
        ]),
        ep("GET", "/api/analytics", "analytics", "Observability counters and structured events // TODO: example")
            .with_params([
                (
                    "provider",
                    query_param("string", false, "Filter by provider id (claude/codex/gemini/opencode/qwen)"),
                ),
                (
                    "channelId",
                    query_param("string", false, "Filter by Discord channel id"),
                ),
                (
                    "eventType",
                    query_param("string", false, "Filter by event type"),
                ),
                (
                    "limit",
                    query_param("integer", false, "Maximum recent events to return").with_default(100),
                ),
            ]),
        ep(
            "GET",
            "/api/analytics/invariants",
            "analytics",
            "Runtime invariant violation counts and recent events",
        )
        .with_params([
            (
                "provider",
                query_param("string", false, "Filter by provider id (claude/codex/gemini/opencode/qwen)"),
            ),
            (
                "channelId",
                query_param("string", false, "Filter by Discord channel id"),
            ),
            (
                "invariant",
                query_param("string", false, "Filter by invariant key"),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum recent violations to return").with_default(50),
            ),
        ])
        .with_example(
            json!({"query": {"limit": 10}}),
            json!({"counts": {"dispatch_without_card": 0}, "recent_violations": []}),
        )
        .with_error_example(
            400,
            json!({"query": {"limit": -1}}),
            json!({"error": "limit must be non-negative"}),
        )
        .with_curl("curl 'http://localhost:8787/api/analytics/invariants?limit=10'"),
        ep(
            "GET",
            "/api/analytics/observability",
            "analytics",
            "Foundation-layer atomic counters per channel×provider + in-memory structured event ring (#1070)",
        )
        .with_params([(
            "recentLimit",
            query_param("integer", false, "Maximum recent events to return (<=1000)")
                .with_default(100),
        )])
        .with_example(
            json!({"query": {"recentLimit": 50}}),
            json!({"counters": [{"channel_id": "1473922824350601297", "provider": "codex", "turns_total": 320, "errors_total": 3}], "recent_events": []}),
        )
        .with_error_example(
            400,
            json!({"query": {"recentLimit": 9999}}),
            json!({"error": "recentLimit must be <= 1000"}),
        )
        .with_curl("curl 'http://localhost:8787/api/analytics/observability?recentLimit=50'"),
        ep(
            "GET",
            "/api/quality/events",
            "analytics",
            "Agent quality raw event stream // TODO: example",
        )
        .with_params([
            (
                "agent_id",
                query_param("string", false, "Filter by agent id"),
            ),
            (
                "days",
                query_param("integer", false, "Lookback window in days").with_default(7),
            ),
            (
                "limit",
                query_param("integer", false, "Maximum recent events to return").with_default(200),
            ),
        ]),
        ep("GET", "/api/streaks", "analytics", "Agent activity streaks // TODO: example"),
        ep("GET", "/api/achievements", "analytics", "Agent achievements // TODO: example"),
        ep(
            "GET",
            "/api/activity-heatmap",
            "analytics",
            "Activity heatmap by hour // TODO: example",
        ),
        ep("GET", "/api/audit-logs", "analytics", "Audit logs // TODO: example"),
        ep(
            "GET",
            "/api/machine-status",
            "analytics",
            "Machine online status // TODO: example",
        ),
        ep(
            "GET",
            "/api/rate-limits",
            "analytics",
            "Cached rate limits per provider // TODO: example",
        ),
        ep("GET", "/api/receipt", "analytics", "Latest usage receipt snapshot // TODO: example"),
        ep(
            "GET",
            "/api/token-analytics",
            "analytics",
            "Token dashboard analytics with daily trend, heatmap, and usage breakdowns // TODO: example",
        )
        .with_params([(
            "period",
            ParamDoc {
                location: "query",
                kind: "string",
                required: false,
                description: "Analytics window",
                enum_values: None,
                default: None,
            }
            .with_enum(&["7d", "30d", "90d"])
            .with_default("30d"),
        )]),
        ep(
            "GET",
            "/api/home/kpi-trends",
            "analytics",
            "Home KPI sparkline data — tokens, cost, in-progress dispatch counts, and rate-limit utilization in a single payload (#1242). Each series exposes `label`, `unit`, and a `values` array sized to the requested `days` window so a sparkline component can render any tile with the same axis. The rate-limit section returns one entry per provider with `current_pct`, `unsupported`, `stale`, `reason`, and a flat `values` sparkline; providers without telemetry come back with `unsupported: true` and an empty `values` array so the dashboard can render a placeholder.",
        )
        .with_params([(
            "days",
            query_param("integer", false, "Lookback window in days (default 14, clamped to [1, 30])"),
        )])
        .with_example(
            json!({"query": {"days": 14}}),
            json!({
                "days": 14,
                "generated_at": "2026-04-27T00:00:00Z",
                "dates": ["2026-04-14", "2026-04-15", "..."],
                "tokens":      {"label": "Today's tokens", "unit": "tokens",     "values": [0, 0]},
                "cost":        {"label": "API cost",       "unit": "usd",        "values": [0.0, 0.0]},
                "in_progress": {"label": "In progress",    "unit": "dispatches", "values": [0, 0]},
                "rate_limit": {
                    "label": "Rate limit",
                    "unit": "percent",
                    "providers": [
                        {"provider": "claude", "current_pct": 25.0, "unsupported": false, "stale": false, "reason": null, "values": [25.0, 25.0]},
                        {"provider": "qwen",   "current_pct": null, "unsupported": true,  "stale": false, "reason": "no telemetry yet", "values": []}
                    ]
                }
            }),
        ),
        ep(
            "GET",
            "/api/skills-trend",
            "analytics",
            "Skill usage trend by day // TODO: example",
        ),
        ep(
            "GET",
            "/api/help",
            "docs",
            "Agent-friendly API inventory with categories, params, and examples",
        )
        .with_example(
            json!({}),
            json!({"categories": [{"name": "queue", "count": 15}], "endpoints": [{"method": "POST", "path": "/api/queue/generate", "category": "queue", "subcategory": "auto-queue"}]}),
        ),
        ep(
            "GET",
            "/api/docs",
            "docs",
            "List the eight #1063 top-level documentation groups, or return the flat endpoint list with format=flat",
        )
        .with_params([(
            "format",
            query_param("string", false, "Use format=flat for the full endpoint array"),
        )])
        .with_example(
            json!({}),
            json!({"groups": [{"name": "runtime", "description": "Turns, sessions, dispatches, message log, and server lifecycle surfaces.", "categories": ["dispatches", "sessions"]}]}),
        )
        .with_error_example(
            400,
            json!({"query": {"format": "xml"}}),
            json!({"error": "unsupported format: xml; use format=flat for the flat endpoint list"}),
        )
        .with_curl("curl http://localhost:8787/api/docs"),
        ep(
            "GET",
            "/api/docs/{group}",
            "docs",
            "List the fine-grained categories inside one of the eight #1063 groups (runtime/kanban/agents/integrations/automation/config/observability/internal). Falls back to legacy flat-category output with X-Deprecated header when a category name is supplied instead.",
        )
        .with_params([(
            "group",
            path_param("Group name such as runtime, kanban, automation, or integrations"),
        )])
        .with_example(
            json!({"path": {"group": "kanban"}}),
            json!({"group": "kanban", "categories": [{"name": "kanban", "endpoint_count": 24}, {"name": "reviews", "endpoint_count": 8}]}),
        ),
        ep(
            "GET",
            "/api/docs/{group}/{category}",
            "docs",
            "Get detailed endpoints for one fine-grained category nested inside a group (e.g. kanban/reviews, automation/auto-queue).",
        )
        .with_params([
            (
                "group",
                path_param("Top-level group name such as kanban or automation"),
            ),
            (
                "category",
                path_param("Category under the group such as reviews or auto-queue"),
            ),
        ])
        .with_example(
            json!({"path": {"group": "kanban", "category": "reviews"}}),
            json!({"group": "kanban", "category": "reviews", "count": 8, "endpoints": [{"method": "POST", "path": "/api/reviews/verdict"}]}),
        ),
        ep(
            "POST",
            "/api/reviews/verdict",
            "reviews",
            "Submit counter-model review verdict for a review dispatch",
        )
        .with_params([
            ("dispatch_id", body_param("string", true, "Review dispatch ID")),
            ("overall", body_param("string", true, "pass | improve | reject | rework | approved")),
            ("notes", body_param("string", false, "Reviewer notes")),
            ("feedback", body_param("string", false, "Reviewer feedback")),
            ("commit", body_param("string", false, "Reviewed commit SHA")),
            ("provider", body_param("string", false, "Verdict submitter provider")),
        ])
        .with_example(
            json!({"body": {"dispatch_id": "review-1", "overall": "pass", "notes": "LGTM"}}),
            json!({"ok": true, "dispatch_id": "review-1", "overall": "pass"}),
        )
        .with_error_example(
            400,
            json!({"body": {"dispatch_id": "review-1"}}),
            json!({"error": "overall must be one of: pass, improve, reject, rework, approved"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/reviews/verdict -H 'Content-Type: application/json' -d '{\"dispatch_id\":\"review-1\",\"overall\":\"pass\",\"notes\":\"LGTM\"}'"),
        ep(
            "POST",
            "/api/reviews/decision",
            "reviews",
            "Submit review-decision action // TODO: example",
        ),
        ep(
            "POST",
            "/api/reviews/tuning/aggregate",
            "reviews",
            "Aggregate review-tuning outcomes // TODO: example",
        ),
        ep(
            "POST",
            "/api/pm-decision",
            "pm",
            "Apply a PM decision to a force-only card",
        )
        .with_params([
            ("card_id", body_param("string", true, "Kanban card ID")),
            (
                "decision",
                body_param("string", true, "PM decision")
                    .with_enum(&["resume", "rework", "dismiss", "requeue"]),
            ),
            (
                "comment",
                body_param("string", false, "Optional PM comment"),
            ),
        ])
        .with_example(
            json!({"body": {"card_id": "card-1", "decision": "requeue", "comment": "needs reprioritization"}}),
            json!({"ok": true, "card_id": "card-1", "decision": "requeue", "message": "Card moved back to ready for reprioritization"}),
        ),
        // #1066 /api/memory dual-mode
        ep(
            "POST",
            "/api/memory/recall",
            "memory",
            "Recall memory fragments by keyword/text. Auto-selects memento or local backend (ADK_FORCE_LOCAL_MEMORY=1 forces local).",
        )
        .with_params([
            (
                "keywords",
                body_param("array", false, "List of keywords matched via LIKE in local mode"),
            ),
            (
                "text",
                body_param("string", false, "Free-form text appended to keyword filters"),
            ),
            (
                "workspace",
                body_param("string", false, "Optional workspace scope filter"),
            ),
            (
                "limit",
                body_param("integer", false, "Max fragments returned (default 20, max 200)"),
            ),
        ])
        .with_example(
            json!({"body": {"keywords": ["postgres"], "workspace": "ops", "limit": 5}}),
            json!({
                "fragments": [{"id": "mem-abc", "content": "PostgreSQL cutover done", "topic": "pg-cutover"}],
                "source": "local",
                "detected_backend": "local"
            }),
        ),
        ep(
            "POST",
            "/api/memory/remember",
            "memory",
            "Persist a memory fragment. Auto-selects memento or local backend.",
        )
        .with_params([
            ("content", body_param("string", true, "Fragment content")),
            ("topic", body_param("string", true, "Topic label for grouping")),
            (
                "type",
                body_param(
                    "string",
                    true,
                    "Fragment type: fact/decision/error/preference/procedure/relation/episode",
                ),
            ),
            (
                "importance",
                body_param("number", false, "Importance score 0.0–1.0"),
            ),
            (
                "workspace",
                body_param("string", false, "Optional workspace scope"),
            ),
            (
                "keywords",
                body_param("array", false, "Optional keyword array"),
            ),
        ])
        .with_example(
            json!({"body": {"content": "Agent #1066 landed", "topic": "release", "type": "decision"}}),
            json!({"id": "mem-abc", "source": "local"}),
        ),
        ep(
            "POST",
            "/api/memory/forget",
            "memory",
            "Remove a memory fragment by id. Returns 404 when the id is not found.",
        )
        .with_params([("id", body_param("string", true, "Fragment id returned by remember"))])
        .with_example(
            json!({"body": {"id": "mem-abc"}}),
            json!({"ok": true, "source": "local"}),
        ),
        // provider-cli safe migration
        ep(
            "GET",
            "/api/provider-cli",
            "provider-cli",
            "List current channel snapshots and migration states for all providers (codex, claude, gemini, qwen).",
        )
        .with_example(
            json!(null),
            json!({
                "providers": [{"provider": "codex", "current": null, "candidate": null}],
                "migrations": [],
                "generated_at": "2026-01-01T00:00:00Z"
            }),
        ),
        ep(
            "PATCH",
            "/api/provider-cli/{provider}",
            "provider-cli",
            "Apply an operator action to a provider migration state. Actions: confirm_promote, rollback, rollback_to_previous.",
        )
        .with_params([
            (
                "provider",
                path_param("Provider id: codex, claude, gemini, or qwen"),
            ),
            ("action", body_param("string", true, "confirm_promote | rollback | rollback_to_previous")),
            ("evidence", body_param("string", false, "Optional operator note recorded in migration history")),
        ])
        .with_example(
            json!({"body": {"action": "confirm_promote", "evidence": "operator approved via Discord"}}),
            json!({"provider": "codex", "action": "confirm_promote", "state": "ProviderAgentsMigrated", "updated_at": "2026-01-01T00:00:00Z"}),
        ),
    ]
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

// ---------------------------------------------------------------------------
// #1443 — Long-form guide pages.
//
// The endpoint catalogue answers "what does this endpoint do?", but the
// 2026-04-30 #1435 incident showed that callers also need a higher-level
// "which of these overlapping endpoints do I call right now?" decision tree.
// `guide_index` powers the `guides` field on the `/api/docs` root, and each
// guide has a dedicated handler returning the full body.
// ---------------------------------------------------------------------------

/// #1443 — header marker exposing the most recent main commit this guide was
/// authored against. Update alongside the prose any time the underlying
/// lifecycle endpoints change shape, per the
/// `docs/agent-maintenance/index.md` freshness convention (#1432).
pub(crate) const CARD_LIFECYCLE_OPS_LAST_REFRESHED: &str =
    "Last refreshed: 2026-04-30 against main @ f74cad35 (post #1442/#1444/#1446/#1448)";

/// #1549 — marker contract guide for structured API-friction reporting.
pub(crate) const API_FRICTION_MARKERS_LAST_REFRESHED: &str =
    "Last refreshed: 2026-05-03 against main @ 91bc116a (#1549)";

/// #1443 list of long-form guide pages exposed under `/api/docs/...`.
fn guide_index() -> Vec<Value> {
    vec![
        json!({
            "name": "card-lifecycle-ops",
            "title": "Card Lifecycle Ops Guide",
            "path": "/api/docs/card-lifecycle-ops",
            "summary": "Decision tree + endpoint reference for /redispatch, /retry, /transition, /queue/generate, /dispatch-next. Read this BEFORE chaining card-lifecycle calls.",
        }),
        json!({
            "name": "api-friction-markers",
            "title": "API Friction Marker Guide",
            "path": "/api/docs/api-friction-markers",
            "summary": "Marker schema and collection path for API_FRICTION reports emitted when /api docs are missing or misleading.",
        }),
    ]
}

/// GET /api/docs/card-lifecycle-ops — #1443.
///
/// Long-form decision tree for card-lifecycle operations
/// (/redispatch, /retry, /transition, /queue/generate, /dispatch-next).
/// Authored to make the 2026-04-30 #1435 duplicate-dispatch incident
/// non-repeatable: every common scenario maps to a single-call answer, and
/// the anti-pattern section names the exact 3-call chain that caused the
/// outage.
///
/// Routed through `resolve_docs_segment` (the shared `/api/docs/{segment}`
/// resolver), so this dedicated handler is reserved for in-process callers
/// (e.g. CLI shims) that want the body without going through the segment
/// dispatcher.
#[allow(dead_code)]
pub async fn api_docs_card_lifecycle_ops() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(card_lifecycle_ops_body()))
}

fn card_lifecycle_ops_body() -> Value {
    json!({
        "title": "Card Lifecycle Ops Guide",
        "path": "/api/docs/card-lifecycle-ops",
        "last_refreshed": CARD_LIFECYCLE_OPS_LAST_REFRESHED,
        "purpose": "Single source of truth for choosing among /redispatch, /retry, /transition, /queue/generate, and /dispatch-next. The 2026-04-30 #1435 incident chained three of these and created duplicate dispatches. Read the decision tree FIRST, then the anti-pattern, then the endpoint table.",
        "sections": {
            "1_decision_tree": {
                "heading": "Section 1: Decision Tree",
                "intro": "Pick the row that matches the symptom. Each answer is a single call — do NOT chain.",
                "scenarios": [
                    {
                        "scenario": "Card stuck in review/dilemma_pending, want to restart",
                        "single_call": "POST /api/kanban-cards/{id}/redispatch",
                        "notes": "Cancels the live dispatch and creates a brand-new dispatch_id. Inspect new_dispatch_id, cancelled_dispatch_id, and next_action in the response. Do NOT follow with /transition or /queue/generate."
                    },
                    {
                        "scenario": "Card done, want to retry the same failed step",
                        "single_call": "POST /api/kanban-cards/{id}/retry",
                        "notes": "Re-executes the same failed step with the same intent (optional assignee swap via assignee_agent_id). Inspect new_dispatch_id, cancelled_dispatch_id, and next_action. Do NOT follow with /transition or /queue/generate."
                    },
                    {
                        "scenario": "Force card to a specific status",
                        "single_call": "POST /api/kanban-cards/{id}/transition with {\"status\": \"<target>\"}",
                        "notes": "If the card has an active dispatch and target=ready, the call returns 409 Conflict (#1444 guard). Pass {\"force\": true} (or legacy cancel_dispatches=true) to cancel + re-transition in one call. Inspect cancelled_dispatch_ids, created_dispatch_id, and next_action_hint."
                    },
                    {
                        "scenario": "Bulk push N issues into the auto-queue",
                        "single_call": "POST /api/queue/generate with {\"issue_numbers\": [...]}",
                        "notes": "Bulk only — never use to restart a single card that already has an active dispatch (it will silent-skip and surface skipped_due_to_active_dispatch). Inspect skipped_due_to_active_dispatch / skipped_due_to_dependency / skipped_due_to_filter."
                    },
                    {
                        "scenario": "Trigger the next dispatch from an existing run",
                        "single_call": "POST /api/queue/dispatch-next",
                        "notes": "Use only after /generate has produced pending entries. Returns dispatched[], count, active_groups, pending_groups."
                    }
                ]
            },
            "2_endpoint_reference_table": {
                "heading": "Section 2: Endpoint Reference Table",
                "columns": ["endpoint", "when_to_use", "single_call_complete", "common_pitfall"],
                "rows": [
                    {
                        "endpoint": "POST /api/kanban-cards/{id}/redispatch",
                        "when_to_use": "Card has a live (pending/dispatched) dispatch and you want to restart with a brand-new dispatch_id.",
                        "single_call_complete": "Y",
                        "common_pitfall": "Chaining /transition or /queue/generate after it creates duplicate dispatches (#1442 incident)."
                    },
                    {
                        "endpoint": "POST /api/kanban-cards/{id}/retry",
                        "when_to_use": "Card landed in a failed terminal state and you want to re-run the SAME step.",
                        "single_call_complete": "Y",
                        "common_pitfall": "Calling on a card with no failed dispatch returns 409. Do NOT chain /transition or /generate."
                    },
                    {
                        "endpoint": "POST /api/kanban-cards/{id}/transition",
                        "when_to_use": "Administrative move to a specific target status (the canonical /force-transition path).",
                        "single_call_complete": "Y",
                        "common_pitfall": "target=ready while a dispatch is live returns 409 unless force=true (#1444 guard). Without force, callers used to chain /redispatch + /transition + /generate — that is the exact #1435 anti-pattern."
                    },
                    {
                        "endpoint": "POST /api/queue/generate",
                        "when_to_use": "Bulk push of multiple issue numbers into a queue run.",
                        "single_call_complete": "Y for the bulk intent",
                        "common_pitfall": "Not a single-card restart tool. Cards that already have a live dispatch are silently skipped and reported in skipped_due_to_active_dispatch — do not retry by chaining /redispatch first."
                    },
                    {
                        "endpoint": "POST /api/queue/dispatch-next",
                        "when_to_use": "Move the next pending entry of an existing run to dispatched.",
                        "single_call_complete": "Y",
                        "common_pitfall": "No-op if there are no pending entries; check the dispatched[] length before assuming progress."
                    }
                ]
            },
            "3_anti_pattern": {
                "heading": "Section 3: Anti-pattern (today's #1435 incident)",
                "wrong_pattern": [
                    "POST /api/kanban-cards/{id}/redispatch              # creates dispatch A",
                    "POST /api/kanban-cards/{id}/transition status:ready # cancels A, creates dispatch B  <- WRONG: this cancel+create is implicit; caller did not realize a fresh dispatch was made",
                    "POST /api/queue/generate                       # adds the card to a queue run; a subsequent /dispatch-next (or activate=true) then creates dispatch C  <- WRONG: silent-skip exists for cards with an active dispatch but is easy to miss in the response"
                ],
                "why_it_broke": "Each of /redispatch, /transition status:ready, and the /queue/generate -> /dispatch-next chain is single-call complete for its intent. Chaining them produced multiple live dispatch rows for one card (dispatch A from /redispatch, dispatch B from /transition's force-transition cleanup, plus the queue-run path from /generate that the activate hook then turned into dispatch C). The runtime started executing the duplicates, causing the outage on 2026-04-30. Note: /generate by itself creates queue entries — dispatch rows are produced by /dispatch-next or the activate=true shortcut.",
                "how_it_is_prevented_now": [
                    "#1442 added new_dispatch_id and cancelled_dispatch_id(s) to /redispatch, /retry, and /transition responses, plus a per-endpoint follow-up signal: /redispatch and /retry return `next_action` (a fixed marker such as 'none_required' or 'assign_agent_then_call_redispatch'); /transition returns `next_action_hint` (a free-form sentence naming the exact follow-up). On the success path both are 'none_required' / point at no further action — if a caller sees that and still chains another mutation, it is a caller bug, not a missing signal.",
                    "#1444 added a 409 Conflict guard on /transition status:ready when an active dispatch exists. Callers must explicitly opt in via force=true (or legacy cancel_dispatches=true) to override.",
                    "#1444 also made /queue/generate surface structured skips (skipped_due_to_active_dispatch / skipped_due_to_dependency / skipped_due_to_filter) instead of silently dropping the entry, so even a misuse is observable from the response. Note: /dispatch-next does NOT return these arrays — it only reports `dispatched`, `count`, `active_groups`, and `pending_groups`."
                ],
                "right_pattern": "Pick ONE row from Section 1 and call it ONCE. Inspect new_dispatch_id, cancelled_dispatch_id(s), next_action / next_action_hint, and (for /generate) skipped_due_to_*. Do NOT call a second mutation unless next_action / next_action_hint says so."
            },
            "4_new_response_fields": {
                "heading": "Section 4: New Response Fields (from #1442 / #1444)",
                "fields": [
                    {
                        "field": "new_dispatch_id",
                        "source": "/redispatch, /retry",
                        "notes": "String. Confirms that a new dispatch row was inserted; absence means the call was a no-op. /transition uses a different name (`created_dispatch_id`) — see below."
                    },
                    {
                        "field": "created_dispatch_id",
                        "source": "/transition (force-transition path)",
                        "notes": "String or null. Populated when the force-transition cleanup created a fresh dispatch as part of the move. Distinct from /redispatch's and /retry's `new_dispatch_id` field."
                    },
                    {
                        "field": "cancelled_dispatch_id (singular)",
                        "source": "/redispatch, /retry",
                        "notes": "String or null. Only populated when the cancel helper actually transitioned a pending/dispatched row to cancelled."
                    },
                    {
                        "field": "cancelled_dispatch_ids (plural)",
                        "source": "/transition (force-transition path)",
                        "notes": "Array of dispatch IDs cancelled by the cleanup pass; pairs with cancelled_dispatches count."
                    },
                    {
                        "field": "next_action",
                        "source": "/redispatch, /retry",
                        "notes": "Concrete next-action string returned by the per-card endpoints. 'none_required' on the success path; otherwise a fixed marker such as 'assign_agent_then_call_retry', 'assign_agent_then_call_redispatch', or 'duplicate_active_dispatch_detected_inspect_card'. If it says 'none_required', do NOT chain another mutation."
                    },
                    {
                        "field": "next_action_hint",
                        "source": "/transition (force-transition path; also returned in the 409 body)",
                        "notes": "Free-form sentence naming the exact follow-up — for example 'call /api/queue/generate to dispatch newly-ready card', or guidance on the 409 override. Distinct from /redispatch and /retry's `next_action` field."
                    },
                    {
                        "field": "skipped_due_to_active_dispatch",
                        "source": "/queue/generate (NOT /dispatch-next)",
                        "notes": "Array of {issue_number, existing_dispatch_id} entries that were silently skipped because the card already had a live dispatch."
                    },
                    {
                        "field": "skipped_due_to_dependency",
                        "source": "/queue/generate (NOT /dispatch-next)",
                        "notes": "Array of {issue_number, unresolved_deps[]} entries skipped because dependency cards were not yet done."
                    },
                    {
                        "field": "skipped_due_to_filter",
                        "source": "/queue/generate (NOT /dispatch-next)",
                        "notes": "Array of entries skipped by repo/agent_id filters."
                    },
                    {
                        "field": "409 Conflict response",
                        "source": "/transition with status=ready and an active dispatch",
                        "notes": "Body shape: {error, active_dispatch_id, active_dispatch_ids, next_action_hint}. Override with {\"force\": true} (or legacy cancel_dispatches=true)."
                    }
                ]
            },
            "5_cross_references": {
                "heading": "Section 5: Cross-references",
                "links": [
                    {"label": "Issue #1442 — response schema (new_dispatch_id, next_action_hint)", "url": "https://github.com/itismyfield/AgentDesk/issues/1442"},
                    {"label": "Issue #1444 — 409 idempotency guard + structured silent-skip", "url": "https://github.com/itismyfield/AgentDesk/issues/1444"},
                    {"label": "Issue #1446 — stall watchdog and THREAD-GUARD stale cleanup", "url": "https://github.com/itismyfield/AgentDesk/issues/1446"},
                    {"label": "Issue #1448 — announce-bot turn-leak fix (issue-card template block-list)", "url": "https://github.com/itismyfield/AgentDesk/issues/1448"},
                    {"label": "docs/agent-maintenance/index.md — freshness convention", "path": "docs/agent-maintenance/index.md"},
                    {"label": "docs/source-of-truth.md — canonical edit paths index", "path": "docs/source-of-truth.md"}
                ]
            }
        }
    })
}

/// GET /api/docs/api-friction-markers — #1549.
///
/// Long-form marker contract for agents that discover an API docs gap while
/// working. The runtime already extracts these markers from turn output,
/// removes valid markers before user-facing delivery, persists them to
/// Postgres, optionally stores Memento memory, and aggregates repeated
/// fingerprints into GitHub issues from the policy tick.
#[allow(dead_code)]
pub async fn api_docs_api_friction_markers() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(api_friction_markers_body()))
}

fn api_friction_markers_body() -> Value {
    json!({
        "title": "API Friction Marker Guide",
        "path": "/api/docs/api-friction-markers",
        "last_refreshed": API_FRICTION_MARKERS_LAST_REFRESHED,
        "purpose": "Capture structured reports when an agent had to infer, trial-and-error, or bypass a missing/misleading /api docs contract. The marker is for docs/API friction only; it is not a replacement for normal task output.",
        "marker_prefix": "API_FRICTION:",
        "schema": {
            "required": {
                "endpoint": "HTTP endpoint or API surface, for example PATCH /api/dispatches/{id}",
                "friction_type": "Short category such as missing-docs, wrong-schema, or docs-bypass",
                "summary": "One-sentence friction summary"
            },
            "optional": {
                "workaround": "What the agent had to do instead",
                "suggested_fix": "Concrete docs/API improvement",
                "docs_category": "Fine-grained docs category such as dispatches or queue",
                "keywords": ["Extra grouping/search terms"]
            },
            "aliases": {
                "surface": "endpoint",
                "type": "friction_type",
                "frictionType": "friction_type",
                "workaround_method": "workaround",
                "suggestedFix": "suggested_fix",
                "docsCategory": "docs_category"
            }
        },
        "example": "API_FRICTION: {\"endpoint\":\"PATCH /api/dispatches/{id}\",\"friction_type\":\"missing-docs\",\"summary\":\"dispatch completion docs omitted PATCH semantics\",\"workaround\":\"read source and called PATCH manually\",\"suggested_fix\":\"document status/result response fields in /api/docs/dispatches\",\"docs_category\":\"dispatches\"}",
        "collection_flow": [
            "Turn bridge scans final and late turn output for lines beginning with API_FRICTION:.",
            "Valid JSON markers are stripped from the delivered response, normalized, fingerprinted by endpoint + friction_type, and inserted into api_friction_events.",
            "When Memento is configured, the same event is stored under topic api-friction with workspace-scoped context.",
            "The policy tick aggregates repeated fingerprints and creates one GitHub issue per unreported pattern."
        ],
        "operator_queries": [
            "SELECT endpoint, friction_type, COUNT(*) FROM api_friction_events GROUP BY 1,2 ORDER BY COUNT(*) DESC;",
            "SELECT fingerprint, issue_url, last_error FROM api_friction_issues ORDER BY updated_at DESC;"
        ],
        "constraints": [
            "Emit at most one marker per distinct docs/API gap in a turn.",
            "Do not include secrets, tokens, private prompt content, or full transcripts.",
            "Do not use DB direct writes as the workaround unless the task explicitly required DB repair; prefer canonical /api endpoints first."
        ],
        "source_of_truth": "docs/source-of-truth.md#api_friction-markers"
    })
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
            Some(400)
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
        assert_eq!(example_body["source"], "system");
    }
}
