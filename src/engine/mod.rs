pub mod hooks;
pub mod intent;
pub mod loader;
pub mod ops;
pub mod sql_guard;
pub mod transition;

use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::thread::{self, JoinHandle, ThreadId};

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

struct PolicyEngineActor {
    tx: mpsc::Sender<EngineCommand>,
    thread_id: Arc<OnceLock<ThreadId>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

enum EngineCommand {
    FireHook {
        hook: Hook,
        payload: serde_json::Value,
        reply: mpsc::Sender<Result<()>>,
    },
    FireHookByName {
        hook_name: String,
        payload: serde_json::Value,
        reply: mpsc::Sender<Result<()>>,
    },
    DrainPendingTransitions {
        reply: mpsc::Sender<Vec<(String, String, String)>>,
    },
    DrainPendingIntents {
        reply: mpsc::Sender<intent::IntentExecutionResult>,
    },
    ListPolicies {
        reply: mpsc::Sender<Vec<PolicyInfo>>,
    },
    DrainStartupHooks {
        reply: mpsc::Sender<()>,
    },
    #[cfg(test)]
    BlockActor {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        reply: mpsc::Sender<()>,
    },
    Shutdown,
}

impl PolicyEngineActor {
    fn spawn(inner: Arc<Mutex<PolicyEngineInner>>, db: Db) -> Result<Arc<Self>> {
        let (tx, rx) = mpsc::channel();
        let thread_id = Arc::new(OnceLock::new());
        let actor = Arc::new(Self {
            tx,
            thread_id: thread_id.clone(),
            join: Mutex::new(None),
        });
        let actor_weak = Arc::downgrade(&actor);
        let handle = thread::Builder::new()
            .name("policy-engine-actor".to_string())
            .spawn(move || Self::run_loop(actor_weak, inner, db, thread_id, rx))
            .map_err(|e| anyhow::anyhow!("failed to spawn policy engine actor: {e}"))?;
        *actor
            .join
            .lock()
            .map_err(|e| anyhow::anyhow!("actor join lock poisoned: {e}"))? = Some(handle);
        Ok(actor)
    }

    fn run_loop(
        actor_weak: Weak<Self>,
        inner: Arc<Mutex<PolicyEngineInner>>,
        db: Db,
        thread_id: Arc<OnceLock<ThreadId>>,
        rx: mpsc::Receiver<EngineCommand>,
    ) {
        let _ = thread_id.set(thread::current().id());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .ok();
        let _runtime_guard = runtime.as_ref().map(|rt| rt.enter());

        while let Ok(command) = rx.recv() {
            if matches!(command, EngineCommand::Shutdown) {
                break;
            }

            let Some(actor) = actor_weak.upgrade() else {
                break;
            };
            let engine = PolicyEngine {
                inner: inner.clone(),
                actor,
                db: db.clone(),
            };

            match command {
                EngineCommand::FireHook {
                    hook,
                    payload,
                    reply,
                } => {
                    let _ = reply.send(engine.fire_hook_inline(hook, payload));
                }
                EngineCommand::FireHookByName {
                    hook_name,
                    payload,
                    reply,
                } => {
                    let _ = reply.send(engine.fire_hook_by_name_inline(&hook_name, payload));
                }
                EngineCommand::DrainPendingTransitions { reply } => {
                    let _ = reply.send(engine.drain_pending_transitions_inline());
                }
                EngineCommand::DrainPendingIntents { reply } => {
                    let _ = reply.send(engine.drain_pending_intents_inline());
                }
                EngineCommand::ListPolicies { reply } => {
                    let _ = reply.send(engine.list_policies_inline());
                }
                EngineCommand::DrainStartupHooks { reply } => {
                    engine.drain_startup_hooks_inline();
                    let _ = reply.send(());
                }
                #[cfg(test)]
                EngineCommand::BlockActor {
                    entered,
                    release,
                    reply,
                } => {
                    let _ = entered.send(());
                    let _ = release.recv();
                    let _ = reply.send(());
                }
                EngineCommand::Shutdown => unreachable!(),
            }
        }
    }

