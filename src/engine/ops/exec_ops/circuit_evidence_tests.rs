use super::*;
use serde_json::json;

fn seed(root: &std::path::Path) -> serde_json::Value {
    let evidence = json!({
        "version": 1, "evidence_id": "alpha-policy-evidence",
        "origin": {
            "episode_key": "original-policy-episode", "provider": "claude",
            "channel_id": 57153, "offset_authority_channel_id": 57153,
            "open_obligation_id": 41,
            "source": {"session_id": "session", "tmux_session_name": "fixture-session",
                       "output_path": "/tmp/alpha-output.jsonl", "spawn_identity": null},
            "delivery_authority": "watcher_lease", "binding": "Unresolved"
        },
        "range_start": 0, "confirmed_end_exclusive": 128, "producer": "watcher_direct"
    });
    let row = json!({
        "version": 7, "provider": "claude", "channel_id": 57153,
        "request_owner_user_id": 7, "user_msg_id": 11, "current_msg_id": 12,
        "current_msg_len": 6, "user_text": "answer", "last_offset": 128,
        "full_response": "answer", "response_sent_offset": 6,
        "started_at": "2026-10-14 00:00:00", "updated_at": "2026-10-14 00:00:00",
        "pending_circuit_evidence": [evidence.clone()]
    });
    std::fs::create_dir_all(root.join("claude")).unwrap();
    std::fs::write(
        root.join("claude/57153.json"),
        serde_json::to_vec_pretty(&row).unwrap(),
    )
    .unwrap();
    evidence
}

fn handed_off(root: &std::path::Path, evidence: &serde_json::Value) -> bool {
    [
        "discord_circuit_evidence",
        "discord_circuit_evidence_escrow",
    ]
    .into_iter()
    .any(|store| {
        let dir = root.parent().unwrap().join(store).join("claude/57153");
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                std::fs::read(entry.path())
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .as_ref()
                    == Some(evidence)
            })
        })
    })
}

#[test]
fn alpha_policy_handoffs_without_observer() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let root = temp.path().join("runtime/discord_inflight");
    let evidence = seed(&root);
    let hook_root = root.clone();
    let hook_evidence = evidence.clone();
    crate::services::discord::policy_op::unlink_hook::install(move |_| {
        assert!(
            handed_off(&hook_root, &hook_evidence),
            "policy unlink preceded handoff"
        );
    });
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        let agentdesk = Object::new(ctx.clone()).unwrap();
        ctx.globals().set("agentdesk", agentdesk).unwrap();
        register_exec_ops(&ctx).unwrap();
        for (provider, channel) in [("Claude", "57153"), ("claude", "057153")] {
            let invalid = root.join(provider).join(format!("{channel}.json"));
            std::fs::create_dir_all(invalid.parent().unwrap()).unwrap();
            // On a case-insensitive filesystem `Claude/` already is the real `claude/` row.
            if !invalid.exists() {
                std::fs::copy(root.join("claude/57153.json"), &invalid).unwrap();
            }
            let before = std::fs::read(&invalid).unwrap();
            let result: String = ctx
                .eval(format!(
                    "JSON.stringify(agentdesk.inflight.remove('{provider}','{channel}'))"
                ))
                .unwrap();
            let result: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(result["ok"], false, "{result}");
            assert_eq!(std::fs::read(invalid).unwrap(), before);
            assert!(!handed_off(&root, &evidence));
        }
        let result: String = ctx
            .eval("JSON.stringify(agentdesk.inflight.remove('claude',57153))")
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["ok"], true);
    });
    assert_eq!(
        crate::services::discord::policy_op::unlink_hook::attempts(),
        1
    );
    crate::services::discord::policy_op::unlink_hook::install(|_| {});
    assert!(!root.join("claude/57153.json").exists());
    assert!(
        handed_off(&root, &evidence),
        "policy remove lost the pending proof before any observer tick"
    );
}

#[test]
fn alpha_policy_storage_failure_is_not_ok() {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let root = temp.path().join("runtime/discord_inflight");
    seed(&root);
    let path = root.join("claude/57153.json");
    let before = std::fs::read(&path).unwrap();
    for store in [
        "discord_circuit_evidence",
        "discord_circuit_evidence_escrow",
    ] {
        std::fs::write(root.parent().unwrap().join(store), "blocked").unwrap();
    }
    crate::services::discord::policy_op::unlink_hook::install(|_| {});
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        ctx.globals()
            .set("agentdesk", Object::new(ctx.clone()).unwrap())
            .unwrap();
        register_exec_ops(&ctx).unwrap();
        let result: String = ctx
            .eval("JSON.stringify(agentdesk.inflight.remove('claude',57153))")
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["ok"], false);
        assert_eq!(result["reason"], "Io", "{result}");
        let detail = result["error"].as_str().unwrap();
        assert!(
            detail.contains("circuit journal:") && detail.contains("escrow:"),
            "{result}"
        );
        assert!(result.get("removed").is_none());
    });
    assert_eq!(
        crate::services::discord::policy_op::unlink_hook::attempts(),
        0
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn alpha_policy_receipt_and_busy_precede_handoff() {
    for blocked in [false, true] {
        for receipt in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let _env = crate::config::set_agentdesk_root_for_test(temp.path());
            let root = temp.path().join("runtime/discord_inflight");
            let evidence = seed(&root);
            let path = root.join("claude/57153.json");
            if receipt {
                let mut row: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                row["herdr_before_start_stop"] = json!({"protocol": 1, "notice": "pending"});
                std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
            }
            let before = std::fs::read(&path).unwrap();
            if blocked {
                for store in [
                    "discord_circuit_evidence",
                    "discord_circuit_evidence_escrow",
                ] {
                    std::fs::write(root.parent().unwrap().join(store), "blocked").unwrap();
                }
            }
            let holder = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path.with_extension("json.lock"))
                .unwrap();
            holder.lock().unwrap();
            let rt = rquickjs::Runtime::new().unwrap();
            let ctx = rquickjs::Context::full(&rt).unwrap();
            ctx.with(|ctx| {
                ctx.globals().set("agentdesk", Object::new(ctx.clone()).unwrap()).unwrap();
                register_exec_ops(&ctx).unwrap();
                let remove = || {
                    let result: String = ctx.eval("JSON.stringify(agentdesk.inflight.remove('claude',57153))").unwrap();
                    serde_json::from_str::<serde_json::Value>(&result).unwrap()
                };
                assert_eq!(remove(), json!({"ok": false, "error": "busy", "reason": "Busy"}));
                assert_eq!(std::fs::read(&path).unwrap(), before);
                assert!(!handed_off(&root, &evidence));
                drop(holder);
                if receipt {
                    assert_eq!(remove(), json!({"ok": false, "error": "protected receipt", "reason": "ProtectedReceipt"}));
                    assert_eq!(std::fs::read(&path).unwrap(), before);
                    assert!(!handed_off(&root, &evidence));
                }
            });
            assert!(path.with_extension("json.lock").exists());
        }
    }
}
