//! `adk query` — first-class queue/dispatch/phase-gate inspector.
//!
//! Issue #2651. Replaces the ad-hoc
//!
//! ```text
//! curl http://localhost:8791/api/queue/status | python -c '
//!   import json,sys
//!   d=json.load(sys.stdin)
//!   ...
//! '
//! ```
//!
//! pattern observed ~56× per babysitter session by aggregating the three
//! commonly-polled endpoints (`/api/queue/status`,
//! `/api/dispatches/pending`, `/api/queue/phase-gates/catalog`) behind one
//! CLI surface with optional filters and a single JSON envelope.
//!
//! ## Output contract
//!
//! `--json` emits:
//!
//! ```json
//! {
//!   "queue":   { "run": {...}, "entries": [...] } | null,
//!   "dispatches": { "dispatches": [...], "count": N } | null,
//!   "phase_gate": { "catalog": [...] } | null,
//!   "errors":  { "queue": "...", ... }   // only present on partial failure
//! }
//! ```
//!
//! Sections the caller did not request (i.e. when invoking a single
//! subcommand) are omitted, not set to `null`, so downstream tooling can
//! `jq -e '.queue'` without surprises. Filters are applied *before*
//! rendering; `--limit > 0` truncates each list section after filtering.

use serde_json::Value;

use super::client;

#[derive(Clone, Copy, Debug)]
pub(crate) enum QuerySection {
    Queue,
    Dispatches,
    PhaseGate,
    All,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct QueryOptions {
    pub json: bool,
    /// Parsed key/value filters. Stored as `(key, value)` pairs to keep
    /// `--filter status:pending --filter dispatch_type:review` order-stable.
    pub filters: Vec<(String, String)>,
    pub agent: Option<String>,
    /// 0 means "no limit".
    pub limit: usize,
}

impl QueryOptions {
    pub fn from_raw(
        json: bool,
        filters: Vec<String>,
        agent: Option<String>,
        limit: usize,
    ) -> Result<Self, String> {
        let parsed = parse_filters(&filters)?;
        Ok(Self {
            json,
            filters: parsed,
            agent: agent
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            limit,
        })
    }
}

fn parse_filters(raw: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Support both `key:value` and `key=value` for ergonomics.
        let split = trimmed.split_once(':').or_else(|| trimmed.split_once('='));
        let Some((k, v)) = split else {
            return Err(format!(
                "invalid --filter '{trimmed}': expected key:value (e.g. status:pending)"
            ));
        };
        let key = k.trim();
        let value = v.trim();
        if key.is_empty() || value.is_empty() {
            return Err(format!(
                "invalid --filter '{trimmed}': key and value must both be non-empty"
            ));
        }
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

pub(crate) fn cmd_query(section: QuerySection, opts: QueryOptions) -> Result<(), String> {
    let mut result = serde_json::Map::new();
    let mut errors = serde_json::Map::new();

    let want_queue = matches!(section, QuerySection::Queue | QuerySection::All);
    let want_dispatches = matches!(section, QuerySection::Dispatches | QuerySection::All);
    let want_gate = matches!(section, QuerySection::PhaseGate | QuerySection::All);

    if want_queue {
        match fetch_queue(&opts) {
            Ok(value) => {
                result.insert("queue".to_string(), value);
            }
            Err(err) => {
                errors.insert("queue".to_string(), Value::String(err));
            }
        }
    }
    if want_dispatches {
        match fetch_dispatches(&opts) {
            Ok(value) => {
                result.insert("dispatches".to_string(), value);
            }
            Err(err) => {
                errors.insert("dispatches".to_string(), Value::String(err));
            }
        }
    }
    if want_gate {
        match fetch_phase_gate() {
            Ok(value) => {
                result.insert("phase_gate".to_string(), value);
            }
            Err(err) => {
                errors.insert("phase_gate".to_string(), Value::String(err));
            }
        }
    }

    if !errors.is_empty() {
        result.insert("errors".to_string(), Value::Object(errors.clone()));
    }

    // Surface failure semantics: if *every* requested section failed, return
    // an error so shells get a non-zero exit. Partial success is still Ok
    // (the JSON envelope carries `.errors` for the caller to inspect).
    let requested = (want_queue as usize) + (want_dispatches as usize) + (want_gate as usize);
    if requested > 0 && errors.len() >= requested {
        let msg = errors
            .iter()
            .map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or("error")))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("all query sections failed: {msg}"));
    }

    if opts.json {
        let payload = Value::Object(result);
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
        );
    } else {
        render_text(&result);
    }
    Ok(())
}

