use rquickjs::{Ctx, Function, Object, Result as JsResult};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};

use crate::domain::agent_handoff::AgentHandoffChannelKind;
use crate::services::engine_runtime::health_port::{
    RuntimeRegistryPort, published_runtime_registry,
};

pub(crate) mod handoff_port;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

// ── Main runtime handle ─────────────────────────────────────────────────────
//
// #3587: the policy-engine thread is a plain std::thread with no tokio runtime,
// so driving the handoff through the generic async bridge builds a *throwaway*
// runtime. `start_agent_handoff_turn` spawns the turn's streaming/bridge/
// watchdog tasks onto the current runtime and returns immediately — but the
// throwaway runtime is dropped the moment `block_on` returns, aborting those
// tasks (JS would see `{ok:true}` for a turn that never actually runs).
//
// To fix this we drive the future on the *main* runtime handle (captured at
// dcserver startup), which lives for the whole process, so the spawned tasks
// survive after this synchronous call returns.
static GLOBAL_RUNTIME_HANDLE: Mutex<Option<tokio::runtime::Handle>> = Mutex::new(None);

/// Called once from dcserver setup (from within the async runtime) so the
/// policy-engine thread can drive turn-start on the durable main runtime.
pub fn set_global_runtime_handle(handle: tokio::runtime::Handle) {
    if let Ok(mut slot) = GLOBAL_RUNTIME_HANDLE.lock() {
        *slot = Some(handle);
    }
}

fn get_runtime_handle() -> Option<tokio::runtime::Handle> {
    GLOBAL_RUNTIME_HANDLE.lock().ok()?.clone()
}

// ── Host binding ───────────────────────────────────────────────────────────

pub(super) fn register_turn_ops<'js>(ctx: &Ctx<'js>, pg_pool: Option<PgPool>) -> JsResult<()> {
    register_turn_ops_with(ctx, pg_pool, published_runtime_registry, get_runtime_handle)
}

fn register_turn_ops_with<'js>(
    ctx: &Ctx<'js>,
    pg_pool: Option<PgPool>,
    registry: impl Fn() -> Option<Arc<dyn RuntimeRegistryPort>> + Send + 'static,
    runtime: impl Fn() -> Option<tokio::runtime::Handle> + Send + 'static,
) -> JsResult<()> {
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
    //   { ok: true,  status: "dispatched", to_agent_id }   (turn started async)
    //   { ok: false, error, status: "unavailable" | "error" }   (sync validation)
    // The turn is dispatched fire-and-forget onto the main runtime; its outcome
    // (started/conflict/error) is logged, not returned synchronously (#3587).
    let pg = pg_pool;
    turn_obj.set(
        "__startRaw",
        Function::new(
            ctx.clone(),
            move |agent_id: String, prompt: String, options_json: String| -> String {
                start_turn_raw(
                    pg.as_ref(),
                    &agent_id,
                    &prompt,
                    &options_json,
                    &registry,
                    &runtime,
                )
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
                if (!result.ok) {
                    var err = new Error(result.error || "turn.start failed");
                    // Preserve the structured status (conflict/unavailable/
                    // timeout/error) so callers can branch instead of regex-ing
                    // the message.
                    err.status = result.status;
                    throw err;
                }
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
    registry: impl FnOnce() -> Option<Arc<dyn RuntimeRegistryPort>>,
    runtime: impl FnOnce() -> Option<tokio::runtime::Handle>,
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

    let channel_kind = match AgentHandoffChannelKind::parse(opts.channel_kind.as_deref()) {
        Ok(kind) => kind,
        Err(error) => return err_json(error, "error"),
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

    let Some(registry) = registry() else {
        return err_json("Discord not available (standalone mode)", "unavailable");
    };

    let Some(runtime) = runtime() else {
        return err_json("policy runtime handle unavailable", "unavailable");
    };

    let agent_id_owned = agent_id.to_string();
    let prompt_owned = prompt.to_string();
    let from_owned = from_agent_id.to_string();
    let pool_owned = pool.clone();

    // Fire-and-forget on the MAIN runtime — do NOT block the policy thread.
    //
    // The policy-engine host fn runs on a non-async thread that must return
    // quickly (cf. agentdesk.message.queue, which only does a fast outbox
    // insert). The turn-start path is the opposite: it claims the agent mailbox
    // and *then* performs fallible pre-spawn work (session/workspace resolution,
    // Discord REST) before spawning the turn's background tasks. Driving that
    // synchronously from the policy thread is unsafe either way (#3587 reviews):
    //   - with a timeout, cancelling mid-flight strands the claimed mailbox;
    //   - without one, a slow/stalled start wedges the policy actor while the
    //     mailbox stays claimed.
    // So we spawn it onto the durable runtime (where its tasks must live anyway)
    // and return immediately. The task is never cancelled, so the mailbox path
    // always runs to completion exactly like the HTTP handoff caller; conflicts
    // and errors are logged rather than surfaced synchronously to JS.
    runtime.spawn(async move {
        match registry
            .start_js_handoff_turn(
                &pool_owned,
                &from_owned,
                &agent_id_owned,
                &prompt_owned,
                channel_kind,
                prefix,
            )
            .await
        {
            Ok(response) => {
                tracing::info!(
                    to_agent = %agent_id_owned,
                    result = %response,
                    "js turn.start dispatched"
                );
            }
            Err((status, error)) => {
                // Classify by the real HTTP status (the busy-mailbox conflict
                // message is "agent mailbox is busy ...", not "conflict"/"409").
                tracing::warn!(
                    to_agent = %agent_id_owned,
                    status = status,
                    error = %error,
                    "js turn.start failed"
                );
            }
        }
    });

    // The turn-start was dispatched onto the main runtime. We acknowledge the
    // dispatch synchronously; the actual start outcome (started/conflict/error)
    // is observable via logs and the target agent's activity.
    serde_json::json!({
        "ok": true,
        "status": "dispatched",
        "to_agent_id": agent_id,
    })
    .to_string()
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
        let mut config = Config::default();
        config.policies = crate::config::PoliciesConfig {
            dir: std::path::PathBuf::from("/nonexistent"),
            hot_reload: false,
            ..crate::config::PoliciesConfig::default()
        };
        PolicyEngine::new(&config).unwrap()
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

    // ── Argument validation ──────────────────────────────────────────────

    #[test]
    fn turn_start_raw_requires_agent_id() {
        let result = start_turn_raw(None, "", "prompt", "{}", || None, || None);
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["error"], "agent_id is required");
    }

    #[test]
    fn turn_start_raw_requires_prompt() {
        let result = start_turn_raw(None, "some-agent", "  ", "{}", || None, || None);
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["error"], "prompt is required");
    }

    #[test]
    fn turn_start_raw_rejects_invalid_channel_kind() {
        let opts = r#"{"channel_kind":"invalid"}"#;
        let result = start_turn_raw(None, "some-agent", "hello", opts, || None, || None);
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["error"], "channel_kind must be cc or cdx");
        assert_eq!(v["status"], "error");
    }
}
