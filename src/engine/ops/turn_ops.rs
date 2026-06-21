use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::PgPool;
use std::sync::{Mutex, Weak};

use crate::services::discord::health::HealthRegistry;

// ── Global registry handle ─────────────────────────────────────────────────
//
// Mirrors the `cancel_tombstones` global-pool pattern: a process-scoped
// `Weak<HealthRegistry>` set once at dcserver startup so the policy-engine
// thread (which has no async executor or access to AppState) can call
// `start_agent_handoff_turn` synchronously via the async bridge.
//
// `Weak` is intentional: the registry is owned by the Discord runtime.
// If it's gone (standalone mode / unit tests without Discord), the host fn
// returns `{ok:false, error:"Discord not available"}` without panicking.

static GLOBAL_HEALTH_REGISTRY: Mutex<Option<Weak<HealthRegistry>>> = Mutex::new(None);

/// Called once from dcserver setup (after both the engine and the registry
/// exist) to wire the JS bridge to the live registry.
pub fn set_global_health_registry(registry: &std::sync::Arc<HealthRegistry>) {
    if let Ok(mut slot) = GLOBAL_HEALTH_REGISTRY.lock() {
        *slot = Some(std::sync::Arc::downgrade(registry));
    }
}

fn get_health_registry() -> Option<std::sync::Arc<HealthRegistry>> {
    GLOBAL_HEALTH_REGISTRY
        .lock()
        .ok()?
        .as_ref()
        .and_then(Weak::upgrade)
}

// ── Host binding ───────────────────────────────────────────────────────────

pub(super) fn register_turn_ops<'js>(ctx: &Ctx<'js>, pg_pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let turn_obj = Object::new(ctx.clone())?;

    // agentdesk.turn.__startRaw(agentId, prompt, optionsJson) → json_string
    //
    // `optionsJson` is a serialised object with optional fields:
    //   channel_kind: "cc" | "cdx"  (default: "cc")
    //   from_agent_id: string        (default: "policy-engine")
    //   prefix: bool                 (default: true)
    //
    // Returns JSON that the JS wrapper parses into:
    //   { ok: true,  turn_id, to_agent_id, channel_id, channel_kind, status }
    //   { ok: false, error, status: "conflict" | "unavailable" | "error" }
    let pg = pg_pool;
    turn_obj.set(
        "__startRaw",
        Function::new(
            ctx.clone(),
            move |agent_id: String, prompt: String, options_json: String| -> String {
                start_turn_raw(pg.as_ref(), &agent_id, &prompt, &options_json)
            },
        )?,
    )?;

    ad.set("turn", turn_obj)?;

    // JS wrapper: agentdesk.turn.start(agentId, prompt, options?)
    ctx.eval::<(), _>(
        r#"
        (function() {
            agentdesk.turn.start = function(agentId, prompt, options) {
                var opts = options || {};
                var optJson = JSON.stringify({
                    channel_kind: opts.channel_kind || "cc",
                    from_agent_id: opts.from_agent_id || "policy-engine",
                    prefix: typeof opts.prefix === "boolean" ? opts.prefix : true
                });
                var raw = agentdesk.turn.__startRaw(
                    agentId  || "",
                    prompt   || "",
                    optJson
                );
                var result = JSON.parse(raw);
                if (!result.ok) throw new Error(result.error || "turn.start failed");
                return result;
            };
        })();
        "#,
    )?;

    Ok(())
}

// ── Synchronous implementation ─────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
struct TurnStartOptions {
    channel_kind: Option<String>,
    from_agent_id: Option<String>,
    prefix: Option<bool>,
}

