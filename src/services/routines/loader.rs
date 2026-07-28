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
    DiscoveredRoutineScript, RoutineDiscoveryHooks, ValidatedHelperSurface,
    ValidatedRoutineRoot, ValidatedRuntimeRoot, add_cached_candidates_for_root,
    bind_routine_root_authority, collect_routine_script_paths, routine_roots_identity, script_ref,
    validate_routine_authority,
};
#[cfg(test)]
use discovery::{bind_routine_root_authority_with_hook, candidate_failure_key};

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
    failure_key: PathBuf,
    snapshot: Option<DiscoveredRoutineScript>,
    cached: Option<LoadedRoutineScript>,
}

#[cfg(test)]
type SourcePathHook = Arc<dyn Fn(&Path) + Send + Sync>;

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
    runtime_root: PathBuf,
    bound_runtime_root: Option<ValidatedRuntimeRoot>,
    bound_roots: Option<Vec<ValidatedRoutineRoot>>,
    bound_helper_surface: Option<ValidatedHelperSurface>,
    #[cfg(test)]
    source_read_hook: Mutex<Option<SourcePathHook>>,
    #[cfg(test)]
    source_read_observer: Mutex<Option<SourcePathHook>>,
    #[cfg(test)]
    before_source_read_hook: Mutex<Option<SourcePathHook>>,
    #[cfg(test)]
    before_candidate_open_hook: Mutex<Option<SourcePathHook>>,
    #[cfg(test)]
    before_scan_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    runtime_root_override: Mutex<Option<PathBuf>>,
    #[cfg(test)]
    current_dir_override: Mutex<Option<PathBuf>>,
}

impl RoutineScriptLoader {
    #[cfg(test)]
    pub fn new() -> Result<Self> {
        Ok(Self::with_state(
            Arc::new(SharedRoutineLoaderState::new()),
            test_runtime_root(),
            None,
            None,
            None,
        ))
    }

    pub fn new_shared(roots: &[PathBuf], runtime_root: &Path) -> Result<Self> {
        let (runtime_authority, validated_roots, helper_authority) =
            bind_routine_root_authority(roots, runtime_root)?;
        let key = routine_roots_identity(
            &runtime_authority,
            &validated_roots,
            &helper_authority,
        );
        let runtime_root = runtime_authority.canonical().to_path_buf();
        let mut states = SHARED_LOADER_STATES
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        states.retain(|_, state| Arc::strong_count(state) > 1);
        let state = states
            .entry(key)
            .or_insert_with(|| Arc::new(SharedRoutineLoaderState::new()))
            .clone();
        Ok(Self::with_state(
            state,
            runtime_root,
            Some(runtime_authority),
            Some(validated_roots),
            Some(helper_authority),
        ))
    }

