pub mod hooks;
pub mod intent;
pub mod loader;
pub mod ops;
pub mod sql_guard;
pub mod transition;
pub mod transition_executor_pg;

use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, AtomicUsize},
    mpsc,
};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::Duration;

use anyhow::Result;
use rquickjs::{Context, Function, Persistent, Runtime};
use sqlx::Row;

use crate::config::Config;
#[cfg(test)]
use crate::db::Db;

use hooks::Hook;
use loader::PolicyStore;

const POLICY_HOOK_WARN_THRESHOLD: Duration = Duration::from_millis(100);

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
    /// Approximate queue depth: incremented when a command is sent, decremented
    /// when the actor pops one off the channel. Exposed for observability (#747).
    queue_depth: Arc<AtomicUsize>,
    /// Short name used in log messages (e.g. "main", "tick").
    label: &'static str,
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
    fn spawn(
        inner: Arc<Mutex<PolicyEngineInner>>,
        runtime_deps: Arc<PolicyEngineRuntimeDeps>,
        tick_hook_in_flight: Arc<AtomicBool>,
        label: &'static str,
    ) -> Result<Arc<Self>> {
        let (tx, rx) = mpsc::channel();
        let thread_id = Arc::new(OnceLock::new());
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let actor = Arc::new(Self {
            tx,
            thread_id: thread_id.clone(),
            join: Mutex::new(None),
            queue_depth: queue_depth.clone(),
            label,
        });
        let actor_weak = Arc::downgrade(&actor);
        let thread_name = format!("policy-engine-actor-{label}");
        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                Self::run_loop(
                    actor_weak,
                    inner,
                    runtime_deps,
                    tick_hook_in_flight,
                    thread_id,
                    queue_depth,
                    rx,
                )
            })
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
        runtime_deps: Arc<PolicyEngineRuntimeDeps>,
        tick_hook_in_flight: Arc<AtomicBool>,
        thread_id: Arc<OnceLock<ThreadId>>,
        queue_depth: Arc<AtomicUsize>,
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
            // We popped a command off the channel, so approximate queue depth
            // should drop. saturating_sub guards against any accidental skew.
            queue_depth
                .fetch_update(
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                    |d| Some(d.saturating_sub(1)),
                )
                .ok();

            if matches!(command, EngineCommand::Shutdown) {
                break;
            }

            let Some(actor) = actor_weak.upgrade() else {
                break;
            };
            let engine = PolicyEngine {
                inner: inner.clone(),
                actor,
                runtime_deps: runtime_deps.clone(),
                tick_hook_in_flight: tick_hook_in_flight.clone(),
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

#[derive(Clone)]
struct PolicyEngineRuntimeDeps {
    #[cfg(test)]
    legacy_db: Option<Db>,
    pg_pool: Option<sqlx::PgPool>,
}

/// Thread-safe handle to the policy engine. Cheap to clone.
#[derive(Clone)]
pub struct PolicyEngine {
    inner: Arc<Mutex<PolicyEngineInner>>,
    actor: Arc<PolicyEngineActor>,
    /// Transitional runtime deps kept only while SQLite compatibility remains.
    runtime_deps: Arc<PolicyEngineRuntimeDeps>,
    tick_hook_in_flight: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct PolicyEngineHandle {
    inner: Weak<Mutex<PolicyEngineInner>>,
    actor: Weak<PolicyEngineActor>,
    runtime_deps: Weak<PolicyEngineRuntimeDeps>,
    tick_hook_in_flight: Arc<AtomicBool>,
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
    pub fn new(config: &Config) -> Result<Self> {
        Self::new_with_pg_and_label(config, None, "main")
    }

    /// Create a dedicated policy engine for the tick loop (#747).
    ///
    /// The tick engine has its own QuickJS runtime, policy actor thread,
    /// and hot-reload watcher — fully isolated from the main engine so that
    /// a long-running or stuck tick hook cannot back up the main engine's
    /// actor queue and starve HTTP/Discord hook paths.
    ///
    /// Both engines load the same policies directory (so any policy that
    /// registers `onTick*` hooks is executed by this engine).
    pub fn new_for_tick(config: &Config, pg_pool: Option<sqlx::PgPool>) -> Result<Self> {
        Self::new_with_pg_and_label(config, pg_pool, "tick")
    }

    pub fn new_with_pg(config: &Config, pg_pool: Option<sqlx::PgPool>) -> Result<Self> {
        Self::new_with_pg_and_label(config, pg_pool, "main")
    }

    #[cfg(test)]
    pub fn new_with_legacy_db(config: &Config, db: Db) -> Result<Self> {
        Self::new_with_test_backends_and_label(config, Some(db), None, "main")
    }

    fn new_with_pg_and_label(
        config: &Config,
        pg_pool: Option<sqlx::PgPool>,
        label: &'static str,
    ) -> Result<Self> {
        let runtime_deps = Arc::new(PolicyEngineRuntimeDeps {
            #[cfg(test)]
            legacy_db: None,
            pg_pool: pg_pool.clone(),
        });
        Self::new_with_runtime_deps(config, runtime_deps, label)
    }

    #[cfg(test)]
    fn new_with_test_backends_and_label(
        config: &Config,
        legacy_db: Option<Db>,
        pg_pool: Option<sqlx::PgPool>,
        label: &'static str,
    ) -> Result<Self> {
        let runtime_deps = Arc::new(PolicyEngineRuntimeDeps { legacy_db, pg_pool });
        Self::new_with_runtime_deps(config, runtime_deps, label)
    }

    fn new_with_runtime_deps(
        config: &Config,
        runtime_deps: Arc<PolicyEngineRuntimeDeps>,
        label: &'static str,
    ) -> Result<Self> {
        let supervisor_bridge = crate::supervisor::BridgeHandle::new();
        let runtime =
            Runtime::new().map_err(|e| anyhow::anyhow!("QuickJS runtime creation failed: {e}"))?;
        let context = Context::full(&runtime)
            .map_err(|e| anyhow::anyhow!("QuickJS context creation failed: {e}"))?;

        // Register bridge ops (agentdesk.*)
        context.with(|ctx| {
            #[cfg(test)]
            {
                return ops::register_globals_with_supervisor_and_test_backends(
                    &ctx,
                    runtime_deps.legacy_db.clone(),
                    runtime_deps.pg_pool.clone(),
                    supervisor_bridge.clone(),
                )
                .map_err(|e| anyhow::anyhow!("Failed to register bridge ops: {e}"));
            }
            #[cfg(not(test))]
            ops::register_globals_with_supervisor_and_pg(
                &ctx,
                runtime_deps.pg_pool.clone(),
                supervisor_bridge.clone(),
            )
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
                #[cfg(test)]
                {
                    return ops::register_globals_with_supervisor_and_test_backends(
                        &ctx,
                        runtime_deps.legacy_db.clone(),
                        runtime_deps.pg_pool.clone(),
                        supervisor_bridge.clone(),
                    )
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to register bridge ops in reload ctx: {e}")
                    });
                }
                #[cfg(not(test))]
                ops::register_globals_with_supervisor_and_pg(
                    &ctx,
                    runtime_deps.pg_pool.clone(),
                    supervisor_bridge.clone(),
                )
                .map_err(|e| anyhow::anyhow!("Failed to register bridge ops in reload ctx: {e}"))
            })?;

            match loader::start_hot_reload(policies_dir.clone(), reload_ctx, store.clone()) {
                Ok(w) => {
                    tracing::info!(
                        engine_label = label,
                        "Policy hot-reload enabled for {}",
                        policies_dir.display()
                    );
                    Some(w)
                }
                Err(e) => {
                    tracing::warn!(
                        engine_label = label,
                        "Failed to start policy hot-reload: {e}"
                    );
                    None
                }
            }
        } else {
            None
        };

        tracing::info!(
            engine_label = label,
            "Policy engine initialized (policies_dir={}, loaded={policy_count})",
            policies_dir.display()
        );

        let inner = Arc::new(Mutex::new(PolicyEngineInner {
            _runtime: runtime,
            context,
            policies: store,
            _watcher: watcher,
        }));
        let tick_hook_in_flight = Arc::new(AtomicBool::new(false));
        let actor = PolicyEngineActor::spawn(
            inner.clone(),
            runtime_deps.clone(),
            tick_hook_in_flight.clone(),
            label,
        )?;
        let engine = Self {
            inner,
            actor,
            runtime_deps,
            tick_hook_in_flight,
        };
        supervisor_bridge.attach_engine(&engine);

        Ok(engine)
    }

    /// Approximate number of commands waiting in the actor queue (#747).
    /// Zero when idle. Reported for observability when a tick hook times out
    /// so operators can tell whether the stuck worker is also holding up
    /// queued callers.
    pub fn actor_queue_depth(&self) -> usize {
        self.actor
            .queue_depth
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Short label identifying this engine instance (e.g. "main", "tick").
    pub fn actor_label(&self) -> &'static str {
        self.actor.label
    }

    pub fn downgrade(&self) -> PolicyEngineHandle {
        PolicyEngineHandle {
            inner: Arc::downgrade(&self.inner),
            actor: Arc::downgrade(&self.actor),
            runtime_deps: Arc::downgrade(&self.runtime_deps),
            tick_hook_in_flight: self.tick_hook_in_flight.clone(),
        }
    }

    pub(crate) fn is_actor_thread(&self) -> bool {
        self.actor.is_actor_thread()
    }

    pub(crate) fn tick_hook_in_flight(&self) -> Arc<AtomicBool> {
        self.tick_hook_in_flight.clone()
    }

    pub(crate) fn pg_pool(&self) -> Option<&sqlx::PgPool> {
        self.runtime_deps.pg_pool.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn legacy_db(&self) -> Option<&Db> {
        self.runtime_deps.legacy_db.as_ref()
    }

    fn roundtrip<T>(&self, command: impl FnOnce(mpsc::Sender<T>) -> EngineCommand) -> Result<T> {
        let (reply_tx, reply_rx) = mpsc::channel();
        // Approximate queue depth is bumped before the send. The actor drops it
        // back down when it pops the command off the channel (#747).
        self.actor
            .queue_depth
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if let Err(e) = self.actor.tx.send(command(reply_tx)) {
            // Send failed — the actor is gone, undo our increment so the gauge
            // does not drift upward forever.
            self.actor
                .queue_depth
                .fetch_update(
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                    |d| Some(d.saturating_sub(1)),
                )
                .ok();
            return Err(anyhow::anyhow!("policy engine actor is unavailable: {e}"));
        }
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

        if let Some(pool) = self.pg_pool().cloned() {
            self.drain_startup_hooks_inline_pg(pool);
            return;
        }

        #[cfg(not(test))]
        {
            return;
        }

        #[cfg(test)]
        {
            let Some(legacy_db) = self.legacy_db() else {
                return;
            };

            // Record server boot time for orphan recovery grace period
            if let Ok(conn) = legacy_db.separate_conn() {
                conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('server_boot_at', datetime('now'))",
                [],
            )
            .ok();
            }

            loop {
                let hooks: Vec<(i64, String, String)> = {
                    let conn = match legacy_db.separate_conn() {
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
                        if let Ok(conn) = legacy_db.separate_conn() {
                            let _ = conn.execute(
                                "UPDATE deferred_hooks SET status = 'pending' WHERE id = ?1",
                                [id],
                            );
                        }
                        continue;
                    }
                    // Success — delete from DB
                    if let Ok(conn) = legacy_db.separate_conn() {
                        let _ = conn.execute("DELETE FROM deferred_hooks WHERE id = ?1", [id]);
                    }
                }
            }
        }
    }

    fn drain_startup_hooks_inline_pg(&self, pool: sqlx::PgPool) {
        let _ = crate::utils::async_bridge::block_on_pg_result(
            &pool,
            move |bridge_pool| async move {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value)
                     VALUES ('server_boot_at', NOW()::TEXT)
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                )
                .execute(&bridge_pool)
                .await
                .map_err(|error| format!("upsert postgres server_boot_at: {error}"))?;
                Ok::<(), String>(())
            },
            |error| {
                tracing::warn!("failed to upsert postgres server_boot_at: {error}");
                error
            },
        );

        loop {
            let fetch_result = crate::utils::async_bridge::block_on_pg_result(
                &pool,
                move |bridge_pool| async move {
                    let rows = sqlx::query(
                        "SELECT id, hook_name, payload
                         FROM deferred_hooks
                         WHERE status IN ('pending', 'processing')
                         ORDER BY id ASC
                         LIMIT 50",
                    )
                    .fetch_all(&bridge_pool)
                    .await
                    .map_err(|error| format!("load postgres deferred hooks: {error}"))?;
                    let hooks = rows
                        .into_iter()
                        .map(|row| {
                            Ok::<_, String>((
                                row.try_get::<i64, _>("id")
                                    .map_err(|error| format!("decode deferred hook id: {error}"))?,
                                row.try_get::<String, _>("hook_name").map_err(|error| {
                                    format!("decode deferred hook hook_name: {error}")
                                })?,
                                row.try_get::<String, _>("payload").map_err(|error| {
                                    format!("decode deferred hook payload: {error}")
                                })?,
                            ))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    for (id, _, _) in &hooks {
                        sqlx::query(
                            "UPDATE deferred_hooks SET status = 'processing' WHERE id = $1",
                        )
                        .bind(*id)
                        .execute(&bridge_pool)
                        .await
                        .map_err(|error| {
                            format!("mark postgres deferred hook {id} processing: {error}")
                        })?;
                    }
                    Ok(hooks)
                },
                |error| error,
            );
            let hooks: Vec<(i64, String, String)> = match fetch_result {
                Ok(hooks) => hooks,
                Err(error) => {
                    tracing::warn!("failed to load postgres deferred hooks: {error}");
                    return;
                }
            };
            if hooks.is_empty() {
                return;
            }

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

                let update_result = if fire_result.is_err() {
                    crate::utils::async_bridge::block_on_pg_result(
                        &pool,
                        {
                            let id = *id;
                            move |bridge_pool| async move {
                                sqlx::query(
                                    "UPDATE deferred_hooks SET status = 'pending' WHERE id = $1",
                                )
                                .bind(id)
                                .execute(&bridge_pool)
                                .await
                                .map_err(|error| {
                                    format!("mark postgres deferred hook {id} pending: {error}")
                                })?;
                                Ok(())
                            }
                        },
                        |error| error,
                    )
                } else {
                    crate::utils::async_bridge::block_on_pg_result(
                        &pool,
                        {
                            let id = *id;
                            move |bridge_pool| async move {
                                sqlx::query("DELETE FROM deferred_hooks WHERE id = $1")
                                    .bind(id)
                                    .execute(&bridge_pool)
                                    .await
                                    .map_err(|error| {
                                        format!("delete postgres deferred hook {id}: {error}")
                                    })?;
                                Ok(())
                            }
                        },
                        |error| error,
                    )
                };

                if let Err(error) = fire_result {
                    tracing::warn!("[startup] deferred hook {hook_name} failed: {error}");
                    if let Err(update_error) = update_result {
                        tracing::warn!(
                            "failed to restore postgres deferred hook {} after replay failure: {}",
                            id,
                            update_error
                        );
                    }
                    continue;
                }

                if let Err(update_error) = update_result {
                    tracing::warn!(
                        "failed to delete postgres deferred hook {} after replay success: {}",
                        id,
                        update_error
                    );
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
                crate::kanban::fire_transition_hooks_with_backends(
                    #[cfg(test)]
                    self.legacy_db(),
                    #[cfg(not(test))]
                    None,
                    self.pg_pool(),
                    self,
                    card_id,
                    old_status,
                    new_status,
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

        let hook_fns: Vec<(String, String, Persistent<Function<'static>>)> = policies
            .iter()
            .filter_map(|p| {
                p.dynamic_hooks
                    .get(hook_name)
                    .map(|f| (p.name.clone(), p.policy_version.clone(), f.clone()))
            })
            .collect();
        drop(policies);

        if hook_fns.is_empty() {
            return Ok(());
        }

        let names: Vec<&str> = hook_fns.iter().map(|(n, _, _)| n.as_str()).collect();
        tracing::info!(policy_count = hook_fns.len(), policies = ?names, "firing dynamic hook");

        inner.context.with(|ctx| -> Result<()> {
            let js_payload = json_to_js(&ctx, &payload)?;

            for (policy_name, policy_version, persistent_fn) in &hook_fns {
                let func = match persistent_fn.clone().restore(&ctx) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!(
                            "Failed to restore dynamic hook {hook_name} for policy '{policy_name}': {e}"
                        );
                        continue;
                    }
                };

                let effects_before = Self::count_pending_effects(&ctx);
                let policy_start = std::time::Instant::now();
                let result: rquickjs::Result<rquickjs::Value> = func.call((js_payload.clone(),));
                let elapsed = policy_start.elapsed();
                let effects_after = Self::count_pending_effects(&ctx);
                let effects_count = effects_after.saturating_sub(effects_before);
                if elapsed >= POLICY_HOOK_WARN_THRESHOLD {
                    tracing::warn!(
                        policy_name,
                        hook_name,
                        elapsed_ms = elapsed.as_millis(),
                        "policy hook slow"
                    );
                } else {
                    tracing::debug!(
                        policy_name,
                        hook_name,
                        elapsed_ms = elapsed.as_millis(),
                        "policy hook completed"
                    );
                }
                let exec_result_str: &str;
                if let Err(ref e) = result {
                    exec_result_str = "err";
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
                } else {
                    exec_result_str = "ok";
                }

                Self::record_policy_hook_event(
                    policy_name,
                    hook_name,
                    policy_version,
                    elapsed,
                    exec_result_str,
                    effects_count,
                );
            }

            Ok(())
        })
    }

    /// Sample effect accumulators on the JS side (pending intents + pending
    /// card transitions). Used by the hook observability wrapper to compute an
    /// `effects_count` delta per policy invocation (#1080).
    fn count_pending_effects(ctx: &rquickjs::Ctx<'_>) -> u64 {
        let code = r#"
            (function() {
                var a = (typeof agentdesk !== "undefined" && agentdesk.__pendingIntents) ? agentdesk.__pendingIntents.length : 0;
                var b = (typeof agentdesk !== "undefined" && agentdesk.kanban && agentdesk.kanban.__pendingTransitions) ? agentdesk.kanban.__pendingTransitions.length : 0;
                return a + b;
            })();
        "#;
        let result: rquickjs::Result<i64> = ctx.eval(code);
        result.map(|v| v.max(0) as u64).unwrap_or(0)
    }

    /// Emit a structured `policy_hook_executed` event into the observability
    /// ring buffer (#1080).
    fn record_policy_hook_event(
        policy_name: &str,
        hook_name: &str,
        policy_version: &str,
        elapsed: std::time::Duration,
        result: &str,
        effects_count: u64,
    ) {
        let payload = serde_json::json!({
            "policy_name": policy_name,
            "hook_name": hook_name,
            "policy_version": policy_version,
            "duration_ms": elapsed.as_millis() as u64,
            "result": result,
            "effects_count": effects_count,
        });
        crate::services::observability::events::record_simple(
            "policy_hook_executed",
            None,
            None,
            payload,
        );
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

        let hook_fns: Vec<(String, String, Persistent<Function<'static>>)> = policies
            .iter()
            .filter_map(|p| {
                p.hooks.get(&hook).map(|f: &Persistent<Function<'static>>| {
                    (p.name.clone(), p.policy_version.clone(), f.clone())
                })
            })
            .collect();
        drop(policies);

        if hook_fns.is_empty() {
            return Ok(());
        }

        let names: Vec<&str> = hook_fns.iter().map(|(n, _, _)| n.as_str()).collect();
        tracing::info!(
            policy_count = hook_fns.len(),
            policies = ?names,
            hook = %hook,
            "firing hook"
        );

        let hook_name_str = hook.to_string();

        // Execute each hook function in the QuickJS context
        inner.context.with(|ctx| -> Result<()> {
            // Convert serde_json::Value to a JS value
            let js_payload = json_to_js(&ctx, &payload)?;

            for (policy_name, policy_version, persistent_fn) in &hook_fns {
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

                let effects_before = Self::count_pending_effects(&ctx);
                let policy_start = std::time::Instant::now();
                let result: rquickjs::Result<rquickjs::Value> = func.call((js_payload.clone(),));
                let elapsed = policy_start.elapsed();
                let effects_after = Self::count_pending_effects(&ctx);
                let effects_count = effects_after.saturating_sub(effects_before);
                if elapsed >= POLICY_HOOK_WARN_THRESHOLD {
                    tracing::warn!(
                        policy_name,
                        hook = %hook,
                        elapsed_ms = elapsed.as_millis(),
                        "policy hook slow"
                    );
                } else {
                    tracing::debug!(
                        policy_name,
                        hook = %hook,
                        elapsed_ms = elapsed.as_millis(),
                        "policy hook completed"
                    );
                }
                let exec_result_str: &str;
                if let Err(ref e) = result {
                    exec_result_str = "err";
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
                } else {
                    exec_result_str = "ok";
                }

                Self::record_policy_hook_event(
                    policy_name,
                    &hook_name_str,
                    policy_version,
                    elapsed,
                    exec_result_str,
                    effects_count,
                );
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
            intent::execute_intents_with_backends(
                #[cfg(test)]
                self.legacy_db(),
                #[cfg(not(test))]
                None,
                self.pg_pool(),
                Some(self),
                intents,
            )
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

    /// Run a closure with a clone of the JS context (test-only helper used by
    /// hook-orchestration pre-validation tests, #1079).
    #[cfg(test)]
    pub(crate) fn with_js_context<R>(&self, f: impl FnOnce(&Context) -> R) -> Result<R> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("engine lock poisoned: {e}"))?;
        Ok(f(&inner.context))
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
    pub(crate) fn block_actor_for_test(
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
            runtime_deps: self.runtime_deps.upgrade()?,
            tick_hook_in_flight: self.tick_hook_in_flight.clone(),
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

    fn test_engine_with_pg(_db: &Db, pg_pool: sqlx::PgPool) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.hot_reload = false;
        PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    fn test_engine_with_pg_and_config(config: Config, pg_pool: sqlx::PgPool) -> PolicyEngine {
        PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
        cleanup_armed: bool,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_engine_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(&admin_url, &database_name, "engine tests")
                .await
                .unwrap();

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
                cleanup_armed: true,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "engine tests")
                .await
                .unwrap()
        }

        async fn drop(mut self) {
            let drop_result = crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "engine tests",
            )
            .await;
            if drop_result.is_ok() {
                self.cleanup_armed = false;
            }
            drop_result.expect("drop postgres test db");
        }
    }

    impl Drop for TestPostgresDb {
        fn drop(&mut self) {
            if !self.cleanup_armed {
                return;
            }

            cleanup_test_postgres_db_from_drop(self.admin_url.clone(), self.database_name.clone());
        }
    }

    fn cleanup_test_postgres_db_from_drop(admin_url: String, database_name: String) {
        let cleanup_database_name = database_name.clone();
        let thread_name = format!("engine tests cleanup {cleanup_database_name}");
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        eprintln!(
                            "engine tests cleanup runtime failed for {database_name}: {error}"
                        );
                        return;
                    }
                };

                if let Err(error) = runtime.block_on(crate::db::postgres::drop_test_database(
                    &admin_url,
                    &database_name,
                    "engine tests",
                )) {
                    eprintln!("engine tests cleanup failed for {database_name}: {error}");
                }
            });

        match spawn_result {
            Ok(handle) => {
                if handle.join().is_err() {
                    eprintln!("engine tests cleanup thread panicked for {cleanup_database_name}");
                }
            }
            Err(error) => {
                eprintln!(
                    "engine tests cleanup thread spawn failed for {cleanup_database_name}: {error}"
                );
            }
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[test]
    fn test_engine_creates_runtime() {
        let db = test_db();
        let config = test_config();
        let engine = PolicyEngine::new_with_legacy_db(&config, db);
        assert!(engine.is_ok(), "Engine should initialize without error");
    }

    #[test]
    fn test_engine_evaluates_js() {
        let db = test_db();
        let config = test_config();
        let engine = PolicyEngine::new_with_legacy_db(&config, db).unwrap();
        let result: i32 = engine.eval_js("1 + 2").unwrap();
        assert_eq!(result, 3);
    }

    #[test]
    fn test_engine_handle_upgrade_preserves_runtime_compat_deps() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('h1', 'HandleBot', 'claude', 'idle', 7)",
                [],
            )
            .unwrap();
        }

        let config = test_config();
        let engine = PolicyEngine::new_with_legacy_db(&config, db).unwrap();
        let handle = engine.downgrade();
        let upgraded = handle.upgrade().expect("policy engine upgrade");

        let xp: i32 = upgraded
            .eval_js(r#"agentdesk.db.query("SELECT xp FROM agents WHERE id = 'h1'")[0].xp"#)
            .unwrap();
        assert_eq!(xp, 7);
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
        let engine = PolicyEngine::new_with_legacy_db(&config, db).unwrap();
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
        let engine = PolicyEngine::new_with_legacy_db(&config, db).unwrap();

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
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();

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
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();
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
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();

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
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();

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
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();

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
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();

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
                        {
                            reset_provider_state: true,
                            recreate_tmux: false,
                            force_new_session: true,
                            reset_reason: "repeated_findings"
                        }
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
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();

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
        assert_eq!(context_json["reset_provider_state"], true);
        assert_eq!(context_json["recreate_tmux"], false);
        assert_eq!(context_json["reset_reason"], "repeated_findings");
    }

    // #1342 ci-red: this test queues an OnReviewEnter hook through the actor
    // queue while the actor itself is blocked on `block_actor_for_test`. The
    // queued hook then calls `agentdesk.kanban.setStatus(..., "done", true)`,
    // which on the PG backend goes through `block_on_pg_result` and needs the
    // tokio runtime that owns the source pool to keep making progress while
    // the test thread waits on `blocker.join()` / `queued.join()`. The
    // current_thread runtime that `#[tokio::test]` provides freezes the pool
    // because the only worker is blocked on those joins, so the JS bridge's
    // `block_in_place + handle.block_on` cannot acquire a connection. A
    // multi-threaded runtime keeps the pool drivers alive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_review_enter_replays_terminal_transition_hooks() {
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

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, priority) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind("card-deferred-review")
        .bind("Deferred review")
        .bind("review")
        .bind("medium")
        .execute(&pool)
        .await
        .unwrap();

        let config = test_config_with_dir(dir.path());
        let engine = test_engine_with_pg_and_config(config, pool.clone());

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

        let deferred_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deferred_hooks")
            .fetch_one(&pool)
            .await
            .expect("deferred_hooks table should be readable");
        assert_eq!(
            deferred_count, 0,
            "queued hook execution must not fall back to deferred_hooks"
        );
        release_tx.send(()).unwrap();
        blocker.join().unwrap();
        queued.join().unwrap();

        let status: String = sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = $1")
            .bind("card-deferred-review")
            .fetch_one(&pool)
            .await
            .expect("queued review hook should still move the card to done");
        assert_eq!(status, "done");

        let terminal_marker: String =
            sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
                .bind("terminal_fired")
                .fetch_one(&pool)
                .await
                .expect("terminal follow-up hook must fire for queued review transitions");
        assert_eq!(terminal_marker, "card-deferred-review");

        pool.close().await;
        pg_db.drop().await;
    }

    // ── Hook orchestration determinism (#1079) ──────────────────────────

    /// Helper: write a policy JS file into a tmpdir and return the dir.
    fn policy_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).unwrap();
        }
        dir
    }

    #[test]
    fn hook_orchestr_duplicate_priority_hook_rejected() {
        // Two policies share priority 200 AND onCardTerminal, with no
        // after/before annotation → pre-validation must reject.
        let dir = policy_dir(&[
            (
                "a.js",
                r#"agentdesk.registerPolicy({
                    name: "policy-a",
                    priority: 200,
                    onCardTerminal: function(p) {}
                });"#,
            ),
            (
                "b.js",
                r#"agentdesk.registerPolicy({
                    name: "policy-b",
                    priority: 200,
                    onCardTerminal: function(p) {}
                });"#,
            ),
        ]);

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new_with_legacy_db(&config, db).unwrap();
        // The validated path is what hot-reload uses.
        let result = engine
            .with_js_context(|ctx| loader::load_policies_from_dir_validated(ctx, dir.path()))
            .unwrap();
        assert!(
            result.is_err(),
            "expected conflict error for duplicate (priority, hook)"
        );
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("onCardTerminal") && err.contains("200"),
            "error should mention hook + priority: {err}"
        );
    }

    #[test]
    fn hook_orchestr_after_annotation_disambiguates() {
        // Same collision as above, but policy-b declares `after: ["policy-a"]`.
        // Pre-validation must accept and topological order puts a before b.
        let dir = policy_dir(&[
            (
                "a.js",
                r#"agentdesk.registerPolicy({
                    name: "policy-a",
                    priority: 200,
                    onCardTerminal: function(p) {}
                });"#,
            ),
            (
                "b.js",
                r#"agentdesk.registerPolicy({
                    name: "policy-b",
                    priority: 200,
                    after: ["policy-a"],
                    onCardTerminal: function(p) {}
                });"#,
            ),
        ]);

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new_with_legacy_db(&config, db).unwrap();
        let policies = engine
            .with_js_context(|ctx| loader::load_policies_from_dir_validated(ctx, dir.path()))
            .unwrap()
            .expect("after annotation should disambiguate");
        let order: Vec<&str> = policies.iter().map(|p| p.name.as_str()).collect();
        let a = order.iter().position(|n| *n == "policy-a").unwrap();
        let b = order.iter().position(|n| *n == "policy-b").unwrap();
        assert!(a < b, "policy-a must precede policy-b: {order:?}");
    }

    #[test]
    fn hook_orchestr_syntax_error_preserves_loaded_version() {
        // Load a valid policy, then simulate hot-reload with a broken file.
        // Pre-validation must fail and the previously loaded store stays
        // intact.
        let dir = policy_dir(&[(
            "good.js",
            r#"agentdesk.registerPolicy({
                name: "good-policy",
                priority: 100,
                onCardTerminal: function(p) {}
            });"#,
        )]);

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new_with_legacy_db(&config, db).unwrap();

        // Pre-condition: the good policy loaded.
        let loaded = engine
            .with_js_context(|ctx| loader::load_policies_from_dir_validated(ctx, dir.path()))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "good-policy");

        // Inject a syntactically broken file.
        std::fs::write(
            dir.path().join("broken.js"),
            "this is not valid javascript $$$$ {",
        )
        .unwrap();

        let result = engine
            .with_js_context(|ctx| loader::load_policies_from_dir_validated(ctx, dir.path()))
            .unwrap();
        assert!(
            result.is_err(),
            "pre-validation must reject broken.js instead of half-swapping"
        );
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("broken.js"),
            "error should name the broken file: {err}"
        );
    }

    #[test]
    fn hook_orchestr_pipeline_merge_automation_no_p200_conflict() {
        // Guard against regression of the #1079 fix: pipeline.js and
        // merge-automation.js must not share (priority, hook).
        // We reconstruct the conflict detector input with just metadata.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pipeline.js"),
            r#"agentdesk.registerPolicy({
                name: "pipeline",
                priority: 200,
                onCardTransition: function(p) {},
                onDispatchCompleted: function(p) {}
            });"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("merge-automation.js"),
            r#"agentdesk.registerPolicy({
                name: "merge-automation",
                priority: 201,
                onCardTerminal: function(p) {},
                onTick5min: function() {}
            });"#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new_with_legacy_db(&config, db).unwrap();
        let policies = engine
            .with_js_context(|ctx| loader::load_policies_from_dir_validated(ctx, dir.path()))
            .unwrap()
            .expect("pipeline + merge-automation must not conflict after #1079 fix");
        assert_eq!(policies.len(), 2);
        // pipeline (200) must come before merge-automation (201).
        let names: Vec<&str> = policies.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["pipeline", "merge-automation"]);
    }

    // ────────────────────────────────────────────────────────────────
    // #1080 policy hook observability tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn policy_hook_observability_records_duration_and_ok_result() {
        crate::services::observability::events::reset_for_tests();

        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("obs-ok-policy.js");
        std::fs::write(
            &policy_path,
            r#"
            var policy = {
                name: "obs-ok-policy",
                priority: 1,
                onTick: function(payload) {
                    // Do nothing — just complete successfully.
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();

        let events: Vec<_> = crate::services::observability::events::recent(100)
            .into_iter()
            .filter(|e| e.event_type == "policy_hook_executed")
            .collect();
        assert!(
            !events.is_empty(),
            "at least one policy_hook_executed event should be recorded"
        );
        let ev = events
            .iter()
            .find(|e| {
                e.payload.get("policy_name").and_then(|v| v.as_str()) == Some("obs-ok-policy")
            })
            .expect("event for obs-ok-policy missing");
        assert_eq!(
            ev.payload.get("hook_name").and_then(|v| v.as_str()),
            Some("onTick")
        );
        assert_eq!(
            ev.payload.get("result").and_then(|v| v.as_str()),
            Some("ok")
        );
        assert!(
            ev.payload
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .is_some(),
            "duration_ms must be present and numeric"
        );
        let version = ev
            .payload
            .get("policy_version")
            .and_then(|v| v.as_str())
            .expect("policy_version must be present");
        assert_eq!(
            version.len(),
            12,
            "policy_version is the 12-char hash prefix"
        );
    }

    #[test]
    fn policy_hook_observability_marks_err_on_exception() {
        crate::services::observability::events::reset_for_tests();

        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("obs-err-policy.js");
        std::fs::write(
            &policy_path,
            r#"
            var policy = {
                name: "obs-err-policy",
                priority: 1,
                onTick: function(payload) {
                    throw new Error("intentional failure for observability test");
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();

        let ev = crate::services::observability::events::recent(100)
            .into_iter()
            .find(|e| {
                e.event_type == "policy_hook_executed"
                    && e.payload.get("policy_name").and_then(|v| v.as_str())
                        == Some("obs-err-policy")
            })
            .expect("obs-err-policy event must be recorded even when hook throws");
        assert_eq!(
            ev.payload.get("result").and_then(|v| v.as_str()),
            Some("err"),
            "result should be 'err' when the hook function throws"
        );
    }

    #[test]
    fn policy_hook_observability_counts_effects() {
        crate::services::observability::events::reset_for_tests();

        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("obs-effects-policy.js");
        // The card must exist so the intent can validate. We use a dynamic
        // hook + agentdesk.queueIntent to synthesize effects via the
        // `__pendingIntents` queue the counter inspects.
        std::fs::write(
            &policy_path,
            r#"
            var policy = {
                name: "obs-effects-policy",
                priority: 1,
                onTick: function(payload) {
                    // Push two fake intents onto the pending queue directly so
                    // the observability effects counter sees them. We cannot
                    // rely on real intents here because they need DB state.
                    if (!agentdesk.__pendingIntents) { agentdesk.__pendingIntents = []; }
                    agentdesk.__pendingIntents.push({ kind: "test", n: 1 });
                    agentdesk.__pendingIntents.push({ kind: "test", n: 2 });
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();

        let ev = crate::services::observability::events::recent(100)
            .into_iter()
            .find(|e| {
                e.event_type == "policy_hook_executed"
                    && e.payload.get("policy_name").and_then(|v| v.as_str())
                        == Some("obs-effects-policy")
            })
            .expect("obs-effects-policy event must be recorded");
        assert_eq!(
            ev.payload.get("effects_count").and_then(|v| v.as_u64()),
            Some(2),
            "effects_count should equal the number of intents queued"
        );
    }

    #[test]
    fn policy_hook_observability_policy_version_stable_across_invocations() {
        crate::services::observability::events::reset_for_tests();

        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("obs-version-policy.js");
        std::fs::write(
            &policy_path,
            r#"
            var policy = {
                name: "obs-version-policy",
                priority: 1,
                onTick: function(payload) {}
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let config = test_config_with_dir(dir.path());
        let engine = PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap();
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();

        let versions: std::collections::HashSet<String> =
            crate::services::observability::events::recent(500)
                .into_iter()
                .filter(|e| {
                    e.event_type == "policy_hook_executed"
                        && e.payload.get("policy_name").and_then(|v| v.as_str())
                            == Some("obs-version-policy")
                })
                .filter_map(|e| {
                    e.payload
                        .get("policy_version")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect();
        assert_eq!(
            versions.len(),
            1,
            "policy_version must be stable across invocations without hot-reload"
        );
    }
}
