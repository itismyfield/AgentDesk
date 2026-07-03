pub mod hooks;
pub mod intent;
pub mod loader;
pub mod ops;
pub mod sql_guard;
pub mod transition;
pub mod transition_executor_pg;
pub(crate) mod transition_timeout;

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

use hooks::Hook;
use loader::PolicyStore;

const POLICY_HOOK_WARN_THRESHOLD: Duration = Duration::from_millis(100);
/// Tick hooks (onTick/onTick30s/onTick1min/onTick5min) routinely run auto-queue /
/// timeout sweeps measured at 500-600ms, so the flat 100ms threshold produced a
/// "policy hook slow" WARN every cycle. Use a dedicated, higher threshold for tick
/// hooks (aligned with `POLICY_TICK_WARN_MS` in server/mod.rs) while keeping the
/// tighter 100ms bound for non-tick hooks so their genuine slow-WARNs survive.
const POLICY_TICK_HOOK_WARN_THRESHOLD: Duration = Duration::from_millis(500);

/// Inner state of the policy engine (not Clone).
struct PolicyEngineInner {
    // Order matters for drop. Rust drops struct fields in declaration order,
    // so we list them from the JS side outwards:
    //   1. `policies`     — Persistent JS values; must be cleared before the
    //                        runtime is dropped (also done explicitly in
    //                        `Drop::drop` to cope with poisoned mutexes).
    //   2. `_hot_reload`  — Watcher + worker thread guard. Its `Drop`
    //                        joins the worker thread, which owns a clone of
    //                        the QuickJS `Context`. Joining here makes that
    //                        clone drop *before* we touch `context` or
    //                        `_runtime` below, preventing a stale `Context`
    //                        from outliving the runtime — the failure mode
    //                        that surfaced as a QuickJS C-assert during
    //                        review-decision CLI shutdown (#2200 sub-fix 2).
    //   3. `context`      — Engine's own Context.
    //   4. `_runtime`     — QuickJS runtime; must be dropped last so every
    //                        Context referencing it has already been dropped.
    policies: PolicyStore,
    _hot_reload: Option<loader::HotReloadGuard>,
    context: Context,
    eval_deadline: Arc<std::sync::atomic::AtomicU64>,
    hook_timeout: Option<Duration>,
    _runtime: Runtime,
}

