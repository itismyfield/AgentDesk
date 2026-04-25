//! Policy loader: scans policies/ directory, evaluates JS files, extracts hooks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rquickjs::{Context, Ctx, Error as JsError, Function, Persistent};

use super::hooks::Hook;

/// A single loaded policy with its metadata and registered hooks.
#[derive(Debug)]
pub struct LoadedPolicy {
    pub name: String,
    pub file: PathBuf,
    pub priority: i32,
    /// Short SHA256 hash (first 12 hex chars) of the policy file contents at
    /// load time. Used as the `policy_version` stamp in hook observability
    /// events — changes automatically on hot-reload (#1080).
    pub policy_version: String,
    pub hooks: HashMap<Hook, Persistent<Function<'static>>>,
    /// Dynamic hooks: custom function names not in the Hook enum.
    /// Keyed by the JS function name (e.g. "onCustomStateEnter").
    pub dynamic_hooks: HashMap<String, Persistent<Function<'static>>>,
    /// Ordering annotations (optional): `after` = this policy must run after
    /// the listed policy names for the same hook; `before` = this policy must
    /// run before them. Enables an explicit DAG override when multiple
    /// policies must register the same hook (issue #1079).
    pub after: Vec<String>,
    pub before: Vec<String>,
}

/// Compute a short content hash used as the policy version stamp.
pub fn compute_policy_version(source: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let full = hex::encode(hasher.finalize());
    full.chars().take(12).collect()
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

    // Conflict detection: reject duplicate (priority, hook) unless
    // disambiguated by an explicit `after`/`before` annotation.
    if let Err(msg) = detect_hook_conflicts(&policies) {
        // At initial load we warn rather than hard-fail so a broken policy
        // never bricks startup. Hot-reload pre-validation returns the error
        // to the caller so the previous version stays loaded (#1079).
        tracing::error!("policy hook orchestration issues:\n{msg}");
    }

    // Sort by priority, then refine within equal-priority tiers using
    // any `after` / `before` annotations (issue #1079).
    policies = order_policies_with_dag(policies);

    Ok(policies)
}

/// Load policies from a directory, returning an error if any validation fails.
/// Used by hot-reload pre-validation so the previous loaded version can be
/// preserved on failure.
pub fn load_policies_from_dir_validated(ctx: &Context, dir: &Path) -> Result<Vec<LoadedPolicy>> {
    let mut policies = Vec::new();

    if !dir.exists() {
        return Ok(policies);
    }

    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "js"))
        .collect();
    entries.sort();

    for path in entries {
        // Syntax + eval check: any single-file load error fails the whole
        // pre-validation so the previously loaded set stays intact.
        let policy = load_single_policy(ctx, &path)
            .map_err(|e| anyhow::anyhow!("policy {} failed to load: {e}", path.display()))?;
        policies.push(policy);
    }

    // Reject any orchestration conflicts during pre-validation.
    if let Err(msg) = detect_hook_conflicts(&policies) {
        return Err(anyhow::anyhow!("hook orchestration conflict(s):\n{msg}"));
    }

    policies = order_policies_with_dag(policies);
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
    let policy_version = compute_policy_version(&source);

    // Use a JS-side capture approach: set up a global __policyCapture holder
    // and a pure-JS registerPolicy that stores the argument there.
    let policy = ctx.with(|ctx| -> Result<LoadedPolicy> {
        let globals = ctx.globals();
        let policy_root = path.parent().unwrap_or_else(|| Path::new("."));
        install_policy_module_loader(&ctx, policy_root, policy_root)
            .map_err(|e| anyhow::anyhow!("failed to install policy module loader: {e}"))?;

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
        let skip_keys = ["name", "priority", "after", "before"];
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

        // Extract optional ordering annotations: `after: ["policy-name", ...]`
        // and `before: [...]`. These provide an explicit DAG override for
        // policies that must register the same hook at similar priorities.
        let after = extract_string_array(&policy_obj, "after");
        let before = extract_string_array(&policy_obj, "before");

        Ok(LoadedPolicy {
            name,
            file: path.to_path_buf(),
            priority,
            policy_version: policy_version.clone(),
            hooks,
            dynamic_hooks,
            after,
            before,
        })
    })?;

    Ok(policy)
}