    fn is_actor_thread(&self) -> bool {
        self.thread_id
            .get()
            .is_some_and(|id| *id == thread::current().id())
    }
}

impl Drop for PolicyEngineActor {
    fn drop(&mut self) {
        if self.is_actor_thread() {
            return;
        }
        let _ = self.tx.send(EngineCommand::Shutdown);
        if let Ok(mut join) = self.join.lock() {
            if let Some(handle) = join.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Thread-safe handle to the policy engine. Cheap to clone.
#[derive(Clone)]
pub struct PolicyEngine {
    inner: Arc<Mutex<PolicyEngineInner>>,
    actor: Arc<PolicyEngineActor>,
    /// DB handle used by bridge ops and compatibility startup replay.
    db: crate::db::Db,
}

#[derive(Clone)]
pub struct PolicyEngineHandle {
    inner: Weak<Mutex<PolicyEngineInner>>,
    actor: Weak<PolicyEngineActor>,
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
        let supervisor_bridge = crate::supervisor::BridgeHandle::new();
        let runtime =
            Runtime::new().map_err(|e| anyhow::anyhow!("QuickJS runtime creation failed: {e}"))?;
        let context = Context::full(&runtime)
            .map_err(|e| anyhow::anyhow!("QuickJS context creation failed: {e}"))?;

        // Register bridge ops (agentdesk.*)
        context.with(|ctx| {
            ops::register_globals_with_supervisor(&ctx, db.clone(), supervisor_bridge.clone())
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
                ops::register_globals_with_supervisor(&ctx, db.clone(), supervisor_bridge.clone())
                    .map_err(|e| {
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

        let inner = Arc::new(Mutex::new(PolicyEngineInner {
            _runtime: runtime,
            context,
            policies: store,
            _watcher: watcher,
        }));
        let actor = PolicyEngineActor::spawn(inner.clone(), db.clone())?;
        let engine = Self {
            inner,
            actor,
            db: db.clone(),
        };
        supervisor_bridge.attach_engine(&engine);

        Ok(engine)
    }

    pub fn downgrade(&self) -> PolicyEngineHandle {
        PolicyEngineHandle {
            inner: Arc::downgrade(&self.inner),
            actor: Arc::downgrade(&self.actor),
            db: self.db.clone(),
        }
    }

    fn roundtrip<T>(&self, command: impl FnOnce(mpsc::Sender<T>) -> EngineCommand) -> Result<T> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.actor
            .tx
            .send(command(reply_tx))
            .map_err(|_| anyhow::anyhow!("policy engine actor is unavailable"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("policy engine actor response channel dropped"))
    }

    fn empty_intent_result() -> intent::IntentExecutionResult {
        intent::IntentExecutionResult {
            transitions: Vec::new(),
            created_dispatches: Vec::new(),
            errors: 0,
        }
    }

    /// Fire a hook with the given JSON payload. All policies that registered
    /// for this hook are called in priority order.
    /// Calls are serialized through the actor queue instead of deferring to DB.
    pub fn try_fire_hook(&self, hook: Hook, payload: serde_json::Value) -> Result<()> {
        if self.actor.is_actor_thread() {
            return self.fire_hook_inline(hook, payload);
        }
        self.roundtrip(|reply| EngineCommand::FireHook {
            hook,
            payload,
            reply,
        })?
    }

    fn fire_hook_inline(&self, hook: Hook, payload: serde_json::Value) -> Result<()> {
        let hook_name = hook.to_string();
        let span = crate::logging::hook_span(&hook_name, &payload);
        let _guard = span.enter();
        {
            let inner = self
                .inner
                .lock()
                .map_err(|e| anyhow::anyhow!("engine lock poisoned: {e}"))?;
            Self::fire_hook_with_guard(&inner, hook, payload)?;
        }
        self.flush_hook_side_effects_inline();
        Ok(())
    }

    /// Drain any legacy deferred hooks that survived a restart (#125).
    /// New runtimes no longer enqueue deferred hooks; this is a compatibility
    /// path so old rows can be replayed once and cleared.
    pub fn drain_startup_hooks(&self) {
        if self.actor.is_actor_thread() {
            self.drain_startup_hooks_inline();
            return;
        }
        if let Err(e) = self.roundtrip(|reply| EngineCommand::DrainStartupHooks { reply }) {
            tracing::warn!("failed to drain legacy startup hooks: {e}");
        }
    }

    fn drain_startup_hooks_inline(&self) {
        tracing::info!("[startup] draining legacy deferred hooks from DB");

        // Record server boot time for orphan recovery grace period
        if let Ok(conn) = self.db.separate_conn() {
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('server_boot_at', datetime('now'))",
                [],
            )
            .ok();
        }

        loop {
            let hooks: Vec<(i64, String, String)> = {
                let conn = match self.db.separate_conn() {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let mut stmt = match conn.prepare(
                    "SELECT id, hook_name, payload FROM deferred_hooks \
                     WHERE status IN ('pending', 'processing') ORDER BY id ASC LIMIT 50",
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
                let span = crate::logging::hook_span(hook_name, &payload);
                let _guard = span.enter();
                tracing::info!(deferred_hook_id = *id, "[startup] replaying deferred hook");

                let fire_result = if let Some(hook) = Hook::from_str(hook_name) {
                    self.fire_hook_inline(hook, payload)
                } else {
                    self.fire_hook_by_name_inline(hook_name, payload)
                };

                if let Err(e) = fire_result {
                    tracing::warn!("[startup] deferred hook {hook_name} failed: {e}");
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
            }
        }
    }

    /// Fire a dynamic hook by name string. Used for pipeline-defined hooks
    /// that aren't in the fixed Hook enum (e.g. custom on_exit hooks).
    pub fn try_fire_hook_by_name(&self, hook_name: &str, payload: serde_json::Value) -> Result<()> {
        if let Some(h) = Hook::from_str(hook_name) {
            return self.try_fire_hook(h, payload);
        }
        if self.actor.is_actor_thread() {
            return self.fire_hook_by_name_inline(hook_name, payload);
        }
        self.roundtrip(|reply| EngineCommand::FireHookByName {
            hook_name: hook_name.to_string(),
            payload,
            reply,
        })?
    }

    /// Blocking variant of `try_fire_hook_by_name` — waits for the engine lock
    /// instead of deferring on contention. Used only in safety-net paths where
    /// the hook MUST execute (e.g. finalize_dispatch review-dispatch guarantee).
    pub fn fire_hook_by_name_blocking(
        &self,
        hook_name: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        self.try_fire_hook_by_name(hook_name, payload)
    }

    fn fire_hook_by_name_inline(&self, hook_name: &str, payload: serde_json::Value) -> Result<()> {
        if let Some(h) = Hook::from_str(hook_name) {
            return self.fire_hook_inline(h, payload);
        }
        let span = crate::logging::hook_span(hook_name, &payload);
        let _guard = span.enter();
        {
            let inner = self
                .inner
                .lock()
                .map_err(|e| anyhow::anyhow!("engine lock poisoned: {e}"))?;
            Self::fire_dynamic_hook_with_guard(&inner, hook_name, payload)?;
        }
        self.flush_hook_side_effects_inline();
        Ok(())
    }

    fn flush_hook_side_effects_inline(&self) {
        loop {
            let intent_result = self.drain_pending_intents_inline();
            let mut transitions = intent_result.transitions;
            transitions.extend(self.drain_pending_transitions_inline());

            if transitions.is_empty() {
                break;
            }

            for (card_id, old_status, new_status) in &transitions {
                crate::kanban::fire_transition_hooks(
                    &self.db, self, card_id, old_status, new_status,
                );
            }
        }
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

        let names: Vec<&str> = hook_fns.iter().map(|(n, _)| n.as_str()).collect();
        tracing::info!(policy_count = hook_fns.len(), policies = ?names, "firing dynamic hook");

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
                    tracing::error!(
                        policy_name,
                        error = %exception_detail,
                        "dynamic hook execution failed"
                    );
                }
            }

            Ok(())
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn fire_hook(&self, hook: Hook, payload: serde_json::Value) -> Result<()> {
        self.try_fire_hook(hook, payload)
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

        let names: Vec<&str> = hook_fns.iter().map(|(n, _)| n.as_str()).collect();
        tracing::info!(
            policy_count = hook_fns.len(),
            policies = ?names,
            hook = %hook,
            "firing hook"
        );

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
                    let exception_detail = ctx
                        .catch()
                        .into_exception()
                        .map(|ex| {
                            let msg = ex.message().unwrap_or_default();
                            let stack = ex.stack().unwrap_or_default();
                            format!("{msg}\n{stack}")
                        })
                        .unwrap_or_else(|| format!("{e}"));
                    tracing::error!(
                        policy_name,
                        hook = %hook,
                        error = %exception_detail,
                        "hook execution failed"
                    );
                }
            }

            Ok(())
        })
    }

    fn take_pending_transitions_with_guard(
        inner: &std::sync::MutexGuard<'_, PolicyEngineInner>,
    ) -> Vec<(String, String, String)> {
        inner.context.with(|ctx| {
            let code = r#"
                var arr = agentdesk.kanban.__pendingTransitions || [];
                agentdesk.kanban.__pendingTransitions = [];
                JSON.stringify(arr);
            "#;
            let result: rquickjs::Result<String> = ctx.eval(code);
            match result {
                Ok(ref json) => serde_json::from_str::<Vec<serde_json::Value>>(json)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|v| {
                        Some((
                            v.get("card_id")?.as_str()?.to_string(),
                            v.get("from")?.as_str()?.to_string(),
                            v.get("to")?.as_str()?.to_string(),
                        ))
                    })
                    .collect(),
                Err(ref e) => {
                    tracing::warn!(error = %e, "failed to eval pending transitions");
                    Vec::new()
                }
            }
        })
    }

    /// Drain pending card transitions accumulated by `agentdesk.kanban.setStatus()`
    /// during hook execution. Each entry is `(card_id, old_status, new_status)`.
    /// Call this after `fire_hook` to process transitions that need follow-up hooks.
    pub fn drain_pending_transitions(&self) -> Vec<(String, String, String)> {
        if self.actor.is_actor_thread() {
            return self.drain_pending_transitions_inline();
        }
        self.roundtrip(|reply| EngineCommand::DrainPendingTransitions { reply })
            .unwrap_or_else(|e| {
                tracing::warn!("drain_pending_transitions roundtrip failed: {e}");
                Vec::new()
            })
    }

    fn drain_pending_transitions_inline(&self) -> Vec<(String, String, String)> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "failed to lock engine for pending transitions");
                return Vec::new();
            }
        };
        let transitions = Self::take_pending_transitions_with_guard(&inner);
        if !transitions.is_empty() {
            tracing::info!(
                transition_count = transitions.len(),
                transitions = ?transitions,
                "drained pending transitions"
            );
        }
        transitions
    }

    /// Drain pending intents accumulated by bridge functions during hook execution.
    /// Calls `intent::execute_intents` to apply them and returns the result.
    /// Transitions in the result should be fed into `fire_transition_hooks`.
    ///
    pub fn drain_pending_intents(&self) -> intent::IntentExecutionResult {
        if self.actor.is_actor_thread() {
            return self.drain_pending_intents_inline();
        }
        self.roundtrip(|reply| EngineCommand::DrainPendingIntents { reply })
            .unwrap_or_else(|e| {
                tracing::warn!("drain_pending_intents roundtrip failed: {e}");
                Self::empty_intent_result()
            })
    }

    fn drain_pending_intents_inline(&self) -> intent::IntentExecutionResult {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "failed to lock engine for pending intents");
                return Self::empty_intent_result();
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
                    tracing::warn!(error = %e, "failed to eval pending intents");
                    "[]".to_string()
                }
            }
        });
        // Must drop inner (engine lock) before executing intents,
        // because intent execution may need DB access that could deadlock.
        drop(inner);

        let intents: Vec<intent::Intent> = serde_json::from_str(&json_str).unwrap_or_default();
        if intents.is_empty() {
            Self::empty_intent_result()
        } else {
            intent::execute_intents(&self.db, intents)
        }
    }

    /// List loaded policies (for API endpoint).
    pub fn list_policies(&self) -> Vec<PolicyInfo> {
        if self.actor.is_actor_thread() {
            return self.list_policies_inline();
        }
        self.roundtrip(|reply| EngineCommand::ListPolicies { reply })
            .unwrap_or_else(|e| {
                tracing::warn!("list_policies roundtrip failed: {e}");
                Vec::new()
            })
    }

    fn list_policies_inline(&self) -> Vec<PolicyInfo> {
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

    #[cfg(test)]
    fn block_actor_for_test(
        &self,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    ) -> Result<()> {
        if self.actor.is_actor_thread() {
            let _ = entered.send(());
            let _ = release.recv();
            return Ok(());
        }
        self.roundtrip(|reply| EngineCommand::BlockActor {
            entered,
            release,
            reply,
        })?;
        Ok(())
    }
}

