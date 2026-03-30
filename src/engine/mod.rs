pub mod hooks;
pub mod intent;
pub mod loader;
pub mod ops;
pub mod transition;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rquickjs::{Context, Function, Persistent, Runtime};

use crate::config::Config;
use crate::db::Db;

use hooks::Hook;
use loader::PolicyStore;

/// Inner state of the policy engine (not Clone).
struct PolicyEngineInner {
    // Order matters for drop: policies (Persistent values) must be dropped
    // before context and runtime.
    policies: PolicyStore,
    context: Context,
    _runtime: Runtime,
    // Keep watcher alive so hot-reload continues working
    _watcher: Option<notify::RecommendedWatcher>,
}

impl Drop for PolicyEngineInner {
    fn drop(&mut self) {
        // Clear all persistent JS values before the runtime is dropped
        if let Ok(mut guard) = self.policies.lock() {
            guard.clear();
        }
    }
}

/// Thread-safe handle to the policy engine. Cheap to clone.
#[derive(Clone)]
pub struct PolicyEngine {
    inner: Arc<Mutex<PolicyEngineInner>>,
    /// DB handle for persistent deferred hooks queue (#125).
    db: crate::db::Db,
}

/// Summary of a loaded policy (for the /api/policies endpoint).
#[derive(serde::Serialize)]
pub struct PolicyInfo {
    pub name: String,
    pub file: String,
    pub priority: i32,
    pub hooks: Vec<String>,
}