/// Read an optional `string[]` property from a JS object. Missing or wrongly
/// typed values return an empty Vec (permissive — annotations are optional).
fn extract_string_array(obj: &rquickjs::Object<'_>, key: &str) -> Vec<String> {
    let Ok(val) = obj.get::<_, rquickjs::Value>(key) else {
        return Vec::new();
    };
    let Some(arr) = val.into_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..arr.len() {
        if let Ok(item) = arr.get::<rquickjs::Value>(i) {
            if let Some(s) = item.as_string().and_then(|s| s.to_string().ok()) {
                out.push(s);
            }
        }
    }
    out
}

/// Detect `(priority, hook)` collisions and missing `after/before` referents.
///
/// Returns a formatted error string if any conflict is found, `Ok(())` otherwise.
/// A collision is: two policies with the **same priority** registered for the
/// **same hook**, where *neither* policy uses an `after` / `before` annotation
/// naming the other to disambiguate ordering.
///
/// This is used both at startup (warn + keep policies) and during hot-reload
/// pre-validation (reject new version, keep old one loaded).
pub fn detect_hook_conflicts(policies: &[LoadedPolicy]) -> std::result::Result<(), String> {
    // Known policy names — used to validate `after`/`before` references.
    let known_names: HashSet<&str> = policies.iter().map(|p| p.name.as_str()).collect();
    let mut errors: Vec<String> = Vec::new();

    // Check dangling `after` / `before` references (warn-level, still collected).
    for p in policies {
        for dep in p.after.iter().chain(p.before.iter()) {
            if !known_names.contains(dep.as_str()) {
                errors.push(format!(
                    "policy '{}' references unknown policy '{}' in after/before",
                    p.name, dep
                ));
            }
        }
    }

    // Group by (hook_name, priority) → list of policy names.
    let mut groups: HashMap<(String, i32), Vec<&LoadedPolicy>> = HashMap::new();

    for p in policies {
        for hook in p.hooks.keys() {
            groups
                .entry((hook.js_name().to_string(), p.priority))
                .or_default()
                .push(p);
        }
        for hook_name in p.dynamic_hooks.keys() {
            groups
                .entry((hook_name.clone(), p.priority))
                .or_default()
                .push(p);
        }
    }

    for ((hook_name, priority), policies_in_group) in &groups {
        if policies_in_group.len() < 2 {
            continue;
        }
        // Have two+ policies at the same (hook, priority). Accept if every
        // pair is disambiguated by an `after` / `before` annotation naming
        // at least one side.
        let mut disambiguated = true;
        'outer: for i in 0..policies_in_group.len() {
            for j in (i + 1)..policies_in_group.len() {
                let a = policies_in_group[i];
                let b = policies_in_group[j];
                let a_refs_b =
                    a.after.iter().any(|n| n == &b.name) || a.before.iter().any(|n| n == &b.name);
                let b_refs_a =
                    b.after.iter().any(|n| n == &a.name) || b.before.iter().any(|n| n == &a.name);
                if !a_refs_b && !b_refs_a {
                    disambiguated = false;
                    break 'outer;
                }
            }
        }
        if !disambiguated {
            let names: Vec<&str> = policies_in_group.iter().map(|p| p.name.as_str()).collect();
            errors.push(format!(
                "hook orchestration conflict: hook '{}' priority {} has {} policies with \
                 ambiguous ordering ({:?}). Change one priority or add `after`/`before` \
                 annotations to disambiguate.",
                hook_name,
                priority,
                policies_in_group.len(),
                names
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Topologically order policies using priority as the primary key and
/// `after`/`before` DAG edges as ties/overrides.
///
/// Priority is the primary sort key (lower = earlier), matching existing
/// semantics. Within a priority tier, `after` / `before` annotations refine
/// the order. Cycles fall back to input order (warn logged).
pub fn order_policies_with_dag(mut policies: Vec<LoadedPolicy>) -> Vec<LoadedPolicy> {
    // Stable sort by priority first.
    policies.sort_by_key(|p| p.priority);

    // Build an index and adjacency list of directed edges (earlier -> later)
    // across policies sharing compatible priorities. We only apply DAG edges
    // when both endpoints have equal priority, so we don't break the priority
    // contract (lower priority always runs first).
    let n = policies.len();
    let name_to_idx: HashMap<String, usize> = policies
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.clone(), i))
        .collect();

    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (i, p) in policies.iter().enumerate() {
        for dep in &p.after {
            if let Some(&j) = name_to_idx.get(dep) {
                if policies[j].priority == p.priority {
                    // dep must run before p → j -> i
                    edges.push((j, i));
                }
            }
        }
        for dep in &p.before {
            if let Some(&j) = name_to_idx.get(dep) {
                if policies[j].priority == p.priority {
                    // p must run before dep → i -> j
                    edges.push((i, j));
                }
            }
        }
    }

    if edges.is_empty() {
        return policies;
    }

    let mut result_idx: Vec<usize> = Vec::with_capacity(n);
    // Walk in current priority order; inject dependencies before each node.
    let mut visited = vec![false; n];
    let mut stack_mark = vec![false; n];

    fn dfs(
        node: usize,
        adj_rev: &[Vec<usize>],
        visited: &mut [bool],
        stack_mark: &mut [bool],
        out: &mut Vec<usize>,
    ) -> bool {
        if stack_mark[node] {
            return false; // cycle
        }
        if visited[node] {
            return true;
        }
        stack_mark[node] = true;
        for &pred in &adj_rev[node] {
            if !dfs(pred, adj_rev, visited, stack_mark, out) {
                stack_mark[node] = false;
                return false;
            }
        }
        stack_mark[node] = false;
        visited[node] = true;
        out.push(node);
        true
    }

    // Reverse adjacency: for each node, which nodes must precede it.
    let mut adj_rev: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (from, to) in &edges {
        adj_rev[*to].push(*from);
    }

    // Iterate in current priority order, DFS-emit predecessors first.
    for i in 0..n {
        if !visited[i] && !dfs(i, &adj_rev, &mut visited, &mut stack_mark, &mut result_idx) {
            tracing::warn!(
                "policy ordering DAG has cycle involving '{}'; falling back to priority order",
                policies[i].name
            );
            // Fallback: return priority-sorted list as-is.
            return policies;
        }
    }

    // Reassemble in topological order (but only swaps within equal-priority
    // tiers because we only added edges for equal-priority pairs).
    let mut out: Vec<Option<LoadedPolicy>> = policies.into_iter().map(Some).collect();
    let mut sorted: Vec<LoadedPolicy> = Vec::with_capacity(n);
    for idx in result_idx {
        if let Some(p) = out[idx].take() {
            sorted.push(p);
        }
    }
    sorted
}

fn js_module_error(message: impl Into<String>) -> JsError {
    JsError::new_from_js_message("policy module", "path", message.into())
}

fn resolve_policy_module(
    policy_root: &Path,
    base_dir: &Path,
    spec: &str,
) -> std::result::Result<PathBuf, String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return Err(format!(
            "only relative policy module paths are supported: {spec}"
        ));
    }

    let root = policy_root
        .canonicalize()
        .map_err(|e| format!("canonicalize policies root {}: {e}", policy_root.display()))?;
    let base = base_dir
        .canonicalize()
        .map_err(|e| format!("canonicalize module base {}: {e}", base_dir.display()))?;
    if !base.starts_with(&root) {
        return Err(format!(
            "module base {} is outside policies root {}",
            base.display(),
            root.display()
        ));
    }

    let mut candidate = base.join(spec);
    if candidate.extension().is_none() {
        candidate.set_extension("js");
    }
    let resolved = candidate
        .canonicalize()
        .map_err(|e| format!("resolve module {spec} from {}: {e}", base.display()))?;
    if !resolved.starts_with(&root) {
        return Err(format!(
            "policy module {} escapes policies root {}",
            resolved.display(),
            root.display()
        ));
    }
    if resolved.extension().and_then(|ext| ext.to_str()) != Some("js") {
        return Err(format!(
            "policy module must be a .js file: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

pub(crate) fn install_policy_module_loader(
    ctx: &Ctx<'_>,
    policy_root: &Path,
    entry_dir: &Path,
) -> rquickjs::Result<()> {
    let root = policy_root.canonicalize().map_err(|e| {
        js_module_error(format!(
            "canonicalize policies root {}: {e}",
            policy_root.display()
        ))
    })?;
    let entry = entry_dir.canonicalize().map_err(|e| {
        js_module_error(format!(
            "canonicalize policy entry dir {}: {e}",
            entry_dir.display()
        ))
    })?;
    let globals = ctx.globals();
    globals.set("__policyEntryDir", entry.to_string_lossy().to_string())?;

    let resolve_root = root.clone();
    let resolve_module = Function::new(
        ctx.clone(),
        move |spec: String, base_dir: String| -> rquickjs::Result<String> {
            resolve_policy_module(&resolve_root, Path::new(&base_dir), &spec)
                .map(|path| path.to_string_lossy().to_string())
                .map_err(js_module_error)
        },
    )?;
    globals.set("__policyResolveModule", resolve_module)?;

    let read_root = root.clone();
    let read_module = Function::new(
        ctx.clone(),
        move |filename: String| -> rquickjs::Result<String> {
            let path = PathBuf::from(&filename);
            let resolved = path.canonicalize().map_err(|e| {
                js_module_error(format!("resolve module file {}: {e}", path.display()))
            })?;
            if !resolved.starts_with(&read_root) {
                return Err(js_module_error(format!(
                    "policy module {} escapes policies root {}",
                    resolved.display(),
                    read_root.display()
                )));
            }
            std::fs::read_to_string(&resolved).map_err(|e| {
                js_module_error(format!("read policy module {}: {e}", resolved.display()))
            })
        },
    )?;
    globals.set("__policyReadModule", read_module)?;

    let dirname = Function::new(ctx.clone(), move |filename: String| -> String {
        Path::new(&filename)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_string_lossy()
            .to_string()
    })?;
    globals.set("__policyDirname", dirname)?;

    ctx.eval::<(), _>(
        r#"
        (function() {
          var moduleCache = Object.create(null);

          function requireFrom(spec, baseDir) {
            if (typeof spec !== "string") {
              throw new TypeError("policy require path must be a string");
            }
            var filename = __policyResolveModule(spec, baseDir);
            if (moduleCache[filename]) {
              return moduleCache[filename].exports;
            }

            var module = { exports: {} };
            moduleCache[filename] = module;
            var dirname = __policyDirname(filename);
            var source = __policyReadModule(filename);
            var wrapper = "(function(module, exports, require, __filename, __dirname) {\n" +
              source +
              "\n})\n//# sourceURL=" + filename;
            var compiled = (0, eval)(wrapper);
            compiled(module, module.exports, function(childSpec) {
              return requireFrom(childSpec, dirname);
            }, filename, dirname);
            return module.exports;
          }

          globalThis.require = function(spec) {
            return requireFrom(spec, __policyEntryDir);
          };
          globalThis.module = { exports: {} };
          globalThis.exports = globalThis.module.exports;
        })();
        "#,
    )?;

    Ok(())
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
        watcher.watch(&policies_dir, RecursiveMode::Recursive)?;
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

                        tracing::info!("Policy file change detected, pre-validating...");
                        // Hot-reload pre-validation (#1079): syntax/eval check
                        // plus hook orchestration conflict check. If either
                        // fails we keep the currently loaded version.
                        match load_policies_from_dir_validated(&ctx, &dir) {
                            Ok(new_policies) => {
                                let count = new_policies.len();
                                if let Ok(mut guard) = store.lock() {
                                    *guard = new_policies;
                                }
                                tracing::info!("Reloaded {count} policies");
                            }
                            Err(e) => {
                                tracing::warn!(
                                    policies_dir = %dir.display(),
                                    error = %e,
                                    "hot-reload pre-validation failed; keeping previously loaded policies"
                                );
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
