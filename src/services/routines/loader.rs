use anyhow::{Result, anyhow};
use rquickjs::{Context, Function, Runtime};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::engine::loader::compute_policy_version;

mod discovery;
use discovery::{
    add_cached_candidates_for_root, candidate_failure_key, collect_routine_script_paths,
    routine_roots_identity, script_ref,
};

fn full_source_version(source: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(source.as_bytes()))
}

#[inline]
fn recover_poisoned_lock<T>(e: PoisonError<T>) -> T {
    tracing::warn!("RoutineScriptLoader lock was poisoned, recovering");
    e.into_inner()
}

#[derive(Debug)]
pub struct LoadedRoutineScript {
    pub name: String,
    pub script_ref: String,
    pub file: PathBuf,
    pub script_version: String,
    pub metadata: Value,
    source: String,
}

impl Clone for LoadedRoutineScript {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            script_ref: self.script_ref.clone(),
            file: self.file.clone(),
            script_version: self.script_version.clone(),
            metadata: self.metadata.clone(),
            source: self.source.clone(),
        }
    }
}

pub type RoutineScriptStore = Arc<Mutex<HashMap<String, LoadedRoutineScript>>>;

#[derive(Debug)]
struct RoutineScriptCandidate {
    root_index: usize,
    root: PathBuf,
    path: PathBuf,
    cached: Option<LoadedRoutineScript>,
}

pub const MAX_OBSERVATIONS_PER_TICK: usize = 100;
pub const MAX_OBSERVATION_PAYLOAD_BYTES: usize = 65536;
pub const MAX_AUTOMATION_INVENTORY_ITEMS: usize = 100;
pub const MAX_AUTOMATION_INVENTORY_PAYLOAD_BYTES: usize = 32768;
pub const ROUTINE_TICK_ERROR_PUBLIC_REASON: &str = "routine_tick_exception";
const LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF: &str = "monitoring/automation-executor-v2.js";
const CANONICAL_AUTOMATION_CANDIDATE_EXECUTOR_REF: &str =
    "monitoring/automation-candidate-executor.js";
const ROUTINE_LOAD_RETRY_BASE: Duration = Duration::from_secs(30);
const ROUTINE_LOAD_RETRY_MAX: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone)]
struct RoutineScriptFailure {
    source_version: Option<String>,
    consecutive_failures: u32,
    retry_at: Instant,
    warning_emitted: bool,
}

struct SharedRoutineLoaderState {
    scripts: RoutineScriptStore,
    failed_scripts: Mutex<HashMap<PathBuf, RoutineScriptFailure>>,
    load_gate: Mutex<()>,
    #[cfg(test)]
    evaluation_attempts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    load_error_emissions: std::sync::atomic::AtomicUsize,
}