impl PolicyEngine {
    /// Create a new policy engine, initializing QuickJS and loading policies.
    pub fn new(config: &Config, db: Db) -> Result<Self> {
        let runtime =
            Runtime::new().map_err(|e| anyhow::anyhow!("QuickJS runtime creation failed: {e}"))?;
        let context = Context::full(&runtime)
            .map_err(|e| anyhow::anyhow!("QuickJS context creation failed: {e}"))?;

        // Register bridge ops (agentdesk.*)
        context.with(|ctx| {
            ops::register_globals(&ctx, db.clone())
                .map_err(|e| anyhow::anyhow!("Failed to register bridge ops: {e}"))
        })?;

        // Load policies from directory
        let policies_dir = config.policies.dir.clone();
        let policies = loader::load_policies_from_dir(&context, &policies_dir)?;
        let policy_count = policies.len();
        let store: PolicyStore = Arc::new(Mutex::new(policies));

        // Start hot-reload watcher if enabled
        let watcher = if config.policies.hot_reload {
            // For hot-reload we need a separate context that shares the same runtime.
            // The watcher thread will use this context to re-evaluate policies.
            let reload_ctx = Context::full(&runtime)
                .map_err(|e| anyhow::anyhow!("QuickJS reload context creation failed: {e}"))?;

            // Register bridge ops in the reload context too
            reload_ctx.with(|ctx| {
                ops::register_globals(&ctx, db.clone()).map_err(|e| {
                    anyhow::anyhow!("Failed to register bridge ops in reload ctx: {e}")
                })
            })?;

            match loader::start_hot_reload(policies_dir.clone(), reload_ctx, store.clone()) {
                Ok(w) => {
                    tracing::info!("Policy hot-reload enabled for {}", policies_dir.display());
                    Some(w)
                }
                Err(e) => {
                    tracing::warn!("Failed to start policy hot-reload: {e}");
                    None
                }
            }
        } else {
            None
        };

        tracing::info!(
            "Policy engine initialized (policies_dir={}, loaded={policy_count})",
            policies_dir.display()
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(PolicyEngineInner {
                _runtime: runtime,
                context,
                policies: store,
                _watcher: watcher,
            })),
            db: db.clone(),
        })
    }

    /// Fire a hook with the given JSON payload. All policies that registered
    /// for this hook are called in priority order.
    /// Best-effort hook execution: skips if engine is busy (try_lock).
    /// Prevents deadlock when multiple code paths compete for the engine lock.
    pub fn try_fire_hook(&self, hook: Hook, payload: serde_json::Value) -> Result<()> {
        let inner = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ⏸ try_fire_hook({hook}): engine busy, deferring to DB");
                // Persist to deferred_hooks table (#125)
                if let Ok(conn) = self.db.separate_conn() {
                    let _ = conn.execute(
                        "INSERT INTO deferred_hooks (hook_name, payload) VALUES (?1, ?2)",
                        rusqlite::params![hook.to_string(), payload.to_string()],
                    );
                }
                return Ok(());
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                return Err(anyhow::anyhow!("engine lock poisoned: {e}"));
            }
        };
        // Execute the requested hook
        Self::fire_hook_with_guard(&inner, hook, payload)?;
        // Drain deferred hooks from DB while holding inner guard
        // (fire_hook_with_guard doesn't need separate lock)
        let mut had_deferred = false;
        loop {
            let rows: Vec<(i64, String, String)> = {
                let conn = match self.db.separate_conn() {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let mut stmt = match conn.prepare(
                    "SELECT id, hook_name, payload FROM deferred_hooks \
                     WHERE status = 'pending' ORDER BY id ASC LIMIT 50",
                ) {
                    Ok(s) => s,
                    Err(_) => break,
                };
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .ok()
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            };
            if rows.is_empty() {
                break;
            }
            had_deferred = true;
            for (id, hook_name, payload_str) in &rows {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 🔄 fire_hook(deferred {hook_name}, id={id})");
                let p: serde_json::Value =
                    serde_json::from_str(payload_str).unwrap_or(serde_json::json!({}));
                let fire_result = if let Some(h) = Hook::from_str(hook_name) {
                    Self::fire_hook_with_guard(&inner, h, p)
                } else {
                    Self::fire_dynamic_hook_with_guard(&inner, hook_name, p)
                };
                if let Err(e) = &fire_result {
                    tracing::warn!("deferred hook {hook_name} (id={id}) failed: {e}");
                }
                // Delete after attempt (success or fail) — deferred hooks are best-effort
                if let Ok(conn) = self.db.separate_conn() {
                    let _ = conn.execute("DELETE FROM deferred_hooks WHERE id = ?1", [id]);
                }
            }
        }
        // Must drop inner (engine lock) BEFORE draining intents — intent
        // execution needs a fresh lock acquisition via drain_pending_intents.
        drop(inner);
        // #202: Always drain pending intents after hook execution.
        // Hooks (including tick hooks like onTick1min) may push intents via
        // dispatch.create() which uses deferred INSERT. Previously this was
        // gated on had_deferred, causing tick-created dispatch intents to
        // never be drained — resulting in phantom auto-queue entries.
        {
            let result = self.drain_pending_intents();
            if !result.created_dispatches.is_empty() || result.errors > 0 {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let source = if had_deferred { "deferred+direct" } else { "direct" };
                eprintln!(
                    "  [{ts}] 🔄 intent drain ({source}): {} dispatches created, {} errors",
                    result.created_dispatches.len(),
                    result.errors
                );
            }
        }
        Ok(())
    }

    /// Drain any deferred hooks that survived a restart (#125).
    /// Called once at server startup before any workers are spawned.
    /// At-least-once: marks rows as 'processing' before firing, deletes only after success.
    /// If the process crashes mid-replay, undeleted rows will be retried on next startup.
    pub fn drain_startup_hooks(&self) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] 🔄 [startup] draining deferred hooks from DB");

        loop {
            // Read a batch and mark as 'processing' so nested drain in try_fire_hook
            // won't re-read them, but they survive a crash.
            let hooks: Vec<(i64, String, String)> = {
                let conn = match self.db.separate_conn() {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let mut stmt = match conn.prepare(
                    "SELECT id, hook_name, payload FROM deferred_hooks \
                     WHERE status = 'pending' ORDER BY id ASC LIMIT 50",
                ) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let rows: Vec<(i64, String, String)> = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .ok()
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();
                if rows.is_empty() {
                    return;
                }
                // Mark as processing — survives crash, invisible to nested drain
                for (id, _, _) in &rows {
                    let _ = conn.execute(
                        "UPDATE deferred_hooks SET status = 'processing' WHERE id = ?1",
                        [id],
                    );
                }
                rows
            };

            // Fire each hook, delete only after successful execution.
            // Supports both known Hook enum names and dynamic hook names.
            for (id, hook_name, payload_str) in &hooks {
                let payload: serde_json::Value =
                    serde_json::from_str(payload_str).unwrap_or(serde_json::json!({}));
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 🔄 [startup] replaying deferred {hook_name} (id={id})");

                let fire_result = if let Some(hook) = Hook::from_str(hook_name) {
                    self.try_fire_hook(hook, payload)
                } else {
                    self.try_fire_hook_by_name(hook_name, payload)
                };

                if let Err(e) = fire_result {
                    tracing::warn!("[startup] deferred hook {hook_name} failed: {e}");
                    // Leave in DB as 'processing' — will be retried on next startup
                    // after resetting status back to pending
                    if let Ok(conn) = self.db.separate_conn() {
                        let _ = conn.execute(
                            "UPDATE deferred_hooks SET status = 'pending' WHERE id = ?1",
                            [id],
                        );
                    }
                    continue;
                }
                // Success — delete from DB
                if let Ok(conn) = self.db.separate_conn() {
                    let _ = conn.execute("DELETE FROM deferred_hooks WHERE id = ?1", [id]);
                }
                // Drain pending transitions
                loop {
                    let transitions = self.drain_pending_transitions();
                    if transitions.is_empty() {
                        break;
                    }
                    for (card_id, old_s, new_s) in &transitions {
                        crate::kanban::fire_transition_hooks(&self.db, self, card_id, old_s, new_s);
                    }
                }
            }
        }
    }

    /// Fire a dynamic hook by name string. Used for pipeline-defined hooks
    /// that aren't in the fixed Hook enum (e.g. custom on_exit hooks).
    pub fn try_fire_hook_by_name(&self, hook_name: &str, payload: serde_json::Value) -> Result<()> {
        // First try as a known hook
        if let Some(h) = Hook::from_str(hook_name) {
            return self.try_fire_hook(h, payload);
        }
        // Dynamic: look up from policy dynamic_hooks (Rust-side, priority-ordered)
        let inner = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ⏸ try_fire_hook_by_name({hook_name}): engine busy, deferring to DB"
                );
                if let Ok(conn) = self.db.separate_conn() {
                    let _ = conn.execute(
                        "INSERT INTO deferred_hooks (hook_name, payload) VALUES (?1, ?2)",
                        rusqlite::params![hook_name, payload.to_string()],
                    );
                }
                return Ok(());
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                return Err(anyhow::anyhow!("engine lock poisoned: {e}"));
            }
        };
        Self::fire_dynamic_hook_with_guard(&inner, hook_name, payload)
    }

    /// Fire a dynamic (non-enum) hook by looking up `dynamic_hooks` on each
    /// loaded policy, in priority order. Mirrors `fire_hook_with_guard` for
    /// the well-known Hook enum variants.
    fn fire_dynamic_hook_with_guard(
        inner: &std::sync::MutexGuard<'_, PolicyEngineInner>,
        hook_name: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        let policies = inner
            .policies
            .lock()
            .map_err(|e| anyhow::anyhow!("policy store lock poisoned: {e}"))?;

        let hook_fns: Vec<(String, Persistent<Function<'static>>)> = policies
            .iter()
            .filter_map(|p| {
                p.dynamic_hooks
                    .get(hook_name)
                    .map(|f| (p.name.clone(), f.clone()))
            })
            .collect();
        drop(policies);

        if hook_fns.is_empty() {
            return Ok(());
        }

        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let names: Vec<&str> = hook_fns.iter().map(|(n, _)| n.as_str()).collect();
            println!(
                "  [{ts}] 🔥 fire_dynamic_hook({hook_name}) → {names:?} ({} policies)",
                hook_fns.len()
            );
        }

        inner.context.with(|ctx| -> Result<()> {
            let js_payload = json_to_js(&ctx, &payload)?;

            for (policy_name, persistent_fn) in &hook_fns {
                let func = match persistent_fn.clone().restore(&ctx) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(
                            "Failed to restore dynamic hook {hook_name} for policy '{policy_name}': {e}"
                        );
                        continue;
                    }
                };

                let result: rquickjs::Result<rquickjs::Value> = func.call((js_payload.clone(),));
                if let Err(ref e) = result {
                    let exception_detail = ctx.catch().into_exception()
                        .map(|ex| {
                            let msg = ex.message().unwrap_or_default();
                            let stack = ex.stack().unwrap_or_default();
                            format!("{msg}\n{stack}")
                        })
                        .unwrap_or_else(|| format!("{e}"));
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!("  [{ts}] ❌ Dynamic hook {hook_name} in policy '{policy_name}' failed: {exception_detail}");
                    tracing::error!("Dynamic hook {hook_name} in policy '{policy_name}' failed: {exception_detail}");
                }
            }

            Ok(())
        })
    }

    pub fn fire_hook(&self, hook: Hook, payload: serde_json::Value) -> Result<()> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("engine lock poisoned: {e}"))?;
        Self::fire_hook_with_guard(&inner, hook, payload)
    }

    fn fire_hook_with_guard(
        inner: &std::sync::MutexGuard<'_, PolicyEngineInner>,
        hook: Hook,
        payload: serde_json::Value,
    ) -> Result<()> {
        // Collect the persistent functions for this hook
        let policies = inner
            .policies
            .lock()
            .map_err(|e| anyhow::anyhow!("policy store lock poisoned: {e}"))?;

        let hook_fns: Vec<(String, Persistent<Function<'static>>)> = policies
            .iter()
            .filter_map(|p| {
                p.hooks
                    .get(&hook)
                    .map(|f: &Persistent<Function<'static>>| (p.name.clone(), f.clone()))
            })
            .collect();
        drop(policies);

        if hook_fns.is_empty() {
            return Ok(());
        }

        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let names: Vec<&str> = hook_fns.iter().map(|(n, _)| n.as_str()).collect();
            println!(
                "  [{ts}] 🔥 fire_hook({hook}) → {names:?} ({} policies)",
                hook_fns.len()
            );
        }

        // Execute each hook function in the QuickJS context
        inner.context.with(|ctx| -> Result<()> {
            // Convert serde_json::Value to a JS value
            let js_payload = json_to_js(&ctx, &payload)?;

            for (policy_name, persistent_fn) in &hook_fns {
                let func = match persistent_fn.clone().restore(&ctx) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(
                            "Failed to restore hook {} for policy '{}': {e}",
                            hook,
                            policy_name
                        );
                        continue;
                    }
                };

                let result: rquickjs::Result<rquickjs::Value> = func.call((js_payload.clone(),));
                if let Err(ref e) = result {
                    let exception_detail = ctx.catch().into_exception()
                        .map(|ex| {
                            let msg = ex.message().unwrap_or_default();
                            let stack = ex.stack().unwrap_or_default();
                            format!("{msg}\n{stack}")
                        })
                        .unwrap_or_else(|| format!("{e}"));
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!("  [{ts}] ❌ Hook {hook} in policy '{policy_name}' failed: {exception_detail}");
                    tracing::error!("Hook {hook} in policy '{policy_name}' failed: {exception_detail}");
                }
            }

            Ok(())
        })
    }

    /// Drain pending card transitions accumulated by `agentdesk.kanban.setStatus()`
    /// during hook execution. Each entry is `(card_id, old_status, new_status)`.
    /// Call this after `fire_hook` to process transitions that need follow-up hooks.
    pub fn drain_pending_transitions(&self) -> Vec<(String, String, String)> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ⚠ drain_pending_transitions: engine lock poisoned: {e}");
                return Vec::new();
            }
        };
        inner.context.with(|ctx| {
            let code = r#"
                var arr = agentdesk.kanban.__pendingTransitions || [];
                agentdesk.kanban.__pendingTransitions = [];
                JSON.stringify(arr);
            "#;
            let result: rquickjs::Result<String> = ctx.eval(code);
            match result {
                Ok(ref json) => {
                    let transitions: Vec<(String, String, String)> =
                        serde_json::from_str::<Vec<serde_json::Value>>(json)
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|v| {
                                Some((
                                    v.get("card_id")?.as_str()?.to_string(),
                                    v.get("from")?.as_str()?.to_string(),
                                    v.get("to")?.as_str()?.to_string(),
                                ))
                            })
                            .collect();
                    if !transitions.is_empty() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] 🔄 drain_pending_transitions: {} transition(s): {:?}",
                            transitions.len(),
                            transitions
                        );
                    }
                    transitions
                }
                Err(ref e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ⚠ drain_pending_transitions: JS eval error: {e}");
                    Vec::new()
                }
            }
        })
    }

    /// Drain pending intents accumulated by bridge functions during hook execution.
    /// Calls `intent::execute_intents` to apply them and returns the result.
    /// Transitions in the result should be fed into `fire_transition_hooks`.
    pub fn drain_pending_intents(&self) -> intent::IntentExecutionResult {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ⚠ drain_pending_intents: engine lock poisoned: {e}");
                return intent::IntentExecutionResult {
                    transitions: Vec::new(),
                    created_dispatches: Vec::new(),
                    errors: 0,
                };
            }
        };
        let json_str: String = inner.context.with(|ctx| {
            let code = r#"
                var arr = agentdesk.__pendingIntents || [];
                agentdesk.__pendingIntents = [];
                JSON.stringify(arr);
            "#;
            let result: rquickjs::Result<String> = ctx.eval(code);
            match result {
                Ok(json) => json,
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ⚠ drain_pending_intents: JS eval error: {e}");
                    "[]".to_string()
                }
            }
        });
        // Must drop inner (engine lock) before executing intents,
        // because intent execution may need DB access that could deadlock.
        drop(inner);

        let intents: Vec<intent::Intent> = serde_json::from_str(&json_str).unwrap_or_default();
        if intents.is_empty() {
            return intent::IntentExecutionResult {
                transitions: Vec::new(),
                created_dispatches: Vec::new(),
                errors: 0,
            };
        }
        intent::execute_intents(&self.db, intents)
    }

    /// List loaded policies (for API endpoint).
    pub fn list_policies(&self) -> Vec<PolicyInfo> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let policies = match inner.policies.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };

        policies
            .iter()
            .map(|p| {
                let mut hook_names: Vec<String> = p
                    .hooks
                    .keys()
                    .map(|h: &Hook| h.js_name().to_string())
                    .collect();
                hook_names.extend(p.dynamic_hooks.keys().cloned());
                PolicyInfo {
                    name: p.name.clone(),
                    file: p.file.display().to_string(),
                    priority: p.priority,
                    hooks: hook_names,
                }
            })
            .collect()
    }

    /// Evaluate arbitrary JS in the engine context (useful for testing).
    #[cfg(test)]
    pub fn eval_js<T: for<'js> rquickjs::FromJs<'js> + Send>(&self, code: &str) -> Result<T> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("engine lock poisoned: {e}"))?;
        let code_owned = code.to_string();
        inner.context.with(|ctx| {
            let result: T = ctx
                .eval(code_owned.as_bytes().to_vec())
                .map_err(|e| anyhow::anyhow!("JS eval error: {e}"))?;
            Ok(result)
        })
    }
}

