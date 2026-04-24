use axum::{
    Json,
    extract::{Path, Query},
    http::StatusCode,
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
        self.example = Some(ExampleDoc { request, response });
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
        "analytics" | "auth" | "docs" | "health" | "monitoring" | "stats" => "ops",
        "discord" | "github" | "github-dashboard" | "meetings" => "integrations",
        "departments" | "offices" | "onboarding" | "policies" | "settings" | "skills" => "admin",
        _ => "ops",
    }
}

fn is_canonical_category(category: &str) -> bool {
    CANONICAL_CATEGORIES.contains(&category)
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
        "messages" => "Message log read/write APIs.",
        "monitoring" => "Channel monitoring status entries and rendered status updates.",
        "offices" => "Office CRUD, ordering, and agent membership.",
        "onboarding" => "Initial setup, provider validation, and prompt generation.",
        "pipeline" => "Pipeline stages, config overrides, graphs, and card history.",
        "pm" => "PM decision workflow for force-only pipeline states.",
        "policies" => "Loaded policy inventory.",
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
                "db": true,
                "dashboard": true,
                "deferred_hooks": 0,
                "queue_depth": 0,
                "watcher_count": 0,
                "outbox_age": 0,
                "recovery_duration": 0.12
            }),
        ),
        ep(
            "POST",
            "/api/discord/send",
            "discord",
            "Send a Discord channel message",
        ),
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
        ]),
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
        ),
        ep("POST", "/api/senddm", "discord", "Deprecated alias for /api/discord/send-dm")
            .deprecated_alias("/api/discord/send-dm"),
        ep("GET", "/api/agents", "agents", "List all agents"),
        ep("POST", "/api/agents", "agents", "Create an agent"),
        ep("GET", "/api/agents/{id}", "agents", "Get agent by ID"),
        ep("PATCH", "/api/agents/{id}", "agents", "Update agent"),
        ep("DELETE", "/api/agents/{id}", "agents", "Delete agent"),
        ep(
            "GET",
            "/api/onboarding/status",
            "onboarding",
            "Get onboarding status",
        ),
        ep(
            "GET",
            "/api/onboarding/draft",
            "onboarding",
            "Get onboarding resume draft",
        ),
        ep(
            "PUT",
            "/api/onboarding/draft",
            "onboarding",
            "Persist onboarding resume draft",
        ),
        ep(
            "DELETE",
            "/api/onboarding/draft",
            "onboarding",
            "Clear onboarding resume draft",
        ),
        ep(
            "POST",
            "/api/onboarding/validate-token",
            "onboarding",
            "Validate onboarding token",
        ),
        ep(
            "GET",
            "/api/onboarding/channels",
            "onboarding",
            "List onboarding candidate channels",
        ),
        ep(
            "POST",
            "/api/onboarding/channels",
            "onboarding",
            "Persist onboarding channel selection",
        ),
        ep(
            "POST",
            "/api/onboarding/complete",
            "onboarding",
            "Complete onboarding",
        ),
        ep(
            "POST",
            "/api/onboarding/check-provider",
            "onboarding",
            "Validate provider installation and credentials",
        ),
        ep(
            "POST",
            "/api/onboarding/generate-prompt",
            "onboarding",
            "Generate onboarding prompt",
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
                    .with_enum(&["claude", "codex", "gemini", "qwen"]),
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
        ),
        ep(
            "GET",
            "/api/agents/{id}/offices",
            "agents",
            "List offices for agent",
        ),
        ep(
            "POST",
            "/api/agents/{id}/signal",
            "agents",
            "Send runtime signal to agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/cron",
            "agents",
            "List cron jobs for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/skills",
            "agents",
            "List skills for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/dispatched-sessions",
            "agents",
            "List dispatched sessions for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/turn",
            "agents",
            "Get active turn status and recent output",
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
        ),
        ep(
            "POST",
            "/api/agents/{id}/turn/stop",
            "agents",
            "Stop the active turn for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/transcripts",
            "agents",
            "List recent completed turn transcripts for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/timeline",
            "agents",
            "Agent activity timeline",
        ),
        ep("GET", "/api/sessions", "sessions", "List sessions"),
        ep("GET", "/api/policies", "policies", "List policies"),
        ep(
            "GET",
            "/api/auth/session",
            "auth",
            "Get current auth session",
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
            ),
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
            ),
        ep(
            "GET",
            "/api/kanban-cards/stalled",
            "kanban",
            "List stalled cards",
        ),
        ep(
            "POST",
            "/api/kanban-cards/bulk-action",
            "kanban",
            "Apply the same action to multiple cards",
        )
        .with_params([
            (
                "action",
                body_param("string", true, "Bulk action name")
                    .with_enum(&["pass", "reset", "cancel", "transition"]),
            ),
            (
                "card_ids",
                body_param("string[]", true, "Target card ids"),
            ),
            (
                "target_status",
                body_param(
                    "string",
                    false,
                    "Required when action=transition; target pipeline status",
                ),
            ),
        ])
        .with_example(
            json!({"body": {"action": "transition", "card_ids": ["card-1", "card-2"], "target_status": "ready"}}),
            json!({"action": "transition", "results": [{"id": "card-1", "ok": true}, {"id": "card-2", "ok": true}]}),
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
        ),
        ep("GET", "/api/kanban-cards/{id}", "kanban", "Get card by ID")
            .with_params([("id", path_param("Kanban card ID"))])
            .with_example(
                json!({"path": {"id": "card-1"}}),
                json!({"card": {"id": "card-1", "title": "Fix docs", "status": "ready"}}),
            ),
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
            ),
        ep(
            "DELETE",
            "/api/kanban-cards/{id}",
            "kanban",
            "Delete card",
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
            "/api/re-review",
            "kanban",
            "Deprecated alias for /api/kanban-cards/batch-rereview",
        )
        .deprecated_alias("/api/kanban-cards/batch-rereview"),
        ep(
            "POST",
            "/api/kanban-cards/batch-transition",
            "kanban",
            "Force-transition multiple cards",
        )
        .with_params([
            (
                "issue_numbers",
                body_param("number[]", false, "GitHub issue numbers to resolve into cards"),
            ),
            (
                "card_ids",
                body_param("string[]", false, "Explicit card ids to transition"),
            ),
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
            json!({"body": {"card_ids": ["card-1", "card-2"], "status": "ready", "cancel_dispatches": true}}),
            json!({"results": [{"card_id": "card-1", "ok": true, "from": "in_progress", "to": "ready", "cancelled_dispatches": 1, "skipped_auto_queue_entries": 1}]}),
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/reopen",
            "kanban",
            "Reopen a terminal card",
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
        ),
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
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/force-transition",
            "kanban",
            "Deprecated alias for /api/kanban-cards/{id}/transition",
        )
        .deprecated_alias("/api/kanban-cards/{id}/transition"),
        ep("POST", "/api/kanban-cards/{id}/retry", "kanban", "Retry card")
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
            ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/redispatch",
            "kanban",
            "Cancel current dispatch and kick off a new one",
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
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/resume",
            "kanban",
            "Resume a stuck card by analyzing current state",
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
        ),
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
            "List reviews for card",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/review-state",
            "kanban",
            "Get canonical review-state record for card",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/audit-log",
            "kanban",
            "Get audit log for card",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-cards/{id}/comments",
            "kanban",
            "Get GitHub comments for linked card issue",
        )
        .with_params([("id", path_param("Kanban card ID"))]),
        ep(
            "GET",
            "/api/kanban-repos",
            "kanban-repos",
            "List kanban repos",
        ),
        ep(
            "POST",
            "/api/kanban-repos",
            "kanban-repos",
            "Create kanban repo",
        ),
        ep(
            "PATCH",
            "/api/kanban-repos/{owner}/{repo}",
            "kanban-repos",
            "Update kanban repo",
        ),
        ep(
            "DELETE",
            "/api/kanban-repos/{owner}/{repo}",
            "kanban-repos",
            "Delete kanban repo",
        ),
        ep(
            "PATCH",
            "/api/kanban-reviews/{id}/decisions",
            "reviews",
            "Update review decisions",
        ),
        ep(
            "POST",
            "/api/kanban-reviews/{id}/trigger-rework",
            "reviews",
            "Trigger rework for review",
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
            ),
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
        ),
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
        ),
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
        ),
        ep(
            "POST",
            "/api/internal/link-dispatch-thread",
            "internal",
            "Link dispatch to an existing Discord thread",
        ),
        ep(
            "GET",
            "/api/internal/card-thread",
            "internal",
            "Resolve thread metadata for a card",
        ),
        ep(
            "GET",
            "/api/internal/pending-dispatch-for-thread",
            "internal",
            "Find pending dispatch bound to a thread",
        ),
        ep(
            "GET",
            "/api/pipeline/stages",
            "pipeline",
            "List pipeline stages",
        ),
        ep(
            "PUT",
            "/api/pipeline/stages",
            "pipeline",
            "Replace all pipeline stages",
        ),
        ep(
            "DELETE",
            "/api/pipeline/stages",
            "pipeline",
            "Delete pipeline stages",
        ),
        ep(
            "GET",
            "/api/pipeline/cards/{cardId}",
            "pipeline",
            "Get card pipeline state",
        ),
        ep(
            "GET",
            "/api/pipeline/cards/{cardId}/history",
            "pipeline",
            "Get card transition history",
        ),
        ep(
            "GET",
            "/api/pipeline/cards/{cardId}/transcripts",
            "pipeline",
            "List completed turn transcripts linked to card dispatches",
        ),
        ep(
            "GET",
            "/api/pipeline/config/default",
            "pipeline",
            "Get default pipeline config",
        ),
        ep(
            "GET",
            "/api/pipeline/config/effective",
            "pipeline",
            "Get effective merged pipeline config",
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
            "Get repo pipeline override",
        ),
        ep(
            "PUT",
            "/api/pipeline/config/repo/{owner}/{repo}",
            "pipeline",
            "Set repo pipeline override",
        ),
        ep(
            "GET",
            "/api/pipeline/config/agent/{agent_id}",
            "pipeline",
            "Get agent pipeline override",
        ),
        ep(
            "PUT",
            "/api/pipeline/config/agent/{agent_id}",
            "pipeline",
            "Set agent pipeline override",
        ),
        ep(
            "GET",
            "/api/pipeline/config/graph",
            "pipeline",
            "Get pipeline graph",
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
        ep("GET", "/api/github/repos", "github", "List GitHub repos"),
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
        ),
        ep(
            "POST",
            "/api/issues",
            "github",
            "Deprecated alias for /api/github/issues/create",
        )
        .deprecated_alias("/api/github/issues/create"),
        ep("POST", "/api/github/repos", "github", "Register GitHub repo"),
        ep(
            "POST",
            "/api/github/repos/{owner}/{repo}/sync",
            "github",
            "Sync GitHub repo",
        ),
        ep(
            "GET",
            "/api/github-repos",
            "github-dashboard",
            "List GitHub repos for dashboard",
        ),
        ep(
            "GET",
            "/api/github-issues",
            "github-dashboard",
            "List GitHub issues for dashboard",
        ),
        ep(
            "PATCH",
            "/api/github-issues/{owner}/{repo}/{number}/close",
            "github-dashboard",
            "Close GitHub issue from dashboard",
        ),
        ep(
            "GET",
            "/api/github-closed-today",
            "github-dashboard",
            "List issues closed today",
        ),
        ep("GET", "/api/offices", "offices", "List offices"),
        ep("POST", "/api/offices", "offices", "Create office"),
        ep(
            "PATCH",
            "/api/offices/reorder",
            "offices",
            "Reorder offices",
        ),
        ep("PATCH", "/api/offices/{id}", "offices", "Update office"),
        ep("DELETE", "/api/offices/{id}", "offices", "Delete office"),
        ep(
            "POST",
            "/api/offices/{id}/agents",
            "offices",
            "Add agent to office",
        ),
        ep(
            "POST",
            "/api/offices/{id}/agents/batch",
            "offices",
            "Batch add agents to office",
        ),
        ep(
            "DELETE",
            "/api/offices/{id}/agents/{agentId}",
            "offices",
            "Remove agent from office",
        ),
        ep(
            "PATCH",
            "/api/offices/{id}/agents/{agentId}",
            "offices",
            "Update office agent",
        ),
        ep(
            "GET",
            "/api/departments",
            "departments",
            "List departments",
        ),
        ep(
            "POST",
            "/api/departments",
            "departments",
            "Create department",
        ),
        ep(
            "PATCH",
            "/api/departments/reorder",
            "departments",
            "Reorder departments",
        ),
        ep(
            "PATCH",
            "/api/departments/{id}",
            "departments",
            "Update department",
        ),
        ep(
            "DELETE",
            "/api/departments/{id}",
            "departments",
            "Delete department",
        ),
        ep("GET", "/api/stats", "stats", "Get system stats"),
        ep(
            "GET",
            "/api/stats/memento",
            "stats",
            "Get hourly Memento logical call counts and dedup hit rates",
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
            "List dispatched sessions",
        ),
        ep(
            "DELETE",
            "/api/dispatched-sessions/cleanup",
            "dispatched-sessions",
            "Delete stale dispatched sessions",
        ),
        ep(
            "DELETE",
            "/api/dispatched-sessions/gc-threads",
            "dispatched-sessions",
            "Garbage-collect orphaned thread sessions",
        ),
        ep(
            "PATCH",
            "/api/dispatched-sessions/{id}",
            "dispatched-sessions",
            "Update dispatched session",
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/webhook",
            "dispatched-sessions",
            "Session webhook",
        ),
        ep(
            "POST",
            "/api/hook/session",
            "dispatched-sessions",
            "Deprecated alias for /api/dispatched-sessions/webhook",
        )
        .deprecated_alias("/api/dispatched-sessions/webhook"),
        ep(
            "DELETE",
            "/api/dispatched-sessions/webhook",
            "dispatched-sessions",
            "Delete session webhook state",
        ),
        ep(
            "DELETE",
            "/api/hook/session",
            "dispatched-sessions",
            "Deprecated alias for /api/dispatched-sessions/webhook",
        )
        .deprecated_alias("/api/dispatched-sessions/webhook"),
        ep(
            "GET",
            "/api/dispatched-sessions/claude-session-id",
            "dispatched-sessions",
            "Resolve Claude session id by session key",
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/clear-stale-session-id",
            "dispatched-sessions",
            "Clear stale Claude session id",
        ),
        ep(
            "POST",
            "/api/dispatched-sessions/clear-session-id",
            "dispatched-sessions",
            "Clear Claude session id by session key",
        ),
        ep(
            "POST",
            "/api/sessions/{session_key}/force-kill",
            "sessions",
            "Force-kill session and optionally retry",
        ),
        ep(
            "GET",
            "/api/session-termination-events",
            "sessions",
            "List recorded session termination events",
        ),
        ep("GET", "/api/messages", "messages", "List messages"),
        ep("POST", "/api/messages", "messages", "Create message"),
        ep(
            "GET",
            "/api/discord-bindings",
            "discord",
            "List Discord bindings",
        ),
        ep(
            "GET",
            "/api/discord/channels/{id}/messages",
            "discord",
            "Read channel or thread messages",
        ),
        ep(
            "GET",
            "/api/discord/channels/{id}",
            "discord",
            "Get channel or thread info",
        ),
        ep(
            "POST",
            "/api/dm-reply/register",
            "discord",
            "Register DM reply handler",
        ),
        ep(
            "GET",
            "/api/round-table-meetings",
            "meetings",
            "List meetings",
        ),
        ep(
            "POST",
            "/api/round-table-meetings",
            "meetings",
            "Create or update meeting",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/start",
            "meetings",
            "Start meeting",
        ),
        ep(
            "GET",
            "/api/round-table-meetings/{id}",
            "meetings",
            "Get meeting by ID",
        ),
        ep(
            "DELETE",
            "/api/round-table-meetings/{id}",
            "meetings",
            "Delete meeting",
        ),
        ep(
            "PATCH",
            "/api/round-table-meetings/{id}/issue-repo",
            "meetings",
            "Update meeting issue repository",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues",
            "meetings",
            "Create meeting issues",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues/discard",
            "meetings",
            "Discard one meeting issue",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues/discard-all",
            "meetings",
            "Discard all meeting issues",
        ),
        ep("GET", "/api/skills/catalog", "skills", "List skill catalog").with_params([(
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
            "Skill usage ranking",
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
        ep("POST", "/api/skills/prune", "skills", "Preview or prune stale skill metadata")
            .with_params([(
                "dry_run",
                query_param(
                    "boolean",
                    false,
                    "When true, report stale skill ids without deleting skills rows",
                ),
            )]),
        ep("GET", "/api/cron-jobs", "cron", "List cron jobs"),
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
        ),
        ep(
            "POST",
            "/api/auto-queue/dispatch",
            "auto-queue",
            "Declaratively generate and optionally activate grouped auto-queue work",
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
        ),
        ep(
            "POST",
            "/api/auto-queue/activate",
            "auto-queue",
            "Deprecated alias for /api/auto-queue/dispatch-next",
        )
        .deprecated_alias("/api/auto-queue/dispatch-next"),
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
        ),
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
        ),
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
        ),
        ep(
            "POST",
            "/api/auto-queue/resume",
            "auto-queue",
            "Resume paused runs and dispatch next entries",
        )
        .with_example(json!({}), json!({"ok": true, "resumed_runs": 1, "dispatched": 1})),
        ep(
            "POST",
            "/api/auto-queue/cancel",
            "auto-queue",
            "Cancel active or paused runs and skip pending entries",
        )
        .with_example(
            json!({}),
            json!({"ok": true, "cancelled_entries": 3, "cancelled_runs": 1}),
        ),
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
            "List queue entries for a Discord channel",
        ),
        ep(
            "GET",
            "/api/dispatches/pending",
            "queue",
            "List pending dispatches",
        ),
        ep(
            "POST",
            "/api/dispatches/{id}/cancel",
            "queue",
            "Cancel a queued dispatch",
        ),
        ep(
            "POST",
            "/api/dispatches/cancel-all",
            "queue",
            "Cancel all queued dispatches",
        ),
        ep(
            "POST",
            "/api/turns/{channel_id}/cancel",
            "queue",
            "Cancel a live turn by channel",
        ),
        ep(
            "POST",
            "/api/turns/{channel_id}/extend-timeout",
            "queue",
            "Extend live turn timeout",
        ),
        ep(
            "POST",
            "/api/channels/{channel_id}/monitoring",
            "monitoring",
            "Create or update a channel monitoring status entry",
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
            "List channel monitoring status entries",
        )
        .with_params([("channel_id", path_param("Discord channel ID"))]),
        ep(
            "DELETE",
            "/api/channels/{channel_id}/monitoring/{key}",
            "monitoring",
            "Remove a channel monitoring status entry",
        )
        .with_params([
            ("channel_id", path_param("Discord channel ID")),
            ("key", path_param("Monitoring entry key")),
        ]),
        ep("GET", "/api/analytics", "analytics", "Observability counters and structured events")
            .with_params([
                (
                    "provider",
                    query_param("string", false, "Filter by provider id (claude/codex/gemini/qwen)"),
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
            "/api/quality/events",
            "analytics",
            "Agent quality raw event stream",
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
        ep("GET", "/api/streaks", "analytics", "Agent activity streaks"),
        ep("GET", "/api/achievements", "analytics", "Agent achievements"),
        ep(
            "GET",
            "/api/activity-heatmap",
            "analytics",
            "Activity heatmap by hour",
        ),
        ep("GET", "/api/audit-logs", "analytics", "Audit logs"),
        ep(
            "GET",
            "/api/machine-status",
            "analytics",
            "Machine online status",
        ),
        ep(
            "GET",
            "/api/rate-limits",
            "analytics",
            "Cached rate limits per provider",
        ),
        ep("GET", "/api/receipt", "analytics", "Latest usage receipt snapshot"),
        ep(
            "GET",
            "/api/token-analytics",
            "analytics",
            "Token dashboard analytics with daily trend, heatmap, and usage breakdowns",
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
            "/api/skills-trend",
            "analytics",
            "Skill usage trend by day",
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
            "List the seven canonical documentation groups or return the legacy flat endpoint list",
        )
        .with_params([(
            "format",
            query_param("string", false, "Use format=flat for the legacy endpoint array"),
        )])
        .with_example(
            json!({}),
            json!({"categories": [{"name": "queue", "count": 15, "description": "Auto-queue generation, activation, ordering, live queue views, and turn control."}]}),
        ),
        ep(
            "GET",
            "/api/docs/{category}",
            "docs",
            "Get detailed docs for one canonical category, including params and examples where available",
        )
        .with_params([(
            "category",
            path_param("Canonical docs category name such as queue, kanban, or dispatches"),
        )])
        .with_example(
            json!({"path": {"category": "dispatches"}}),
            json!({"category": "dispatches", "count": 5, "endpoints": [{"method": "POST", "path": "/api/dispatches", "category": "dispatches", "description": "Create dispatch"}]}),
        ),
        ep(
            "POST",
            "/api/reviews/verdict",
            "reviews",
            "Submit review verdict",
        ),
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
            "Submit review-decision action",
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
            "Aggregate review-tuning outcomes",
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

/// GET /api/docs — summary by canonical category, or legacy flat format when `?format=flat`.
pub async fn api_docs(Query(query): Query<ApiDocsQuery>) -> (StatusCode, Json<Value>) {
    let endpoints = all_endpoints();
    if query
        .format
        .as_deref()
        .is_some_and(|format| format.eq_ignore_ascii_case("flat"))
    {
        return (StatusCode::OK, Json(json!({ "endpoints": endpoints })));
    }

    (
        StatusCode::OK,
        Json(json!({
            "categories": category_summaries(&endpoints),
        })),
    )
}

/// GET /api/docs/{category} — detailed endpoints for one canonical category.
pub async fn api_docs_category(Path(category): Path<String>) -> (StatusCode, Json<Value>) {
    let endpoints = all_endpoints();
    let requested = category.as_str();
    let canonical = if is_canonical_category(requested) {
        Some(requested)
    } else {
        endpoints
            .iter()
            .find(|endpoint| endpoint.subcategory == Some(requested))
            .map(|endpoint| endpoint.category)
    };

    let Some(canonical) = canonical else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown docs category: {category}") })),
        );
    };

    let legacy_drilldown = requested != canonical;
    let matching: Vec<EndpointDoc> = if legacy_drilldown {
        endpoints
            .into_iter()
            .filter(|endpoint| endpoint.subcategory == Some(requested))
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
            Json(json!({ "error": format!("unknown docs category: {category}") })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "category": category.clone(),
            "canonical_category": canonical,
            "description": category_description(&category),
            "count": matching.len(),
            "deprecated": legacy_drilldown,
            "subcategories": subcategory_summaries(&matching, canonical),
            "endpoints": matching,
        })),
    )
}
