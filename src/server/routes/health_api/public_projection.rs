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