impl PolicyEngineHandle {
    pub fn upgrade(&self) -> Option<PolicyEngine> {
        Some(PolicyEngine {
            inner: self.inner.upgrade()?,
            actor: self.actor.upgrade()?,
            db: self.db.clone(),
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
    fn test_triage_policy_uses_typed_facade() {
        let dir = tempfile::tempdir().unwrap();
        let triage_src = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies/triage-rules.js"),
        )
        .unwrap();
        std::fs::write(dir.path().join("triage-rules.js"), triage_src).unwrap();

        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id) \
                 VALUES ('ch-backend', 'Backend', 'claude', '111')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, metadata, github_issue_number, github_issue_url, created_at, updated_at) \
                 VALUES ('triage-card', 'Typed facade triage', 'backlog', 'medium', '{\"labels\":\"agent:backend priority:high\"}', 348, 'https://github.com/itismyfield/AgentDesk/issues/348', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db.clone()).unwrap();
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();

        let conn = db.lock().unwrap();
        let (assigned_agent_id, priority): (Option<String>, String) = conn
            .query_row(
                "SELECT assigned_agent_id, priority FROM kanban_cards WHERE id = 'triage-card'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(assigned_agent_id.as_deref(), Some("ch-backend"));
        assert_eq!(priority, "high");
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

    /// Regression test — dispatch.create() called from a dynamic hook
    /// must materialize both the dispatch row and notify outbox before return.
    #[test]
    fn test_dynamic_hook_dispatch_create_produces_db_row() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("dispatch-hook.js"),
            r#"
            var policy = {
                name: "dispatch-hook",
                priority: 1,
                onCustomEnter: function(payload) {
                    var id = agentdesk.dispatch.create(
                        payload.card_id,
                        payload.agent_id,
                        "implementation",
                        "Dynamic hook dispatch"
                    );
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('dyn_dispatch_id', '" + id + "')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        // Seed: agent + kanban card
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp, discord_channel_id, discord_channel_alt) \
                 VALUES ('bot1', 'Bot', 'claude', 'idle', 0, '1234567890', '1234567891')",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority) VALUES ('card1', 'Test', 'ready', 'medium')",
                [],
            ).unwrap();
        }

        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db.clone()).unwrap();