fn fetch_queue(opts: &QueryOptions) -> Result<Value, String> {
    let mut path = String::from("/api/queue/status");
    if let Some(agent) = opts.agent.as_deref() {
        path.push_str("?agent_id=");
        path.push_str(&urlencode(agent));
    }
    let raw = client::get_json(&path)?;
    let mut obj = raw
        .as_object()
        .cloned()
        .ok_or_else(|| "queue/status response was not a JSON object".to_string())?;

    if let Some(Value::Array(entries)) = obj.remove("entries") {
        let filtered = apply_filters(entries, &opts.filters);
        let truncated = truncate(filtered, opts.limit);
        obj.insert("entries".to_string(), Value::Array(truncated));
    }
    Ok(Value::Object(obj))
}

fn fetch_dispatches(opts: &QueryOptions) -> Result<Value, String> {
    let raw = client::get_json("/api/dispatches/pending")?;
    let mut obj = raw
        .as_object()
        .cloned()
        .ok_or_else(|| "dispatches/pending response was not a JSON object".to_string())?;

    if let Some(Value::Array(items)) = obj.remove("dispatches") {
        let filtered = apply_filters(items, &opts.filters);
        let truncated = truncate(filtered, opts.limit);
        let len = truncated.len();
        obj.insert("dispatches".to_string(), Value::Array(truncated));
        obj.insert("count".to_string(), Value::Number(len.into()));
    }
    Ok(Value::Object(obj))
}

fn fetch_phase_gate() -> Result<Value, String> {
    let raw = client::get_json("/api/queue/phase-gates/catalog")?;
    Ok(raw)
}

fn apply_filters(items: Vec<Value>, filters: &[(String, String)]) -> Vec<Value> {
    if filters.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|item| filters.iter().all(|(k, v)| value_field_matches(item, k, v)))
        .collect()
}

/// Match a row against `key=value`. Supported value forms on the row:
/// string / number / bool — all compared via their canonical string repr.
/// Dotted keys (`run.status`) walk nested objects.
fn value_field_matches(item: &Value, key: &str, expected: &str) -> bool {
    let mut cursor = item;
    for segment in key.split('.') {
        match cursor {
            Value::Object(map) => match map.get(segment) {
                Some(next) => cursor = next,
                None => return false,
            },
            _ => return false,
        }
    }
    let actual = match cursor {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => return false,
        other => other.to_string(),
    };
    actual.eq_ignore_ascii_case(expected)
}

fn truncate(items: Vec<Value>, limit: usize) -> Vec<Value> {
    if limit == 0 || items.len() <= limit {
        return items;
    }
    items.into_iter().take(limit).collect()
}

fn urlencode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

fn render_text(result: &serde_json::Map<String, Value>) {
    if let Some(queue) = result.get("queue") {
        render_queue(queue);
    }
    if let Some(dispatches) = result.get("dispatches") {
        render_dispatches(dispatches);
    }
    if let Some(gate) = result.get("phase_gate") {
        render_phase_gate(gate);
    }
    if let Some(errors) = result.get("errors").and_then(Value::as_object) {
        eprintln!();
        eprintln!("Partial failures:");
        for (key, err) in errors {
            eprintln!("  - {key}: {}", err.as_str().unwrap_or("error"));
        }
    }
}