/// Convert a serde_json::Value to a rquickjs::Value.
fn json_to_js<'js>(
    ctx: &rquickjs::Ctx<'js>,
    val: &serde_json::Value,
) -> Result<rquickjs::Value<'js>> {
    match val {
        serde_json::Value::Null => Ok(rquickjs::Value::new_null(ctx.clone())),
        serde_json::Value::Bool(b) => Ok(rquickjs::Value::new_bool(ctx.clone(), *b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(rquickjs::Value::new_int(ctx.clone(), i as i32))
            } else if let Some(f) = n.as_f64() {
                Ok(rquickjs::Value::new_float(ctx.clone(), f))
            } else {
                Ok(rquickjs::Value::new_null(ctx.clone()))
            }
        }
        serde_json::Value::String(s) => {
            let js_str = rquickjs::String::from_str(ctx.clone(), s)
                .map_err(|e| anyhow::anyhow!("string conversion: {e}"))?;
            Ok(js_str.into())
        }
        serde_json::Value::Array(arr) => {
            let js_arr = rquickjs::Array::new(ctx.clone())
                .map_err(|e| anyhow::anyhow!("array creation: {e}"))?;
            for (i, item) in arr.iter().enumerate() {
                let js_item = json_to_js(ctx, item)?;
                js_arr
                    .set(i, js_item)
                    .map_err(|e| anyhow::anyhow!("array set: {e}"))?;
            }
            Ok(js_arr.into_value())
        }
        serde_json::Value::Object(map) => {
            let obj = rquickjs::Object::new(ctx.clone())
                .map_err(|e| anyhow::anyhow!("object creation: {e}"))?;
            for (k, v) in map {
                let js_v = json_to_js(ctx, v)?;
                obj.set(&**k, js_v)
                    .map_err(|e| anyhow::anyhow!("object set: {e}"))?;
            }
            Ok(obj.into_value())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_config() -> Config {
        Config {
            policies: crate::config::PoliciesConfig {
                dir: std::path::PathBuf::from("/nonexistent"),
                hot_reload: false,
            },
            ..Config::default()
        }
    }

    fn test_config_with_dir(dir: &std::path::Path) -> Config {
        Config {
            policies: crate::config::PoliciesConfig {
                dir: dir.to_path_buf(),
                hot_reload: false,
            },
            ..Config::default()
        }
    }

    #[test]
    fn test_engine_creates_runtime() {
        let db = test_db();
        let config = test_config();
        let engine = PolicyEngine::new(&config, db);
        assert!(engine.is_ok(), "Engine should initialize without error");
    }

    #[test]
    fn test_engine_evaluates_js() {
        let db = test_db();
        let config = test_config();
        let engine = PolicyEngine::new(&config, db).unwrap();
        let result: i32 = engine.eval_js("1 + 2").unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn test_engine_db_query_via_engine() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('x1', 'Xbot', 'claude', 'idle', 42)",
                [],
            ).unwrap();
        }

        let config = test_config();
        let engine = PolicyEngine::new(&config, db).unwrap();
        let xp: i32 = engine
            .eval_js(r#"agentdesk.db.query("SELECT xp FROM agents WHERE id = 'x1'")[0].xp"#)
            .unwrap();
        assert_eq!(xp, 42);
    }

    #[test]
    fn test_engine_load_policy_file() {
        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("test-policy.js");
        std::fs::write(
            &policy_path,
            r#"
            var policy = {
                name: "test-policy",
                priority: 5,
                onTick: function() {
                    agentdesk.log.info("[test-policy] tick fired");
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db).unwrap();

        let policies = engine.list_policies();
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].name, "test-policy");
        assert_eq!(policies[0].priority, 5);
        assert!(policies[0].hooks.contains(&"onTick".to_string()));
    }

    #[test]
    fn test_engine_register_and_fire_hook() {
        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("hook-policy.js");
        std::fs::write(
            &policy_path,
            r#"
            var policy = {
                name: "hook-policy",
                priority: 1,
                onTick: function(payload) {
                    // Write a marker into kv_meta to prove this ran
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('hook_test', 'fired')",
                        []
                    );
                },
                onCardTerminal: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('terminal_card', payload.card_id || 'unknown')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db.clone()).unwrap();

        // Fire onTick
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();

        // Check the marker was written
        let conn = db.lock().unwrap();
        let val: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'hook_test'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "fired");
    }

    #[test]
    fn test_engine_fire_hook_with_payload() {
        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("payload-policy.js");
        std::fs::write(
            &policy_path,
            r#"
            var policy = {
                name: "payload-policy",
                priority: 1,
                onCardTerminal: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('terminal_id', '" + payload.card_id + "')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db.clone()).unwrap();

        engine
            .fire_hook(
                Hook::OnCardTerminal,
                serde_json::json!({"card_id": "card-123"}),
            )
            .unwrap();

        let conn = db.lock().unwrap();
        let val: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'terminal_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "card-123");
    }

    #[test]
    fn test_engine_fire_dynamic_hook_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("dynamic-hook.js");
        std::fs::write(
            &policy_path,
            r#"
            var policy = {
                name: "dynamic-hook-policy",
                priority: 1,
                onCustomStateEnter: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('dyn_hook', '" + payload.status + "')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db.clone()).unwrap();

        // Verify the dynamic hook was detected
        let policies = engine.list_policies();
        assert_eq!(policies.len(), 1);
        assert!(
            policies[0]
                .hooks
                .contains(&"onCustomStateEnter".to_string()),
            "dynamic hook should appear in list_policies"
        );

        // Fire by name — this should reach the dynamic_hooks path
        engine
            .try_fire_hook_by_name(
                "onCustomStateEnter",
                serde_json::json!({"status": "custom_state"}),
            )
            .unwrap();

        let conn = db.lock().unwrap();
        let val: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'dyn_hook'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "custom_state");
    }

    #[test]
    fn test_engine_dynamic_hook_priority_order() {
        let dir = tempfile::tempdir().unwrap();
        // Low priority (runs second)
        std::fs::write(
            dir.path().join("aaa-low.js"),
            r#"
            var policy = {
                name: "low-priority",
                priority: 100,
                onMyHook: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('order', 'low')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();
        // High priority (runs first)
        std::fs::write(
            dir.path().join("bbb-high.js"),
            r#"
            var policy = {
                name: "high-priority",
                priority: 1,
                onMyHook: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('order', 'high')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db.clone()).unwrap();

        engine
            .try_fire_hook_by_name("onMyHook", serde_json::json!({}))
            .unwrap();

        // Both run in priority order: high(1) then low(100).
        // Last write wins, so value should be "low".
        let conn = db.lock().unwrap();
        let val: String = conn
            .query_row("SELECT value FROM kv_meta WHERE key = 'order'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(val, "low", "low-priority policy runs last (priority=100)");
    }
}
