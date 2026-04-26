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

    /// #1068 (904-6) curl 1-liner. Must stay a single physical line so it can
    /// be copy-pasted into a terminal without escaping line breaks.
    fn with_curl(mut self, curl: &'static str) -> Self {
        self.curl_example = Some(curl);
        self
    }

    fn deprecated_alias(mut self, canonical_path: &'static str) -> Self {
        self.deprecated = true;
        self.canonical_path = Some(canonical_path);
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
    ("POST", "/api/kanban-cards/{id}/transition"),
    ("POST", "/api/kanban-cards/{id}/retry"),
    ("POST", "/api/kanban-cards/{id}/redispatch"),
    ("POST", "/api/kanban-cards/{id}/resume"),
    ("POST", "/api/kanban-cards/{id}/reopen"),
    ("POST", "/api/kanban-cards/assign-issue"),
    ("GET", "/api/dispatches"),
    ("POST", "/api/dispatches"),
    ("GET", "/api/dispatches/{id}"),
    ("PATCH", "/api/dispatches/{id}"),
    ("POST", "/api/auto-queue/generate"),
    ("POST", "/api/auto-queue/dispatch-next"),
    ("GET", "/api/auto-queue/status"),
    ("POST", "/api/auto-queue/pause"),
    ("POST", "/api/auto-queue/resume"),
    ("POST", "/api/auto-queue/cancel"),
    ("PATCH", "/api/auto-queue/reorder"),
    ("POST", "/api/github/issues/create"),
    ("GET", "/api/pipeline/cards/{card_id}"),
    ("GET", "/api/analytics/observability"),
    ("GET", "/api/analytics/invariants"),
    ("POST", "/api/reviews/verdict"),
    ("GET", "/api/docs"),
];