        engine
            .try_fire_hook_by_name(
                "onCustomEnter",
                serde_json::json!({"card_id": "card1", "agent_id": "bot1"}),
            )
            .unwrap();

        let conn = db.lock().unwrap();
        // The dispatch ID was stashed in kv_meta by the hook
        let dispatch_id: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'dyn_dispatch_id'",
                [],
                |r| r.get(0),
            )
            .expect("hook should have written dispatch_id to kv_meta");

        // Verify the dispatch row actually exists in task_dispatches
        let title: String = conn
            .query_row(
                "SELECT title FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |r| r.get(0),
            )
            .expect("dispatch row should exist in task_dispatches");
        assert_eq!(title, "Dynamic hook dispatch");

        let notify_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
                [&dispatch_id],
                |r| r.get(0),
            )
            .expect("notify outbox row should exist for dynamic-hook dispatch");
        assert_eq!(
            notify_count, 1,
            "dynamic hook dispatch must enqueue exactly one notify outbox row"
        );
    }

    #[test]
    fn test_dynamic_hook_dispatch_create_persists_context_object() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("dispatch-hook-context.js"),
            r#"
            var policy = {
                name: "dispatch-hook-context",
                priority: 1,
                onCustomEnter: function(payload) {
                    var id = agentdesk.dispatch.create(
                        payload.card_id,
                        payload.agent_id,
                        "rework",
                        "Contextful dispatch",
                        { force_new_session: true, reset_reason: "repeated_findings" }
                    );
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('dyn_dispatch_ctx_id', '" + id + "')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp, discord_channel_id, discord_channel_alt) \
                 VALUES ('bot2', 'Bot 2', 'claude', 'idle', 0, '2234567890', '2234567891')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority) VALUES ('card2', 'Test 2', 'ready', 'medium')",
                [],
            )
            .unwrap();
        }

        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db.clone()).unwrap();

        engine
            .try_fire_hook_by_name(
                "onCustomEnter",
                serde_json::json!({"card_id": "card2", "agent_id": "bot2"}),
            )
            .unwrap();

        let conn = db.lock().unwrap();
        let dispatch_id: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'dyn_dispatch_ctx_id'",
                [],
                |r| r.get(0),
            )
            .expect("hook should have written dispatch_id to kv_meta");
        let context: String = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [&dispatch_id],
                |r| r.get(0),
            )
            .expect("dispatch row should persist context");
        let context_json: serde_json::Value = serde_json::from_str(&context).unwrap();
        assert_eq!(context_json["force_new_session"], true);
        assert_eq!(context_json["reset_reason"], "repeated_findings");
    }

    #[test]
    fn queued_review_enter_replays_terminal_transition_hooks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("deferred-review-terminal.js"),
            r#"
            var policy = {
                name: "deferred-review-terminal",
                priority: 1,
                onReviewEnter: function(payload) {
                    agentdesk.kanban.setStatus(payload.card_id, "done", true);
                },
                onCardTerminal: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('terminal_fired', ?)",
                        [payload.card_id]
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority) \
                 VALUES ('card-deferred-review', 'Deferred review', 'review', 'medium')",
                [],
            )
            .unwrap();
        }

        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new(&config, db.clone()).unwrap();

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocker_engine = engine.clone();
        let blocker = std::thread::spawn(move || {
            blocker_engine
                .block_actor_for_test(entered_tx, release_rx)
                .unwrap();
        });
        entered_rx.recv().unwrap();

        let queued_engine = engine.clone();
        let queued = std::thread::spawn(move || {
            queued_engine
                .try_fire_hook(
                    Hook::OnReviewEnter,
                    serde_json::json!({"card_id": "card-deferred-review"}),
                )
                .unwrap();
        });

        std::thread::sleep(std::time::Duration::from_millis(50));

        let deferred_count: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM deferred_hooks", [], |row| row.get(0))
            .expect("deferred_hooks table should be readable");
        assert_eq!(
            deferred_count, 0,
            "queued hook execution must not fall back to deferred_hooks"
        );
        release_tx.send(()).unwrap();
        blocker.join().unwrap();
        queued.join().unwrap();

        let conn = db.lock().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-deferred-review'",
                [],
                |row| row.get(0),
            )
            .expect("queued review hook should still move the card to done");
        assert_eq!(status, "done");

        let terminal_marker: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'terminal_fired'",
                [],
                |row| row.get(0),
            )
            .expect("terminal follow-up hook must fire for queued review transitions");
        assert_eq!(terminal_marker, "card-deferred-review");
    }
}
