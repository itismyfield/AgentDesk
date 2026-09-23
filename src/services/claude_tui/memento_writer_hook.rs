//! Local receipts for model-owned MCP writes, spanning Pre/Post hook processes.
//! No fragment content is retained and no remote memory is read or mutated.
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::services::memory::memento_writer_guard::{
    invalidate_writer_receipts, writer_fingerprint,
};

struct ReceiptStore<'a> {
    dir: &'a Path,
    endpoint: &'a str,
    identity: &'a str,
}

/// Only confirmed remember/reflect duplicates are write gates. Store mutations
/// always pass and invalidate old receipts, including on uncertain outcomes.
pub(super) fn observe(
    provider: &str,
    event: &str,
    session: &str,
    payload: &Value,
) -> Option<String> {
    let provider = provider.trim().to_ascii_lowercase();
    if !is_write(payload) || !matches!(provider.as_str(), "claude" | "codex") {
        return None;
    }
    if !event.eq_ignore_ascii_case("PreToolUse") && !event.eq_ignore_ascii_case("PostToolUse") {
        return None;
    }
    let Some(root) = crate::config::runtime_root() else {
        tracing::warn!("Memento writer receipt directory unavailable; allowing managed MCP write");
        return None;
    };
    // The model's managed MCP can use a different server/tenant from the
    // backend adapter. Unknown provider-global config must not false-dedup
    // a genuine fact against another server's receipts.
    let (endpoint, identity) = crate::services::mcp_config::managed_memento_hook_identity()?;
    let dir = root.join("state/memento-writer");
    observe_at(
        ReceiptStore {
            dir: &dir,
            endpoint: &endpoint,
            identity: &identity,
        },
        &provider,
        event,
        session,
        payload,
    )
}

