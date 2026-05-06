use anyhow::{Result, anyhow};
use rquickjs::{Context, Function, Runtime};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::loader::compute_policy_version;

#[derive(Debug)]
pub struct LoadedRoutineScript {
    pub name: String,
    pub script_ref: String,
    pub file: PathBuf,
    pub script_version: String,
    source: String,
}

impl Clone for LoadedRoutineScript {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            script_ref: self.script_ref.clone(),
            file: self.file.clone(),
            script_version: self.script_version.clone(),
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
/// mutating the store, so callers keep the last-known-good registry.
pub struct RoutineScriptLoader {
    scripts: RoutineScriptStore,
}

impl RoutineScriptLoader {
    pub fn new() -> Result<Self> {
        Ok(Self {
            scripts: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    #[cfg(test)]
    pub fn load_script(&self, root: &Path, path: &Path) -> Result<String> {
        let script = load_single_routine_script(root, path)?;
        tracing::debug!(
            routine_script = %script.script_ref,
            name = %script.name,
            file = %script.file.display(),
            version = %script.script_version,
            "loaded routine script"
        );
        let script_ref = script.script_ref.clone();
        self.scripts
            .lock()
            .map_err(|_| anyhow!("routine script store lock poisoned"))?
            .insert(script_ref.clone(), script);
        Ok(script_ref)
    }

    // Backward-compatible single-directory shim for callers that have not
    // migrated to `load_dirs`.
    #[allow(dead_code)]
    pub fn load_dir(&self, root: &Path) -> Result<usize> {
        self.load_dirs(&[root.to_path_buf()])
    }

    pub fn load_dirs(&self, roots: &[PathBuf]) -> Result<usize> {
        let mut seen_refs = HashSet::new();
        let mut candidates_by_ref: BTreeMap<String, Vec<RoutineScriptCandidate>> = BTreeMap::new();
        let existing_scripts: HashMap<String, LoadedRoutineScript> = self
            .scripts
            .lock()
            .map_err(|_| anyhow!("routine script store lock poisoned"))?
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
            if let Err(e) = collect_routine_script_paths(root, &mut entries) {
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

        let mut loaded = 0;
        let mut loaded_scripts = Vec::new();
        for (script_ref, candidates) in candidates_by_ref {
            let has_existing = existing_refs.contains(&script_ref);
            let mut selected = None;
            for candidate in candidates.iter().rev() {
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
                match load_single_routine_script(&candidate.root, &candidate.path) {
                    Ok(script) => {
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
                        tracing::error!(
                            routine_script = %candidate.path.display(),
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

        let pruned = self.apply_dir_reload(loaded_scripts, &seen_refs)?;
        if pruned > 0 {
            tracing::info!(count = pruned, "pruned missing routine scripts");
        }

        Ok(loaded)
    }

    pub fn get_script(&self, script_ref: &str) -> Result<Option<LoadedRoutineScript>> {
        Ok(self
            .scripts
            .lock()
            .map_err(|_| anyhow!("routine script store lock poisoned"))?
            .get(script_ref)
            .cloned())
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
            .scripts
            .lock()
            .map_err(|_| anyhow!("routine script store lock poisoned"))?
            .contains_key(script_ref))
    }

    pub fn script_refs(&self) -> Result<Vec<String>> {
        let mut refs: Vec<String> = self
            .scripts
            .lock()
            .map_err(|_| anyhow!("routine script store lock poisoned"))?
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
            .scripts
            .lock()
            .map_err(|_| anyhow!("routine script store lock poisoned"))?;
        for script in loaded_scripts {
            scripts.insert(script.script_ref.clone(), script);
        }
        let before = scripts.len();
        scripts.retain(|script_ref, _| seen_refs.contains(script_ref));
        Ok(before.saturating_sub(scripts.len()))
    }
}

impl Drop for RoutineScriptLoader {
    fn drop(&mut self) {
        if let Ok(mut scripts) = self.scripts.lock() {
            scripts.clear();
        }
    }
}

pub fn load_single_routine_script(root: &Path, path: &Path) -> Result<LoadedRoutineScript> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("read routine script {}: {e}", path.display()))?;
    let fallback_name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let script_ref = script_ref(root, path);
    let script_version = compute_policy_version(&source);

    let name = evaluate_routine_script_metadata(&source, &fallback_name, &script_ref, path)?;

    Ok(LoadedRoutineScript {
        name,
        script_ref,
        file: path.to_path_buf(),
        script_version,
        source,
    })
}

fn evaluate_routine_script_metadata(
    source: &str,
    fallback_name: &str,
    script_ref: &str,
    path: &Path,
) -> Result<String> {
    let runtime =
        Runtime::new().map_err(|e| anyhow!("routine QuickJS runtime creation failed: {e}"))?;
    install_interrupt_handler(&runtime, Duration::from_secs(5));
    let context = Context::full(&runtime)
        .map_err(|e| anyhow!("routine QuickJS context creation failed: {e}"))?;

    context.with(|ctx| -> Result<String> {
        let (name, _tick) =
            capture_registered_routine(ctx.clone(), source, fallback_name, script_ref, path)?;
        Ok(name)
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
        let (_, tick) = capture_registered_routine(
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
        let action_value: rquickjs::Value = tick
            .call((js_context,))
            .map_err(|e| anyhow!("routine script {} tick(ctx) failed: {e}", script.script_ref))?;
        ensure_acyclic_js_value(ctx, action_value.clone())?;
        js_value_to_json(action_value)
    })
}

fn install_interrupt_handler(runtime: &Runtime, timeout: Duration) {
    let started = Instant::now();
    runtime.set_interrupt_handler(Some(Box::new(move || started.elapsed() > timeout)));
}

fn capture_registered_routine<'js>(
    ctx: rquickjs::Ctx<'js>,
    source: &str,
    fallback_name: &str,
    script_ref: &str,
    path: &Path,
) -> Result<(String, Function<'js>)> {
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
        return Err(anyhow!(
            "JS eval error in routine script {}: {e}",
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

    Ok((name, tick))
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
                  throw new Error("routine action contains cyclic object graph");
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
    checker
        .call::<_, ()>((value,))
        .map_err(|e| anyhow!("routine action cycle check failed: {e}"))
}

fn script_ref(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn add_cached_candidates_for_root(
    existing_scripts: &HashMap<String, LoadedRoutineScript>,
    candidates_by_ref: &mut BTreeMap<String, Vec<RoutineScriptCandidate>>,
    seen_refs: &mut HashSet<String>,
    root_index: usize,
    root: &Path,
) {
    for (script_ref, script) in existing_scripts
        .iter()
        .filter(|(_, script)| script.file.starts_with(root))
    {
        seen_refs.insert(script_ref.clone());
        candidates_by_ref
            .entry(script_ref.clone())
            .or_default()
            .push(RoutineScriptCandidate {
                root_index,
                root: root.to_path_buf(),
                path: script.file.clone(),
                cached: Some(script.clone()),
            });
    }
}

fn collect_routine_script_paths(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_routine_script_paths(&path, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "js") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("routines");
        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dir(&root).unwrap(), 7);
        assert_eq!(
            loader.script_refs().unwrap(),
            vec![
                "agent-checkpoint-review.js".to_string(),
                "monitoring/automation-candidate-detector.js".to_string(),
                "monitoring/automation-candidate-recommender.js".to_string(),
                "monitoring/automation-executor.js".to_string(),
                "monitoring/memento-digest-writer.js".to_string(),
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
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("routines");
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
                assert!(
                    result
                        .get("suppression_summary")
                        .and_then(Value::as_str)
                        .is_some_and(|summary| summary.contains("자동화 인벤토리 상태=implemented"))
                );
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
}
