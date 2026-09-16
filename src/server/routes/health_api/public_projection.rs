//! What the UNAUTHENTICATED `/api/health` body is allowed to carry.
//!
//! `super::public_health_json` is an explicit allowlist: a key it does not name
//! is dropped. The two jobs that allowlist delegates live here — rewriting an
//! operator-chosen provider id out of `degraded_reasons` (#4386), and
//! re-projecting the expired-ledger vector (#5942) — because both are decisions
//! about disclosure rather than about assembling a response, and because
//! `health_api.rs` is a registered `shrink` giant (#4710) that #5942 r4 must not
//! grow.

/// #5942: re-project the expired reachability ledgers for the public body.
///
/// #5942 REMOVES three routine channels from `degraded_reasons` every day.
/// `/api/health` is the credential-free surface — `/api/health/detail` sits
/// under `protected_api_domain` — so without this projection those channels
/// would vanish from the unauthenticated body with no replacement signal at
/// all, which is the "hide the channel" outcome the design refuses. This vector
/// is what replaces them, so it lands on the same body in the same pass that
/// empties them out of the reasons.
///
/// ALWAYS returns an array, empty when the detail side published nothing. The
/// field's contract is "beside `degraded_reasons`, never inside it", and a
/// reader told to COUNT entries must not have to tell a missing key from an
/// empty one first.
///
/// Re-projected entry by entry rather than cloned, so the key can only ever
/// carry strings even if the detail side later publishes richer entries. It is
/// deliberately NOT fed into `degraded_reasons`, `degraded` or `status`:
/// counting expiries into the degraded axis is the saturation #5942 reported.
/// No new disclosure — an entry is
/// `relay_verdict_expired_{provider}_{channel_id}`, the shape
/// `degraded_reasons` already publishes verbatim for the non-expired verdicts
/// (`public_health_json_carries_the_relay_verdict_axis_onto_the_summary`).
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
/// `provider_probe::classify_provider`. Keep in sync with that producer.
const PROVIDER_BARE_REASONS: &[&str] = &[
    "disconnected",
    "restart_pending",
    "reconcile_in_progress",
    // #5449: the finite-obligation promotion of `reconcile_in_progress`, and the
    // standby role reason that predates it. A reason missing from this array is
    // flattened to `provider:unsupported` by the fail-closed sanitizer, which
    // makes exactly the states an operator needs to name unnameable in public
    // health.
    "reconcile_stalled",
    "gateway_standby",
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
/// #4386 round-2 defect: `<name>` is operator-controlled and — because a legacy
/// `bot_settings.json` `provider` value is preserved verbatim as
/// `ProviderKind::Unsupported(_)` — may itself contain `:`. A first-colon split
/// (`split_once`) leaves everything after the first colon in the "reason" tail,
/// leaking the rest of the name (`provider:prod-mini-01:customerA:disconnected`
/// -> `customerA` survives). A left-anchored "is the first segment a known id"
/// test is also bypassable (`provider:codex:leak:disconnected`). We therefore
/// anchor on the FIXED reason vocabulary from the RIGHT: the trailing 1-2
/// segments must match a known classification; everything before them is the
/// name, which is replaced WHOLESALE with `unsupported` unless it is exactly a
/// supported provider id (registry ids never contain `:`). Only the fixed reason
/// keyword and an all-digits count can survive, so no arbitrary name byte leaks.
/// Any unrecognized shape fails CLOSED to `provider:unsupported`.
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

/// #4382 / #4386-review defect 1: `degraded_reasons` embeds `provider:<name>:...`
/// where `<name>` can be an ARBITRARY, operator-chosen string — a legacy
/// `bot_settings.json` `provider` field is parsed via
/// `ProviderKind::from_str_or_unsupported`, which preserves the raw value as
/// `Unsupported(_)` and re-emits it verbatim. Copying reasons unredacted onto the
/// UNAUTHENTICATED public `/api/health` would leak internal identifiers/hostnames
/// (e.g. `provider:prod-mini-01:disconnected`), breaking the allowlist guarantee
/// the public projection is documented to uphold. Rewrite any `provider:<name>:`
/// whose `<name>` is not a known, supported provider id to `provider:unsupported:`
/// (see `sanitize_provider_reason` for the colon-safe, fail-closed parsing).
/// `/api/health/detail` (authenticated) keeps the verbatim reasons. The rewrite is
/// 1:1 so the `degraded <=> non-empty` invariant is preserved.
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
