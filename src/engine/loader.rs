//! Policy loader: scans policies/ directory, evaluates JS files, extracts hooks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rquickjs::{Context, Function, Persistent};

use super::hooks::Hook;

/// A single loaded policy with its metadata and registered hooks.
#[derive(Debug)]
pub struct LoadedPolicy {
    pub name: String,
    pub file: PathBuf,
    pub priority: i32,
    pub hooks: HashMap<Hook, Persistent<Function<'static>>>,
    /// Dynamic hooks: custom function names not in the Hook enum.
    /// Keyed by the JS function name (e.g. "onCustomStateEnter").
    pub dynamic_hooks: HashMap<String, Persistent<Function<'static>>>,
}

// SAFETY: LoadedPolicy is only accessed while holding a Mutex.
// The Persistent<Function> values contain raw pointers to the QuickJS
// runtime, which is compiled with the "parallel" feature (thread-safe).
// All actual JS execution is serialized through Context::with() which
// acquires the runtime lock.
unsafe impl Send for LoadedPolicy {}
unsafe impl Sync for LoadedPolicy {}

/// Thread-safe container for loaded policies.
pub type PolicyStore = Arc<Mutex<Vec<LoadedPolicy>>>;

/// Scan the given directory for *.js files and load each as a policy.
pub fn load_policies_from_dir(ctx: &Context, dir: &Path) -> Result<Vec<LoadedPolicy>> {
    let mut policies = Vec::new();

    if !dir.exists() {
        tracing::warn!("Policies directory does not exist: {}", dir.display());
        return Ok(policies);
    }

    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "js"))
        .collect();
    entries.sort();

    for path in entries {
        match load_single_policy(ctx, &path) {
            Ok(policy) => {
                let dyn_count = policy.dynamic_hooks.len();
                if dyn_count > 0 {
                    tracing::info!(
                        "Loaded policy '{}' from {} ({} hooks, {} dynamic)",
                        policy.name,
                        path.display(),
                        policy.hooks.len(),
                        dyn_count,
                    );
                } else {
                    tracing::info!(
                        "Loaded policy '{}' from {} ({} hooks)",
                        policy.name,
                        path.display(),
                        policy.hooks.len()
                    );
                }
                policies.push(policy);
            }
            Err(e) => {
                tracing::error!("Failed to load policy {}: {e}", path.display());
            }
        }
    }

    // Sort by priority (lower number = higher priority)
    policies.sort_by_key(|p| p.priority);

    Ok(policies)
}