impl Drop for PolicyEngineInner {
    fn drop(&mut self) {
        // Clear all persistent JS values before the runtime is dropped.
        if let Ok(mut guard) = self.policies.lock() {
            guard.clear();
        }
        // Proactively tear down the hot-reload worker so its `Context` clone
        // is dropped before this function returns and the remaining fields
        // (context, runtime) get dropped. Without this the worker would only
        // be torn down when `_hot_reload` is dropped via the normal field
        // drop order, which is safe in isolation but fragile under panics
        // and mutex poisoning. Explicit shutdown here is the belt to the
        // declaration-order suspenders.
        if let Some(mut guard) = self._hot_reload.take() {
            guard.shutdown();
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
        // Keep policy hooks outside a Tokio runtime so synchronous PG bridge calls
        // use their isolated bridge pools instead of contending for the shared pool.
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
    pg_pool: Option<sqlx::PgPool>,
}

/// Thread-safe handle to the policy engine. Cheap to clone.
#[derive(Clone)]
pub struct PolicyEngine {
    inner: Arc<Mutex<PolicyEngineInner>>,
    actor: Arc<PolicyEngineActor>,
    /// Runtime deps for bridge ops that need PostgreSQL access.
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
    // reason: non-PG constructor consumed only by cross-module test setups
    // (server route tests build engines via new_with_pg); kept as the public
    // no-runtime-deps entry point.
    #[allow(dead_code)]
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

    fn new_with_pg_and_label(
        config: &Config,
        pg_pool: Option<sqlx::PgPool>,
        label: &'static str,
    ) -> Result<Self> {
        let runtime_deps = Arc::new(PolicyEngineRuntimeDeps {
            pg_pool: pg_pool.clone(),
        });
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
        let policy_hardening_disabled = std::env::var("AGENTDESK_POLICY_HARDENING")
            .ok()
            .is_some_and(|value| matches!(value.trim(), "0" | "off" | "OFF" | "false" | "FALSE"));
        if !policy_hardening_disabled && config.policies.memory_limit_bytes > 0 {
            runtime.set_memory_limit(config.policies.memory_limit_bytes);
        }
        let eval_deadline = loader::new_eval_deadline_slot();
        loader::install_policy_interrupt_handler(&runtime, eval_deadline.clone(), None);
        let hook_timeout = (!policy_hardening_disabled && config.policies.hook_timeout_ms > 0)
            .then(|| Duration::from_millis(config.policies.hook_timeout_ms));
        let context = Context::full(&runtime)
            .map_err(|e| anyhow::anyhow!("QuickJS context creation failed: {e}"))?;

        // Register bridge ops (agentdesk.*)
        context.with(|ctx| {
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
                ops::register_globals_with_supervisor_and_pg(
                    &ctx,
                    runtime_deps.pg_pool.clone(),
                    supervisor_bridge.clone(),
                )
                .map_err(|e| anyhow::anyhow!("Failed to register bridge ops in reload ctx: {e}"))
            })?;

            match loader::start_hot_reload(
                policies_dir.clone(),
                reload_ctx,
                store.clone(),
                eval_deadline.clone(),
            ) {
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
            policies: store,
            _hot_reload: watcher,
            context,
            eval_deadline,
            hook_timeout,
            _runtime: runtime,
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

        {
            return;
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

                let _deadline_guard = inner
                    .hook_timeout
                    .map(|budget| loader::ArmedDeadline::new(&inner.eval_deadline, budget));
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

                let _deadline_guard = inner
                    .hook_timeout
                    .map(|budget| loader::ArmedDeadline::new(&inner.eval_deadline, budget));
                let effects_before = Self::count_pending_effects(&ctx);
                let policy_start = std::time::Instant::now();
                let result: rquickjs::Result<rquickjs::Value> = func.call((js_payload.clone(),));
                let elapsed = policy_start.elapsed();
                let effects_after = Self::count_pending_effects(&ctx);
                let effects_count = effects_after.saturating_sub(effects_before);
                let warn_threshold = if matches!(
                    hook,
                    Hook::OnTick | Hook::OnTick30s | Hook::OnTick1min | Hook::OnTick5min
                ) {
                    POLICY_TICK_HOOK_WARN_THRESHOLD
                } else {
                    POLICY_HOOK_WARN_THRESHOLD
                };
                if elapsed >= warn_threshold {
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
            intent::execute_intents_with_backends(self.pg_pool(), Some(self), intents)
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

    /// Evaluate arbitrary JS in the engine context (test-only helper that does
    /// not depend on the legacy SQLite fixture path).
    #[cfg(test)]
    pub(crate) fn eval_js<T: for<'js> rquickjs::FromJs<'js> + Send>(
        &self,
        code: &str,
    ) -> Result<T> {
        let inner = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("engine lock poisoned: {e}"))?;
        let code_owned = code.to_string();
        inner.context.with(|ctx| {
            let result: T = ctx.eval(code_owned.as_bytes().to_vec()).map_err(|e| {
                let exception_detail = ctx
                    .catch()
                    .into_exception()
                    .map(|ex| {
                        let msg = ex.message().unwrap_or_default();
                        let stack = ex.stack().unwrap_or_default();
                        format!("{msg}\n{stack}")
                    })
                    .unwrap_or_else(|| format!("{e}"));
                anyhow::anyhow!("JS eval error: {exception_detail}")
            })?;
            Ok(result)
        })
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
mod runtime_policy_tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            policies: crate::config::PoliciesConfig {
                dir: std::path::PathBuf::from("/nonexistent"),
                hot_reload: false,
                ..crate::config::PoliciesConfig::default()
            },
            ..Config::default()
        }
    }

    fn test_config_with_dir(dir: &std::path::Path) -> Config {
        Config {
            policies: crate::config::PoliciesConfig {
                dir: dir.to_path_buf(),
                hot_reload: false,
                ..crate::config::PoliciesConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn policy_runtime_memory_limit_rejects_large_allocation() {
        let mut config = test_config();
        config.policies.memory_limit_bytes = 8 * 1024 * 1024;
        let engine = PolicyEngine::new_with_pg(&config, None).unwrap();

        let allocation_failed: bool = engine
            .eval_js("try { new ArrayBuffer(64 * 1024 * 1024); false } catch (e) { true }")
            .unwrap();

        assert!(
            allocation_failed,
            "QuickJS memory limit should reject a large policy allocation"
        );
    }

    #[test]
    fn policy_hook_deadline_interrupts_runaway_live_hook() {
        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("runaway-hook.js");
        std::fs::write(
            &policy_path,
            r#"
            agentdesk.registerPolicy({
                name: "runaway-hook",
                priority: 1,
                onTick: function() {
                    while (true) {}
                }
            });
            "#,
        )
        .unwrap();

        let mut config = test_config_with_dir(dir.path());
        config.policies.hook_timeout_ms = 100;
        let engine = PolicyEngine::new_with_pg(&config, None).unwrap();

        let start = std::time::Instant::now();
        engine
            .fire_hook(Hook::OnTick, serde_json::json!({}))
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "runaway live hook should be interrupted promptly, took {elapsed:?}"
        );
    }
}