    fn with_state(
        state: Arc<SharedRoutineLoaderState>,
        runtime_root: PathBuf,
        bound_runtime_root: Option<ValidatedRuntimeRoot>,
        bound_roots: Option<Vec<ValidatedRoutineRoot>>,
        bound_helper_surface: Option<ValidatedHelperSurface>,
    ) -> Self {
        Self {
            state,
            runtime_root,
            bound_runtime_root,
            bound_roots,
            bound_helper_surface,
            #[cfg(test)]
            source_read_hook: Mutex::new(None),
            #[cfg(test)]
            source_read_observer: Mutex::new(None),
            #[cfg(test)]
            before_source_read_hook: Mutex::new(None),
            #[cfg(test)]
            before_candidate_open_hook: Mutex::new(None),
            #[cfg(test)]
            before_scan_hook: Mutex::new(None),
            #[cfg(test)]
            runtime_root_override: Mutex::new(None),
            #[cfg(test)]
            current_dir_override: Mutex::new(None),
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

    // Backward-compatible single-directory shim for callers that have not
    // migrated to `load_dirs`.
    #[allow(dead_code)]
    pub fn load_dir(&self, root: &Path) -> Result<usize> {
        self.load_dirs(&[root.to_path_buf()])
    }

    fn verify_bound_authority(
        &self,
        runtime_root: &Path,
        current_dir_override: Option<&Path>,
    ) -> Result<()> {
        if let Some(runtime_authority) = &self.bound_runtime_root {
            runtime_authority.verify_current()?;
        }
        let Some(helper_authority) = &self.bound_helper_surface else {
            return Ok(());
        };
        let (_, observed) =
            validate_routine_authority(&[], runtime_root, current_dir_override)?;
        helper_authority.verify_observed(&observed)?;
        if let Some(runtime_authority) = &self.bound_runtime_root {
            runtime_authority.verify_current()?;
        }
        Ok(())
    }

    pub fn load_dirs(&self, roots: &[PathBuf]) -> Result<usize> {
        let _load_permit = self
            .state
            .load_gate
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        #[cfg(test)]
        let runtime_root = self
            .runtime_root_override
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .clone()
            .unwrap_or_else(|| self.runtime_root.clone());
        #[cfg(not(test))]
        let runtime_root = self.runtime_root.clone();
        #[cfg(test)]
        let current_dir_override = self
            .current_dir_override
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .clone();
        #[cfg(not(test))]
        let current_dir_override: Option<PathBuf> = None;
        self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())?;
        let (validated_roots, observed_helper_surface) = validate_routine_authority(
            roots,
            &runtime_root,
            current_dir_override.as_deref(),
        )?;
        if let Some(expected) = &self.bound_helper_surface {
            expected.verify_observed(&observed_helper_surface)?;
        }
        if let Some(bound_roots) = &self.bound_roots {
            if bound_roots.len() != validated_roots.len() {
                return Err(discovery::RoutineRootValidationError::ConfiguredRootCountChanged {
                    expected: bound_roots.len(),
                    observed: validated_roots.len(),
                }
                .into());
            }
            for (root, expected) in validated_roots.iter().zip(bound_roots) {
                if root.canonical != expected.canonical {
                    return Err(discovery::RoutineRootValidationError::RootAuthorityChanged {
                        root_index: root.index,
                        root: root.configured.clone(),
                        expected_canonical_root: expected.canonical.clone(),
                        observed_canonical_root: root.canonical.clone(),
                    }
                    .into());
                }
                if !expected.retains_bound_identity(root) {
                    return Err(discovery::RoutineRootValidationError::RootIdentityChanged {
                        root_index: root.index,
                        root: root.configured.clone(),
                        canonical_root: root.canonical.clone(),
                    }
                    .into());
                }
            }
        }
        #[cfg(test)]
        if let Some(hook) = self
            .before_scan_hook
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .clone()
        {
            hook();
        }
        self.verify_bound_authority(
            &runtime_root,
            current_dir_override.as_deref(),
        )?;
        let mut seen_refs = HashSet::new();
        let mut candidates_by_ref: BTreeMap<String, Vec<RoutineScriptCandidate>> = BTreeMap::new();
        let existing_scripts: HashMap<String, LoadedRoutineScript> = self
            .state
            .scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .clone();
        let mut staged_failures = self
            .state
            .failed_scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .clone();

        for root in &validated_roots {
            if !root.exists {
                tracing::warn!(
                    "Routines directory does not exist: {}",
                    root.configured.display()
                );
                add_cached_candidates_for_root(
                    &existing_scripts,
                    &mut candidates_by_ref,
                    &mut seen_refs,
                    root.index,
                    &root.canonical,
                );
                continue;
            }

            #[cfg(test)]
            let before_open = self
                .before_candidate_open_hook
                .lock()
                .unwrap_or_else(recover_poisoned_lock)
                .clone();
            #[cfg(test)]
            let before_read = self
                .before_source_read_hook
                .lock()
                .unwrap_or_else(recover_poisoned_lock)
                .clone();
            #[cfg(test)]
            let read_observer = self
                .source_read_observer
                .lock()
                .unwrap_or_else(recover_poisoned_lock)
                .clone();
            let authority_check = || {
                self.verify_bound_authority(
                    &runtime_root,
                    current_dir_override.as_deref(),
                )
                .map_err(|error| std::io::Error::other(error.to_string()))
            };
            #[cfg(test)]
            let discovery_hooks = RoutineDiscoveryHooks {
                before_open: before_open.as_deref(),
                before_read: before_read.as_deref(),
                read_observer: read_observer.as_deref(),
                authority_check: Some(&authority_check),
            };
            #[cfg(not(test))]
            let discovery_hooks = RoutineDiscoveryHooks {
                before_open: None,
                before_read: None,
                read_observer: None,
                authority_check: Some(&authority_check),
            };
            let entries_result = collect_routine_script_paths(root, discovery_hooks);
            self.verify_bound_authority(
                &runtime_root,
                current_dir_override.as_deref(),
            )?;
            let entries = match entries_result {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!(
                        routines_dir = %root.canonical.display(),
                        configured_routines_dir = %root.configured.display(),
                        error = %e,
                        "failed to scan routines directory; skipping root"
                    );
                    add_cached_candidates_for_root(
                        &existing_scripts,
                        &mut candidates_by_ref,
                        &mut seen_refs,
                        root.index,
                        &root.canonical,
                    );
                    continue;
                }
            };

            for snapshot in entries {
                let path = snapshot.path.clone();
                let script_ref = script_ref(&root.canonical, &path);
                seen_refs.insert(script_ref.clone());
                candidates_by_ref
                    .entry(script_ref)
                    .or_default()
                    .push(RoutineScriptCandidate {
                        root_index: root.index,
                        root: root.canonical.clone(),
                        failure_key: path.clone(),
                        path,
                        snapshot: Some(snapshot),
                        cached: None,
                    });
            }
        }

        self.verify_bound_authority(
            &runtime_root,
            current_dir_override.as_deref(),
        )?;

        let existing_refs: HashSet<String> = existing_scripts.keys().cloned().collect();
        let candidate_paths: HashSet<PathBuf> = candidates_by_ref
            .values()
            .flat_map(|candidates| {
                candidates
                    .iter()
                    .map(|candidate| candidate.failure_key.clone())
            })
            .collect();

        let mut loaded = 0;
        let mut loaded_scripts = Vec::new();
        for (script_ref, candidates) in candidates_by_ref {
            let has_existing = existing_refs.contains(&script_ref);
            let mut selected = None;
            for candidate in candidates.iter().rev() {
                let candidate_key = candidate.failure_key.clone();
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

                let source_result = candidate
                    .snapshot
                    .as_ref()
                    .expect("fresh routine candidate must retain its source snapshot")
                    .read_source();
                let source = match source_result {
                    Ok(source) => source,
                    Err(e) => {
                        self.verify_bound_authority(
                            &runtime_root,
                            current_dir_override.as_deref(),
                        )?;
                        let now = Instant::now();
                        if Self::should_retry_candidate_in(
                            &mut staged_failures,
                            &candidate_key,
                            None,
                            now,
                            has_existing,
                        ) {
                            let retry_delay = Self::record_failure_in(
                                &mut staged_failures,
                                &candidate_key,
                                None,
                                now,
                            );
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
                self.verify_bound_authority(
                    &runtime_root,
                    current_dir_override.as_deref(),
                )?;
                let now = Instant::now();
                if !Self::should_retry_candidate_in(
                    &mut staged_failures,
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

                self.verify_bound_authority(
                    &runtime_root,
                    current_dir_override.as_deref(),
                )?;
                #[cfg(test)]
                self.state
                    .evaluation_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let evaluation = load_single_routine_script_from_source(
                    &candidate.root,
                    &candidate.path,
                    source,
                );
                self.verify_bound_authority(
                    &runtime_root,
                    current_dir_override.as_deref(),
                )?;
                match evaluation {
                    Ok(script) => {
                        staged_failures.remove(&candidate_key);
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
                        let retry_delay = Self::record_failure_in(
                            &mut staged_failures,
                            &candidate_key,
                            Some(source_version.clone()),
                            now,
                        );
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

        staged_failures.retain(|path, _| candidate_paths.contains(path));
        self.verify_bound_authority(
            &runtime_root,
            current_dir_override.as_deref(),
        )?;
        let pruned = self.apply_dir_reload(loaded_scripts, &seen_refs)?;
        *self
            .state
            .failed_scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock) = staged_failures;
        if pruned > 0 {
            tracing::info!(count = pruned, "pruned missing routine scripts");
        }

        Ok(loaded)
    }

    fn should_retry_candidate_in(
        failures: &mut HashMap<PathBuf, RoutineScriptFailure>,
        path: &Path,
        source_version: Option<&String>,
        now: Instant,
        has_existing: bool,
    ) -> bool {
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

    #[cfg(test)]
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
        Self::record_failure_in(&mut failures, path, source_version, now)
    }

    fn record_failure_in(
        failures: &mut HashMap<PathBuf, RoutineScriptFailure>,
        path: &Path,
        source_version: Option<String>,
        now: Instant,
    ) -> Duration {
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
        self.verify_bound_authority(&self.runtime_root, None)?;
        let scripts = self
            .state
            .scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock);
        let script = if let Some(script) = scripts.get(script_ref).cloned() {
            Some(script)
        } else if script_ref == LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF {
            scripts
                .get(CANONICAL_AUTOMATION_CANDIDATE_EXECUTOR_REF)
                .cloned()
        } else {
            None
        };
        drop(scripts);
        self.verify_bound_authority(&self.runtime_root, None)?;
        Ok(script)
    }

    pub fn execute_tick(
        &self,
        script_ref: &str,
        tick_context: RoutineTickContext,
    ) -> Result<crate::services::routines::RoutineAction> {
        let Some(script) = self.get_script(script_ref)? else {
            return Err(anyhow!("routine script {script_ref} is not loaded"));
        };
        self.verify_bound_authority(&self.runtime_root, None)?;
        let evaluation = evaluate_tick_action(&script, &tick_context);
        self.verify_bound_authority(&self.runtime_root, None)?;
        let action_json = evaluation?;
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

#[cfg(test)]
fn test_runtime_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("routine-loader-test-authority")
}

#[cfg(test)]
fn load_single_routine_script(root: &Path, path: &Path) -> Result<LoadedRoutineScript> {
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
mod tests {
    use super::discovery::{
        PathResolutionError, RoutineRootValidationError, validate_routine_authority_with_hook,
        validate_routine_roots,
    };
    use super::*;
    use std::io::{self, Write};
    use std::sync::Barrier;
    use std::thread;
    use tracing_subscriber::fmt::writer::MakeWriter;

    #[derive(Clone)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_debug_logs<F>(emit: F) -> String
    where
        F: FnOnce(),
    {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_writer(CapturingWriter {
                buffer: buffer.clone(),
            })
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    fn fixture_routines_root() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("routines")
    }

    fn isolated_release_surfaces() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let release = tempfile::tempdir().unwrap();
        let routines = release.path().join("routines");
        let helpers = release.path().join("routine-helpers");
        std::fs::create_dir_all(&routines).unwrap();
        std::fs::create_dir_all(&helpers).unwrap();
        std::fs::write(
            routines.join("tracked.js"),
            "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(helpers.join("helper.js"), "module.exports = {};").unwrap();
        (release, routines, helpers)
    }

    const PREFLIGHT_EVALUATION_SENTINEL: usize = 7;

    fn preflight_observed_loader() -> (
        RoutineScriptLoader,
        Arc<std::sync::atomic::AtomicUsize>,
        PathBuf,
    ) {
        let loader = RoutineScriptLoader::new().unwrap();
        loader.state.evaluation_attempts.store(
            PREFLIGHT_EVALUATION_SENTINEL,
            std::sync::atomic::Ordering::Relaxed,
        );
        let failure_sentinel = PathBuf::from("preflight-existing-failure.js");
        loader.state.failed_scripts.lock().unwrap().insert(
            failure_sentinel.clone(),
            RoutineScriptFailure {
                source_version: Some("preflight-sentinel".to_string()),
                consecutive_failures: 1,
                retry_at: Instant::now(),
                warning_emitted: true,
            },
        );
        let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_source_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
            observed_source_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));
        (loader, source_reads, failure_sentinel)
    }

    fn assert_preflight_rejection_has_no_load_side_effects(
        loader: &RoutineScriptLoader,
        source_reads: &std::sync::atomic::AtomicUsize,
        failure_sentinel: &Path,
    ) {
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            PREFLIGHT_EVALUATION_SENTINEL
        );
        assert_eq!(source_reads.load(std::sync::atomic::Ordering::Relaxed), 0);
        let failed_scripts = loader.state.failed_scripts.lock().unwrap();
        assert_eq!(failed_scripts.len(), 1);
        assert!(failed_scripts.contains_key(failure_sentinel));
        assert!(loader.script_refs().unwrap().is_empty());
    }

    #[test]
    fn loads_registered_routine_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daily-summary.js");
        std::fs::write(
            &path,
            r#"
            agentdesk.routines.register({
              name: "Daily Summary",
              tick(ctx) {
                return { action: "complete", result: { ok: true } };
              }
            });
            "#,
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        let script_ref = loader.load_script(dir.path(), &path).unwrap();
        assert_eq!(script_ref, "daily-summary.js");
        assert!(loader.has_script("daily-summary.js").unwrap());
        assert_eq!(loader.script_refs().unwrap(), vec!["daily-summary.js"]);
    }

    #[test]
    fn captures_registered_routine_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("portable.js");
        std::fs::write(
            &path,
            r#"
            agentdesk.routines.register({
              name: "Portable",
              metadata: {
                migrated_launchd: {
                  entrypoint: "scripts/launchd-migrated/portable.sh",
                  required_connectors: ["obsidian_skill_root"]
                }
              },
              tick(ctx) {
                return { action: "complete", result: { ok: true } };
              }
            });
            "#,
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        loader.load_script(dir.path(), &path).unwrap();
        let script = loader.get_script("portable.js").unwrap().unwrap();

        assert_eq!(
            script.metadata["migrated_launchd"]["entrypoint"],
            "scripts/launchd-migrated/portable.sh"
        );
        assert_eq!(
            script.metadata["migrated_launchd"]["required_connectors"][0],
            "obsidian_skill_root"
        );
    }

    #[test]
    fn rejects_cyclic_registered_routine_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-cycle.js");
        std::fs::write(
            &path,
            r#"
            const metadata = { migrated_launchd: { entrypoint: "scripts/launchd-migrated/test.sh" } };
            metadata.self = metadata;
            agentdesk.routines.register({
              name: "Metadata Cycle",
              metadata,
              tick(ctx) {
                return { action: "complete", result: { ok: true } };
              }
            });
            "#,
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        let error = loader.load_script(dir.path(), &path).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("routine metadata cycle check failed")
                || message.contains("cyclic object graph"),
            "{message}"
        );
    }

    #[test]
    fn failed_load_keeps_last_known_good_registry() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.js");
        let bad = dir.path().join("bad.js");
        std::fs::write(
            &good,
            "agentdesk.routines.register({ name: 'Good', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(&bad, "agentdesk.routines.register({ name: 'Bad' });").unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        loader.load_script(dir.path(), &good).unwrap();
        let err = loader.load_script(dir.path(), &bad).unwrap_err();

        assert!(err.to_string().contains("missing tick"));
        assert_eq!(loader.script_refs().unwrap(), vec!["good.js"]);
    }

    #[test]
    fn isolates_global_bindings_between_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.js");
        let second = dir.path().join("second.js");
        let source = |name: &str| {
            format!(
                "const config = {{ name: '{name}' }}; agentdesk.routines.register({{ name: config.name, tick() {{ return {{ action: 'skip' }}; }} }});"
            )
        };
        std::fs::write(&first, source("First")).unwrap();
        std::fs::write(&second, source("Second")).unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 2);
        assert_eq!(
            loader.script_refs().unwrap(),
            vec!["first.js".to_string(), "second.js".to_string()]
        );
    }

    #[test]
    fn load_dir_recurses_into_nested_script_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("ops").join("daily");
        std::fs::create_dir_all(&nested).unwrap();
        let path = nested.join("summary.js");
        std::fs::write(
            &path,
            "agentdesk.routines.register({ name: 'Nested', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 1);
        assert_eq!(loader.script_refs().unwrap(), vec!["ops/daily/summary.js"]);
        assert!(loader.has_script("ops/daily/summary.js").unwrap());
    }

    #[test]
    fn load_dir_ignores_sibling_node_helpers_and_preserves_quickjs_refs() {
        let parent = tempfile::tempdir().unwrap();
        let routines = parent.path().join("routines");
        let nested = routines.join("monitoring");
        let helpers = parent.path().join("routine-helpers");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&helpers).unwrap();

        let nested_routine = nested.join("inventory.js");
        let root_routine = routines.join("tracked.js");
        let node_helper = helpers.join("inventory.js");
        std::fs::write(
            &node_helper,
            "throw new Error('sibling Node helper must never be evaluated by QuickJS');",
        )
        .unwrap();
        std::fs::write(
            &nested_routine,
            "agentdesk.routines.register({ name: 'Nested Inventory', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            &root_routine,
            "agentdesk.routines.register({ name: 'Tracked Root', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        let source_reads = Arc::new(Mutex::new(Vec::new()));
        let observed_source_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |path| {
            observed_source_reads
                .lock()
                .unwrap()
                .push(path.to_path_buf());
        }));

        assert_eq!(loader.load_dir(&routines).unwrap(), 2);
        assert_eq!(
            loader.script_refs().unwrap(),
            vec!["monitoring/inventory.js", "tracked.js"]
        );
        assert!(loader.has_script("monitoring/inventory.js").unwrap());
        assert!(loader.has_script("tracked.js").unwrap());
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        let source_reads = source_reads.lock().unwrap();
        let nested_routine = nested_routine.canonicalize().unwrap();
        let root_routine = root_routine.canonicalize().unwrap();
        let node_helper = node_helper.canonicalize().unwrap();
        assert_eq!(source_reads.len(), 2);
        assert!(source_reads.contains(&nested_routine));
        assert!(source_reads.contains(&root_routine));
        assert!(
            !source_reads.contains(&node_helper),
            "sibling Node helper was source-read"
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn preflight_rejects_sibling_helper_as_additional_root_without_side_effects() {
        let (release, routines, helpers) = isolated_release_surfaces();

        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        let error = loader.load_dirs(&[routines, helpers]).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
        ));
        assert!(
            error
                .to_string()
                .contains("overlaps reserved runtime helper surface")
        );
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[test]
    fn preflight_rejects_dot_root_that_contains_runtime_helper_surface() {
        let (release, _routines, _helpers) = isolated_release_surfaces();
        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        *loader.current_dir_override.lock().unwrap() = Some(release.path().to_path_buf());

        let error = loader.load_dirs(&[PathBuf::from(".")]).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 0, .. })
        ));
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[test]
    fn preflight_rejects_runtime_helper_with_custom_primary_root() {
        let (release, _routines, helpers) = isolated_release_surfaces();
        let custom = tempfile::tempdir().unwrap();
        std::fs::write(
            custom.path().join("custom.js"),
            "agentdesk.routines.register({ name: 'Custom', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

        let error = loader
            .load_dirs(&[custom.path().to_path_buf(), helpers])
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
        ));
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[test]
    fn disjoint_custom_sibling_and_cwd_helper_named_roots_are_allowed() {
        let release = tempfile::tempdir().unwrap();
        let custom = tempfile::tempdir().unwrap();
        let routines = custom.path().join("routines");
        let custom_helpers = custom.path().join("routine-helpers");
        std::fs::create_dir_all(&routines).unwrap();
        std::fs::create_dir_all(&custom_helpers).unwrap();
        std::fs::write(
            routines.join("primary.js"),
            "agentdesk.routines.register({ name: 'Primary', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            custom_helpers.join("operator.js"),
            "agentdesk.routines.register({ name: 'Operator', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        *loader.current_dir_override.lock().unwrap() = Some(custom.path().to_path_buf());

        assert_eq!(
            loader
                .load_dirs(&[
                    PathBuf::from("routines"),
                    PathBuf::from("routine-helpers"),
                ])
                .unwrap(),
            2
        );
        assert_eq!(loader.script_refs().unwrap(), vec!["operator.js", "primary.js"]);
    }

    #[test]
    fn preflight_rejects_release_parent_as_additional_root_without_side_effects() {
        let (release, routines, _helpers) = isolated_release_surfaces();

        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        let error = loader
            .load_dirs(&[routines, release.path().to_path_buf()])
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
        ));
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[test]
    fn preflight_rejects_root_below_sibling_helper_without_side_effects() {
        let (release, routines, helpers) = isolated_release_surfaces();
        let nested_helper_root = helpers.join("nested");
        std::fs::create_dir_all(&nested_helper_root).unwrap();

        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        let error = loader
            .load_dirs(&[routines, nested_helper_root])
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
        ));
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[test]
    fn preflight_rejects_primary_and_child_roots_without_side_effects() {
        let release = tempfile::tempdir().unwrap();
        let routines = release.path().join("routines");
        let nested = routines.join("monitoring");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("tracked.js"),
            "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        let error = loader.load_dirs(&[routines, nested]).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::CanonicalRootOverlap {
                first_index: 0,
                second_index: 1,
                ..
            })
        ));
        assert!(error.to_string().contains("overlap after canonicalization"));
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[test]
    fn preflight_rejects_same_canonical_root_without_side_effects() {
        let release = tempfile::tempdir().unwrap();
        let routines = release.path().join("routines");
        let nested = routines.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let lexical_alias = nested.join("..");

        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        let error = loader.load_dirs(&[routines, lexical_alias]).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::DuplicateCanonicalRoot {
                first_index: 0,
                second_index: 1,
                ..
            })
        ));
        assert!(error.to_string().contains("same canonical directory"));
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[cfg(unix)]
    #[test]
    fn validated_root_alias_retarget_cannot_redirect_discovery_to_helpers() {
        use std::os::unix::fs::symlink;

        let (release, routines, helpers) = isolated_release_surfaces();
        let aliases = tempfile::tempdir().unwrap();
        let root_alias = aliases.path().join("routines");
        symlink(&routines, &root_alias).unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        let alias_to_replace = root_alias.clone();
        let helper_target = helpers.clone();
        *loader.before_scan_hook.lock().unwrap() = Some(Arc::new(move || {
            std::fs::remove_file(&alias_to_replace).unwrap();
            symlink(&helper_target, &alias_to_replace).unwrap();
        }));
        let source_reads = Arc::new(Mutex::new(Vec::new()));
        let observed_source_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |path| {
            observed_source_reads
                .lock()
                .unwrap()
                .push(path.to_path_buf());
        }));

        assert_eq!(loader.load_dirs(&[root_alias]).unwrap(), 1);
        assert_eq!(loader.script_refs().unwrap(), vec!["tracked.js"]);
        let expected_source = routines.canonicalize().unwrap().join("tracked.js");
        assert_eq!(
            source_reads.lock().unwrap().as_slice(),
            &[expected_source]
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn validated_canonical_root_replacement_cannot_redirect_discovery_to_helpers() {
        use std::os::unix::fs::symlink;

        let (release, routines, helpers) = isolated_release_surfaces();
        let original_routines = release.path().join("routines-original");
        let loader = RoutineScriptLoader::new().unwrap();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        let routines_to_replace = routines.clone();
        let original_target = original_routines.clone();
        let helper_target = helpers.clone();
        *loader.before_scan_hook.lock().unwrap() = Some(Arc::new(move || {
            std::fs::rename(&routines_to_replace, &original_target).unwrap();
            symlink(&helper_target, &routines_to_replace).unwrap();
        }));
        let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
            observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        assert_eq!(loader.load_dirs(&[routines]).unwrap(), 0);
        assert_eq!(
            source_reads.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_root_alias_retarget_cannot_change_bound_routine_authority() {
        use std::os::unix::fs::symlink;

        let layout = tempfile::tempdir().unwrap();
        let first_runtime = layout.path().join("runtime-one");
        let second_runtime = layout.path().join("runtime-two");
        let first_routines = first_runtime.join("routines");
        let second_helpers = second_runtime.join("routine-helpers");
        std::fs::create_dir_all(&first_routines).unwrap();
        std::fs::create_dir_all(&second_helpers).unwrap();
        std::fs::write(
            first_routines.join("tracked.js"),
            "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            second_helpers.join("helper.js"),
            "throw new Error('retargeted runtime helper must never be read');",
        )
        .unwrap();
        symlink(&second_helpers, second_runtime.join("routines")).unwrap();
        let runtime_alias = layout.path().join("current-runtime");
        symlink(&first_runtime, &runtime_alias).unwrap();
        let roots = vec![runtime_alias.join("routines")];
        let loader = RoutineScriptLoader::new_shared(&roots, &runtime_alias).unwrap();

        std::fs::remove_file(&runtime_alias).unwrap();
        symlink(&second_runtime, &runtime_alias).unwrap();
        let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
            observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        let error = loader.load_dirs(&roots).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::RuntimeRootAuthorityChanged { .. })
        ));
        assert_eq!(
            source_reads.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_root_alias_retarget_rejects_new_actual_helper_as_external_root() {
        use std::os::unix::fs::symlink;

        let layout = tempfile::tempdir().unwrap();
        let first_runtime = layout.path().join("runtime-one");
        let second_runtime = layout.path().join("runtime-two");
        let first_helpers = first_runtime.join("routine-helpers");
        let second_helpers = second_runtime.join("routine-helpers");
        std::fs::create_dir_all(&first_helpers).unwrap();
        std::fs::create_dir_all(&second_helpers).unwrap();
        std::fs::write(
            second_helpers.join("helper.js"),
            "agentdesk.routines.register({ name: 'External Before Retarget', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        let runtime_alias = layout.path().join("current-runtime");
        symlink(&first_runtime, &runtime_alias).unwrap();
        let roots = vec![second_helpers];
        let loader = RoutineScriptLoader::new_shared(&roots, &runtime_alias).unwrap();
        assert_eq!(loader.load_dirs(&roots).unwrap(), 1);

        std::fs::remove_file(&runtime_alias).unwrap();
        symlink(&second_runtime, &runtime_alias).unwrap();
        let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
            observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        let lookup_error = loader.get_script("helper.js").unwrap_err();
        let error = loader.load_dirs(&roots).unwrap_err();

        assert!(matches!(
            lookup_error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::RuntimeRootAuthorityChanged { .. })
        ));
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::RuntimeRootAuthorityChanged { .. })
        ));
        assert_eq!(
            source_reads.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn missing_helper_cannot_hide_same_path_runtime_root_replacement() {
        let layout = tempfile::tempdir().unwrap();
        let runtime = layout.path().join("runtime");
        let original_runtime = layout.path().join("runtime-original");
        let routines = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(
            routines.path().join("tracked.js"),
            "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        let roots = vec![routines.path().to_path_buf()];
        let loader = RoutineScriptLoader::new_shared(&roots, &runtime).unwrap();

        std::fs::rename(&runtime, &original_runtime).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let replacement_loader = RoutineScriptLoader::new_shared(&roots, &runtime).unwrap();

        assert!(!Arc::ptr_eq(&loader.state, &replacement_loader.state));
        let error = loader.load_dirs(&roots).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::RuntimeRootAuthorityChanged { .. })
        ));
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_root_alias_retarget_during_binding_is_rejected() {
        use std::os::unix::fs::symlink;

        let layout = tempfile::tempdir().unwrap();
        let first_runtime = layout.path().join("runtime-one");
        let second_runtime = layout.path().join("runtime-two");
        std::fs::create_dir_all(first_runtime.join("routines")).unwrap();
        std::fs::create_dir_all(second_runtime.join("routine-helpers")).unwrap();
        symlink(
            second_runtime.join("routine-helpers"),
            second_runtime.join("routines"),
        )
        .unwrap();
        let runtime_alias = layout.path().join("current-runtime");
        symlink(&first_runtime, &runtime_alias).unwrap();
        let roots = vec![runtime_alias.join("routines")];
        let alias_to_retarget = runtime_alias.clone();
        let second_target = second_runtime.clone();

        let error = bind_routine_root_authority_with_hook(
            &roots,
            &runtime_alias,
            move || {
                std::fs::remove_file(&alias_to_retarget).unwrap();
                symlink(&second_target, &alias_to_retarget).unwrap();
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RoutineRootValidationError::RuntimeRootAuthorityChanged { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn shared_loader_rejects_same_path_root_identity_replacement() {
        let runtime = tempfile::tempdir().unwrap();
        let layout = tempfile::tempdir().unwrap();
        let routines = layout.path().join("routines");
        let original = layout.path().join("routines-original");
        std::fs::create_dir_all(&routines).unwrap();
        std::fs::write(
            routines.join("tracked.js"),
            "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        let roots = vec![routines.clone()];
        let loader = RoutineScriptLoader::new_shared(&roots, runtime.path()).unwrap();

        std::fs::rename(&routines, &original).unwrap();
        std::fs::create_dir_all(&routines).unwrap();
        std::fs::write(
            routines.join("tracked.js"),
            "throw new Error('replacement root must not be authorized');",
        )
        .unwrap();
        let replacement_loader = RoutineScriptLoader::new_shared(&roots, runtime.path()).unwrap();
        assert!(!Arc::ptr_eq(&loader.state, &replacement_loader.state));
        let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
            observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        let error = loader.load_dirs(&roots).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::RootIdentityChanged { root_index: 0, .. })
        ));
        assert_eq!(
            source_reads.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn opened_candidate_survives_path_swap_to_helper_without_reading_helper() {
        use std::os::unix::fs::symlink;

        let (release, routines, helpers) = isolated_release_surfaces();
        let candidate = routines.join("tracked.js");
        let original = routines.join("tracked-original.js");
        let helper = helpers.join("helper.js");
        std::fs::write(
            &helper,
            "throw new Error('reserved helper must never be read or evaluated');",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        let candidate_to_compare = candidate.canonicalize().unwrap();
        let candidate_to_replace = candidate.clone();
        let original_path = original.clone();
        let helper_target = helper.clone();
        *loader.before_source_read_hook.lock().unwrap() = Some(Arc::new(move |path| {
            if path == candidate_to_compare.as_path() {
                std::fs::rename(&candidate_to_replace, &original_path).unwrap();
                symlink(&helper_target, &candidate_to_replace).unwrap();
            }
        }));

        assert_eq!(loader.load_dirs(&[routines]).unwrap(), 1);
        assert_eq!(
            loader.get_script("tracked.js").unwrap().unwrap().name,
            "Tracked"
        );
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
        assert!(std::fs::symlink_metadata(&candidate)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn candidate_identity_swap_to_helper_is_rejected_before_source_read() {
        let (release, routines, helpers) = isolated_release_surfaces();
        let candidate = routines.join("tracked.js");
        let original = routines.join("tracked-original.js");
        let helper = helpers.join("helper.js");
        std::fs::write(
            &helper,
            "throw new Error('replacement helper must never be read');",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        let candidate_to_compare = candidate.canonicalize().unwrap();
        let candidate_to_replace = candidate.clone();
        let original_target = original.clone();
        let helper_to_move = helper.clone();
        *loader.before_candidate_open_hook.lock().unwrap() = Some(Arc::new(move |path| {
            if path == candidate_to_compare.as_path() {
                std::fs::rename(&candidate_to_replace, &original_target).unwrap();
                std::fs::rename(&helper_to_move, &candidate_to_replace).unwrap();
            }
        }));
        let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
            observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        assert_eq!(loader.load_dirs(&[routines]).unwrap(), 0);
        assert_eq!(
            source_reads.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn missing_root_resolution_preserves_symlink_parent_semantics() {
        use std::os::unix::fs::symlink;

        let release = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("foo");
        std::fs::create_dir_all(&target).unwrap();
        let link = release.path().join("link");
        symlink(&target, &link).unwrap();
        let configured = link.join("..").join("future-routines");

        let validated =
            validate_routine_roots(&[configured], release.path(), Some(release.path())).unwrap();

        assert_eq!(
            validated[0].canonical,
            outside.path().canonicalize().unwrap().join("future-routines")
        );
        assert!(!validated[0].exists);
    }

    #[cfg(unix)]
    #[test]
    fn missing_root_symlink_insertion_during_validation_is_rejected() {
        use std::os::unix::fs::symlink;

        let release = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let missing_parent = release.path().join("missing-parent");
        let configured = missing_parent.join("routines");
        let parent_to_create = missing_parent.clone();
        let outside_target = outside.path().to_path_buf();

        let error = validate_routine_authority_with_hook(
            &[configured],
            release.path(),
            Some(release.path()),
            move |root_index| {
                assert_eq!(root_index, 0);
                symlink(&outside_target, &parent_to_create).unwrap();
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RoutineRootValidationError::RootAuthorityChangedDuringValidation {
                root_index: 0,
                ..
            }
        ));
    }

    #[test]
    fn preflight_rejects_parent_after_missing_component_as_ambiguous() {
        let release = tempfile::tempdir().unwrap();
        let configured = release
            .path()
            .join("missing")
            .join("..")
            .join("routines");
        let loader = RoutineScriptLoader::new().unwrap();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

        let error = loader.load_dirs(&[configured]).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::RootCanonicalization {
                source: PathResolutionError::AmbiguousMissingPath { .. },
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_parent_cannot_lexically_hide_runtime_helper_surface() {
        use std::os::unix::fs::symlink;

        let (release, _routines, helpers) = isolated_release_surfaces();
        let nested = release.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let aliases = tempfile::tempdir().unwrap();
        let link = aliases.path().join("link");
        symlink(&nested, &link).unwrap();
        let configured = link.join("..").join("routine-helpers");
        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

        let error = loader.load_dirs(&[configured]).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 0, .. })
        ));
        assert!(helpers.exists());
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_dangling_symlink_in_missing_root_prefix() {
        use std::os::unix::fs::symlink;

        let release = tempfile::tempdir().unwrap();
        let dangling = release.path().join("dangling");
        symlink(release.path().join("missing-target"), &dangling).unwrap();
        let loader = RoutineScriptLoader::new().unwrap();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

        let error = loader
            .load_dirs(&[dangling.join("nested").join("routines")])
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::RootCanonicalization {
                source: PathResolutionError::DanglingSymlink { .. },
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preflight_reports_typed_current_directory_failure() {
        const CHILD_MARKER: &str = "AGENTDESK_TEST_DELETED_ROUTINE_CWD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let deleted_cwd = tempfile::tempdir().unwrap();
            std::env::set_current_dir(deleted_cwd.path()).unwrap();
            std::fs::remove_dir(deleted_cwd.path()).unwrap();

            let error = match RoutineScriptLoader::new_shared(
                &[PathBuf::from("routines")],
                Path::new("runtime"),
            ) {
                Ok(_) => panic!("deleted cwd must not authorize relative routine paths"),
                Err(error) => error,
            };
            assert!(matches!(
                error.downcast_ref::<RoutineRootValidationError>(),
                Some(RoutineRootValidationError::CurrentDirectoryUnavailable { .. })
            ));
            std::mem::forget(deleted_cwd);
            return;
        }

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("preflight_reports_typed_current_directory_failure")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn load_dir_hashes_and_evaluates_the_same_source_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic-source.js");
        std::fs::write(&path, "throw new Error('broken snapshot');").unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        let replacement_compare = path.canonicalize().unwrap();
        let replacement_path = path.clone();
        *loader.source_read_hook.lock().unwrap() = Some(Arc::new(move |candidate| {
            if candidate == replacement_compare.as_path() {
                std::fs::write(
                    &replacement_path,
                    "agentdesk.routines.register({ name: 'Replacement', tick() { return { action: 'skip' }; } });",
                )
                .unwrap();
            }
        }));

        assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
        assert_eq!(
            loader
                .state
                .failed_scripts
                .lock()
                .unwrap()
                .get(&candidate_failure_key(&path))
                .unwrap()
                .source_version,
            Some(full_source_version("throw new Error('broken snapshot');"))
        );
        assert!(!loader.has_script("atomic-source.js").unwrap());
    }

    #[test]
    fn valid_disjoint_operator_root_loads_and_overrides_nested_routine() {
        let bundled = tempfile::tempdir().unwrap();
        let operator = tempfile::tempdir().unwrap();
        let bundled_monitoring = bundled.path().join("monitoring");
        let operator_monitoring = operator.path().join("monitoring");
        std::fs::create_dir_all(&bundled_monitoring).unwrap();
        std::fs::create_dir_all(&operator_monitoring).unwrap();
        std::fs::write(
            bundled_monitoring.join("inventory.js"),
            "agentdesk.routines.register({ name: 'Bundled Inventory', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            operator_monitoring.join("inventory.js"),
            "agentdesk.routines.register({ name: 'Operator Inventory', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(
            loader
                .load_dirs(&[bundled.path().to_path_buf(), operator.path().to_path_buf()])
                .unwrap(),
            1
        );
        assert_eq!(
            loader
                .get_script("monitoring/inventory.js")
                .unwrap()
                .unwrap()
                .name,
            "Operator Inventory"
        );
    }

    #[test]
    fn source_failure_identity_uses_full_sha256() {
        let first = full_source_version("routine source one");
        let second = full_source_version("routine source two");

        assert_eq!(first.len(), 64);
        assert_eq!(second.len(), 64);
        assert_ne!(first, second);
        assert_eq!(first, full_source_version("routine source one"));
    }

    #[test]
    fn failure_backoff_doubles_and_caps_at_one_hour() {
        let loader = RoutineScriptLoader::new().unwrap();
        let path = Path::new("broken.js");
        let version = Some("version-1".to_string());
        let now = Instant::now();

        let mut delays = Vec::new();
        for _ in 0..10 {
            delays.push(loader.record_failure(path, version.clone(), now));
        }

        assert_eq!(delays[0], Duration::from_secs(30));
        assert_eq!(delays[1], Duration::from_secs(60));
        assert_eq!(delays[2], Duration::from_secs(120));
        assert_eq!(delays[7], Duration::from_secs(60 * 60));
        assert_eq!(delays[9], Duration::from_secs(60 * 60));
    }

    #[test]
    fn request_loader_reuses_runtime_lkg_during_backoff_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let roots = vec![dir.path().to_path_buf()];
        let runtime_root = test_runtime_root();
        let path = dir.path().join("request-routine.js");
        std::fs::write(
            &path,
            "agentdesk.routines.register({ name: 'Last Known Good', tick() { return { action: 'skip', reason: 'lkg' }; } });",
        )
        .unwrap();

        let runtime = RoutineScriptLoader::new_shared(&roots, &runtime_root).unwrap();
        assert_eq!(runtime.load_dirs(&roots).unwrap(), 1);
        std::fs::write(&path, "throw new Error('transient request failure');").unwrap();
        assert_eq!(runtime.load_dirs(&roots).unwrap(), 0);
        assert_eq!(
            runtime
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );

        let request = RoutineScriptLoader::new_shared(&roots, &runtime_root).unwrap();
        assert!(Arc::ptr_eq(&runtime.state, &request.state));
        assert_eq!(request.load_dirs(&roots).unwrap(), 0);
        assert_eq!(
            request
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "a request during backoff must not re-evaluate the transient failure"
        );
        assert_eq!(
            request
                .get_script("request-routine.js")
                .unwrap()
                .unwrap()
                .name,
            "Last Known Good"
        );

        std::fs::write(
            &path,
            "agentdesk.routines.register({ name: 'Recovered Request', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        let recovered = RoutineScriptLoader::new_shared(&roots, &runtime_root).unwrap();
        assert_eq!(recovered.load_dirs(&roots).unwrap(), 1);
        assert_eq!(
            recovered
                .get_script("request-routine.js")
                .unwrap()
                .unwrap()
                .name,
            "Recovered Request"
        );
    }

    #[test]
    fn concurrent_shared_loaders_singleflight_one_failure() {
        let dir = tempfile::tempdir().unwrap();
        let roots = vec![dir.path().to_path_buf()];
        let runtime_root = test_runtime_root();
        let path = dir.path().join("concurrent.js");
        std::fs::write(&path, "throw new Error('singleflight failure');").unwrap();

        let loaders = (0..8)
            .map(|_| {
                Arc::new(RoutineScriptLoader::new_shared(&roots, &runtime_root).unwrap())
            })
            .collect::<Vec<_>>();
        let state = Arc::clone(&loaders[0].state);
        let barrier = Arc::new(Barrier::new(loaders.len()));
        let handles = loaders
            .into_iter()
            .map(|loader| {
                let roots = roots.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    loader.load_dirs(&roots).unwrap()
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            assert_eq!(handle.join().unwrap(), 0);
        }
        assert_eq!(
            state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let failure = state
            .failed_scripts
            .lock()
            .unwrap()
            .get(&candidate_failure_key(&path))
            .unwrap()
            .clone();
        assert_eq!(failure.consecutive_failures, 1);
        let retry_delay = failure.retry_at.saturating_duration_since(Instant::now());
        assert!(retry_delay <= ROUTINE_LOAD_RETRY_BASE);
        assert!(retry_delay > Duration::ZERO);
    }

    #[test]
    fn shared_loader_identity_includes_runtime_helper_authority() {
        let routines = tempfile::tempdir().unwrap();
        let first_runtime = tempfile::tempdir().unwrap();
        let second_runtime = tempfile::tempdir().unwrap();
        let roots = vec![routines.path().to_path_buf()];

        let first = RoutineScriptLoader::new_shared(&roots, first_runtime.path()).unwrap();
        let second = RoutineScriptLoader::new_shared(&roots, second_runtime.path()).unwrap();

        assert!(!Arc::ptr_eq(&first.state, &second.state));
    }

    #[cfg(unix)]
    #[test]
    fn shared_loader_identity_separates_distinct_runtime_alias_authorities() {
        use std::os::unix::fs::symlink;

        let layout = tempfile::tempdir().unwrap();
        let runtime = layout.path().join("runtime");
        let first_alias = layout.path().join("runtime-first");
        let second_alias = layout.path().join("runtime-second");
        let routines = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        symlink(&runtime, &first_alias).unwrap();
        symlink(&runtime, &second_alias).unwrap();
        let roots = vec![routines.path().to_path_buf()];

        let first = RoutineScriptLoader::new_shared(&roots, &first_alias).unwrap();
        let second = RoutineScriptLoader::new_shared(&roots, &second_alias).unwrap();

        assert!(!Arc::ptr_eq(&first.state, &second.state));
    }

    #[cfg(unix)]
    #[test]
    fn shared_loader_identity_changes_when_helper_surface_is_replaced() {
        let routines = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let helper = runtime.path().join("routine-helpers");
        let old_helper = runtime.path().join("routine-helpers-old");
        std::fs::create_dir_all(&helper).unwrap();
        let roots = vec![routines.path().to_path_buf()];

        let first = RoutineScriptLoader::new_shared(&roots, runtime.path()).unwrap();
        std::fs::rename(&helper, &old_helper).unwrap();
        std::fs::create_dir_all(&helper).unwrap();
        let second = RoutineScriptLoader::new_shared(&roots, runtime.path()).unwrap();

        assert!(!Arc::ptr_eq(&first.state, &second.state));
    }

    #[cfg(unix)]
    #[test]
    fn helper_surface_alias_change_before_candidate_read_aborts_without_side_effects() {
        use std::os::unix::fs::symlink;

        let (release, routines, helper) = isolated_release_surfaces();
        let helper_original = release.path().join("routine-helpers-original");
        let roots = vec![routines.clone()];
        let loader = RoutineScriptLoader::new_shared(&roots, release.path()).unwrap();
        assert_eq!(loader.load_dirs(&roots).unwrap(), 1);
        let loaded_version = loader
            .get_script("tracked.js")
            .unwrap()
            .unwrap()
            .script_version;
        let evaluation_attempts = loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed);
        let failure_sentinel = routines.join("failure-sentinel.js");
        let retry_at = Instant::now() + Duration::from_secs(17);
        loader.state.failed_scripts.lock().unwrap().insert(
            failure_sentinel.clone(),
            RoutineScriptFailure {
                source_version: Some("sentinel".to_owned()),
                consecutive_failures: 4,
                retry_at,
                warning_emitted: true,
            },
        );

        let helper_to_replace = helper.clone();
        let helper_backup = helper_original.clone();
        let helper_alias_target = routines.clone();
        *loader.before_source_read_hook.lock().unwrap() = Some(Arc::new(move |_| {
            std::fs::rename(&helper_to_replace, &helper_backup).unwrap();
            symlink(&helper_alias_target, &helper_to_replace).unwrap();
        }));
        let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = Arc::clone(&source_reads);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
            observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        let error = loader.load_dirs(&roots).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::HelperSurfaceAuthorityChanged { .. })
        ));
        assert_eq!(
            source_reads.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            evaluation_attempts
        );
        assert_eq!(
            loader
                .state
                .scripts
                .lock()
                .unwrap()
                .get("tracked.js")
                .unwrap()
                .script_version,
            loaded_version
        );
        let failures = loader.state.failed_scripts.lock().unwrap();
        assert_eq!(failures.len(), 1);
        let sentinel = failures.get(&failure_sentinel).unwrap();
        assert_eq!(sentinel.source_version.as_deref(), Some("sentinel"));
        assert_eq!(sentinel.consecutive_failures, 4);
        assert_eq!(sentinel.retry_at, retry_at);
        assert!(sentinel.warning_emitted);
    }

    #[test]
    fn missing_relative_root_creation_gets_fresh_shared_authority() {
        let relative =
            PathBuf::from("target").join(format!("routine-root-{}", uuid::Uuid::new_v4()));
        let absolute = std::env::current_dir().unwrap().join(&relative);
        let configured = vec![relative.clone()];
        let runtime_root = test_runtime_root();
        let before = RoutineScriptLoader::new_shared(&configured, &runtime_root).unwrap();

        std::fs::create_dir_all(&absolute).unwrap();
        std::fs::write(
            absolute.join("created.js"),
            "agentdesk.routines.register({ name: 'Created Later', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        let after =
            RoutineScriptLoader::new_shared(&[absolute.clone()], &runtime_root).unwrap();

        assert!(!Arc::ptr_eq(&before.state, &after.state));
        assert_eq!(after.load_dirs(&[absolute.clone()]).unwrap(), 1);
        let error = before.load_dirs(&configured).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::RootIdentityChanged { root_index: 0, .. })
        ));
        assert!(!before.has_script("created.js").unwrap());
        std::fs::remove_dir_all(absolute).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_symlink_alias_to_sibling_helper_without_side_effects() {
        use std::os::unix::fs::symlink;

        let (release, routines, helpers) = isolated_release_surfaces();
        let alias_parent = tempfile::tempdir().unwrap();
        let helper_alias = alias_parent.path().join("helper-alias");
        symlink(&helpers, &helper_alias).unwrap();

        let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
        *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
        let error = loader.load_dirs(&[routines, helper_alias]).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
        ));
        assert_preflight_rejection_has_no_load_side_effects(
            &loader,
            source_reads.as_ref(),
            &failure_sentinel,
        );
    }

    #[test]
    fn failed_script_backoff_skips_eval_until_due_and_content_change_bypasses_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recoverable.js");
        std::fs::write(&path, "throw new Error('broken');").unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "unchanged script must not be evaluated during backoff"
        );

        loader
            .state
            .failed_scripts
            .lock()
            .unwrap()
            .get_mut(&candidate_failure_key(&path))
            .unwrap()
            .retry_at = Instant::now();
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
        assert_eq!(
            loader
                .state
                .evaluation_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "script must be retried after backoff expires"
        );

        std::fs::write(
            &path,
            "agentdesk.routines.register({ name: 'Recovered', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 1);
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
        assert_eq!(
            loader.get_script("recoverable.js").unwrap().unwrap().name,
            "Recovered"
        );
    }

    #[test]
    fn quickjs_eval_error_includes_exception_message_and_stack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node-only.js");
        std::fs::write(&path, "require('node:fs');").unwrap();

        let error = load_single_routine_script(dir.path(), &path).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("require is not defined"), "{message}");
        assert!(message.contains("at <eval>"), "{message}");
    }

    #[test]
    fn quickjs_eval_error_with_empty_message_starts_with_stack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty-message.js");
        std::fs::write(&path, "const error = new Error(''); throw error;").unwrap();

        let error = load_single_routine_script(dir.path(), &path).unwrap_err();
        let message = error.to_string();
        let detail = message
            .strip_prefix(&format!(
                "JS eval error in routine script {}: ",
                path.display()
            ))
            .unwrap();
        assert!(!detail.trim().is_empty(), "{detail:?}");
        assert_eq!(detail, detail.trim_start(), "{detail:?}");
        assert!(
            detail.lines().any(|line| !line.trim().is_empty()),
            "{detail:?}"
        );
    }

    #[test]
    fn quickjs_eval_error_includes_primitive_throw_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("primitive-throw.js");
        std::fs::write(&path, "throw 'bad config';").unwrap();

        let error = load_single_routine_script(dir.path(), &path).unwrap_err();
        assert!(error.to_string().contains("bad config"));
    }

    #[test]
    fn load_dirs_supports_operator_override_dirs() {
        let bundled = tempfile::tempdir().unwrap();
        let operator = tempfile::tempdir().unwrap();
        let bundled_nested = bundled.path().join("ops");
        let operator_nested = operator.path().join("ops");
        std::fs::create_dir_all(&bundled_nested).unwrap();
        std::fs::create_dir_all(&operator_nested).unwrap();
        std::fs::write(
            bundled.path().join("bundled-only.js"),
            "agentdesk.routines.register({ name: 'Bundled Only', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            bundled_nested.join("shared.js"),
            "agentdesk.routines.register({ name: 'Bundled Shared', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            operator.path().join("operator-only.js"),
            "agentdesk.routines.register({ name: 'Operator Only', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            operator_nested.join("shared.js"),
            "agentdesk.routines.register({ name: 'Operator Shared', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(
            loader
                .load_dirs(&[bundled.path().to_path_buf(), operator.path().to_path_buf()])
                .unwrap(),
            3
        );
        assert_eq!(
            loader.script_refs().unwrap(),
            vec![
                "bundled-only.js".to_string(),
                "operator-only.js".to_string(),
                "ops/shared.js".to_string()
            ]
        );
        let shared = loader.get_script("ops/shared.js").unwrap().unwrap();
        assert_eq!(shared.name, "Operator Shared");
        assert!(shared.file.starts_with(operator.path()));
    }

    #[test]
    fn load_dirs_keeps_last_known_good_operator_override() {
        let bundled = tempfile::tempdir().unwrap();
        let operator = tempfile::tempdir().unwrap();
        std::fs::write(
            bundled.path().join("shared.js"),
            "agentdesk.routines.register({ name: 'Bundled Shared', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        let operator_script = operator.path().join("shared.js");
        std::fs::write(
            &operator_script,
            "agentdesk.routines.register({ name: 'Operator Shared', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        let roots = [bundled.path().to_path_buf(), operator.path().to_path_buf()];
        assert_eq!(loader.load_dirs(&roots).unwrap(), 1);
        assert_eq!(
            loader.get_script("shared.js").unwrap().unwrap().name,
            "Operator Shared"
        );

        std::fs::write(
            &operator_script,
            "agentdesk.routines.register({ name: 'Broken Operator' });",
        )
        .unwrap();

        assert_eq!(loader.load_dirs(&roots).unwrap(), 0);
        assert_eq!(
            loader.get_script("shared.js").unwrap().unwrap().name,
            "Operator Shared"
        );
    }

    #[test]
    fn load_dirs_skips_invalid_root_and_keeps_loading_remaining_roots() {
        let temp = tempfile::tempdir().unwrap();
        let invalid_root = temp.path().join("not-a-directory");
        std::fs::write(&invalid_root, "not a directory").unwrap();
        let healthy_root = temp.path().join("healthy");
        std::fs::create_dir_all(&healthy_root).unwrap();
        std::fs::write(
            healthy_root.join("healthy.js"),
            "agentdesk.routines.register({ name: 'Healthy', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(
            loader
                .load_dirs(&[invalid_root, healthy_root.clone()])
                .unwrap(),
            1
        );
        assert_eq!(loader.script_refs().unwrap(), vec!["healthy.js"]);
        assert_eq!(
            loader.get_script("healthy.js").unwrap().unwrap().file,
            healthy_root.join("healthy.js")
        );
    }

    #[test]
    fn load_dirs_preserves_cached_override_when_root_scan_fails() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("bundled");
        let operator = temp.path().join("operator");
        std::fs::create_dir_all(&bundled).unwrap();
        std::fs::create_dir_all(&operator).unwrap();
        std::fs::write(
            bundled.join("shared.js"),
            "agentdesk.routines.register({ name: 'Bundled Shared', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            operator.join("shared.js"),
            "agentdesk.routines.register({ name: 'Operator Shared', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        let roots = [bundled.clone(), operator.clone()];
        assert_eq!(loader.load_dirs(&roots).unwrap(), 1);
        assert_eq!(
            loader.get_script("shared.js").unwrap().unwrap().name,
            "Operator Shared"
        );

        std::fs::remove_dir_all(&operator).unwrap();
        std::fs::write(&operator, "not a directory").unwrap();

        assert_eq!(loader.load_dirs(&roots).unwrap(), 0);
        assert_eq!(
            loader.get_script("shared.js").unwrap().unwrap().name,
            "Operator Shared"
        );

        std::fs::remove_file(&operator).unwrap();

        assert_eq!(loader.load_dirs(&roots).unwrap(), 0);
        assert_eq!(
            loader.get_script("shared.js").unwrap().unwrap().name,
            "Operator Shared"
        );
    }

    #[test]
    fn load_dir_prunes_removed_scripts_and_keeps_failed_seen_script() {
        let dir = tempfile::tempdir().unwrap();
        let removed = dir.path().join("removed.js");
        let retained = dir.path().join("retained.js");
        std::fs::write(
            &removed,
            "agentdesk.routines.register({ name: 'Removed', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
        std::fs::write(
            &retained,
            "agentdesk.routines.register({ name: 'Retained', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 2);

        std::fs::remove_file(&removed).unwrap();
        std::fs::write(
            &retained,
            "agentdesk.routines.register({ name: 'Broken' });",
        )
        .unwrap();

        assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
        assert_eq!(loader.script_refs().unwrap(), vec!["retained.js"]);
        assert_eq!(
            loader
                .state
                .failed_scripts
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec![candidate_failure_key(&retained)]
        );

        std::fs::remove_file(&retained).unwrap();
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
        assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    }

    #[test]
    fn read_failure_emits_one_error_during_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unreadable.js");
        std::fs::write(&path, [0xff]).unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        let read_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_attempts = Arc::clone(&read_attempts);
        *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
            observed_attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));

        let logs = capture_debug_logs(|| {
            assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
            assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
        });
        assert_eq!(
            logs.matches("failed to read routine script; keeping last-known-good registry")
                .count(),
            1,
            "logs={logs}"
        );
        assert_eq!(
            loader
                .state
                .load_error_emissions
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "only the first failed read may emit an ERROR during backoff"
        );
        assert_eq!(read_attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
        let failure = loader
            .state
            .failed_scripts
            .lock()
            .unwrap()
            .get(&candidate_failure_key(&path))
            .unwrap()
            .clone();
        assert_eq!(failure.source_version, None);
        assert_eq!(failure.consecutive_failures, 1);
    }

    #[test]
    fn executes_tick_and_validates_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("complete.js");
        std::fs::write(
            &path,
            r#"
            agentdesk.routines.register({
              name: "Complete",
              tick(ctx) {
                return {
                  action: "complete",
                  result: { routineId: ctx.routine.id, runId: ctx.run.id },
                  lastResult: "ok"
                };
              }
            });
            "#,
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        loader.load_script(dir.path(), &path).unwrap();
        let action = loader
            .execute_tick(
                "complete.js",
                RoutineTickContext {
                    routine: RoutineTickRoutine {
                        id: "routine-1".to_string(),
                        agent_id: None,
                        script_ref: "complete.js".to_string(),
                        name: "Complete".to_string(),
                        execution_strategy: "fresh".to_string(),
                        fresh_context_guaranteed: false,
                    },
                    run: RoutineTickRun {
                        id: "run-1".to_string(),
                        lease_expires_at: chrono::Utc::now(),
                    },
                    agent: None,
                    checkpoint: None,
                    now: chrono::Utc::now(),
                    observations: None,
                    automation_inventory: None,
                    limits: ObservationLimits::default(),
                },
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Complete {
                result_json,
                last_result,
                ..
            } => {
                assert_eq!(last_result.as_deref(), Some("ok"));
                assert_eq!(
                    result_json.unwrap(),
                    serde_json::json!({"routineId": "routine-1", "runId": "run-1"})
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn tick_error_includes_primitive_throw_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("primitive-tick.js");
        std::fs::write(
            &path,
            "agentdesk.routines.register({ name: 'Primitive Tick', tick() { throw 'tick unavailable'; } });",
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        loader.load_script(dir.path(), &path).unwrap();
        let error = loader
            .execute_tick(
                "primitive-tick.js",
                RoutineTickContext {
                    routine: RoutineTickRoutine {
                        id: "routine-1".to_string(),
                        agent_id: None,
                        script_ref: "primitive-tick.js".to_string(),
                        name: "Primitive Tick".to_string(),
                        execution_strategy: "fresh".to_string(),
                        fresh_context_guaranteed: false,
                    },
                    run: RoutineTickRun {
                        id: "run-1".to_string(),
                        lease_expires_at: chrono::Utc::now(),
                    },
                    agent: None,
                    checkpoint: None,
                    now: chrono::Utc::now(),
                    observations: None,
                    automation_inventory: None,
                    limits: ObservationLimits::default(),
                },
            )
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("tick unavailable"), "{message}");
        assert!(
            !message.contains("Exception generated by QuickJS"),
            "{message}"
        );
    }

    #[test]
    fn legacy_automation_executor_v2_ref_resolves_to_canonical_script() {
        let dir = tempfile::tempdir().unwrap();
        let monitoring_dir = dir.path().join("monitoring");
        std::fs::create_dir_all(&monitoring_dir).unwrap();
        let path = monitoring_dir.join("automation-candidate-executor.js");
        std::fs::write(
            &path,
            r#"
            agentdesk.routines.register({
              name: "Automation Candidate Executor",
              tick(ctx) {
                return {
                  action: "complete",
                  result: { scriptRef: ctx.routine.script_ref },
                  lastResult: "legacy-compatible"
                };
              }
            });
            "#,
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 1);
        assert!(
            loader
                .get_script(LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF)
                .unwrap()
                .is_some()
        );

        let action = loader
            .execute_tick(
                LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF,
                RoutineTickContext {
                    routine: RoutineTickRoutine {
                        id: "routine-legacy".to_string(),
                        agent_id: None,
                        script_ref: LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF.to_string(),
                        name: "Legacy Automation Executor".to_string(),
                        execution_strategy: "fresh".to_string(),
                        fresh_context_guaranteed: false,
                    },
                    run: RoutineTickRun {
                        id: "run-legacy".to_string(),
                        lease_expires_at: chrono::Utc::now(),
                    },
                    agent: None,
                    checkpoint: None,
                    now: chrono::Utc::now(),
                    observations: None,
                    automation_inventory: None,
                    limits: ObservationLimits::default(),
                },
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Complete {
                result_json,
                last_result,
                ..
            } => {
                assert_eq!(last_result.as_deref(), Some("legacy-compatible"));
                assert_eq!(
                    result_json.unwrap(),
                    serde_json::json!({"scriptRef": LEGACY_AUTOMATION_CANDIDATE_EXECUTOR_REF})
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn exposes_tick_agent_idle_state_to_js() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-idle.js");
        std::fs::write(
            &path,
            r#"
            agentdesk.routines.register({
              name: "Agent Idle",
              tick(ctx) {
                if (!ctx.agent.is_idle) {
                  return {
                    action: "skip",
                    reason: "agent not idle",
                    result: { isIdle: ctx.agent.is_idle },
                    lastResult: "skipped"
                  };
                }

                return {
                  action: "complete",
                  result: { isIdle: ctx.agent.is_idle },
                  lastResult: "idle"
                };
              }
            });
            "#,
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        loader.load_script(dir.path(), &path).unwrap();

        let context_for = |is_idle: bool| RoutineTickContext {
            routine: RoutineTickRoutine {
                id: "routine-1".to_string(),
                agent_id: Some("monitoring".to_string()),
                script_ref: "agent-idle.js".to_string(),
                name: "Agent Idle".to_string(),
                execution_strategy: "fresh".to_string(),
                fresh_context_guaranteed: false,
            },
            run: RoutineTickRun {
                id: "run-1".to_string(),
                lease_expires_at: chrono::Utc::now(),
            },
            agent: Some(RoutineTickAgent {
                id: "monitoring".to_string(),
                status: if is_idle { "idle" } else { "working" }.to_string(),
                is_idle,
                current_task_id: None,
                current_thread_channel_id: None,
            }),
            checkpoint: None,
            now: chrono::Utc::now(),
            observations: None,
            automation_inventory: None,
            limits: ObservationLimits::default(),
        };

        let idle_action = loader
            .execute_tick("agent-idle.js", context_for(true))
            .unwrap();
        match idle_action {
            crate::services::routines::RoutineAction::Complete {
                result_json,
                last_result,
                ..
            } => {
                assert_eq!(last_result.as_deref(), Some("idle"));
                assert_eq!(result_json.unwrap(), serde_json::json!({"isIdle": true}));
            }
            other => panic!("unexpected idle action: {other:?}"),
        }

        let working_action = loader
            .execute_tick("agent-idle.js", context_for(false))
            .unwrap();
        match working_action {
            crate::services::routines::RoutineAction::Skip {
                reason,
                result_json,
                last_result,
                ..
            } => {
                assert_eq!(reason.as_deref(), Some("agent not idle"));
                assert_eq!(last_result.as_deref(), Some("skipped"));
                assert_eq!(result_json.unwrap(), serde_json::json!({"isIdle": false}));
            }
            other => panic!("unexpected working action: {other:?}"),
        }
    }

    #[test]
    fn rejects_cyclic_action_result_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cycle.js");
        std::fs::write(
            &path,
            r#"
            agentdesk.routines.register({
              name: "Cycle",
              tick() {
                const result = { ok: true };
                result.self = result;
                return { action: "complete", result };
              }
            });
            "#,
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        loader.load_script(dir.path(), &path).unwrap();
        let error = loader
            .execute_tick(
                "cycle.js",
                RoutineTickContext {
                    routine: RoutineTickRoutine {
                        id: "routine-1".to_string(),
                        agent_id: None,
                        script_ref: "cycle.js".to_string(),
                        name: "Cycle".to_string(),
                        execution_strategy: "fresh".to_string(),
                        fresh_context_guaranteed: false,
                    },
                    run: RoutineTickRun {
                        id: "run-1".to_string(),
                        lease_expires_at: chrono::Utc::now(),
                    },
                    agent: None,
                    checkpoint: None,
                    now: chrono::Utc::now(),
                    observations: None,
                    automation_inventory: None,
                    limits: ObservationLimits::default(),
                },
            )
            .unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("cycle check failed") || message.contains("cyclic object graph"),
            "{message}"
        );
    }

    #[test]
    fn bundled_sample_routines_load_and_validate() {
        // Operator routines live in gitignored `routines/` in real deployments.
        // Keep this battery hermetic by validating the tracked fixture contract.
        let root = fixture_routines_root();
        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dir(&root).unwrap(), 8);
        assert_eq!(
            loader.script_refs().unwrap(),
            vec![
                "agent-checkpoint-review.js".to_string(),
                "family-profile-probe-obujang.js".to_string(),
                "family-profile-probe-yohoejang.js".to_string(),
                "migrated-launchd/cookingheart-daily-briefing.js".to_string(),
                "migrated-launchd/queue-stability-batch.js".to_string(),
                "monitoring/automation-candidate-recommender.js".to_string(),
                "monitoring/working-watchdog.js".to_string(),
                "script-summary.js".to_string(),
            ]
        );

        let context_for = |script_ref: &str, name: &str| RoutineTickContext {
            routine: RoutineTickRoutine {
                id: "routine-1".to_string(),
                agent_id: Some("maker".to_string()),
                script_ref: script_ref.to_string(),
                name: name.to_string(),
                execution_strategy: "fresh".to_string(),
                fresh_context_guaranteed: false,
            },
            run: RoutineTickRun {
                id: "run-1".to_string(),
                lease_expires_at: chrono::Utc::now(),
            },
            agent: None,
            checkpoint: None,
            now: chrono::Utc::now(),
            observations: None,
            automation_inventory: None,
            limits: ObservationLimits::default(),
        };

        assert!(matches!(
            loader
                .execute_tick(
                    "script-summary.js",
                    context_for("script-summary.js", "script-only-summary")
                )
                .unwrap(),
            crate::services::routines::RoutineAction::Complete { .. }
        ));
        assert!(matches!(
            loader
                .execute_tick(
                    "monitoring/automation-candidate-recommender.js",
                    context_for(
                        "monitoring/automation-candidate-recommender.js",
                        "automation-candidate-recommender"
                    )
                )
                .unwrap(),
            crate::services::routines::RoutineAction::Complete { .. }
        ));
        assert!(matches!(
            loader
                .execute_tick(
                    "monitoring/working-watchdog.js",
                    context_for(
                        "monitoring/working-watchdog.js",
                        "monitoring-working-watchdog"
                    )
                )
                .unwrap(),
            crate::services::routines::RoutineAction::Complete { .. }
        ));
        assert!(matches!(
            loader
                .execute_tick(
                    "agent-checkpoint-review.js",
                    context_for("agent-checkpoint-review.js", "agent-checkpoint-review")
                )
                .unwrap(),
            crate::services::routines::RoutineAction::Agent { .. }
        ));
        // Spot-check one of the migrated launchd routines: must return Agent.
        assert!(matches!(
            loader
                .execute_tick(
                    "migrated-launchd/cookingheart-daily-briefing.js",
                    context_for(
                        "migrated-launchd/cookingheart-daily-briefing.js",
                        "cookingheart-daily-briefing"
                    )
                )
                .unwrap(),
            crate::services::routines::RoutineAction::Agent { .. }
        ));
        assert!(matches!(
            loader
                .execute_tick(
                    "migrated-launchd/queue-stability-batch.js",
                    context_for(
                        "migrated-launchd/queue-stability-batch.js",
                        "queue-stability-batch"
                    )
                )
                .unwrap(),
            crate::services::routines::RoutineAction::Agent { .. }
        ));
    }

    #[test]
    fn family_profile_probe_agent_action_defers_daily_marker_until_delivery() {
        let root = fixture_routines_root();
        let loader = RoutineScriptLoader::new().unwrap();
        loader
            .load_script(&root, &root.join("family-profile-probe-obujang.js"))
            .unwrap();

        let now = chrono::DateTime::parse_from_rfc3339("2026-05-30T03:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let action = loader
            .execute_tick(
                "family-profile-probe-obujang.js",
                RoutineTickContext {
                    routine: RoutineTickRoutine {
                        id: "routine-family-profile".to_string(),
                        agent_id: Some("family-counsel".to_string()),
                        script_ref: "family-profile-probe-obujang.js".to_string(),
                        name: "family-profile-probe-obujang".to_string(),
                        execution_strategy: "fresh".to_string(),
                        fresh_context_guaranteed: false,
                    },
                    run: RoutineTickRun {
                        id: "run-family-profile".to_string(),
                        lease_expires_at: now,
                    },
                    agent: None,
                    checkpoint: Some(serde_json::json!({
                        "plan": {"date": "2026-05-30", "hour": 12, "minute": 0}
                    })),
                    now,
                    observations: None,
                    automation_inventory: None,
                    limits: ObservationLimits::default(),
                },
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Agent {
                dm_user_id,
                checkpoint,
                ..
            } => {
                assert_eq!(dm_user_id.as_deref(), Some("343742347365974026"));
                let checkpoint = checkpoint.expect("agent checkpoint");
                assert!(
                    checkpoint.get("lastTriggeredDate").is_none(),
                    "generated-but-undelivered DM must not consume today's marker"
                );
                assert_eq!(
                    checkpoint
                        .pointer("/pendingDelivery/kind")
                        .and_then(serde_json::Value::as_str),
                    Some("family-profile-probe")
                );
                assert_eq!(
                    checkpoint
                        .pointer("/pendingDelivery/triggerDate")
                        .and_then(serde_json::Value::as_str),
                    Some("2026-05-30")
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    fn automation_recommender_context(
        checkpoint: Option<serde_json::Value>,
        observations: Vec<serde_json::Value>,
        automation_inventory: Vec<serde_json::Value>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> RoutineTickContext {
        RoutineTickContext {
            routine: RoutineTickRoutine {
                id: "routine-automation".to_string(),
                agent_id: Some("maker".to_string()),
                script_ref: "monitoring/automation-candidate-recommender.js".to_string(),
                name: "automation-candidate-recommender".to_string(),
                execution_strategy: "fresh".to_string(),
                fresh_context_guaranteed: false,
            },
            run: RoutineTickRun {
                id: "run-automation".to_string(),
                lease_expires_at: now,
            },
            agent: None,
            checkpoint,
            now,
            observations: Some(observations),
            automation_inventory: Some(automation_inventory),
            limits: ObservationLimits::default(),
        }
    }

    fn automation_recommender_loader() -> RoutineScriptLoader {
        let root = fixture_routines_root();
        let loader = RoutineScriptLoader::new().unwrap();
        loader
            .load_script(
                &root,
                &root.join("monitoring/automation-candidate-recommender.js"),
            )
            .unwrap();
        loader
    }

    fn routine_observation(signature: &str, weight: u8, timestamp: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "source": "routine_result",
            "category": "routine-candidate",
            "signature": signature,
            "summary": "routine completed with repeated evidence",
            "weight": weight,
            "evidence_ref": format!("routine_run:{signature}:{timestamp}"),
        })
    }

    fn routine_observations(signature: &str, weight: u8, count: usize) -> Vec<serde_json::Value> {
        (0..count)
            .map(|index| {
                routine_observation(signature, weight, &format!("2026-04-30T06:59:{index:02}Z"))
            })
            .collect()
    }

    fn categorized_observation(
        signature: &str,
        category: &str,
        source: &str,
        occurrences: u8,
        timestamp: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "source": source,
            "category": category,
            "signature": signature,
            "summary": format!("{category} repeated evidence"),
            "weight": 2,
            "occurrences": occurrences,
            "evidence_ref": format!("{source}:{signature}:{timestamp}"),
        })
    }

    #[test]
    fn automation_recommender_inventory_wildcard_suppresses_matching_observations() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let observations = (0..6)
            .map(|_| {
                routine_observation(
                    "monitoring/working-watchdog.js:complete",
                    2,
                    "2026-04-30T06:59:00Z",
                )
            })
            .collect::<Vec<_>>();
        let inventory = vec![serde_json::json!({
            "pattern_id": "monitoring/working-watchdog.js:*",
            "status": "implemented",
            "reason": "registered routine",
            "source_ref": "routine:monitoring-working-watchdog",
            "updated_at": "2026-04-30T06:00:00Z"
        })];

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(None, observations, inventory, now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Complete {
                result_json,
                checkpoint,
                last_result,
                ..
            } => {
                assert_eq!(
                    last_result.as_deref(),
                    Some("성공 요약: 새 자동화 추천 후보 없음 (관찰=6, 후보=0, 오늘 추천=0)")
                );
                let result = result_json.expect("complete action should include summary result");
                assert_eq!(
                    result.get("summary").and_then(Value::as_str),
                    Some("관찰=6, 후보=0, 오늘 추천=0")
                );
                assert!(
                    result
                        .get("outcome_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.starts_with("성공 요약:"))
                );
                assert!(result
                    .get("suppression_summary")
                    .and_then(Value::as_str)
                    .is_some_and(|summary| summary.contains("자동화 인벤토리 상태=implemented")));
                assert_eq!(
                    result.get("scoring_summary").and_then(Value::as_str),
                    Some(
                        "scored=0, deduped=0, suppressed=6, ema_scored=0.000, saturation_ticks=1, fast_fail_ticks=0, reopt_count=0"
                    )
                );
                let checkpoint = checkpoint.unwrap();
                assert_eq!(
                    checkpoint
                        .get("candidates")
                        .and_then(Value::as_object)
                        .unwrap()
                        .len(),
                    0
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_requires_durable_ref_before_accepted_inventory_suppresses() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let observations = routine_observations("ops/retry.js:complete", 2, 5);
        let inventory = vec![serde_json::json!({
            "pattern_id": "ops/retry.js:complete",
            "status": "accepted",
            "reason": "proposal accepted but not implemented",
            "updated_at": "2026-04-30T06:00:00Z"
        })];

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(None, observations, inventory, now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Agent {
                prompt, checkpoint, ..
            } => {
                assert!(prompt.contains("지속 증거가 없는 accepted"));
                let checkpoint = checkpoint.unwrap();
                assert!(
                    checkpoint
                        .pointer("/candidates/ops~1retry.js:complete")
                        .is_some()
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_inventory_wildcard_drops_matching_checkpoint_candidates() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let checkpoint = serde_json::json!({
            "version": 1,
            "cursors": {},
            "candidates": {
                "monitoring/working-watchdog.js:complete": {
                    "category": "routine-candidate",
                    "state": "recommended",
                    "score": 100,
                    "evidence_count": 89,
                    "cooldown_until": null
                }
            },
            "suppressions": {},
            "recommendations": [{
                "pattern_id": "monitoring/working-watchdog.js:complete",
                "recommended_at": "2026-04-30T06:59:00Z",
                "hash": "existing",
                "score": 100,
                "evidence_count": 89
            }],
            "last_tick_at": "2026-04-30T06:59:00Z",
            "stats": {
                "ticks": 7,
                "observations_seen": 100,
                "agent_escalations": 1,
                "recommendations_today": 1,
                "recommendation_day": "2026-04-30"
            }
        });
        let inventory = vec![serde_json::json!({
            "pattern_id": "monitoring/working-watchdog.js:*",
            "status": "implemented",
            "reason": "registered routine",
            "source_ref": "routine:monitoring-working-watchdog",
            "updated_at": "2026-04-30T06:00:00Z"
        })];

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(Some(checkpoint), vec![], inventory, now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Complete { checkpoint, .. } => {
                let checkpoint = checkpoint.unwrap();
                assert_eq!(
                    checkpoint
                        .get("candidates")
                        .and_then(Value::as_object)
                        .unwrap()
                        .len(),
                    0
                );
                assert_eq!(
                    checkpoint
                        .get("recommendations")
                        .and_then(Value::as_array)
                        .unwrap()
                        .len(),
                    0
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_uses_weight_for_error_assessment_and_persists_fields() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let observations = routine_observations("ops/retry.js:complete", 2, 5);

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(None, observations, vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Agent {
                prompt, checkpoint, ..
            } => {
                assert!(prompt.contains("반복 실패 루틴에 대한 자동 재시도 또는 알림"));
                assert!(prompt.contains("실패 요약:"));
                let checkpoint = checkpoint.unwrap();
                let candidate = checkpoint
                    .pointer("/candidates/ops~1retry.js:complete")
                    .expect("candidate should be persisted");
                assert_eq!(
                    candidate
                        .get("suggested_automation")
                        .and_then(Value::as_str),
                    Some("반복 실패 루틴에 대한 자동 재시도 또는 알림")
                );
                assert!(
                    candidate
                        .get("outcome_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.starts_with("실패 요약:"))
                );
                assert!(
                    candidate
                        .get("decision_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.starts_with("선택 이유:"))
                );
                assert!(
                    candidate
                        .get("top_evidence_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.contains("repeated evidence"))
                );
                assert_eq!(
                    candidate
                        .get("score_delta_last_tick")
                        .and_then(Value::as_f64),
                    Some(150.0)
                );
                assert_eq!(
                    candidate
                        .get("recommended_execution")
                        .and_then(Value::as_str),
                    Some("agent")
                );
                assert!(candidate.get("before_after").is_some());
                assert!(candidate.get("expected_files").is_some());
                assert!(candidate.get("expected_side_effects").is_some());
                assert!(candidate.get("verification_method").is_some());
                assert_eq!(
                    candidate
                        .pointer("/gated_handoff/status")
                        .and_then(Value::as_str),
                    Some("requires_human_approval")
                );
                assert!(
                    checkpoint
                        .pointer("/recommendations/0/outcome_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.starts_with("실패 요약:"))
                );
                assert!(
                    checkpoint
                        .pointer("/recommendations/0/decision_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.starts_with("선택 이유:"))
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_prompt_includes_quality_sections_and_gated_handoff() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let observations = routine_observations("ops/retry.js:complete", 2, 5);

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(None, observations, vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Agent { prompt, .. } => {
                assert!(prompt.contains("에이전트가 도출한 내용은 반드시 한국어"));
                assert!(prompt.contains("## 성공/실패 한 줄 요약"));
                assert!(prompt.contains("## 선택 판단 근거"));
                assert!(prompt.contains("## 루트 기반 JS 자동화 패턴 탐지 가이드"));
                assert!(prompt.contains("## 이전 작업/체크포인트 수렴 대응"));
                assert!(prompt.contains("대체 탐색 경로"));
                assert!(prompt.contains("반복 제안이 되지 않게"));
                assert!(prompt.contains("## 이미 자동화됨 판단 기준"));
                assert!(prompt.contains("automation_ref 또는 source_ref"));
                assert!(prompt.contains("지속 증거가 없는 accepted"));
                assert!(prompt.contains("## 자료 범위 및 검색 정책"));
                assert!(prompt.contains("외부 웹자료 검색은 기본 동작이 아닙니다"));
                assert!(prompt.contains("PostgreSQL-backed routine observation"));
                assert!(prompt.contains("루트 원인 또는 반복 수동 작업 가설"));
                assert!(prompt.contains("rule-vs-agent 선택 이유"));
                assert!(prompt.contains("오탐/중복 억제 방법"));
                assert!(prompt.contains("다른 탐색/진행 방식"));
                assert!(prompt.contains("## Before / After"));
                assert!(prompt.contains("## 예상 구현 파일"));
                assert!(prompt.contains("## 검증 방법"));
                assert!(prompt.contains("## 게이트된 핸드오프 초안"));
                assert!(prompt.contains("requires_human_approval"));
                assert!(prompt.contains("구현, 파일 수정, 서비스 재시작"));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_prompt_includes_prior_checkpoint_convergence_guidance() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let checkpoint = serde_json::json!({
            "version": 1,
            "cursors": {},
            "candidates": {
                "ops/retry.js:complete": {
                    "category": "routine-candidate",
                    "state": "recommended",
                    "score": 70,
                    "evidence_count": 4,
                    "examples": [],
                    "last_recommended_at": "2026-04-30T05:00:00Z",
                    "last_recommendation_hash": "old-hash",
                    "cooldown_until": null
                }
            },
            "suppressions": {},
            "recommendations": [],
            "last_tick_at": "2026-04-30T06:59:00Z",
            "stats": {
                "ticks": 7,
                "observations_seen": 10,
                "agent_escalations": 1,
                "recommendations_today": 0,
                "recommendation_day": "2026-04-30"
            }
        });
        let observations = vec![routine_observation(
            "ops/retry.js:complete",
            1,
            "2026-04-30T06:59:00Z",
        )];

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(Some(checkpoint), observations, vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Agent { prompt, .. } => {
                assert!(prompt.contains("이 후보는 이전 추천/체크포인트 이력이 있습니다"));
                assert!(prompt.contains("이전 추천 시각=2026-04-30T05:00:00Z"));
                assert!(prompt.contains("같은 결론에 수렴하더라도"));
                assert!(prompt.contains("대체 탐색 경로"));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_truncates_prompt_by_utf8_bytes_without_node_buffer() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let long_summary = "가나다라마바사아자차카타파하".repeat(320);
        let observations = (0..5)
            .map(|idx| {
                serde_json::json!({
                    "timestamp": "2026-04-30T06:59:00Z",
                    "source": "routine_result",
                    "category": "routine-candidate",
                    "signature": "ops/long.js:complete",
                    "summary": format!("{idx}: {long_summary}"),
                    "occurrences": 1,
                    "evidence_ref": format!("long:{idx}"),
                })
            })
            .collect::<Vec<_>>();

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(None, observations, vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Agent { prompt, .. } => {
                assert!(prompt.len() <= 12_288);
                assert!(prompt.contains("## 이전 작업/체크포인트 수렴 대응"));
                assert!(prompt.contains("## 이미 자동화됨 판단 기준"));
                assert!(prompt.contains("## 자료 범위 및 검색 정책"));
                assert!(prompt.contains("## 지시사항"));
                assert!(!prompt.contains('\u{FFFD}'));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_expands_api_friction_category() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let observations = vec![categorized_observation(
            "api-friction:/api/docs/kanban",
            "api-friction",
            "api_friction",
            5,
            "2026-04-30T06:59:00Z",
        )];

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(None, observations, vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Agent {
                prompt, checkpoint, ..
            } => {
                assert!(prompt.contains("카테고리: api-friction"));
                assert!(prompt.contains("API 마찰 모니터"));
                assert!(prompt.contains("src/services/api_friction.rs"));
                let candidate = checkpoint
                    .unwrap()
                    .pointer("/candidates/api-friction:~1api~1docs~1kanban/category")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_string();
                assert_eq!(candidate, "api-friction");
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_expands_release_and_outbox_categories() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let release_action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(
                    None,
                    vec![categorized_observation(
                        "release-freshness:worker-inventory",
                        "release-freshness",
                        "precomputed_digest",
                        5,
                        "2026-04-30T06:59:00Z",
                    )],
                    vec![],
                    now,
                ),
            )
            .unwrap();
        match release_action {
            crate::services::routines::RoutineAction::Agent { prompt, .. } => {
                assert!(prompt.contains("카테고리: release-freshness"));
                assert!(prompt.contains("릴리스 신선도 모니터"));
                let inventory_path = ["docs", "generated", "worker-inventory.md"].join("/");
                assert!(prompt.contains(&inventory_path));
            }
            other => panic!("unexpected action: {other:?}"),
        }

        let outbox_action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(
                    None,
                    vec![categorized_observation(
                        "outbox-delivery:notify:routine_run_failed",
                        "outbox-delivery",
                        "message_outbox",
                        5,
                        "2026-04-30T06:59:00Z",
                    )],
                    vec![],
                    now,
                ),
            )
            .unwrap();
        match outbox_action {
            crate::services::routines::RoutineAction::Agent { prompt, .. } => {
                assert!(prompt.contains("카테고리: outbox-delivery"));
                assert!(prompt.contains("메시지 아웃박스 전달 모니터"));
                assert!(prompt.contains("src/services/message_outbox.rs"));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_accepts_memento_digest_occurrence_counts() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let observations = vec![categorized_observation(
            "memento-hygiene:api-friction-memory",
            "memento-hygiene",
            "memento_digest",
            5,
            "2026-04-30T06:59:00Z",
        )];

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(None, observations, vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Agent {
                prompt, checkpoint, ..
            } => {
                assert!(prompt.contains("카테고리: memento-hygiene"));
                assert!(prompt.contains("Memento 위생 다이제스트 모니터"));
                assert!(prompt.contains("src/services/memory"));
                assert_eq!(
                    checkpoint
                        .unwrap()
                        .pointer("/candidates/memento-hygiene:api-friction-memory/evidence_count")
                        .and_then(Value::as_i64),
                    Some(5)
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_requires_minimum_evidence_count_before_agent_action() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let observations = routine_observations("ops/bursty.js:complete", 2, 4);

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(None, observations, vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Complete {
                result_json,
                checkpoint,
                ..
            } => {
                let result = result_json.expect("complete action should explain why no agent ran");
                assert!(
                    result
                        .get("decision_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.contains("최소 5회 미만"))
                );
                assert!(
                    result
                        .get("top_evidence_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.contains("score=100"))
                );
                let checkpoint = checkpoint.unwrap();
                let candidate = checkpoint
                    .pointer("/candidates/ops~1bursty.js:complete")
                    .expect("candidate should be tracked below the evidence floor");
                assert_eq!(candidate.get("score").and_then(Value::as_i64), Some(100));
                assert_eq!(
                    candidate.get("evidence_count").and_then(Value::as_i64),
                    Some(4)
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_expires_stale_candidates_before_escalation() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let checkpoint = serde_json::json!({
            "version": 1,
            "cursors": {},
            "candidates": {
                "stale.js:complete": {
                    "category": "routine-candidate",
                    "state": "observing",
                    "score": 100,
                    "evidence_count": 20,
                    "first_seen_at": "2026-03-01T00:00:00Z",
                    "last_seen_at": "2026-03-01T00:00:00Z",
                    "examples": [],
                    "last_recommended_at": null,
                    "last_recommendation_hash": null,
                    "cooldown_until": null,
                    "automation_ref": null
                }
            },
            "suppressions": {},
            "recommendations": [],
            "last_tick_at": null,
            "stats": {
                "ticks": 0,
                "observations_seen": 0,
                "agent_escalations": 0,
                "recommendations_today": 0,
                "recommendation_day": null
            }
        });

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(Some(checkpoint), vec![], vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Complete { checkpoint, .. } => {
                assert_eq!(
                    checkpoint
                        .unwrap()
                        .pointer("/candidates/stale.js:complete/state")
                        .and_then(Value::as_str),
                    Some("expired")
                );
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn automation_recommender_checkpoint_guard_prunes_lru_candidate_first() {
        let loader = automation_recommender_loader();
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-30T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let checkpoint = serde_json::json!({
            "version": 1,
            "cursors": {},
            "candidates": {
                "old-high-score.js:complete": {
                    "category": "routine-candidate",
                    "state": "observing",
                    "score": 99,
                    "evidence_count": 20,
                    "first_seen_at": "2026-04-20T00:00:00Z",
                    "last_seen_at": "2026-04-20T00:00:00Z",
                    "examples": [{"summary": "x".repeat(70000), "timestamp": "2026-04-20T00:00:00Z"}],
                    "last_recommended_at": null,
                    "last_recommendation_hash": null,
                    "cooldown_until": null,
                    "automation_ref": null
                },
                "recent-low-score.js:complete": {
                    "category": "routine-candidate",
                    "state": "observing",
                    "score": 1,
                    "evidence_count": 1,
                    "first_seen_at": "2026-04-30T06:59:00Z",
                    "last_seen_at": "2026-04-30T06:59:00Z",
                    "examples": [],
                    "last_recommended_at": null,
                    "last_recommendation_hash": null,
                    "cooldown_until": null,
                    "automation_ref": null
                }
            },
            "suppressions": {},
            "recommendations": [],
            "last_tick_at": null,
            "stats": {
                "ticks": 0,
                "observations_seen": 0,
                "agent_escalations": 0,
                "recommendations_today": 3,
                "recommendation_day": "2026-04-30"
            }
        });

        let action = loader
            .execute_tick(
                "monitoring/automation-candidate-recommender.js",
                automation_recommender_context(Some(checkpoint), vec![], vec![], now),
            )
            .unwrap();

        match action {
            crate::services::routines::RoutineAction::Complete { checkpoint, .. } => {
                let candidates = checkpoint
                    .unwrap()
                    .get("candidates")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap();
                assert!(!candidates.contains_key("old-high-score.js:complete"));
                assert!(candidates.contains_key("recent-low-score.js:complete"));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn loader_recovers_after_lock_poisoning() {
        let loader = Arc::new(RoutineScriptLoader::new().unwrap());

        let loader_clone = Arc::clone(&loader);
        let result = thread::spawn(move || {
            let _lock = loader_clone.state.scripts.lock().unwrap();
            panic!("intentional panic to poison the lock");
        })
        .join();
        assert!(result.is_err(), "thread should have panicked");

        let refs = loader.script_refs();
        assert!(refs.is_ok(), "should recover from poison and not panic");
    }
}