/// Load a single policy file.
pub fn load_single_policy(ctx: &Context, path: &Path) -> Result<LoadedPolicy> {
    let source = std::fs::read_to_string(path)?;
    let file_name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Use a JS-side capture approach: set up a global __policyCapture holder
    // and a pure-JS registerPolicy that stores the argument there.
    let policy = ctx.with(|ctx| -> Result<LoadedPolicy> {
        let globals = ctx.globals();

        // Set up capture holder and registerPolicy in JS
        let _: rquickjs::Value = ctx
            .eval(
                r#"
            var __policyCapture = { captured: null };
            agentdesk.registerPolicy = function(obj) {
                __policyCapture.captured = obj;
            };
        "#,
            )
            .map_err(|e| anyhow::anyhow!("failed to set up registerPolicy: {e}"))?;

        // Evaluate the policy file (non-strict so policies can use sloppy mode)
        let mut eval_opts = rquickjs::context::EvalOptions::default();
        eval_opts.strict = false;
        let eval_result: rquickjs::Result<rquickjs::Value> =
            ctx.eval_with_options(source.as_bytes().to_vec(), eval_opts);

        if let Err(e) = eval_result {
            return Err(anyhow::anyhow!("JS eval error in {}: {e}", path.display()));
        }

        // Retrieve the captured policy object from JS global
        let capture: rquickjs::Object = globals
            .get("__policyCapture")
            .map_err(|e| anyhow::anyhow!("__policyCapture missing: {e}"))?;
        let captured: rquickjs::Value = capture
            .get("captured")
            .map_err(|e| anyhow::anyhow!("get captured: {e}"))?;

        if captured.is_null() || captured.is_undefined() {
            return Err(anyhow::anyhow!(
                "Policy {} did not call agentdesk.registerPolicy()",
                path.display()
            ));
        }

        let policy_obj = captured
            .into_object()
            .ok_or_else(|| anyhow::anyhow!("registerPolicy argument is not an object"))?;

        // Extract name
        let name: String = policy_obj
            .get::<_, rquickjs::Value>("name")
            .ok()
            .and_then(|v| v.as_string().and_then(|s| s.to_string().ok()))
            .unwrap_or_else(|| file_name.clone());

        // Extract priority
        let priority: i32 = policy_obj
            .get::<_, rquickjs::Value>("priority")
            .ok()
            .and_then(|v| v.as_int())
            .unwrap_or(100);

        // Extract known hooks (Hook enum variants)
        let mut hooks = HashMap::new();
        let known_js_names: Vec<&str> = Hook::all().iter().map(|h| h.js_name()).collect();
        for hook in Hook::all() {
            let hook_val: rquickjs::Result<rquickjs::Value> = policy_obj.get(hook.js_name());
            if let Ok(val) = hook_val {
                if val.is_function() {
                    let func = val.into_function().unwrap();
                    let persistent = Persistent::save(&ctx, func);
                    hooks.insert(*hook, persistent);
                }
            }
        }

        // Extract dynamic hooks: any function starting with "on" that isn't a known hook
        let mut dynamic_hooks = HashMap::new();
        let skip_keys = ["name", "priority"];
        let props = policy_obj.keys::<String>();
        for key_result in props {
            if let Ok(key) = key_result {
                if skip_keys.contains(&key.as_str()) || known_js_names.contains(&key.as_str()) {
                    continue;
                }
                if let Ok(val) = policy_obj.get::<_, rquickjs::Value>(&key) {
                    if val.is_function() {
                        let func = val.into_function().unwrap();
                        let persistent = Persistent::save(&ctx, func);
                        dynamic_hooks.insert(key, persistent);
                    }
                }
            }
        }

        Ok(LoadedPolicy {
            name,
            file: path.to_path_buf(),
            priority,
            hooks,
            dynamic_hooks,
        })
    })?;

    Ok(policy)
}

// ── Hot reload via notify ────────────────────────────────────────

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// Start watching the policies directory for changes.
/// Returns a watcher handle that must be kept alive.
pub fn start_hot_reload(
    policies_dir: PathBuf,
    ctx: Context,
    store: PolicyStore,
) -> Result<RecommendedWatcher> {
    let (tx, rx) = std::sync::mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            use notify::EventKind;
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                    let _ = tx.send(event);
                }
                _ => {}
            }
        }
    })?;

    if policies_dir.exists() {
        watcher.watch(&policies_dir, RecursiveMode::NonRecursive)?;
    } else {
        tracing::warn!(
            "Policies dir {} does not exist yet; hot-reload will not work until it is created",
            policies_dir.display()
        );
    }

    // Spawn a background thread to process file-change events
    let dir = policies_dir.clone();
    std::thread::Builder::new()
        .name("policy-hot-reload".into())
        .spawn(move || {
            // Debounce: wait for events to settle
            use std::time::{Duration, Instant};
            let debounce = Duration::from_millis(500);
            let mut last_reload = Instant::now() - debounce;

            loop {
                match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(_event) => {
                        // Debounce: skip if we reloaded recently
                        if last_reload.elapsed() < debounce {
                            // Drain remaining events in the debounce window
                            while rx.try_recv().is_ok() {}
                            continue;
                        }

                        // Drain any queued events
                        while rx.try_recv().is_ok() {}

                        tracing::info!("Policy file change detected, reloading...");
                        match load_policies_from_dir(&ctx, &dir) {
                            Ok(new_policies) => {
                                let count = new_policies.len();
                                if let Ok(mut guard) = store.lock() {
                                    *guard = new_policies;
                                }
                                tracing::info!("Reloaded {count} policies");
                            }
                            Err(e) => {
                                tracing::error!("Failed to reload policies: {e}");
                            }
                        }
                        last_reload = Instant::now();
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::info!("Policy hot-reload watcher shutting down");
                        break;
                    }
                }
            }
        })?;

    Ok(watcher)
}
