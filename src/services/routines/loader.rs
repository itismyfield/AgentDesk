use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

mod discovery;
mod evaluator;
#[cfg(test)]
use discovery::candidate_failure_key;
use discovery::{
    DiscoveredRoutineScript, RoutineDiscoveryHooks, ValidatedHelperSurface, ValidatedRoutineRoot,
    ValidatedRuntimeRoot, add_cached_candidates_for_root, bind_routine_root_authority,
    collect_routine_script_paths, routine_roots_identity, script_ref, validate_routine_authority,
};
#[cfg(test)]
use evaluator::load_single_routine_script;
use evaluator::{evaluate_tick_action, load_single_routine_script_from_source};

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
        let key = routine_roots_identity(&runtime_authority, &validated_roots, &helper_authority);
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
        let (_, observed) = validate_routine_authority(&[], runtime_root, current_dir_override)?;
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
        let (validated_roots, observed_helper_surface) =
            validate_routine_authority(roots, &runtime_root, current_dir_override.as_deref())?;
        if let Some(expected) = &self.bound_helper_surface {
            expected.verify_observed(&observed_helper_surface)?;
        }
        if let Some(bound_roots) = &self.bound_roots {
            if bound_roots.len() != validated_roots.len() {
                return Err(
                    discovery::RoutineRootValidationError::ConfiguredRootCountChanged {
                        expected: bound_roots.len(),
                        observed: validated_roots.len(),
                    }
                    .into(),
                );
            }
            for (root, expected) in validated_roots.iter().zip(bound_roots) {
                if root.canonical != expected.canonical {
                    return Err(
                        discovery::RoutineRootValidationError::RootAuthorityChanged {
                            root_index: root.index,
                            root: root.configured.clone(),
                            expected_canonical_root: expected.canonical.clone(),
                            observed_canonical_root: root.canonical.clone(),
                        }
                        .into(),
                    );
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
        self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())?;
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
                self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())
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
            self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())?;
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

        self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())?;

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
                self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())?;
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

                self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())?;
                #[cfg(test)]
                self.state
                    .evaluation_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let evaluation = load_single_routine_script_from_source(
                    &candidate.root,
                    &candidate.path,
                    source,
                );
                self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())?;
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
        self.verify_bound_authority(&runtime_root, current_dir_override.as_deref())?;
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
mod tests;
