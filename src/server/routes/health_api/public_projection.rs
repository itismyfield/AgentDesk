//! What the UNAUTHENTICATED `/api/health` body is allowed to carry.
//!
//! `super::public_health_json` is an explicit allowlist; the disclosure decisions it
//! delegates (provider-id redaction, the expired-ledger vector) live here.

/// Re-project the expired reachability ledgers for the public body.
///
/// Stands in for the channels kept out of `degraded_reasons`, so they do not vanish from
/// the credential-free body. Always an array, so a counting reader never has to tell a
/// missing key from an empty one. Strings only, and never fed into `degraded_reasons`,
/// `degraded` or `status`. The `relay_verdict_expired_{provider}_{channel_id}` shape is
/// already public for non-expired verdicts, so nothing new is disclosed.
pub(super) fn expired_relay_ledgers(json: &serde_json::Value) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = json
        .get("expired_relay_ledgers")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|entry| serde_json::Value::String(entry.to_string()))
                .collect()
        })
        .unwrap_or_default();
    serde_json::Value::Array(entries)
}

/// Applied TUI gateway restriction per provider as `<id>:<runtime_role>:<complete|incomplete>:<n>`,
/// the role evidence readiness needs; a report of this node's state, not a copy of the config.
pub(super) fn tui_output_gateway_channels(json: &serde_json::Value) -> Vec<serde_json::Value> {
    let supported = crate::services::provider::supported_provider_ids();
    let providers = json.get("providers").and_then(serde_json::Value::as_array);
    providers
        .into_iter()
        .flatten()
        .filter_map(|provider| {
            let channels = provider.get("tui_output_gateway_channels")?.as_u64()?;
            let name = provider.get("name")?.as_str()?;
            let role = provider.get("runtime_role")?.as_str()?;
            if !supported.contains(&name) || role.contains(':') {
                return None;
            }
            let complete = provider.get("runtime_state_complete") == Some(&true.into());
            let state = if complete { "complete" } else { "incomplete" };
            Some(serde_json::Value::String(format!(
                "{name}:{role}:{state}:{channels}"
            )))
        })
        .collect()
}

/// Bare (argument-less) provider degraded-reason classifications emitted by
/// `provider_probe::classify_provider`. Keep in sync with that producer: a reason
/// missing here is flattened to `provider:unsupported` by the fail-closed sanitizer.
const PROVIDER_BARE_REASONS: &[&str] = &[
    "disconnected",
    "restart_pending",
    "reconcile_in_progress",
    "reconcile_stalled",
    "gateway_standby",
    "tui_output_requires_gateway",
];
/// Counted (`<keyword>:<N>`) provider degraded-reason classifications emitted by
/// `provider_probe::classify_provider`. Keep in sync with that producer.
const PROVIDER_COUNTED_REASONS: &[&str] = &[
    "deferred_hooks_backlog",
    "pending_queue_depth",
    "recovering_channels",
];

/// Sanitize one `provider:<name>:<reason>` string for public exposure.
///
/// `<name>` is operator-controlled and may contain `:`, so a first-colon split or a
/// known-id prefix check leaks part of it. Anchor on the fixed reason vocabulary from the
/// RIGHT instead: the name survives only if it is exactly a supported id (those never
/// contain `:`), else it is replaced wholesale with `unsupported`. Only the reason keyword
/// and an all-digits count can survive; any other shape fails closed to `provider:unsupported`.
fn sanitize_provider_reason(rest: &str, supported: &[&str]) -> String {
    let segments: Vec<&str> = rest.split(':').collect();
    // Counted reason: `<name...> : <keyword> : <digits>`.
    if segments.len() >= 3 {
        let count = segments[segments.len() - 1];
        let keyword = segments[segments.len() - 2];
        if !count.is_empty()
            && count.bytes().all(|b| b.is_ascii_digit())
            && PROVIDER_COUNTED_REASONS.contains(&keyword)
        {
            let name = segments[..segments.len() - 2].join(":");
            let reason = format!("{keyword}:{count}");
            return sanitized_provider_reason(&name, &reason, supported);
        }
    }
    // Bare reason: `<name...> : <keyword>`.
    if segments.len() >= 2 {
        let keyword = segments[segments.len() - 1];
        if PROVIDER_BARE_REASONS.contains(&keyword) {
            let name = segments[..segments.len() - 1].join(":");
            return sanitized_provider_reason(&name, keyword, supported);
        }
    }
    // Unknown / malformed shape: drop everything after `provider:` (fail closed).
    "provider:unsupported".to_string()
}

fn sanitized_provider_reason(name: &str, reason: &str, supported: &[&str]) -> String {
    if supported.contains(&name) {
        format!("provider:{name}:{reason}")
    } else {
        format!("provider:unsupported:{reason}")
    }
}