fn render_queue(value: &Value) {
    println!("== Queue ==");
    let run = value.get("run");
    let status = run
        .and_then(|r| r.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("idle");
    let agent = run
        .and_then(|r| r.get("agent_id"))
        .and_then(Value::as_str)
        .unwrap_or("-");
    let unified = run
        .and_then(|r| r.get("unified_thread"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_threads = run
        .and_then(|r| r.get("max_concurrent_threads"))
        .and_then(Value::as_i64)
        .unwrap_or(1);
    println!("  Run: {status} | agent={agent} | unified={unified} | max_threads={max_threads}");

    let empty: Vec<Value> = Vec::new();
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    println!("  Entries ({}):", entries.len());
    for entry in entries {
        let num = entry
            .get("github_issue_number")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let status = entry.get("status").and_then(Value::as_str).unwrap_or("?");
        let title = entry
            .get("card_title")
            .and_then(Value::as_str)
            .unwrap_or("");
        let title_short: String = title.chars().take(60).collect();
        println!("    #{num:<5} {status:<14} {title_short}");
    }
}

fn render_dispatches(value: &Value) {
    println!("== Dispatches ==");
    let empty: Vec<Value> = Vec::new();
    let items = value
        .get("dispatches")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let count = value
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(items.len() as i64);
    println!("  Pending ({count}):");
    for d in items {
        let id = d.get("id").and_then(Value::as_str).unwrap_or("?");
        let dispatch_type = d
            .get("dispatch_type")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let status = d.get("status").and_then(Value::as_str).unwrap_or("?");
        let title = d.get("title").and_then(Value::as_str).unwrap_or("");
        let title_short: String = title.chars().take(50).collect();
        println!("    {id:<24} {dispatch_type:<12} {status:<12} {title_short}");
    }
}

fn render_phase_gate(value: &Value) {
    println!("== Phase gates ==");
    let empty: Vec<Value> = Vec::new();
    let catalog = value
        .get("catalog")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .unwrap_or(&empty);
    if catalog.is_empty() {
        println!("  (none)");
        return;
    }
    for gate in catalog {
        let kind = gate.get("kind").and_then(Value::as_str).unwrap_or("?");
        let summary = gate
            .get("summary")
            .and_then(Value::as_str)
            .or_else(|| gate.get("description").and_then(Value::as_str))
            .unwrap_or("");
        println!("  - {kind}: {summary}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_colon_filter() {
        let opts =
            QueryOptions::from_raw(false, vec!["status:pending".to_string()], None, 0).unwrap();
        assert_eq!(opts.filters, vec![("status".into(), "pending".into())]);
    }

    #[test]
    fn parses_equals_filter() {
        let opts =
            QueryOptions::from_raw(false, vec!["status=running".to_string()], None, 0).unwrap();
        assert_eq!(opts.filters, vec![("status".into(), "running".into())]);
    }

    #[test]
    fn rejects_malformed_filter() {
        let err = QueryOptions::from_raw(false, vec!["bogus".to_string()], None, 0)
            .err()
            .expect("should reject");
        assert!(err.contains("invalid --filter"));
    }

    #[test]
    fn rejects_empty_key_or_value() {
        assert!(QueryOptions::from_raw(false, vec![":x".into()], None, 0).is_err());
        assert!(QueryOptions::from_raw(false, vec!["k:".into()], None, 0).is_err());
    }

    #[test]
    fn filters_match_string_and_number() {
        let row = json!({"status": "pending", "retry_count": 3});
        assert!(value_field_matches(&row, "status", "pending"));
        assert!(value_field_matches(&row, "status", "PENDING")); // case-insensitive
        assert!(value_field_matches(&row, "retry_count", "3"));
        assert!(!value_field_matches(&row, "status", "running"));
        assert!(!value_field_matches(&row, "missing", "x"));
    }

    #[test]
    fn filters_walk_nested_objects() {
        let row = json!({"run": {"status": "active"}});
        assert!(value_field_matches(&row, "run.status", "active"));
        assert!(!value_field_matches(&row, "run.status", "idle"));
    }

    #[test]
    fn truncate_respects_limit() {
        let items = vec![json!(1), json!(2), json!(3)];
        assert_eq!(truncate(items.clone(), 0).len(), 3);
        assert_eq!(truncate(items.clone(), 5).len(), 3);
        assert_eq!(truncate(items, 2).len(), 2);
    }

    #[test]
    fn urlencode_escapes_unsafe() {
        assert_eq!(urlencode("foo bar/baz"), "foo%20bar%2Fbaz");
        assert_eq!(urlencode("agent-id_1.2~3"), "agent-id_1.2~3");
    }

    #[test]
    fn apply_filters_no_filters_returns_input() {
        let items = vec![json!({"status": "a"}), json!({"status": "b"})];
        let out = apply_filters(items.clone(), &[]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn apply_filters_drops_non_matching() {
        let items = vec![
            json!({"status": "pending"}),
            json!({"status": "running"}),
            json!({"status": "pending"}),
        ];
        let filters = vec![("status".to_string(), "pending".to_string())];
        let out = apply_filters(items, &filters);
        assert_eq!(out.len(), 2);
    }
}