fn start_turn_raw(
    pg_pool: Option<&PgPool>,
    agent_id: &str,
    prompt: &str,
    options_json: &str,
) -> String {
    let agent_id = agent_id.trim();
    if agent_id.is_empty() {
        return err_json("agent_id is required", "error");
    }
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return err_json("prompt is required", "error");
    }

    let opts: TurnStartOptions = serde_json::from_str(options_json).unwrap_or_default();

    let channel_kind_str = opts.channel_kind.as_deref().unwrap_or("cc");
    let channel_kind = match crate::services::discord::agent_handoff::AgentHandoffChannelKind::parse(
        Some(channel_kind_str),
    ) {
        Ok(kind) => kind,
        Err(e) => return err_json(&e.one_line(), "error"),
    };
    let from_agent_id = opts
        .from_agent_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("policy-engine");
    let prefix = opts.prefix.unwrap_or(true);

    let Some(pool) = pg_pool else {
        return err_json("postgres backend is unavailable", "unavailable");
    };

    let Some(registry) = get_health_registry() else {
        return err_json("Discord not available (standalone mode)", "unavailable");
    };

    // Run the async handoff synchronously via the bridge.
    let agent_id_owned = agent_id.to_string();
    let prompt_owned = prompt.to_string();
    let from_owned = from_agent_id.to_string();

    let pool_owned = pool.clone();
    let result = crate::utils::async_bridge::block_on_result(
        async move {
            crate::services::discord::agent_handoff::start_agent_handoff_turn(
                &registry,
                &pool_owned,
                &from_owned,
                &agent_id_owned,
                &prompt_owned,
                channel_kind,
                prefix,
                None, // expect_reply: None → no contract appended
                Some("js:turn.start".to_string()),
                None, // metadata
            )
            .await
            .map_err(|e| e.one_line())
        },
        |e: String| e,
    );

    match result {
        Ok(response) => {
            let mut v = response.to_value();
            v["ok"] = serde_json::Value::Bool(true);
            v.to_string()
        }
        Err(err_msg) => {
            // Detect conflict vs generic failure from the error string.
            let status = if err_msg.contains("conflict") || err_msg.contains("409") {
                "conflict"
            } else if err_msg.contains("unavailable") || err_msg.contains("503") {
                "unavailable"
            } else {
                "error"
            };
            err_json(&err_msg, status)
        }
    }
}

fn err_json(error: &str, status: &str) -> String {
    serde_json::json!({
        "ok": false,
        "error": error,
        "status": status,
    })
    .to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::PolicyEngine;

    fn test_engine() -> PolicyEngine {
        let config = Config {
            policies: crate::config::PoliciesConfig {
                dir: std::path::PathBuf::from("/nonexistent"),
                hot_reload: false,
                ..crate::config::PoliciesConfig::default()
            },
            ..Config::default()
        };
        PolicyEngine::new_with_pg(&config, None).unwrap()
    }

    // ── Registration check ───────────────────────────────────────────────

    #[test]
    fn turn_start_host_fn_is_registered() {
        let engine = test_engine();
        // agentdesk.turn.__startRaw must exist as a function
        let is_fn: bool = engine
            .eval_js(r#"typeof agentdesk.turn.__startRaw === "function""#)
            .unwrap();
        assert!(is_fn, "agentdesk.turn.__startRaw should be a function");

        // agentdesk.turn.start must exist as a function
        let is_fn2: bool = engine
            .eval_js(r#"typeof agentdesk.turn.start === "function""#)
            .unwrap();
        assert!(is_fn2, "agentdesk.turn.start should be a function");
    }

    // ── No-registry path ─────────────────────────────────────────────────

    #[test]
    fn turn_start_returns_error_without_registry() {
        let engine = test_engine();
        // Without a registry the raw fn returns a JSON error object (does not
        // panic and does not throw).
        let result: String = engine
            .eval_js(
                r#"JSON.stringify(
                    JSON.parse(
                        agentdesk.turn.__startRaw("some-agent", "hello", "{}")
                    )
                )"#,
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        // Must carry either "postgres" or "Discord not available" in the error.
        let error = v["error"].as_str().unwrap_or("");
        assert!(
            error.contains("postgres") || error.contains("Discord"),
            "unexpected error: {error}"
        );
    }

    // ── Argument validation ──────────────────────────────────────────────

    #[test]
    fn turn_start_raw_requires_agent_id() {
        let result = start_turn_raw(None, "", "prompt", "{}");
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["error"], "agent_id is required");
    }

    #[test]
    fn turn_start_raw_requires_prompt() {
        let result = start_turn_raw(None, "some-agent", "  ", "{}");
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["error"], "prompt is required");
    }

    #[test]
    fn turn_start_raw_rejects_invalid_channel_kind() {
        let opts = r#"{"channel_kind":"invalid"}"#;
        let result = start_turn_raw(None, "some-agent", "hello", opts);
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        let error = v["error"].as_str().unwrap_or("");
        assert!(
            error.contains("channel_kind"),
            "expected channel_kind error, got: {error}"
        );
    }
}
