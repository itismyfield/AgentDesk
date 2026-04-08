use axum::{Json, http::StatusCode};
use serde_json::{Value, json};

fn ep(method: &str, path: &str, category: &str, description: &str) -> Value {
    json!({"method": method, "path": path, "category": category, "description": description})
}

/// GET /api/docs — static listing of all API endpoints
pub async fn api_docs() -> (StatusCode, Json<Value>) {
    let endpoints: Vec<Value> = vec![
        // Health
        ep("GET", "/api/health", "health", "Health check"),
        // Agents
        ep("GET", "/api/agents", "agents", "List all agents"),
        ep("POST", "/api/agents", "agents", "Create an agent"),
        ep("GET", "/api/agents/{id}", "agents", "Get agent by ID"),
        ep("PATCH", "/api/agents/{id}", "agents", "Update agent"),
        ep("DELETE", "/api/agents/{id}", "agents", "Delete agent"),
        ep(
            "GET",
            "/api/agents/{id}/offices",
            "agents",
            "List offices for agent",
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
            "Get active turn status and recent output for agent",
        ),
        ep(
            "POST",
            "/api/agents/{id}/turn/stop",
            "agents",
            "Stop the active turn for agent",
        ),
        ep(
            "GET",
            "/api/agents/{id}/timeline",
            "agents",
            "Agent activity timeline",
        ),
        // Sessions
        ep("GET", "/api/sessions", "sessions", "List sessions"),
        ep(
            "GET",
            "/api/sessions/search?q=keyword",
            "sessions",
            "Search persisted session transcripts with optional Haiku summary",
        ),
        ep(
            "POST",
            "/api/sessions/{session_key}/force-kill",
            "sessions",
            "Force-kill session, clear inflight state, fail active dispatch, and optionally retry",
        ),
        // Policies
        ep("GET", "/api/policies", "policies", "List policies"),
        // Auth
        ep(
            "GET",
            "/api/auth/session",
            "auth",
            "Get current auth session",
        ),
        // Kanban
        ep("GET", "/api/kanban-cards", "kanban", "List kanban cards"),
        ep("POST", "/api/kanban-cards", "kanban", "Create kanban card"),
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
            "Bulk action on cards",
        ),
        ep(
            "POST",
            "/api/kanban-cards/assign-issue",
            "kanban",
            "Assign issue to card",
        ),
        ep("GET", "/api/kanban-cards/{id}", "kanban", "Get card by ID"),
        ep("PATCH", "/api/kanban-cards/{id}", "kanban", "Update card"),
        ep("DELETE", "/api/kanban-cards/{id}", "kanban", "Delete card"),
        ep(
            "POST",
            "/api/kanban-cards/{id}/assign",
            "kanban",
            "Assign card to agent",
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/retry",
            "kanban",
            "Retry card",
        ),
        ep(
            "POST",
            "/api/kanban-cards/{id}/redispatch",
            "kanban",
            "Redispatch card",
        ),
        ep(
            "PATCH",
            "/api/kanban-cards/{id}/defer-dod",
            "kanban",
            "Defer card DoD",
        ),
        ep(
            "GET",
            "/api/kanban-cards/{id}/reviews",
            "kanban",
            "List reviews for card",
        ),
        // Kanban repos
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
        // Reviews
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
        // Dispatches
        ep("GET", "/api/dispatches", "dispatches", "List dispatches"),
        ep(
            "POST",
            "/api/dispatches",
            "dispatches",
            "Create dispatch (supports optional skip_outbox for bookkeeping-only dispatches)",
        ),
        ep(
            "GET",
            "/api/dispatches/{id}",
            "dispatches",
            "Get dispatch by ID",
        ),
        ep(
            "PATCH",
            "/api/dispatches/{id}",
            "dispatches",
            "Update dispatch",
        ),
        // Pipeline (legacy)
        ep(
            "GET",
            "/api/pipeline-stages",
            "pipeline",
            "List pipeline stages (legacy)",
        ),
        ep(
            "POST",
            "/api/pipeline-stages",
            "pipeline",
            "Create pipeline stage (legacy)",
        ),
        ep(
            "DELETE",
            "/api/pipeline-stages/{id}",
            "pipeline",
            "Delete pipeline stage (legacy)",
        ),
        // Pipeline (v2)
        ep(
            "GET",
            "/api/pipeline/stages",
            "pipeline",
            "List pipeline stages (v2)",
        ),
        ep(
            "PUT",
            "/api/pipeline/stages",
            "pipeline",
            "Bulk replace pipeline stages (v2)",
        ),
        ep(
            "DELETE",
            "/api/pipeline/stages",
            "pipeline",
            "Delete pipeline stages (v2)",
        ),
        ep(
            "GET",
            "/api/pipeline/cards/{cardId}",
            "pipeline",
            "Get card pipeline state",
        ),
        // Pipeline config hierarchy (#135)
        ep(
            "GET",
            "/api/pipeline/config/default",
            "pipeline",
            "Get default pipeline config",
        ),
        ep(
            "GET",
            "/api/pipeline/config/effective?repo=...&agent_id=...",
            "pipeline",
            "Get effective (merged) pipeline for repo/agent",
        ),
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
            "/api/pipeline/config/graph?repo=...&agent_id=...",
            "pipeline",
            "Get pipeline as visual graph (nodes + edges)",
        ),
        // GitHub
        ep("GET", "/api/github/repos", "github", "List GitHub repos"),
        ep(
            "POST",
            "/api/github/repos",
            "github",
            "Register GitHub repo",
        ),
        ep(
            "POST",
            "/api/github/repos/{owner}/{repo}/sync",
            "github",
            "Sync GitHub repo",
        ),
        // GitHub dashboard
        ep(
            "GET",
            "/api/github-repos",
            "github-dashboard",
            "List GitHub repos (dashboard)",
        ),
        ep(
            "GET",
            "/api/github-issues",
            "github-dashboard",
            "List GitHub issues",
        ),
        ep(
            "PATCH",
            "/api/github-issues/{owner}/{repo}/{number}/close",
            "github-dashboard",
            "Close GitHub issue",
        ),
        ep(
            "GET",
            "/api/github-closed-today",
            "github-dashboard",
            "Issues closed today",
        ),
        // Offices
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
        // Departments
        ep("GET", "/api/departments", "departments", "List departments"),
        ep(
            "POST",
            "/api/departments",
            "departments",
            "Create department",
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
        // Stats
        ep("GET", "/api/stats", "stats", "Get stats"),
        // Settings
        ep(
            "GET",
            "/api/settings",
            "settings",
            "Get company settings JSON from kv_meta['settings']",
        ),
        ep(
            "PUT",
            "/api/settings",
            "settings",
            "Replace company settings JSON in kv_meta['settings'] (server strips retired legacy keys)",
        ),
        ep(
            "GET",
            "/api/settings/config",
            "settings",
            "Get whitelisted individual kv_meta policy/config keys",
        ),
        ep(
            "PATCH",
            "/api/settings/config",
            "settings",
            "Patch whitelisted individual kv_meta policy/config keys",
        ),
        ep(
            "GET",
            "/api/settings/runtime-config",
            "settings",
            "Get runtime config",
        ),
        ep(
            "PUT",
            "/api/settings/runtime-config",
            "settings",
            "Update runtime config",
        ),
        // Dispatched sessions
        ep(
            "GET",
            "/api/dispatched-sessions",
            "dispatched-sessions",
            "List dispatched sessions",
        ),
        ep(
            "PATCH",
            "/api/dispatched-sessions/{id}",
            "dispatched-sessions",
            "Update dispatched session",
        ),
        ep(
            "POST",
            "/api/hook/session",
            "dispatched-sessions",
            "Session webhook",
        ),
        // Messages
        ep("GET", "/api/messages", "messages", "List messages"),
        ep("POST", "/api/messages", "messages", "Create message"),
        // Discord
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
            "Read channel/thread messages (limit, before, after params)",
        ),
        ep(
            "GET",
            "/api/discord/channels/{id}",
            "discord",
            "Get channel/thread info",
        ),
        // Meetings
        ep(
            "GET",
            "/api/round-table-meetings",
            "meetings",
            "List meetings",
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
            "Update meeting issue repo",
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
            "Discard meeting issue",
        ),
        ep(
            "POST",
            "/api/round-table-meetings/{id}/issues/discard-all",
            "meetings",
            "Discard all meeting issues",
        ),
        // Skills
        ep("GET", "/api/skills/catalog", "skills", "List skill catalog"),
        ep(
            "GET",
            "/api/skills/ranking",
            "skills",
            "Skill usage ranking",
        ),
        // Cron
        ep("GET", "/api/cron-jobs", "cron", "List cron jobs"),
        // Auto-queue
        ep(
            "POST",
            "/api/auto-queue/generate",
            "auto-queue",
            "Generate auto-queue entries",
        ),
        ep(
            "POST",
            "/api/auto-queue/activate",
            "auto-queue",
            "Activate auto-queue",
        ),
        ep(
            "GET",
            "/api/auto-queue/status",
            "auto-queue",
            "Auto-queue status",
        ),
        ep(
            "PATCH",
            "/api/auto-queue/entries/{id}/skip",
            "auto-queue",
            "Skip auto-queue entry",
        ),
        ep(
            "PATCH",
            "/api/auto-queue/runs/{id}",
            "auto-queue",
            "Update auto-queue run",
        ),
        ep(
            "PATCH",
            "/api/auto-queue/reorder",
            "auto-queue",
            "Reorder auto-queue",
        ),
        ep(
            "POST",
            "/api/auto-queue/pause",
            "auto-queue",
            "Pause all active runs",
        ),
        ep(
            "POST",
            "/api/auto-queue/resume",
            "auto-queue",
            "Resume paused runs + dispatch next",
        ),
        ep(
            "POST",
            "/api/auto-queue/cancel",
            "auto-queue",
            "Cancel all active/paused runs",
        ),
        ep(
            "POST",
            "/api/auto-queue/reset",
            "auto-queue",
            "Reset auto-queue (delete all runs and entries)",
        ),
        ep(
            "POST",
            "/api/auto-queue/enqueue",
            "auto-queue",
            "Enqueue single issue by number (repo, issue_number, agent_id)",
        ),
        ep(
            "POST",
            "/api/auto-queue/runs/{id}/order",
            "auto-queue",
            "Submit ordered issue list for a pending run (order: [issue_numbers])",
        ),
        // Analytics
        ep("GET", "/api/streaks", "analytics", "Agent activity streaks"),
        ep(
            "GET",
            "/api/achievements",
            "analytics",
            "Agent achievements",
        ),
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
        ep(
            "GET",
            "/api/skills-trend",
            "analytics",
            "Skill usage trend by day",
        ),
        // Cancel dispatch + kill session
        ep(
            "POST",
            "/api/dispatch-cancel/{id}",
            "dispatches",
            "Cancel dispatch and kill associated agent session",
        ),
        // Resume
        ep(
            "POST",
            "/api/kanban-cards/{id}/resume",
            "kanban",
            "Resume stuck card from current state (accepts issue number or card ID)",
        ),
        // Docs
        ep("GET", "/api/docs", "docs", "API endpoint listing"),
        // Review verdict
        ep(
            "POST",
            "/api/review-verdict",
            "reviews",
            "Submit review verdict",
        ),
    ];

    (StatusCode::OK, Json(json!({"endpoints": endpoints})))
}