fn tool_name(payload: &Value) -> String {
    payload
        .get("tool_name")
        .or_else(|| payload.get("toolName"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn is_mutation(payload: &Value) -> bool {
    matches!(
        tool_name(payload).as_str(),
        "mcp__memento__amend"
            | "memento.amend"
            | "memento/amend"
            | "mcp__memento__forget"
            | "memento.forget"
            | "memento/forget"
            | "mcp__memento__memory_consolidate"
            | "memento.memory_consolidate"
            | "memento/memory_consolidate"
            | "mcp__memento__session_rotate"
            | "memento.session_rotate"
            | "memento/session_rotate"
    )
}

fn is_write(payload: &Value) -> bool {
    let name = tool_name(payload);
    is_mutation(payload)
        || matches!(
            name.as_str(),
            "mcp__memento__remember"
                | "memento.remember"
                | "memento/remember"
                | "mcp__memento__reflect"
                | "memento.reflect"
                | "memento/reflect"
        )
}

fn observe_at(
    store: ReceiptStore<'_>,
    provider: &str,
    event: &str,
    session: &str,
    payload: &Value,
) -> Option<String> {
    if !is_write(payload) {
        return None;
    }
    let pre = event.eq_ignore_ascii_case("PreToolUse");
    if !pre && !event.eq_ignore_ascii_case("PostToolUse") {
        return None;
    }
    let result = process(store, provider, pre, session, payload);
    match result {
        Ok(reason) => reason.map(deny),
        // Local observation failure does not establish a duplicate. Preserve
        // normal writes instead of making disk/payload problems a tool ban.
        Err(error) => {
            tracing::warn!(%error, "Memento writer receipt observation failed; allowing MCP write");
            None
        }
    }
}

fn deny(reason: &str) -> String {
    // Claude and Codex (verified against rust-v0.155.1 output_parser.rs)
    // accept this contract with a nonempty permissionDecisionReason.
    json!({"hookSpecificOutput": {"hookEventName":"PreToolUse", "permissionDecision":"deny", "permissionDecisionReason":reason}}).to_string()
}

fn process(
    store: ReceiptStore<'_>,
    provider: &str,
    pre: bool,
    session: &str,
    payload: &Value,
) -> Result<Option<&'static str>, String> {
    let ReceiptStore {
        dir,
        endpoint,
        identity,
    } = store;
    if is_mutation(payload) {
        // Pre invalidates already confirmed writes; Post invalidates writes
        // confirmed while the mutation was in flight. Neither event denies.
        invalidate_writer_receipts(dir, endpoint, identity)?;
        return Ok(None);
    }
    let args = payload
        .get("tool_input")
        .or_else(|| payload.get("toolInput"))
        .ok_or("missing tool_input")?;
    let mut args = if let Some(text) = args.as_str() {
        serde_json::from_str(text).map_err(|_| "invalid tool_input")?
    } else {
        args.clone()
    };
    if !args.is_object() {
        return Err("invalid tool_input object".into());
    }
    let tool_id = payload
        .get("tool_use_id")
        .or_else(|| payload.get("toolUseId"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or("missing tool_use_id")?;
    if session.trim().is_empty() {
        return Err("missing session_id".into());
    }
    if args.get("scope").and_then(Value::as_str) == Some("session")
        && args
            .get("sessionId")
            .and_then(Value::as_str)
            .is_none_or(|id| id.trim().is_empty())
    {
        // Provider session is only fingerprint metadata; never rewrite the
        // dispatched MCP arguments. Equal facts in distinct sessions are new.
        args["sessionId"] = json!(["agentdesk-provider-session", provider, session]);
    }
    let mut key = writer_fingerprint(dir, endpoint, identity, &args)?;
    if tool_name(payload).ends_with("reflect") {
        key = format!("{:x}", Sha256::digest(format!("reflect:{key}").as_bytes()));
    }
    let owner = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!([provider, session, tool_id])).map_err(|e| e.to_string())?
        )
    );
    let pending = dir.join(format!("{key}.{owner}.pending"));
    let confirmed = dir.join(format!("{key}.confirmed"));
    if pre {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        // A PreToolUse callback does not prove dispatch: a later permission
        // decision or cancellation may prevent it. Only confirmed writes may
        // block the next invocation. Per-invocation observations preserve Post
        // correlation without permanently suppressing a genuinely new fact.
        if read_receipt(&confirmed)
            .ok()
            .is_some_and(|owner| valid_owner(&owner))
        {
            return Ok(Some(
                "This same Memento write payload was already stored successfully. No new knowledge is present. Skip this write; use amend only for a real correction or remember for genuinely new information.",
            ));
        }
        match create_receipt(&pending) {
            Ok(mut file) => {
                file.write_all(owner.as_bytes())
                    .and_then(|_| file.sync_all())
                    .map_err(|e| e.to_string())?;
                sync_receipt_directory(&pending)?;
                Ok(None)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    } else {
        // PostToolUse cannot claim another call's result, including an unrelated
        // tool/session or a blocked duplicate with a different invocation ID.
        if read_receipt(&pending).ok().as_deref() != Some(owner.as_str()) {
            return Ok(None);
        }
        let response = payload
            .get("tool_response")
            .or_else(|| payload.get("toolResponse"));
        if response.is_some_and(|response| successful_result(response, 0)) {
            // Publish only a complete, synced confirmation. A previously torn
            // receipt must be repairable by a later correlated successful call.
            // Concurrent successful owners can safely replace one another.
            confirm_receipt(dir, &confirmed, &owner)?;
        }
        Ok(None)
    }
}

fn confirm_receipt(dir: &Path, confirmed: &Path, owner: &str) -> Result<(), String> {
    let temporary = dir.join(format!(
        ".confirmation-{}-{:016x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut file = create_receipt(&temporary).map_err(|error| error.to_string())?;
    let result = file
        .write_all(owner.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, confirmed))
        .map_err(|error| error.to_string())
        .and_then(|()| sync_receipt_directory(confirmed));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn valid_owner(owner: &str) -> bool {
    owner.len() == 64 && owner.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn create_receipt(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn read_receipt(path: &Path) -> std::io::Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut result = String::new();
    options.open(path)?.take(65).read_to_string(&mut result)?;
    Ok(result)
}

/// On Unix, flushes the directory holding `entry` and that directory's parent;
/// elsewhere nothing is flushed.
fn sync_receipt_directory(entry: &Path) -> Result<(), String> {
    #[cfg(unix)]
    for child in std::iter::once(entry).chain(entry.parent()) {
        crate::services::discord::runtime_store::fsync_parent_dir(child)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn successful_result(value: &Value, depth: u8) -> bool {
    if depth > 4 {
        return false;
    }
    match value {
        Value::Object(map) => {
            if map.get("isError") == Some(&Value::Bool(true))
                || map.get("success") == Some(&Value::Bool(false))
                || map.get("error").is_some_and(|v| !v.is_null())
            {
                return false;
            }
            if let Some(result) = map.get("result") {
                return successful_result(result, depth + 1);
            }
            // A JSON-RPC request/response ID is not a stored fragment ID.
            if map.contains_key("jsonrpc") {
                return false;
            }
            if let Some(content) = map.get("content") {
                return successful_result(content, depth + 1);
            }
            map.get("success") == Some(&Value::Bool(true))
                || ["fragmentId", "fragment_id", "id"].iter().any(|key| {
                    map.get(*key).is_some_and(|v| {
                        v.as_u64().is_some_and(|id| id > 0)
                            || v.as_str().is_some_and(|id| !id.trim().is_empty())
                    })
                })
        }
        Value::Array(blocks) => {
            !blocks.is_empty()
                && blocks.iter().all(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .is_some_and(|v| successful_result(&v, depth + 1))
                })
        }
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .is_some_and(|v| successful_result(&v, depth + 1)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(id: &str, content: &str) -> Value {
        json!({"tool_name":"mcp__memento__remember", "tool_use_id":id, "tool_input":{"content":content,"topic":"family","type":"fact","workspace":"family"}})
    }
    fn run(dir: &Path, event: &str, payload: &Value) -> Option<String> {
        observe_at(
            ReceiptStore {
                dir: dir,
                endpoint: "http://memento.test/mcp",
                identity: "test-identity",
            },
            "codex",
            event,
            "session",
            payload,
        )
    }

    #[test]
    fn confirmed_duplicate_is_denied_but_new_fact_metadata_scope_and_amend_pass() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = payload("one", "Family meal is at 18:00.");
        assert!(run(dir.path(), "PreToolUse", &first).is_none());
        first["tool_response"] =
            json!({"content":[{"type":"text", "text":"{\"success\":true,\"id\":31}"}]});
        assert!(run(dir.path(), "PostToolUse", &first).is_none());
        let mut retry = payload("two", "Family meal is at 18:00.");
        assert!(
            run(dir.path(), "PreToolUse", &retry)
                .unwrap()
                .contains("already stored successfully")
        );
        retry["tool_input"]["content"] = json!("Family meal is at 19:00.");
        assert!(run(dir.path(), "PreToolUse", &retry).is_none());
        retry["tool_input"]["assertionStatus"] = json!("confirmed");
        assert!(run(dir.path(), "PreToolUse", &retry).is_none());
        retry["tool_input"]["workspace"] = json!("another-family");
        assert!(run(dir.path(), "PreToolUse", &retry).is_none());
        retry["tool_name"] = json!("mcp__memento__amend");
        assert!(run(dir.path(), "PreToolUse", &retry).is_none());
    }

    #[test]
    fn uncertain_results_and_unrelated_posts_never_claim_success_or_block_new_facts() {
        let dir = tempfile::tempdir().unwrap();
        let first = payload("one", "New fact");
        assert!(run(dir.path(), "PreToolUse", &first).is_none());
        let mut other = payload("two", "New fact");
        other["tool_response"] = json!({"success":true});
        run(dir.path(), "PostToolUse", &other);
        assert!(run(dir.path(), "PreToolUse", &other).is_none());
        other["tool_use_id"] = json!("one");
        other["tool_response"] = json!({"isError":true,"success":true});
        run(dir.path(), "PostToolUse", &other);
        assert!(run(dir.path(), "PreToolUse", &other).is_none());
        other["tool_response"] = json!({"success":true});
        observe_at(
            ReceiptStore {
                dir: dir.path(),
                endpoint: "http://memento.test/mcp",
                identity: "test-identity",
            },
            "codex",
            "PostToolUse",
            "other-session",
            &other,
        );
        assert!(run(dir.path(), "PreToolUse", &other).is_none());
    }

    #[test]
    fn direct_reflect_replay_is_blocked_and_new_summary_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let mut reflect = json!({"tool_name":"mcp__memento__reflect", "tool_use_id":"reflect-one", "tool_input":{"summary":"Confirmed family preference", "workspace":"family"}});
        assert!(run(dir.path(), "PreToolUse", &reflect).is_none());
        reflect["tool_response"] = json!({"success":true});
        run(dir.path(), "PostToolUse", &reflect);
        reflect["tool_use_id"] = json!("reflect-two");
        assert!(
            run(dir.path(), "PreToolUse", &reflect)
                .unwrap()
                .contains("already stored successfully")
        );
        reflect["tool_input"]["summary"] = json!("Confirmed family preference with a new fact");
        assert!(run(dir.path(), "PreToolUse", &reflect).is_none());
    }

    #[test]
    fn mutations_invalidate_old_and_in_flight_confirmations_and_always_pass() {
        for tool in [
            "mcp__memento__amend",
            "mcp__memento__forget",
            "mcp__memento__memory_consolidate",
            "mcp__memento__session_rotate",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut first = payload("first", "Family meal is at 18:00.");
            run(dir.path(), "PreToolUse", &first);
            first["tool_response"] = json!({"success":true});
            run(dir.path(), "PostToolUse", &first);
            assert!(
                run(
                    dir.path(),
                    "PreToolUse",
                    &payload("duplicate", "Family meal is at 18:00.")
                )
                .is_some()
            );
            // Mutation events need neither parsable args nor a success response:
            // they are never denied, and uncertainty must invalidate receipts.
            let mutation = json!({"tool_name":tool});
            assert!(run(dir.path(), "PreToolUse", &mutation).is_none());
            let mut during = payload("during-mutation", "Family meal is at 18:00.");
            assert!(run(dir.path(), "PreToolUse", &during).is_none());
            during["tool_response"] = json!({"success":true});
            run(dir.path(), "PostToolUse", &during);
            assert!(run(dir.path(), "PostToolUse", &mutation).is_none());
            // A delayed old completion cannot make a newer generation stale.
            run(dir.path(), "PostToolUse", &first);
            assert!(
                run(
                    dir.path(),
                    "PreToolUse",
                    &payload("after-mutation", "Family meal is at 18:00.")
                )
                .is_none()
            );
        }
    }

    #[test]
    fn session_scope_uses_effective_session_without_collapsing_new_scope_or_state() {
        let dir = tempfile::tempdir().unwrap();
        for explicit in [Value::Null, json!(""), json!("explicit-session")] {
            let mut first = payload(&format!("first-{explicit}"), "New family fact");
            first["tool_input"]["scope"] = json!("session");
            first["tool_input"]["sessionId"] = explicit.clone();
            run(dir.path(), "PreToolUse", &first);
            first["tool_response"] = json!({"success":true});
            run(dir.path(), "PostToolUse", &first);
            first["tool_use_id"] = json!(format!("duplicate-{explicit}"));
            assert!(run(dir.path(), "PreToolUse", &first).is_some());
            let other_session = observe_at(
                ReceiptStore {
                    dir: dir.path(),
                    endpoint: "http://memento.test/mcp",
                    identity: "test-identity",
                },
                "codex",
                "PreToolUse",
                "another-session",
                &first,
            );
            assert_eq!(
                other_session.is_some(),
                explicit == json!("explicit-session")
            );
            for (field, value) in [
                ("scope", "workspace"),
                ("assertionStatus", "confirmed"),
                ("resolutionStatus", "resolved"),
                ("content", "New family fact with a correction"),
            ] {
                let mut changed = first.clone();
                changed["tool_input"][field] = json!(value);
                assert!(run(dir.path(), "PreToolUse", &changed).is_none(), "{field}");
            }
            // Isolate loop cases with an actual supported mutation event.
            run(
                dir.path(),
                "PreToolUse",
                &json!({"tool_name":"memento.amend"}),
            );
        }
    }

    #[test]
    fn local_receipt_errors_missing_payload_and_content_whitespace_do_not_ban_writes() {
        let dir = tempfile::tempdir().unwrap();
        let blocked_directory = dir.path().join("not-a-directory");
        fs::write(&blocked_directory, "file").unwrap();
        assert!(
            run(
                &blocked_directory,
                "PreToolUse",
                &payload("one", "New family fact")
            )
            .is_none()
        );
        assert!(
            run(
                dir.path(),
                "PreToolUse",
                &json!({"tool_name":"memento.remember"})
            )
            .is_none()
        );
        let mut first = payload("first", "a  b");
        run(dir.path(), "PreToolUse", &first);
        first["tool_response"] = json!({"success":true});
        run(dir.path(), "PostToolUse", &first);
        assert!(run(dir.path(), "PreToolUse", &payload("changed-string", "a b")).is_none());
        assert!(
            run(
                dir.path(),
                "PreToolUse",
                &payload("exact-duplicate", "a  b")
            )
            .is_some()
        );
    }

    #[test]
    fn unconfirmed_concurrent_or_cancelled_calls_do_not_block_new_facts() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let allowed = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for index in 0..8 {
                let barrier = barrier.clone();
                let path = dir.path();
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    run(
                        path,
                        "PreToolUse",
                        &payload(&index.to_string(), "New family fact"),
                    )
                    .is_none()
                }));
            }
            handles
                .into_iter()
                .map(|handle| usize::from(handle.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(allowed, 8);
        assert!(
            run(
                dir.path(),
                "PreToolUse",
                &payload("restart", "New family fact")
            )
            .is_none()
        );
        let mut completed = payload("0", "New family fact");
        completed["tool_response"] = json!({"success":true});
        run(dir.path(), "PostToolUse", &completed);
        for id in ["0", "restart", "fresh"] {
            assert!(
                run(dir.path(), "PreToolUse", &payload(id, "New family fact"))
                    .unwrap()
                    .contains("already stored successfully")
            );
        }
        // Different actual servers and tenants must still accept new facts.
        assert!(
            observe_at(
                ReceiptStore {
                    dir: dir.path(),
                    endpoint: "http://other.test/mcp",
                    identity: "test-identity"
                },
                "codex",
                "PreToolUse",
                "session",
                &completed
            )
            .is_none()
        );
        assert!(
            observe_at(
                ReceiptStore {
                    dir: dir.path(),
                    endpoint: "http://memento.test/mcp",
                    identity: "another-tenant"
                },
                "codex",
                "PreToolUse",
                "session",
                &completed
            )
            .is_none()
        );
    }

    #[test]
    fn result_envelopes_do_not_promote_nested_content_or_errors() {
        assert!(!successful_result(&json!({"jsonrpc":"2.0","id":2}), 0));
        assert!(!successful_result(
            &json!({"jsonrpc":"2.0","id":2,"result":{"error":"not saved"}}),
            0
        ));
        assert!(!successful_result(
            &json!({"jsonrpc":"2.0","id":2,"error":{"code":-32603}}),
            0
        ));
        assert!(successful_result(&json!({"success":true,"id":42}), 0));
        assert!(!successful_result(&json!({"success":false,"id":42}), 0));
        assert!(!successful_result(
            &json!({"isError":true,"content":[{"text":"{\"success\":true}"}]}),
            0
        ));
        assert!(!successful_result(
            &json!({"fragment":{"content":"{\"success\":true}"}}),
            0
        ));
        assert!(!successful_result(
            &json!({"content":[{"text":"{\"success\":true}"},{"text":"{\"success\":false}"}]}),
            0
        ));
    }

    #[test]
    fn torn_confirmation_is_repaired_by_a_correlated_successful_post() {
        for torn in ["", "abc", "invalid-confirmation"] {
            let dir = tempfile::tempdir().unwrap();
            let mut call = payload("repair", "A confirmed new family fact");
            assert!(run(dir.path(), "PreToolUse", &call).is_none());
            let key = writer_fingerprint(
                dir.path(),
                "http://memento.test/mcp",
                "test-identity",
                &call["tool_input"],
            )
            .unwrap();
            let confirmed = dir.path().join(format!("{key}.confirmed"));
            fs::write(&confirmed, torn).unwrap();
            assert!(run(dir.path(), "PreToolUse", &call).is_none());
            call["tool_response"] = json!({"success":true});
            run(dir.path(), "PostToolUse", &call);
            assert!(valid_owner(&read_receipt(&confirmed).unwrap()));
            call["tool_use_id"] = json!("later-retry");
            assert!(
                run(dir.path(), "PreToolUse", &call)
                    .unwrap()
                    .contains("already stored successfully")
            );
            assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            }));
        }
    }
}
