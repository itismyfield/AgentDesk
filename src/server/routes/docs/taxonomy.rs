use serde::Serialize;
use std::collections::BTreeMap;

use super::inventory::EndpointDoc;

#[derive(Debug, Clone, Serialize)]
pub(super) struct CategorySummary {
    pub(super) name: &'static str,
    pub(super) count: usize,
    pub(super) description: &'static str,
    pub(super) subcategories: Vec<SubcategorySummary>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SubcategorySummary {
    pub(super) name: &'static str,
    pub(super) count: usize,
    pub(super) description: &'static str,
}

pub(super) const CANONICAL_CATEGORIES: [&str; 8] = [
    "agents",
    "kanban",
    "dispatches",
    "queue",
    "routines",
    "ops",
    "integrations",
    "admin",
];

pub(super) fn canonical_category(category: &str) -> &'static str {
    match category {
        "agents" => "agents",
        "kanban" | "kanban-repos" | "pipeline" | "pm" | "reviews" | "automation-candidates" => {
            "kanban"
        }
        "dispatches" | "dispatched-sessions" | "internal" | "messages" | "sessions" => "dispatches",
        "auto-queue" | "cron" | "queue" => "queue",
        "routines" => "routines",
        "analytics" | "auth" | "cluster" | "docs" | "health" | "monitoring" | "stats" | "v1"
        | "provider-cli" | "claude-accounts" => "ops",
        "discord" | "github" | "github-dashboard" | "meetings" => "integrations",
        "departments" | "memory" | "offices" | "onboarding" | "policies" | "settings"
        | "skills" => "admin",
        _ => "ops",
    }
}

pub(super) fn is_canonical_category(category: &str) -> bool {
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

pub(super) const GROUP_NAMES: [&str; 8] = [
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
pub(super) fn category_to_group(category: &str) -> &'static str {
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
        "auto-queue" | "automation-candidates" | "queue" | "cron" | "policies" | "routines" => {
            "automation"
        }
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
pub(super) fn group_description(group: &str) -> &'static str {
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
pub(super) fn effective_category(endpoint: &EndpointDoc) -> &'static str {
    endpoint.subcategory.unwrap_or(endpoint.category)
}

/// For a given top-level group, return the list of distinct fine-grained
/// categories under it plus their endpoint counts, in a deterministic order.
pub(super) fn categories_for_group(
    endpoints: &[EndpointDoc],
    group: &str,
) -> Vec<(&'static str, usize)> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for endpoint in endpoints {
        let category = effective_category(endpoint);
        if category_to_group(category) == group {
            *counts.entry(category).or_default() += 1;
        }
    }
    counts.into_iter().collect()
}

pub(super) fn category_description(category: &str) -> &'static str {
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
        "automation-candidates" => {
            "Loop-enabled automation candidate cards, iteration results, worktrees, and final gates."
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
        "claude-accounts" => {
            "Claude account usage listing (cswap) and operator-initiated global auth switch."
        }
        "reviews" => "Review verdict submission, decisions, and tuning aggregation.",
        "routines" => "Durable script-backed routines, run history, and manual routine controls.",
        "sessions" => "Sessions, tmux cleanup, force-kill, and termination events.",
        "settings" => "Settings surfaces, live overrides, precedence, and onboarding contracts.",
        "skills" => "Skill catalog and usage ranking.",
        "stats" => "Aggregate system counters.",
        "v1" => "Versioned dashboard read models and compatibility settings endpoints.",
        _ => "Miscellaneous API endpoints.",
    }
}

pub(super) fn category_summaries(endpoints: &[EndpointDoc]) -> Vec<CategorySummary> {
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

pub(super) fn subcategory_summaries(
    endpoints: &[EndpointDoc],
    category: &str,
) -> Vec<SubcategorySummary> {
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