/// Redact operator-chosen provider ids (legacy `Unsupported(_)` values, which can be
/// hostnames) from `degraded_reasons` before they reach the unauthenticated body.
/// `/api/health/detail` keeps them verbatim. The rewrite is 1:1, so
/// `degraded <=> non-empty` still holds.
pub(super) fn sanitize_public_degraded_reasons(reasons: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Array(items) = reasons else {
        return serde_json::json!([]);
    };
    let supported = crate::services::provider::supported_provider_ids();
    let sanitized: Vec<serde_json::Value> = items
        .into_iter()
        .map(|item| {
            let Some(reason) = item.as_str() else {
                return item;
            };
            match reason.strip_prefix("provider:") {
                Some(rest) => serde_json::Value::String(sanitize_provider_reason(rest, &supported)),
                None => serde_json::Value::String(reason.to_string()),
            }
        })
        .collect();
    serde_json::Value::Array(sanitized)
}

pub(super) fn public_health_json(json: serde_json::Value) -> serde_json::Value {
    let status = json
        .get("status")
        .cloned()
        .unwrap_or_else(|| serde_json::json!("unknown"));
    let version = json
        .get("version")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(env!("CARGO_PKG_VERSION")));
    let db = json
        .get("db")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(false));
    let dashboard = json
        .get("dashboard")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(false));
    let server_up = json.get("server_up").cloned().unwrap_or_else(|| db.clone());
    let fully_recovered = json
        .get("fully_recovered")
        .cloned()
        .unwrap_or_else(|| server_up.clone());
    let cluster_standby = json
        .get("cluster_standby")
        .cloned()
        .unwrap_or_else(|| serde_json::json!(false));
    let startup_status = json.get("startup_status").cloned();
    let startup_degraded = json.get("startup_degraded").cloned();
    let startup_degraded_reasons = json.get("startup_degraded_reasons").cloned();
    let delivery_record_rollout = json.get("delivery_record_rollout").cloned();
    let release_source = json.get("release_source").cloned();
    let intake_routing = json.get("intake_routing").cloned();
    // Only credential-free signal of stranded cleanup. Field by field so new detail fields (run
    // id, error) cannot leak; kept out of `degraded_reasons`/`status` so it never moves `ok`.
    let auto_queue_cleanup = json.get("auto_queue_cleanup").map(|block| {
        serde_json::json!({
            "pending": block.get("pending").cloned().unwrap_or(serde_json::json!(0)),
            "dead_lettered": block
                .get("dead_lettered")
                .cloned()
                .unwrap_or(serde_json::json!(0)),
        })
    });
    // Count-only: never the per-server `warm_servers` array, pids, ports or startup tails.
    let opencode_public = json.get("opencode").map(|block| {
        serde_json::json!({
            "warm_server_count": block.get("warm_server_count").cloned().unwrap_or(serde_json::json!(0)),
            "warm_server_active_sessions": block.get("warm_server_active_sessions").cloned().unwrap_or(serde_json::json!(0)),
            "warm_server_suspicious_count": block.get("warm_server_suspicious_count").cloned().unwrap_or(serde_json::json!(0)),
        })
    });
    let degraded = status.as_str().is_some_and(|status| status != "healthy");
    // Always an array, so `degraded <=> non-empty degraded_reasons` holds on the public
    // shape and callers need not guess the cause from `startup_degraded_reasons`.
    let degraded_reasons = sanitize_public_degraded_reasons(
        json.get("degraded_reasons")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    );
    let expired_relay_ledgers = expired_relay_ledgers(&json);
    let tui_output_gateway_channels = tui_output_gateway_channels(&json);
    let mut public = serde_json::json!({
        "ok": !degraded,
        "status": status,
        "version": version,
        "db": db,
        "dashboard": dashboard,
        "server_up": server_up,
        "fully_recovered": fully_recovered,
        "cluster_standby": cluster_standby,
        "runtime_profile": json.get("runtime_profile"),
        "modules": json.get("modules"),
        "dashboard_required": json.get("dashboard_required"),
        "degraded": degraded,
        "degraded_reasons": degraded_reasons,
        "expired_relay_ledgers": expired_relay_ledgers,
    });
    if let Some(startup_status) = startup_status {
        public["startup_status"] = startup_status;
    }
    if let Some(startup_degraded) = startup_degraded {
        public["startup_degraded"] = startup_degraded;
    }
    if let Some(startup_degraded_reasons) = startup_degraded_reasons {
        public["startup_degraded_reasons"] = startup_degraded_reasons;
    }
    if let Some(delivery_record_rollout) = delivery_record_rollout {
        public["delivery_record_rollout"] = delivery_record_rollout;
    }
    if let Some(release_source) = release_source {
        public["release_source"] = release_source;
    }
    if let Some(intake_routing) = intake_routing {
        public["intake_routing"] = intake_routing;
    }
    if let Some(opencode_public) = opencode_public {
        public["opencode"] = opencode_public;
    }
    if let Some(auto_queue_cleanup) = auto_queue_cleanup {
        public["auto_queue_cleanup"] = auto_queue_cleanup;
    }
    if !tui_output_gateway_channels.is_empty() {
        public["tui_output_gateway_channels"] = tui_output_gateway_channels.into();
    }
    public
}

#[cfg(all(test, unix))]
#[path = "tui_output_readiness_tests.rs"]
mod tui_output_readiness_tests;