const CANONICAL_CATEGORIES: [&str; 7] = [
    "agents",
    "kanban",
    "dispatches",
    "queue",
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
        "analytics" | "auth" | "docs" | "health" | "monitoring" | "stats" | "provider-cli" => "ops",
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
        "auto-queue" | "queue" | "cron" | "policies" => "automation",
        // config — settings, onboarding, knowledge, source-of-truth, skills,
        // offices, departments, memory (#1066 /api/memory dual-mode)
        "settings" | "onboarding" | "skills" | "offices" | "departments" | "memory" => "config",
        // observability — analytics, metrics, events, slo, diagnostics,
        // monitoring, stats, health, auth
        "analytics" | "monitoring" | "stats" | "health" | "auth" => "observability",
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
            "/api/discord/send",
            "discord",
            "Send a Discord channel message",
        )
        .with_params([
            ("channel_id", body_param("string", true, "Target Discord channel snowflake")),
            ("message", body_param("string", true, "Message body (markdown supported)")),
        ])
        .with_example(
            json!({"body": {"channel_id": "1473922824350601297", "message": "hello"}}),
            json!({"ok": true, "message_id": "1500000000000000000"}),
        )
        .with_error_example(
            400,
            json!({"body": {"message": "hello"}}),
            json!({"error": "channel_id is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/discord/send -H 'Content-Type: application/json' -d '{\"channel_id\":\"1473922824350601297\",\"message\":\"hello\"}'"),
        ep("POST", "/api/send", "discord", "Deprecated alias for /api/discord/send")
            .deprecated_alias("/api/discord/send"),
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
            "/api/send_to_agent",
            "discord",
            "Deprecated alias for /api/discord/send-to-agent",
        )
        .deprecated_alias("/api/discord/send-to-agent"),
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
        ep("POST", "/api/senddm", "discord", "Deprecated alias for /api/discord/send-dm")
            .deprecated_alias("/api/discord/send-dm"),
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
        .with_curl("curl -X POST http://localhost:8787/api/agents/family-counsel/turn/start -H 'Content-Type: application/json' -d '{\"prompt\":\"hello\",\"source\":\"system\"}'"),
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
            "Create or update a card from a GitHub issue",
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
            json!({"card": {"id": "card-426", "status": "ready", "github_issue_number": 426, "assigned_agent_id": "project-agentdesk"}}),
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
        ep("PATCH", "/api/kanban-cards/{id}", "kanban", "Update card")
            .with_params([
                ("id", path_param("Kanban card ID")),
                ("title", body_param("string", false, "Updated title")),
                (
                    "status",
                    body_param("string", false, "Target pipeline status"),
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
                json!({"path": {"id": "card-1"}, "body": {"status": "ready", "priority": "high"}}),
                json!({"card": {"id": "card-1", "status": "ready", "priority": "high"}}),
            )
            .with_error_example(
                404,
                json!({"path": {"id": "ghost-card"}, "body": {"status": "ready"}}),
                json!({"error": "card not found: ghost-card"}),
            )
            .with_curl("curl -X PATCH http://localhost:8787/api/kanban-cards/card-1 -H 'Content-Type: application/json' -d '{\"status\":\"ready\",\"priority\":\"high\"}'"),
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
            "Assign card to agent",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            ("agent_id", body_param("string", true, "Agent ID")),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"agent_id": "ch-td"}}),
            json!({"card": {"id": "card-1", "assigned_agent_id": "ch-td", "status": "requested"}}),
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/rereview",
            "kanban",
            "Force a card back through review",
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
            "Transition a single card with administrative force semantics",
        )
        .with_params([
            ("id", path_param("Kanban card ID")),
            ("status", body_param("string", true, "Target pipeline status")),
            (
                "cancel_dispatches",
                body_param(
                    "boolean",
                    false,
                    "When transitioning to backlog/ready, also cancel active dispatches",
                )
                .with_default(true),
            ),
        ])
        .with_example(
            json!({"path": {"id": "card-1"}, "body": {"status": "ready", "cancel_dispatches": true}}),
            json!({"card": {"id": "card-1", "status": "ready"}, "forced": true, "from": "in_progress", "to": "ready", "cancelled_dispatches": 1, "skipped_auto_queue_entries": 1}),
        )
        .with_error_example(
            400,
            json!({"path": {"id": "card-1"}, "body": {}}),
            json!({"error": "status is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/kanban-cards/card-1/transition -H 'Content-Type: application/json' -d '{\"status\":\"ready\",\"cancel_dispatches\":true}'"),
        ep("POST", "/api/kanban-cards/{id}/retry", "kanban", "Retry card: re-execute the same failed step with the same intent and context (optionally swapping assignee). Distinct from /redispatch (creates a NEW dispatch id), /resume (continues a checkpointed turn), and /reopen (re-admits a closed card).")
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
                json!({"card": {"id": "card-1", "assigned_agent_id": "agent-review", "latest_dispatch_id": "dispatch-retry-1"}}),
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
            "Redispatch: cancel the current live dispatch and create a brand-new dispatch entry with a new dispatch_id for the same card intent. Distinct from /retry (re-executes the SAME step with the same params), /resume (continues a checkpoint), and /reopen (re-admits a closed card).",
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
            json!({"card": {"id": "card-1", "latest_dispatch_id": "dispatch-redispatch-1", "status": "requested"}}),
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
            "PATCH",
            "/api/dispatches/{id}",
            "dispatches",
            "Update dispatch",
        )
        .with_params([
            ("id", path_param("Dispatch ID")),
            (
                "status",
                body_param("string", false, "New dispatch status"),
            ),
            (
                "result",
                body_param("object", false, "Structured dispatch result payload"),
            ),
        ])
        .with_example(
            json!({"path": {"id": "dispatch-1"}, "body": {"status": "completed", "result": {"summary": "done"}}}),
            json!({"dispatch": {"id": "dispatch-1", "status": "completed", "result": {"summary": "done"}}}),
        )
        .with_error_example(
            404,
            json!({"path": {"id": "dispatch-ghost"}, "body": {"status": "completed"}}),
            json!({"error": "dispatch not found: dispatch-ghost"}),
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
        ep(
            "POST",
            "/api/issues",
            "github",
            "Deprecated alias for /api/github/issues/create",
        )
        .deprecated_alias("/api/github/issues/create"),
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
            "List dispatched sessions // TODO: example",
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
            "/api/discord-bindings",
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
            "POST",
            "/api/auto-queue/generate",
            "auto-queue",
            "Generate auto-queue entries",
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
            json!({"run": {"id": "run-1", "status": "generated", "review_mode": "disabled", "thread_group_count": 2, "max_concurrent_threads": 2, "unified_thread": false}, "entries": [{"id": "entry-1", "github_issue_number": 423, "thread_group": 0, "priority_rank": 0, "status": "pending"}]}),
        )
        .with_error_example(
            400,
            json!({"body": {"repo": "test-repo", "issue_numbers": []}}),
            json!({"error": "issue_numbers or entries must be non-empty"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/auto-queue/generate -H 'Content-Type: application/json' -d '{\"repo\":\"test-repo\",\"issue_numbers\":[423,405]}'"),
        ep(
            "POST",
            "/api/auto-queue/dispatch",
            "auto-queue",
            "Deprecated (#1064): use /api/auto-queue/generate + /api/auto-queue/dispatch-next. Remains functional for legacy CLI callers with the groups body shape.",
        )
        .with_params([
            (
                "repo",
                body_param("string", false, "Restrict issues to one repository"),
            ),
            (
                "agent_id",
                body_param(
                    "string",
                    false,
                    "Target agent; also used for auto-assignment when requested",
                ),
            ),
            (
                "groups",
                body_param(
                    "object[]",
                    true,
                    "Ordered issue groups. Each item accepts issues, sequential, batch_phase, and thread_group",
                ),
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
                "activate",
                body_param("boolean", false, "Immediately promote and dispatch the generated run")
                    .with_default(true),
            ),
            (
                "review_mode",
                body_param(
                    "string",
                    false,
                    "Run review mode: 'enabled' keeps the normal review gate; 'disabled' skips review dispatch creation and relies on main-merge detection to advance the card and queue entry to done",
                )
                .with_default("enabled"),
            ),
            (
                "auto_assign_agent",
                body_param(
                    "boolean",
                    false,
                    "Assign unowned cards to agent_id before queue generation",
                )
                .with_default(true),
            ),
            (
                "max_concurrent_threads",
                body_param("number", false, "Upper bound for simultaneously active groups"),
            ),
        ])
        .with_example(
            json!({"body": {"repo": "test-repo", "agent_id": "project-agentdesk", "groups": [{"issues": [423, 405], "sequential": true}, {"issues": [407]}], "review_mode": "disabled", "unified_thread": true, "activate": true, "auto_assign_agent": true, "max_concurrent_threads": 2}}),
            json!({"run": {"id": "run-1", "status": "active", "review_mode": "disabled", "thread_group_count": 2, "max_concurrent_threads": 2}, "entries": [{"id": "entry-1", "github_issue_number": 423, "thread_group": 0, "priority_rank": 0, "status": "dispatched"}], "thread_groups": {"0": {"status": "active"}, "1": {"status": "pending"}}, "activated": true, "dispatch": {"count": 1}}),
        ),
        ep(
            "POST",
            "/api/auto-queue/dispatch-next",
            "auto-queue",
            "Dispatch the next pending auto-queue entries",
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
        .with_curl("curl -X POST http://localhost:8787/api/auto-queue/dispatch-next -H 'Content-Type: application/json' -d '{\"repo\":\"test-repo\"}'"),
        ep(
            "GET",
            "/api/auto-queue/status",
            "auto-queue",
            "Get latest auto-queue run state",
        )
        .with_params([
            (
                "repo",
                query_param("string", false, "Restrict status view to a repo"),
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
        .with_curl("curl 'http://localhost:8787/api/auto-queue/status?repo=test-repo'"),
        ep(
            "GET",
            "/api/auto-queue/history",
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
            "/api/auto-queue/entries/{id}",
            "auto-queue",
            "Update one pending auto-queue entry",
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
        ])
        .with_example(
            json!({"path": {"id": "entry-1"}, "body": {"thread_group": 1, "batch_phase": 2, "priority_rank": 0}}),
            json!({"ok": true, "entry": {"id": "entry-1", "thread_group": 1, "batch_phase": 2, "priority_rank": 0, "status": "pending"}}),
        ),
        ep(
            "PATCH",
            "/api/auto-queue/entries/{id}/skip",
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
            "/api/auto-queue/runs/{id}",
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
            "/api/auto-queue/reorder",
            "auto-queue",
            "Reorder pending auto-queue entries",
        )
        .with_params([
            (
                "orderedIds",
                body_param("string[]", true, "Ordered entry ids in desired priority order"),
            ),
            (
                "agentId",
                body_param("string", false, "Optional agent scope for reordering"),
            ),
        ])
        .with_example(
            json!({"body": {"orderedIds": ["entry-2", "entry-1"], "agentId": "agent-1"}}),
            json!({"ok": true}),
        )
        .with_error_example(
            400,
            json!({"body": {"agentId": "agent-1"}}),
            json!({"error": "orderedIds is required"}),
        )
        .with_curl("curl -X PATCH http://localhost:8787/api/auto-queue/reorder -H 'Content-Type: application/json' -d '{\"orderedIds\":[\"entry-2\",\"entry-1\"],\"agentId\":\"agent-1\"}'"),
        ep(
            "POST",
            "/api/auto-queue/slots/{agent_id}/{slot_index}/reset-thread",
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
            "/api/auto-queue/reset",
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
            "/api/auto-queue/reset-global",
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
            "/api/auto-queue/pause",
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
        .with_curl("curl -X POST http://localhost:8787/api/auto-queue/pause -H 'Content-Type: application/json' -d '{}'"),
        ep(
            "POST",
            "/api/auto-queue/resume",
            "auto-queue",
            "Resume paused runs and dispatch next entries: continues a run from its paused checkpoint (vs. /retry which re-executes a failed step, /redispatch which creates a new dispatch id, or /reopen which re-admits a closed card).",
        )
        .with_example(json!({}), json!({"ok": true, "resumed_runs": 1, "dispatched": 1}))
        .with_error_example(
            409,
            json!({}),
            json!({"error": "no paused runs to resume"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/auto-queue/resume"),
        ep(
            "POST",
            "/api/auto-queue/cancel",
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
        .with_curl("curl -X POST http://localhost:8787/api/auto-queue/cancel -H 'Content-Type: application/json' -d '{}'"),
        ep(
            "POST",
            "/api/auto-queue/runs/{id}/order",
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
            json!({"ok": true, "created": 2, "run_id": "run-1", "message": "Queue active. Call POST /api/auto-queue/dispatch-next to start dispatching."}),
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
             inflight_state_present, last_relay_ts_ms, has_pending_queue. \
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
            "queue",
            "Cancel a queued dispatch // TODO: example",
        ),
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
            json!({"categories": [{"name": "queue", "count": 15}], "endpoints": [{"method": "POST", "path": "/api/auto-queue/dispatch", "category": "queue", "subcategory": "auto-queue"}]}),
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
            "Submit review verdict",
        )
        .with_params([
            ("card_id", body_param("string", true, "Kanban card under review")),
            ("verdict", body_param("string", true, "approved | rejected | needs_changes")),
            ("summary", body_param("string", false, "Short reviewer summary")),
        ])
        .with_example(
            json!({"body": {"card_id": "card-1", "verdict": "approved", "summary": "LGTM"}}),
            json!({"ok": true, "review_id": "review-1", "card": {"id": "card-1", "review_status": "approved"}}),
        )
        .with_error_example(
            400,
            json!({"body": {"card_id": "card-1"}}),
            json!({"error": "verdict is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/reviews/verdict -H 'Content-Type: application/json' -d '{\"card_id\":\"card-1\",\"verdict\":\"approved\",\"summary\":\"LGTM\"}'"),
        ep(
            "POST",
            "/api/review-verdict",
            "reviews",
            "Deprecated alias for /api/reviews/verdict",
        )
        .deprecated_alias("/api/reviews/verdict"),
        ep(
            "POST",
            "/api/reviews/decision",
            "reviews",
            "Submit review-decision action // TODO: example",
        ),
        ep(
            "POST",
            "/api/review-decision",
            "reviews",
            "Deprecated alias for /api/reviews/decision",
        )
        .deprecated_alias("/api/reviews/decision"),
        ep(
            "POST",
            "/api/reviews/tuning/aggregate",
            "reviews",
            "Aggregate review-tuning outcomes // TODO: example",
        ),
        ep(
            "POST",
            "/api/review-tuning/aggregate",
            "reviews",
            "Deprecated alias for /api/reviews/tuning/aggregate",
        )
        .deprecated_alias("/api/reviews/tuning/aggregate"),
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
            ("force_recreate_active", body_param("boolean", false, "Advanced override for active old-channel launch artifacts")),
        ])
        .with_example(
            json!({"body": {"action": "confirm_promote", "evidence": "operator approved via Discord", "force_recreate_active": false}}),
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
pub async fn api_docs(Query(query): Query<ApiDocsQuery>) -> (StatusCode, Json<Value>) {
    let endpoints = all_endpoints();
    if query
        .format
        .as_deref()
        .is_some_and(|format| format.eq_ignore_ascii_case("flat"))
    {
        return (StatusCode::OK, Json(json!({ "endpoints": endpoints })));
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

    (StatusCode::OK, Json(json!({ "groups": groups })))
}

/// Core logic for the single-segment docs route, shared between the HTTP
/// handler and the in-process CLI helper. Returns `(status, headers, body)`.
fn resolve_docs_segment(segment: &str, flat: bool) -> (StatusCode, HeaderMap, Value) {
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