impl SharedRoutineLoaderState {
    fn new() -> Self {
        Self {
            scripts: Arc::new(Mutex::new(HashMap::new())),
            failed_scripts: Mutex::new(HashMap::new()),
            load_gate: Mutex::new(()),
            #[cfg(test)]
            evaluation_attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            load_error_emissions: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

static SHARED_LOADER_STATES: LazyLock<Mutex<HashMap<PathBuf, Arc<SharedRoutineLoaderState>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationLimits {
    pub max_observations_per_tick: usize,
    pub max_observation_payload_bytes: usize,
    pub max_automation_inventory_items: usize,
    pub max_automation_inventory_payload_bytes: usize,
}

impl Default for ObservationLimits {
    fn default() -> Self {
        Self {
            max_observations_per_tick: MAX_OBSERVATIONS_PER_TICK,
            max_observation_payload_bytes: MAX_OBSERVATION_PAYLOAD_BYTES,
            max_automation_inventory_items: MAX_AUTOMATION_INVENTORY_ITEMS,
            max_automation_inventory_payload_bytes: MAX_AUTOMATION_INVENTORY_PAYLOAD_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutineTickContext {
    pub routine: RoutineTickRoutine,
    pub run: RoutineTickRun,
    pub agent: Option<RoutineTickAgent>,
    pub checkpoint: Option<Value>,
    pub now: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observations: Option<Vec<Value>>,
    #[serde(
        rename = "automationInventory",
        skip_serializing_if = "Option::is_none"
    )]
    pub automation_inventory: Option<Vec<Value>>,
    pub limits: ObservationLimits,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutineTickRoutine {
    pub id: String,
    pub agent_id: Option<String>,
    pub script_ref: String,
    pub name: String,
    pub execution_strategy: String,
    pub fresh_context_guaranteed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutineTickRun {
    pub id: String,
    pub lease_expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutineTickAgent {
    pub id: String,
    pub status: String,
    pub is_idle: bool,
    pub current_task_id: Option<String>,
    pub current_thread_channel_id: Option<String>,
}

/// Isolated QuickJS loader for `agentdesk.routines.register({ name, tick })`.
///
/// This intentionally does not use the PolicyEngine store or
/// `agentdesk.registerPolicy()` namespace. Failed loads return an error before
/// mutating the store, so callers keep the last-known-good registry. Read and
/// evaluation failures use bounded exponential backoff; unchanged candidates
/// are always retried within one hour instead of remaining disabled for the
/// process lifetime.
pub struct RoutineScriptLoader {
    state: Arc<SharedRoutineLoaderState>,
    #[cfg(test)]
    source_read_hook: Mutex<Option<Arc<dyn Fn(&Path) + Send + Sync>>>,
    #[cfg(test)]
    source_reader: Mutex<Option<Arc<dyn Fn(&Path) -> std::io::Result<String> + Send + Sync>>>,
}

impl RoutineScriptLoader {
    #[cfg(test)]
    pub fn new() -> Result<Self> {
        Ok(Self::with_state(Arc::new(SharedRoutineLoaderState::new())))
    }

    pub fn new_shared(roots: &[PathBuf]) -> Result<Self> {
        let key = routine_roots_identity(roots);
        let mut states = SHARED_LOADER_STATES
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        states.retain(|_, state| Arc::strong_count(state) > 1);
        let state = states
            .entry(key)
            .or_insert_with(|| Arc::new(SharedRoutineLoaderState::new()))
            .clone();
        Ok(Self::with_state(state))
    }

    fn with_state(state: Arc<SharedRoutineLoaderState>) -> Self {
        Self {
            state,
            #[cfg(test)]
            source_read_hook: Mutex::new(None),
            #[cfg(test)]
            source_reader: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub fn load_script(&self, root: &Path, path: &Path) -> Result<String> {
        self.state
            .evaluation_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let script = load_single_routine_script(root, path)?;
        tracing::debug!(
            routine_script = %script.script_ref,
            name = %script.name,
            file = %script.file.display(),
            version = %script.script_version,
            "loaded routine script"
        );
        let script_ref = script.script_ref.clone();
        self.state
            .scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .insert(script_ref.clone(), script);
        Ok(script_ref)
    }

    #[cfg(test)]
    pub fn load_dir(&self, root: &Path) -> Result<usize> {
        self.load_dirs(&[root.to_path_buf()])
    }

    pub fn load_dirs(&self, roots: &[PathBuf]) -> Result<usize> {
        let _load_permit = self
            .state
            .load_gate
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        let mut seen_refs = HashSet::new();
        let mut candidates_by_ref: BTreeMap<String, Vec<RoutineScriptCandidate>> = BTreeMap::new();
        let existing_scripts: HashMap<String, LoadedRoutineScript> = self
            .state
            .scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .clone();

        for (root_index, root) in roots.iter().enumerate() {
            if !root.exists() {
                tracing::warn!("Routines directory does not exist: {}", root.display());
                add_cached_candidates_for_root(
                    &existing_scripts,
                    &mut candidates_by_ref,
                    &mut seen_refs,
                    root_index,
                    root,
                );
                continue;
            }

            let mut entries = Vec::new();
            if let Err(e) = collect_routine_script_paths(root, &existing_scripts, &mut entries) {
                tracing::warn!(
                    routines_dir = %root.display(),
                    error = %e,
                    "failed to scan routines directory; skipping root"
                );
                add_cached_candidates_for_root(
                    &existing_scripts,
                    &mut candidates_by_ref,
                    &mut seen_refs,
                    root_index,
                    root,
                );
                continue;
            }
            entries.sort();

            for path in entries {
                let script_ref = script_ref(root, &path);
                seen_refs.insert(script_ref.clone());
                candidates_by_ref
                    .entry(script_ref)
                    .or_default()
                    .push(RoutineScriptCandidate {
                        root_index,
                        root: root.clone(),
                        path,
                        cached: None,
                    });
            }
        }

        let existing_refs: HashSet<String> = existing_scripts.keys().cloned().collect();
        let candidate_paths: HashSet<PathBuf> = candidates_by_ref
            .values()
            .flat_map(|candidates| {
                candidates
                    .iter()
                    .map(|candidate| candidate_failure_key(&candidate.path))
            })
            .collect();

        let mut loaded = 0;
        let mut loaded_scripts = Vec::new();
        for (script_ref, candidates) in candidates_by_ref {
            let has_existing = existing_refs.contains(&script_ref);
            let mut selected = None;
            for candidate in candidates.iter().rev() {
                let candidate_key = candidate_failure_key(&candidate.path);
                if let Some(script) = &candidate.cached {
                    tracing::warn!(
                        routine_script = %script_ref,
                        root = %candidate.root.display(),
                        root_index = candidate.root_index,
                        "preserving cached routine script after root scan failure"
                    );
                    selected = Some((script.clone(), false));
                    break;
                }

                #[cfg(test)]
                let source_result = if let Some(reader) = self
                    .source_reader
                    .lock()
                    .unwrap_or_else(recover_poisoned_lock)
                    .clone()
                {
                    reader(&candidate.path)
                } else {
                    std::fs::read_to_string(&candidate.path)
                };
                #[cfg(not(test))]
                let source_result = std::fs::read_to_string(&candidate.path);
                let source = match source_result {
                    Ok(source) => source,
                    Err(e) => {
                        let now = Instant::now();
                        if self.should_retry_candidate(&candidate_key, None, now, has_existing) {
                            let retry_delay = self.record_failure(&candidate_key, None, now);
                            #[cfg(test)]
                            self.state
                                .load_error_emissions
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::error!(
                                routine_script = %candidate.path.display(),
                                error = %e,
                                retry_after_seconds = retry_delay.as_secs(),
                                "failed to read routine script; keeping last-known-good registry"
                            );
                        }
                        if has_existing {
                            break;
                        }
                        continue;
                    }
                };
                if let Some(script) = existing_scripts
                    .get(&script_ref)
                    .filter(|script| script.file == candidate.path && script.source == source)
                {
                    self.state
                        .failed_scripts
                        .lock()
                        .unwrap_or_else(recover_poisoned_lock)
                        .remove(&candidate_key);
                    selected = Some((script.clone(), false));
                    break;
                }
                let source_version = full_source_version(&source);
                #[cfg(test)]
                if let Some(hook) = self
                    .source_read_hook
                    .lock()
                    .unwrap_or_else(recover_poisoned_lock)
                    .clone()
                {
                    hook(&candidate.path);
                }
                let now = Instant::now();
                if !self.should_retry_candidate(
                    &candidate_key,
                    Some(&source_version),
                    now,
                    has_existing,
                ) {
                    if has_existing {
                        break;
                    }
                    continue;
                }

                #[cfg(test)]
                self.state
                    .evaluation_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                match load_single_routine_script_from_source(
                    &candidate.root,
                    &candidate.path,
                    source,
                ) {
                    Ok(script) => {
                        self.state
                            .failed_scripts
                            .lock()
                            .unwrap_or_else(recover_poisoned_lock)
                            .remove(&candidate_key);
                        if candidates.len() > 1 {
                            tracing::info!(
                                routine_script = %script_ref,
                                root = %candidate.root.display(),
                                root_index = candidate.root_index,
                                "selected routine script override"
                            );
                        }
                        selected = Some((script, true));
                        break;
                    }
                    Err(e) => {
                        let retry_delay =
                            self.record_failure(&candidate_key, Some(source_version.clone()), now);
                        #[cfg(test)]
                        self.state
                            .load_error_emissions
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::error!(
                            routine_script = %candidate.path.display(),
                            script_version = %source_version,
                            retry_after_seconds = retry_delay.as_secs(),
                            error = %e,
                            "failed to load routine script; keeping last-known-good registry"
                        );
                        if has_existing {
                            break;
                        }
                    }
                }
            }

            if let Some((script, fresh_load)) = selected {
                let script_ref = script.script_ref.clone();
                if fresh_load {
                    loaded += 1;
                    tracing::info!(routine_script = %script_ref, "loaded routine script");
                } else {
                    tracing::debug!(routine_script = %script_ref, "kept cached routine script");
                }
                loaded_scripts.push(script);
            }
        }

        self.state
            .failed_scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .retain(|path, _| candidate_paths.contains(path));
        let pruned = self.apply_dir_reload(loaded_scripts, &seen_refs)?;
        if pruned > 0 {
            tracing::info!(count = pruned, "pruned missing routine scripts");
        }

        Ok(loaded)
    }

    fn should_retry_candidate(
        &self,
        path: &Path,
        source_version: Option<&String>,
        now: Instant,
        has_existing: bool,
    ) -> bool {
        let mut failures = self
            .state
            .failed_scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        let Some(failure) = failures.get_mut(path) else {
            return true;
        };
        if failure.source_version.as_ref() != source_version {
            return true;
        }
        if now >= failure.retry_at {
            return true;
        }

        let retry_after = failure.retry_at.saturating_duration_since(now);
        if !has_existing && !failure.warning_emitted {
            tracing::warn!(
                routine_script = %path.display(),
                retry_after_seconds = retry_after.as_secs(),
                consecutive_failures = failure.consecutive_failures,
                "routine script is not loaded; retry deferred by failure backoff"
            );
            failure.warning_emitted = true;
        } else {
            tracing::debug!(
                routine_script = %path.display(),
                retry_after_seconds = retry_after.as_secs(),
                consecutive_failures = failure.consecutive_failures,
                "skipped routine script until failure backoff expires"
            );
        }
        false
    }

    fn record_failure(
        &self,
        path: &Path,
        source_version: Option<String>,
        now: Instant,
    ) -> Duration {
        let mut failures = self
            .state
            .failed_scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        let consecutive_failures = failures
            .get(path)
            .filter(|failure| failure.source_version == source_version)
            .map_or(1, |failure| failure.consecutive_failures.saturating_add(1));
        let exponent = consecutive_failures.saturating_sub(1).min(31);
        let retry_delay = ROUTINE_LOAD_RETRY_BASE
            .checked_mul(1_u32 << exponent)
            .unwrap_or(ROUTINE_LOAD_RETRY_MAX)
            .min(ROUTINE_LOAD_RETRY_MAX);
        failures.insert(
            path.to_path_buf(),
            RoutineScriptFailure {
                source_version,
                consecutive_failures,
                retry_at: now + retry_delay,
                warning_emitted: false,
            },
        );
        retry_delay
    }

    pub fn get_script(&self, script_ref: &str) -> Result<Option<LoadedRoutineScript>> {
        let scripts = self
            .state
            .scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        if let Some(script) = scripts.get(script_ref).cloned() {
            return Ok(Some(script));
        }
        if script_ref == LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF {
            return Ok(scripts
                .get(CANONICAL_AUTOMATION_CANDIDATE_EXECUTOR_REF)
                .cloned());
        }
        Ok(None)
    }

    pub fn execute_tick(
        &self,
        script_ref: &str,
        tick_context: RoutineTickContext,
    ) -> Result<crate::services::routines::RoutineAction> {
        let Some(script) = self.get_script(script_ref)? else {
            return Err(anyhow!("routine script {script_ref} is not loaded"));
        };
        let action_json = evaluate_tick_action(&script, &tick_context)?;
        crate::services::routines::RoutineAction::validate(action_json)
    }

    #[cfg(test)]
    pub fn has_script(&self, script_ref: &str) -> Result<bool> {
        Ok(self
            .state
            .scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .contains_key(script_ref))
    }

    pub fn script_refs(&self) -> Result<Vec<String>> {
        let mut refs: Vec<String> = self
            .state
            .scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .keys()
            .cloned()
            .collect();
        refs.sort();
        Ok(refs)
    }

    fn apply_dir_reload(
        &self,
        loaded_scripts: Vec<LoadedRoutineScript>,
        seen_refs: &HashSet<String>,
    ) -> Result<usize> {
        let mut scripts = self
            .state
            .scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        for script in loaded_scripts {
            scripts.insert(script.script_ref.clone(), script);
        }
        let before = scripts.len();
        scripts.retain(|script_ref, _| seen_refs.contains(script_ref));
        Ok(before.saturating_sub(scripts.len()))
    }
}

/// Script refs (root-relative `*.js` paths, `/`-separated) present on disk
/// under `roots`, using the same walk and register-call rule as
/// `RoutineScriptLoader::load_dirs` but without evaluating anything. Missing
/// or unreadable roots contribute nothing. Sorted and de-duplicated.
pub fn discover_routine_script_refs(roots: &[PathBuf]) -> Vec<String> {
    let mut refs = std::collections::BTreeSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let mut entries = Vec::new();
        if collect_routine_script_paths(root, &HashMap::new(), &mut entries).is_err() {
            continue;
        }
        for path in entries {
            refs.insert(script_ref(root, &path));
        }
    }
    refs.into_iter().collect()
}

/// Discovered script refs that have no `routines` row (any status) — files
/// an operator dropped into `routines.dir` but never attached via
/// `POST /api/routines`. Preserves the (sorted) discovery order.
pub fn unregistered_routine_script_refs(
    discovered: &[String],
    registered: &HashSet<String>,
) -> Vec<String> {
    discovered
        .iter()
        .filter(|script_ref| !registered.contains(script_ref.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
pub fn load_single_routine_script(root: &Path, path: &Path) -> Result<LoadedRoutineScript> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("read routine script {}: {e}", path.display()))?;
    load_single_routine_script_from_source(root, path, source)
}

fn load_single_routine_script_from_source(
    root: &Path,
    path: &Path,
    source: String,
) -> Result<LoadedRoutineScript> {
    let fallback_name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let script_ref = script_ref(root, path);
    let script_version = compute_policy_version(&source);

    let (name, metadata) =
        evaluate_routine_script_metadata(&source, &fallback_name, &script_ref, path)?;

    Ok(LoadedRoutineScript {
        name,
        script_ref,
        file: path.to_path_buf(),
        script_version,
        metadata,
        source,
    })
}

fn evaluate_routine_script_metadata(
    source: &str,
    fallback_name: &str,
    script_ref: &str,
    path: &Path,
) -> Result<(String, Value)> {
    let runtime =
        Runtime::new().map_err(|e| anyhow!("routine QuickJS runtime creation failed: {e}"))?;
    install_interrupt_handler(&runtime, Duration::from_secs(5));
    let context = Context::full(&runtime)
        .map_err(|e| anyhow!("routine QuickJS context creation failed: {e}"))?;

    context.with(|ctx| -> Result<(String, Value)> {
        let registration =
            capture_registered_routine(ctx.clone(), source, fallback_name, script_ref, path)?;
        Ok((registration.name, registration.metadata))
    })
}

fn evaluate_tick_action(
    script: &LoadedRoutineScript,
    tick_context: &RoutineTickContext,
) -> Result<Value> {
    let runtime =
        Runtime::new().map_err(|e| anyhow!("routine QuickJS runtime creation failed: {e}"))?;
    install_interrupt_handler(&runtime, Duration::from_secs(5));
    let context = Context::full(&runtime)
        .map_err(|e| anyhow!("routine QuickJS context creation failed: {e}"))?;
    let fallback_name = script
        .file
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    context.with(|ctx| -> Result<Value> {
        let registration = capture_registered_routine(
            ctx.clone(),
            &script.source,
            &fallback_name,
            &script.script_ref,
            &script.file,
        )?;
        let context_json = serde_json::to_string(tick_context)
            .map_err(|e| anyhow!("encode routine tick context: {e}"))?;
        let context_literal = serde_json::to_string(&context_json)
            .map_err(|e| anyhow!("encode routine tick context literal: {e}"))?;
        let js_context: rquickjs::Value = ctx
            .eval(format!("JSON.parse({context_literal})"))
            .map_err(|e| anyhow!("build routine tick context: {e}"))?;
        let action_value: rquickjs::Value = match registration.tick.call((js_context,)) {
            Ok(value) => value,
            Err(e) => {
                let detail = quickjs_exception_detail(&ctx, &e);
                return Err(anyhow!(
                    "routine script {} tick(ctx) failed: {detail}",
                    script.script_ref
                ));
            }
        };
        ensure_acyclic_js_value(ctx, action_value.clone(), "routine action")?;
        js_value_to_json(action_value)
    })
}

fn install_interrupt_handler(runtime: &Runtime, timeout: Duration) {
    let started = Instant::now();
    runtime.set_interrupt_handler(Some(Box::new(move || started.elapsed() > timeout)));
}

fn quickjs_exception_detail(ctx: &rquickjs::Ctx<'_>, error: &rquickjs::Error) -> String {
    let caught = ctx.catch();
    if let Some(exception) = caught.clone().into_exception() {
        let message = exception.message().unwrap_or_default();
        let stack = exception.stack().unwrap_or_default();
        return match (message.is_empty(), stack.is_empty()) {
            (false, false) => format!("{message}\n{stack}"),
            (false, true) => message,
            (true, false) => stack.trim_start().to_string(),
            (true, true) => error.to_string(),
        };
    }

    <rquickjs::convert::Coerced<String> as rquickjs::FromJs>::from_js(ctx, caught)
        .map(|rquickjs::convert::Coerced(detail)| detail)
        .ok()
        .filter(|detail| !detail.is_empty())
        .unwrap_or_else(|| error.to_string())
}

struct CapturedRoutineRegistration<'js> {
    name: String,
    tick: Function<'js>,
    metadata: Value,
}

fn capture_registered_routine<'js>(
    ctx: rquickjs::Ctx<'js>,
    source: &str,
    fallback_name: &str,
    script_ref: &str,
    path: &Path,
) -> Result<CapturedRoutineRegistration<'js>> {
    let globals = ctx.globals();
    let _: rquickjs::Value = ctx
        .eval(
            r#"
            globalThis.agentdesk = globalThis.agentdesk || {};
            agentdesk.routines = {};
            var __routineCapture = { captured: null };
            agentdesk.routines.register = function(obj) {
                __routineCapture.captured = obj;
            };
            "#,
        )
        .map_err(|e| anyhow!("failed to set up routine register capture: {e}"))?;

    let mut eval_opts = rquickjs::context::EvalOptions::default();
    eval_opts.strict = false;
    let eval_result: rquickjs::Result<rquickjs::Value> =
        ctx.eval_with_options(source.as_bytes().to_vec(), eval_opts);
    if let Err(e) = eval_result {
        let exception_detail = quickjs_exception_detail(&ctx, &e);
        return Err(anyhow!(
            "JS eval error in routine script {}: {exception_detail}",
            path.display()
        ));
    }

    let capture: rquickjs::Object = globals
        .get("__routineCapture")
        .map_err(|e| anyhow!("__routineCapture missing: {e}"))?;
    let captured: rquickjs::Value = capture
        .get("captured")
        .map_err(|e| anyhow!("get routine capture: {e}"))?;

    if captured.is_null() || captured.is_undefined() {
        return Err(anyhow!(
            "routine script {} did not call agentdesk.routines.register()",
            path.display()
        ));
    }

    let routine_obj = captured
        .into_object()
        .ok_or_else(|| anyhow!("agentdesk.routines.register argument is not an object"))?;

    let name: String = routine_obj
        .get::<_, rquickjs::Value>("name")
        .ok()
        .and_then(|v| v.as_string().and_then(|s| s.to_string().ok()))
        .unwrap_or_else(|| fallback_name.to_string());

    let tick_value: rquickjs::Value = routine_obj
        .get("tick")
        .map_err(|e| anyhow!("routine script {script_ref} missing tick(ctx): {e}"))?;
    if tick_value.is_null() || tick_value.is_undefined() {
        return Err(anyhow!("routine script {script_ref} missing tick(ctx)"));
    }
    if !tick_value.is_function() {
        return Err(anyhow!(
            "routine script {script_ref} tick must be a function"
        ));
    }
    let tick = tick_value
        .into_function()
        .ok_or_else(|| anyhow!("routine script {script_ref} tick must be a function"))?;

    let metadata = routine_obj
        .get::<_, rquickjs::Value>("metadata")
        .ok()
        .filter(|value| !value.is_null() && !value.is_undefined())
        .map(|value| {
            ensure_acyclic_js_value(ctx.clone(), value.clone(), "routine metadata")?;
            js_value_to_json(value)
        })
        .transpose()?
        .unwrap_or(Value::Null);

    Ok(CapturedRoutineRegistration {
        name,
        tick,
        metadata,
    })
}

fn js_value_to_json(value: rquickjs::Value<'_>) -> Result<Value> {
    if value.is_null() || value.is_undefined() {
        return Ok(Value::Null);
    }
    if let Some(value) = value.as_bool() {
        return Ok(Value::Bool(value));
    }
    if let Some(value) = value.as_int() {
        return Ok(Value::Number(Number::from(value)));
    }
    if let Some(value) = value.as_float() {
        let Some(number) = Number::from_f64(value) else {
            return Err(anyhow!("routine action contains non-finite number"));
        };
        return Ok(Value::Number(number));
    }
    if let Some(value) = value.as_string() {
        return Ok(Value::String(value.to_string().map_err(|e| {
            anyhow!("routine action string conversion failed: {e}")
        })?));
    }
    if value.is_array() {
        let array = value
            .into_array()
            .ok_or_else(|| anyhow!("routine action array conversion failed"))?;
        let mut out = Vec::with_capacity(array.len());
        for index in 0..array.len() {
            let item: rquickjs::Value = array
                .get(index)
                .map_err(|e| anyhow!("routine action array[{index}] conversion failed: {e}"))?;
            out.push(js_value_to_json(item)?);
        }
        return Ok(Value::Array(out));
    }
    if value.is_object() {
        let object = value
            .into_object()
            .ok_or_else(|| anyhow!("routine action object conversion failed"))?;
        let mut out = Map::new();
        for key in object.keys::<String>() {
            let key =
                key.map_err(|e| anyhow!("routine action object key conversion failed: {e}"))?;
            let item: rquickjs::Value = object
                .get(key.as_str())
                .map_err(|e| anyhow!("routine action field {key} conversion failed: {e}"))?;
            out.insert(key, js_value_to_json(item)?);
        }
        return Ok(Value::Object(out));
    }

    Err(anyhow!(
        "routine action returned unsupported JavaScript value"
    ))
}

fn ensure_acyclic_js_value<'js>(
    ctx: rquickjs::Ctx<'js>,
    value: rquickjs::Value<'js>,
    label: &'static str,
) -> Result<()> {
    let checker: rquickjs::Function = ctx
        .eval(
            r#"
            (value) => {
              const seen = new WeakSet();
              const visit = (item) => {
                if (item === null || typeof item !== "object") {
                  return;
                }
                if (seen.has(item)) {
                  throw new Error("value contains cyclic object graph");
                }
                seen.add(item);
                if (Array.isArray(item)) {
                  for (const child of item) {
                    visit(child);
                  }
                } else {
                  for (const key of Object.keys(item)) {
                    visit(item[key]);
                  }
                }
                seen.delete(item);
              };
              visit(value);
            }
            "#,
        )
        .map_err(|e| anyhow!("routine action cycle checker init failed: {e}"))?;
    if let Err(e) = checker.call::<_, ()>((value,)) {
        let detail = quickjs_exception_detail(&ctx, &e);
        return Err(anyhow!("{label} cycle check failed: {detail}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
