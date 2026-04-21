use crate::services::process::{configure_child_process_group, wait_with_output_timeout};
use crate::supervisor::BridgeHandle;
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const DEFAULT_INVENTORY_REFRESH_TIMEOUT_MS: u64 = 60_000;

fn resolve_inventory_refresh_timeout_ms(timeout_ms: Option<u64>) -> u64 {
    timeout_ms
        .filter(|timeout_ms| *timeout_ms > 0)
        .unwrap_or(DEFAULT_INVENTORY_REFRESH_TIMEOUT_MS)
}

fn resolve_python3_path() -> OsString {
    std::env::var_os("AGENTDESK_PYTHON3_PATH")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("python3"))
}

fn resolve_inventory_script(worktree_path: &Path) -> Result<PathBuf, String> {
    if !worktree_path.is_dir() {
        return Err(format!(
            "inventory refresh worktree is missing: {}",
            worktree_path.display()
        ));
    }

    let script_path = worktree_path.join("scripts/generate_inventory_docs.py");
    if !script_path.is_file() {
        return Err(format!(
            "inventory generator missing: {}",
            script_path.display()
        ));
    }

    Ok(script_path)
}

fn refresh_inventory_docs_json(worktree_path: &str, timeout_ms: Option<u64>) -> String {
    let canonical_worktree = match std::fs::canonicalize(worktree_path) {
        Ok(path) => path,
        Err(error) => {
            return serde_json::json!({
                "ok": false,
                "error": format!("inventory refresh worktree resolve failed: {error}"),
                "worktree_path": worktree_path,
            })
            .to_string();
        }
    };

    let script_path = match resolve_inventory_script(&canonical_worktree) {
        Ok(path) => path,
        Err(error) => {
            return serde_json::json!({
                "ok": false,
                "error": error,
                "worktree_path": canonical_worktree.display().to_string(),
            })
            .to_string();
        }
    };

    let mut command = Command::new(resolve_python3_path());
    command
        .arg(&script_path)
        .current_dir(&canonical_worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_process_group(&mut command);

    let timeout_ms = resolve_inventory_refresh_timeout_ms(timeout_ms);
    let result = command
        .spawn()
        .map_err(|error| format!("Failed to start inventory refresh: {error}"))
        .and_then(|child| {
            wait_with_output_timeout(
                child,
                Duration::from_millis(timeout_ms),
                "inventory refresh",
            )
        });

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if output.status.success() {
                serde_json::json!({
                    "ok": true,
                    "stdout": stdout,
                    "stderr": stderr,
                    "worktree_path": canonical_worktree.display().to_string(),
                    "script_path": script_path.display().to_string(),
                })
                .to_string()
            } else {
                let error = if !stderr.is_empty() {
                    format!("inventory refresh failed: {stderr}")
                } else if !stdout.is_empty() {
                    format!("inventory refresh failed: {stdout}")
                } else {
                    "inventory refresh failed".to_string()
                };
                serde_json::json!({
                    "ok": false,
                    "error": error,
                    "stdout": stdout,
                    "stderr": stderr,
                    "worktree_path": canonical_worktree.display().to_string(),
                    "script_path": script_path.display().to_string(),
                })
                .to_string()
            }
        }
        Err(error) => serde_json::json!({
            "ok": false,
            "error": error,
            "worktree_path": canonical_worktree.display().to_string(),
            "script_path": script_path.display().to_string(),
        })
        .to_string(),
    }
}

pub(super) fn register_runtime_ops<'js>(
    ctx: &Ctx<'js>,
    db: Option<crate::db::Db>,
    pg_pool: Option<sqlx::PgPool>,
    bridge: BridgeHandle,
) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let runtime_obj = Object::new(ctx.clone())?;
    let bridge_for_signal = bridge.clone();

    runtime_obj.set(
        "__emitSignalRaw",
        Function::new(
            ctx.clone(),
            move |signal_name: String, evidence_json: String| -> String {
                crate::supervisor::emit_signal_json(
                    &bridge_for_signal,
                    &signal_name,
                    &evidence_json,
                )
            },
        )?,
    )?;
    let bridge_should_defer_signal = bridge.clone();
    runtime_obj.set(
        "__shouldDeferSignalRaw",
        Function::new(ctx.clone(), move || -> bool {
            should_defer_signal(&bridge_should_defer_signal)
        })?,
    )?;
    let db_for_retrospective = db.clone();
    let pg_for_retrospective = pg_pool.clone();
    runtime_obj.set(
        "__recordCardRetrospectiveRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, terminal_status: String| -> String {
                crate::services::retrospectives::record_card_retrospective_json(
                    db_for_retrospective.as_ref(),
                    pg_for_retrospective.as_ref(),
                    &card_id,
                    &terminal_status,
                )
            },
        )?,
    )?;
    runtime_obj.set(
        "__refreshInventoryDocsRaw",
        Function::new(
            ctx.clone(),
            move |worktree_path: String, timeout_ms: Option<u64>| -> String {
                refresh_inventory_docs_json(&worktree_path, timeout_ms)
            },
        )?,
    )?;

    ad.set("runtime", runtime_obj)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.runtime.emitSignal = function(signalName, evidence) {
                var normalizedSignal = signalName || "";
                var normalizedEvidence = evidence || {};
                if (agentdesk.runtime.__shouldDeferSignalRaw()) {
                    agentdesk.__pendingIntents = agentdesk.__pendingIntents || [];
                    agentdesk.__pendingIntents.push({
                        type: "emit_supervisor_signal",
                        signal_name: normalizedSignal,
                        evidence: normalizedEvidence
                    });
                    return {
                        ok: true,
                        deferred: true,
                        signal: normalizedSignal,
                        executed: false,
                        note: "deferred until hook completion"
                    };
                }
                var result = JSON.parse(
                    agentdesk.runtime.__emitSignalRaw(
                        normalizedSignal,
                        JSON.stringify(normalizedEvidence)
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
            agentdesk.runtime.recordCardRetrospective = function(cardId, terminalStatus) {
                return JSON.parse(
                    agentdesk.runtime.__recordCardRetrospectiveRaw(
                        cardId || "",
                        terminalStatus || ""
                    )
                );
            };
            agentdesk.runtime.refreshInventoryDocs = function(worktreePath, options) {
                var rawTimeout = null;
                if (typeof options === "number" && isFinite(options) && options > 0) {
                    rawTimeout = Math.floor(options);
                } else if (options && typeof options === "object") {
                    var candidate = options.timeout_ms;
                    if (!(typeof candidate === "number" && isFinite(candidate) && candidate > 0)) {
                        candidate = options.timeoutMs;
                    }
                    if (typeof candidate === "number" && isFinite(candidate) && candidate > 0) {
                        rawTimeout = Math.floor(candidate);
                    }
                }
                var result = JSON.parse(
                    agentdesk.runtime.__refreshInventoryDocsRaw(
                        worktreePath || "",
                        rawTimeout
                    )
                );
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}

fn should_defer_signal(bridge: &BridgeHandle) -> bool {
    bridge
        .upgrade_engine()
        .map(|engine| engine.is_actor_thread())
        .unwrap_or(false)
}
