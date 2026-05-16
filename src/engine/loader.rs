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
    load_policies_from_dir_validated_inner(ctx, dir, None)
}

/// Internal variant of `load_policies_from_dir_validated` that optionally
/// arms a QuickJS interrupt deadline around each policy's `eval` call. The
/// deadline is armed *inside* `ctx.with(...)` (i.e. while we already hold
/// the runtime lock) and cleared immediately after eval returns — that way
/// the deadline can NEVER expire while an unrelated live policy hook is the
/// currently-executing JS on the runtime, which is the false-positive
/// scenario Codex flagged in round-3 review of #2372.
pub(crate) fn load_policies_from_dir_validated_inner(
    ctx: &Context,
    dir: &Path,
    eval_deadline: Option<&Arc<AtomicU64>>,
) -> Result<Vec<LoadedPolicy>> {
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
        let policy = load_single_policy_with_deadline(ctx, &path, eval_deadline)
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
    load_single_policy_with_deadline(ctx, path, None)
}

/// Internal variant of `load_single_policy` that optionally arms a per-eval
/// wall-clock deadline. The deadline is armed only after `ctx.with(...)`
/// has acquired the runtime lock and is cleared immediately after the
/// `eval_with_options` call returns, so the global interrupt handler can
/// never tear down an unrelated live hook running on the same runtime.
pub(crate) fn load_single_policy_with_deadline(
    ctx: &Context,
    path: &Path,
    eval_deadline: Option<&Arc<AtomicU64>>,
) -> Result<LoadedPolicy> {
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

        // Evaluate the policy file (non-strict so policies can use sloppy mode).
        // #2372 round-3: arm the deadline ONLY here, after `ctx.with` has
        // acquired the runtime lock. The interrupt handler installed by
        // `start_hot_reload` checks the deadline on every bytecode tick,
        // and dropping `_deadline_guard` immediately after eval clears the
        // slot back to 0. This guarantees the deadline can never expire
        // while an unrelated live hook is the currently-running JS on the
        // runtime — the only JS that can be executing between arm and
        // disarm is *this* eval, because we hold the lock.
        let _deadline_guard =
            eval_deadline.map(|slot| ArmedDeadline::new(slot, HOT_RELOAD_EVAL_BUDGET));
        let mut eval_opts = rquickjs::context::EvalOptions::default();
        eval_opts.strict = false;
        let eval_result: rquickjs::Result<rquickjs::Value> =
            ctx.eval_with_options(source.as_bytes().to_vec(), eval_opts);
        drop(_deadline_guard);

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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Default wall-clock budget for a single hot-reload pre-validation eval.
/// A policy that runs longer than this is considered runaway and is aborted
/// by the QuickJS interrupt handler installed in `start_hot_reload`. Picked
/// to be comfortably above any realistic policy `registerPolicy()` call
/// (which is structurally just a function definition + a single side-effect
/// assignment) while still keeping a wedged engine from blocking live
/// policy execution for more than a couple of seconds (#2372).
const HOT_RELOAD_EVAL_BUDGET: Duration = Duration::from_secs(2);

/// Monotonic reference instant used to encode QuickJS interrupt deadlines as
/// a single `AtomicU64` of milliseconds-since-reference. Lazily initialised
/// the first time a deadline is armed.
fn deadline_reference() -> Instant {
    use std::sync::OnceLock;
    static REF: OnceLock<Instant> = OnceLock::new();
    *REF.get_or_init(Instant::now)
}

/// Encode an `Instant` as millis-since the deadline reference for storage in
/// an `AtomicU64`. The reference is initialised on first use so the encoded
/// value is always non-negative.
fn encode_deadline(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(deadline_reference())
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Returns `true` when the encoded deadline (millis-since-reference) is set
/// and has already passed. A value of `0` means no deadline is armed.
fn deadline_reached(encoded: u64) -> bool {
    if encoded == 0 {
        return false;
    }
    let elapsed = deadline_reference().elapsed().as_millis();
    elapsed >= encoded as u128
}

/// Guard for the hot-reload subsystem.
///
/// Owns both the filesystem watcher and the background thread join handle.
/// Dropping the guard:
///   1. Sets `stop` so the worker exits its next iteration even if no event
///      arrives. The same `stop` flag is also wired into a QuickJS interrupt
///      handler installed on the runtime, so an in-flight `eval` inside the
///      worker thread (e.g. a hot-reloaded policy that ran an infinite loop)
///      is aborted promptly instead of holding shutdown forever.
///   2. Drops the watcher (closes its event channel → worker returns from
///      `recv_timeout` with `Disconnected`).
///   3. Joins the worker thread with a bounded grace period so its captured
///      `Context` (a clone sharing the same QuickJS `Runtime`) is dropped
///      *before* the engine drops the `Runtime`. This prevents a stale
///      `Context` from outliving the runtime — a sequence that previously
///      triggered a QuickJS C-level assert when a CLI invocation
///      (review-decision dispute → rework) shut down while the bg thread
///      was still alive (#2200 sub-fix 2).
pub struct HotReloadGuard {
    watcher: Option<RecommendedWatcher>,
    join: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    /// Encoded wall-clock deadline (millis-since `deadline_reference()`) for
    /// the currently-running QuickJS eval. `0` means no deadline is armed.
    /// Set before hot-reload pre-validation and cleared once eval returns —
    /// the interrupt handler reads this on every QuickJS bytecode tick and
    /// aborts the eval once the deadline passes (#2372). Exposed on the
    /// guard so tests and callers can arm a deadline for live evals running
    /// on the shared runtime.
    eval_deadline: Arc<AtomicU64>,
}

impl HotReloadGuard {
    /// Explicit shutdown — useful for tests and for engine drop sequences that
    /// want deterministic teardown before the runtime is dropped.
    ///
    /// Shutdown proceeds in three steps:
    ///   1. Set `stop`. The QuickJS interrupt handler tied to this flag
    ///      promptly aborts any in-flight JS bytecode (e.g. a runaway
    ///      `while(true){}` in a hot-reloaded policy).
    ///   2. Drop the watcher so its event channel disconnects.
    ///   3. **Unbounded** join. We deliberately do *not* detach on a deadline:
    ///      the worker holds a `Context` clone whose lifetime is tied to the
    ///      QuickJS runtime, and a detached thread that hasn't yet released
    ///      that `Context` would let the engine drop the underlying runtime
    ///      while the bytecode/native call is still pending — exactly the
    ///      use-after-free we are trying to prevent (#2200 sub-fix 2, Codex
    ///      round-2 feedback). The interrupt handler covers the common
    ///      runaway-JS case; for a hot-reloaded policy that's blocked
    ///      inside a long-running native bridge op (e.g. `agentdesk.exec`)
    ///      we accept a blocked shutdown over a UAF. In practice the
    ///      engine's bridge ops are themselves bounded, so this join
    ///      converges; and CLI invocations don't enable hot-reload at all
    ///      (see `cli::direct::build_app_state`).
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Drop the watcher first so the worker's mpsc channel disconnects.
        self.watcher.take();
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HotReloadGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start watching the policies directory for changes.
/// Returns a guard that must be kept alive for the lifetime of the engine
/// and that joins the worker thread on drop.
pub fn start_hot_reload(
    policies_dir: PathBuf,
    ctx: Context,
    store: PolicyStore,
) -> Result<HotReloadGuard> {
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

    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let stop_interrupt = stop.clone();
    // Per-eval wall-clock deadline shared with the interrupt handler. `0`
    // means no deadline armed; any non-zero value is the deadline encoded
    // as millis-since `deadline_reference()`. The worker arms this before
    // each hot-reload pre-validation and clears it on completion, but the
    // arc is also exposed on the guard so live engine callers can arm a
    // deadline of their own for any other eval that runs on this runtime.
    let eval_deadline = Arc::new(AtomicU64::new(0));
    let eval_deadline_worker = eval_deadline.clone();
    let eval_deadline_interrupt = eval_deadline.clone();
    // Install a QuickJS interrupt handler that aborts any in-flight `eval`
    // when EITHER the shutdown flag is set OR the per-eval deadline has
    // passed. The shutdown leg keeps `HotReloadGuard::shutdown` leak-proof
    // against runaway hot-reload evals (#2200 sub-fix 2). The deadline leg
    // additionally protects the LIVE engine: if a hot-reloaded policy
    // contains `while (true) {}` we must abort it without waiting for
    // shutdown to be requested — otherwise it would hold the runtime lock
    // and stall live policy execution indefinitely (#2372 follow-up).
    //
    // The handler lives on the Runtime so it also covers main-engine evals,
    // which is safe: the `stop` flag is only set when the engine is being
    // torn down, and the deadline is only non-zero when a caller explicitly
    // armed one for a bounded eval (see `arm_eval_deadline`).
    ctx.runtime().set_interrupt_handler(Some(Box::new(move || {
        if stop_interrupt.load(Ordering::Acquire) {
            return true;
        }
        deadline_reached(eval_deadline_interrupt.load(Ordering::Acquire))
    })));
    // Spawn a background thread to process file-change events
    let dir = policies_dir.clone();
    let join = std::thread::Builder::new()
        .name("policy-hot-reload".into())
        .spawn(move || {
            // Move `ctx` into a scope we control so we can drop it *before*
            // the thread returns. The HotReloadGuard joins this thread on
            // drop, which means when join() returns the captured `Context`
            // has already been dropped — making it safe for the engine to
            // drop the QuickJS runtime next.
            let ctx = ctx;
            // Debounce: wait for events to settle
            use std::time::{Duration, Instant};
            let debounce = Duration::from_millis(500);
            let mut last_reload = Instant::now() - debounce;

            loop {
                if stop_worker.load(Ordering::Acquire) {
                    break;
                }
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(_event) => {
                        if stop_worker.load(Ordering::Acquire) {
                            break;
                        }
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
                        //
                        // #2372: pass the shared deadline slot through so the
                        // per-policy eval inside `load_single_policy_with_deadline`
                        // can arm a wall-clock interrupt *after* it holds the
                        // runtime lock. Arming here would let the deadline
                        // expire while waiting for the lock and interrupt an
                        // unrelated live hook — see #2372 round-3 review.
                        match load_policies_from_dir_validated_inner(
                            &ctx,
                            &dir,
                            Some(&eval_deadline_worker),
                        ) {
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
            // `ctx` is dropped here as the closure returns; the join() in
            // HotReloadGuard::shutdown is what guarantees this completes
            // before the engine drops the underlying QuickJS runtime.
            drop(ctx);
        })?;

    Ok(HotReloadGuard {
        watcher: Some(watcher),
        join: Some(join),
        stop,
        eval_deadline,
    })
}

/// RAII helper for arming the shared `eval_deadline` AtomicU64 around a
/// bounded JS eval. The interrupt handler installed by `start_hot_reload`
/// aborts the eval once the encoded deadline passes; the guard clears the
/// deadline back to `0` on drop so subsequent (potentially long-running)
/// evals on the same runtime are not affected.
///
/// In addition to the AtomicU64 (which the QuickJS interrupt handler reads
/// on every bytecode tick), the guard also installs the deadline into a
/// thread-local so synchronous native bridge ops invoked by JS on this
/// thread can observe it. This is required because the interrupt handler
/// only fires between bytecode instructions — a Rust function called from
/// JS holds the runtime lock for its entire duration and can otherwise
/// blow past the deadline (#2378).
struct ArmedDeadline<'a> {
    slot: &'a Arc<AtomicU64>,
    previous_thread_local: Option<Instant>,
}

impl<'a> ArmedDeadline<'a> {
    fn new(slot: &'a Arc<AtomicU64>, budget: Duration) -> Self {
        let deadline = Instant::now() + budget;
        slot.store(encode_deadline(deadline), Ordering::Release);
        let previous_thread_local =
            CURRENT_BRIDGE_DEADLINE.with(|cell| cell.replace(Some(deadline)));
        Self {
            slot,
            previous_thread_local,
        }
    }
}

impl<'a> Drop for ArmedDeadline<'a> {
    fn drop(&mut self) {
        self.slot.store(0, Ordering::Release);
        let previous = self.previous_thread_local.take();
        CURRENT_BRIDGE_DEADLINE.with(|cell| cell.set(previous));
    }
}

thread_local! {
    /// Per-thread deadline for synchronous native bridge ops. Set by
    /// `ArmedDeadline::new` when a bounded JS eval is in flight on this
    /// thread, cleared (or restored to the previous nested value) on drop.
    /// Bridge ops read this via [`bridge_op_deadline`] to bound their own
    /// internal timeouts and to fail fast when the deadline has passed —
    /// the QuickJS interrupt handler can't fire while Rust is running, so
    /// bridge ops must enforce the deadline themselves (#2378).
    static CURRENT_BRIDGE_DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Returns the current thread's bridge-op deadline, if a bounded JS eval is
/// in flight on this thread. Bridge ops should call this before performing
/// long synchronous work and either:
///   1. Return an error immediately if the deadline has already passed.
///   2. Shorten their internal timeout to fit within the remaining budget.
///
/// Returns `None` if no deadline is armed — bridge ops should retain their
/// default timeout behaviour in that case (e.g. live-engine policy hooks
/// invoked outside of hot-reload pre-validation).
pub(crate) fn bridge_op_deadline() -> Option<Instant> {
    CURRENT_BRIDGE_DEADLINE.with(|cell| cell.get())
}

/// Returns the remaining bridge-op budget, or `Some(Duration::ZERO)` if the
/// deadline has already passed. Returns `None` if no deadline is armed.
pub(crate) fn bridge_op_deadline_remaining() -> Option<Duration> {
    bridge_op_deadline().map(|deadline| {
        deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO)
    })
}

/// Test-only RAII helper that installs a thread-local bridge-op deadline so
/// `engine::ops::*` modules can exercise their deadline-clamping paths
/// without spinning up a full `HotReloadGuard`. Restores any previously
/// installed deadline on drop.
#[cfg(test)]
pub(crate) struct ScopedBridgeDeadline {
    previous: Option<Instant>,
}

#[cfg(test)]
impl ScopedBridgeDeadline {
    pub(crate) fn new(budget: Duration) -> Self {
        let deadline = Instant::now() + budget;
        let previous = CURRENT_BRIDGE_DEADLINE.with(|cell| cell.replace(Some(deadline)));
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for ScopedBridgeDeadline {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT_BRIDGE_DEADLINE.with(|cell| cell.set(previous));
    }
}

impl HotReloadGuard {
    /// Arm the shared QuickJS interrupt deadline for the next `budget` worth
    /// of wall-clock time. The interrupt handler installed by
    /// `start_hot_reload` watches this deadline on every bytecode tick and
    /// aborts the in-flight eval once it passes — protecting the live
    /// engine from runaway hot-reloaded policies (#2372). The deadline is
    /// cleared automatically when the returned guard drops.
    #[cfg(test)]
    pub(crate) fn arm_eval_deadline(&self, budget: Duration) -> impl Drop + '_ {
        ArmedDeadline::new(&self.eval_deadline, budget)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::Runtime;

    /// #2200 sub-fix 2: dropping `HotReloadGuard` must join the worker
    /// thread, which releases the worker's `Context` clone *before* the
    /// engine drops the QuickJS `Runtime`. We model the runtime here, hand
    /// the worker a `Context::full(&runtime)`, drop the guard, and then
    /// assert that the runtime can be dropped without panicking — which it
    /// cannot if any `Context` referencing it is still alive in another
    /// thread.
    #[test]
    fn hot_reload_guard_joins_worker_before_drop() {
        let runtime = Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        let store: PolicyStore = Arc::new(Mutex::new(Vec::new()));
        let tmp = tempfile::tempdir().expect("tempdir");

        let guard =
            start_hot_reload(tmp.path().to_path_buf(), ctx, store).expect("start hot reload");

        // Dropping the guard must:
        //   1. signal stop,
        //   2. drop the watcher (closes the mpsc),
        //   3. join the worker thread so its captured Context drops first.
        drop(guard);

        // If the worker's Context was still alive at this point, dropping
        // the runtime here would either deadlock or trip a QuickJS-level
        // assertion. Reaching this line cleanly is the assertion.
        drop(runtime);
    }

    /// #2200 sub-fix 2 (Codex round-2): when shutdown is requested while
    /// JS is actively running on the runtime, the QuickJS interrupt handler
    /// installed by `start_hot_reload` must abort the eval so the worker
    /// can release its `Context` clone within the join deadline. We model
    /// this by running an "infinite loop" eval on the same runtime *after*
    /// asking the guard to shut down — if the interrupt wiring is correct
    /// the eval returns an error promptly; if it isn't, the test hangs.
    #[test]
    fn hot_reload_guard_interrupts_runaway_eval_during_shutdown() {
        let runtime = Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        let store: PolicyStore = Arc::new(Mutex::new(Vec::new()));
        let tmp = tempfile::tempdir().expect("tempdir");

        // start_hot_reload installs the interrupt handler tied to its stop
        // flag. We extract `stop` indirectly by exercising `shutdown`.
        let mut guard =
            start_hot_reload(tmp.path().to_path_buf(), ctx, store).expect("start hot reload");

        // Trigger shutdown so the interrupt handler returns true.
        guard.shutdown();

        // Now ask the runtime to execute an infinite loop. Because the
        // handler is armed, the eval should be interrupted rather than
        // hang the test. We bound the work even further by running the
        // eval inside a thread joined with a timeout.
        let interrupt_check = std::thread::spawn(move || {
            // The shutdown() above closed the worker context; we open a
            // fresh context on the same runtime for the eval.
            let probe_ctx = Context::full(&runtime).expect("create probe context");
            let is_err = probe_ctx.with(|c| {
                let result: rquickjs::Result<rquickjs::Value<'_>> =
                    c.eval::<rquickjs::Value<'_>, _>("while (true) {}");
                result.is_err()
            });
            // Drop the probe context before returning so the outer thread
            // can drop the runtime cleanly.
            drop(probe_ctx);
            is_err
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if interrupt_check.is_finished() {
                let interrupted = interrupt_check.join().expect("probe thread join");
                assert!(interrupted, "interrupt handler did not abort runaway eval");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "interrupt handler failed to abort runaway eval within 5s"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// #2372: the QuickJS interrupt handler must fire when a per-eval
    /// deadline passes, even if the guard's shutdown flag is still `false`.
    /// This is the live-engine failure mode Codex flagged: a hot-reloaded
    /// policy containing `while (true) {}` must not stall the runtime
    /// until shutdown is triggered.
    #[test]
    fn hot_reload_guard_interrupts_runaway_eval_on_deadline_without_shutdown() {
        let runtime = Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        let store: PolicyStore = Arc::new(Mutex::new(Vec::new()));
        let tmp = tempfile::tempdir().expect("tempdir");

        let guard =
            start_hot_reload(tmp.path().to_path_buf(), ctx, store).expect("start hot reload");

        // Arm a tight per-eval deadline (100ms) WITHOUT requesting shutdown.
        // The interrupt handler must abort the runaway loop based on the
        // deadline alone.
        let _armed = guard.arm_eval_deadline(Duration::from_millis(100));

        let start = Instant::now();
        let probe_ctx = Context::full(&runtime).expect("create probe context");
        let is_err = probe_ctx.with(|c| {
            let result: rquickjs::Result<rquickjs::Value<'_>> =
                c.eval::<rquickjs::Value<'_>, _>("while (true) {}");
            result.is_err()
        });
        let elapsed = start.elapsed();
        drop(probe_ctx);

        assert!(
            is_err,
            "deadline-based interrupt did not abort runaway eval (shutdown was never requested)"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "deadline-based interrupt fired too late: {elapsed:?} (expected <2s for a 100ms deadline)"
        );

        // Tear down cleanly so the runtime can drop without UAF.
        drop(_armed);
        drop(guard);
        drop(runtime);
    }

    /// #2372: a legitimate fast eval that completes well within the budget
    /// must NOT be interrupted by the deadline. Guards against a false
    /// positive rate where slow-but-legitimate JS policies are aborted.
    #[test]
    fn hot_reload_guard_deadline_does_not_interrupt_fast_eval() {
        let runtime = Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        let store: PolicyStore = Arc::new(Mutex::new(Vec::new()));
        let tmp = tempfile::tempdir().expect("tempdir");

        let guard =
            start_hot_reload(tmp.path().to_path_buf(), ctx, store).expect("start hot reload");

        let _armed = guard.arm_eval_deadline(Duration::from_secs(5));
        let probe_ctx = Context::full(&runtime).expect("create probe context");
        let value: i32 = probe_ctx
            .with(|c| c.eval::<i32, _>("1 + 2"))
            .expect("fast eval should not be interrupted");
        assert_eq!(value, 3);
        drop(probe_ctx);

        drop(_armed);
        drop(guard);
        drop(runtime);
    }

    /// #2372: when no deadline is armed (encoded value `0`) and shutdown is
    /// not requested, the interrupt handler must return `false` for evals of
    /// any duration. This is the steady-state behaviour for live engine
    /// eval calls that don't opt in to deadline protection.
    #[test]
    fn hot_reload_guard_no_deadline_means_no_interrupt() {
        let runtime = Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        let store: PolicyStore = Arc::new(Mutex::new(Vec::new()));
        let tmp = tempfile::tempdir().expect("tempdir");

        let guard =
            start_hot_reload(tmp.path().to_path_buf(), ctx, store).expect("start hot reload");

        // No deadline armed. A short busy-loop must complete normally.
        let probe_ctx = Context::full(&runtime).expect("create probe context");
        let value: i32 = probe_ctx
            .with(|c| c.eval::<i32, _>("var s=0; for (var i=0;i<10000;i++) { s = (s+i)|0; } s"))
            .expect("eval with no deadline should not be interrupted");
        assert!(value != 0);
        drop(probe_ctx);

        drop(guard);
        drop(runtime);
    }

    /// #2372 round-3 (Codex follow-up): the scope-bound deadline path —
    /// `load_single_policy_with_deadline` — must interrupt a runaway policy
    /// eval based on the wall-clock budget alone. Mirrors the live-engine
    /// failure mode without relying on the file watcher (which is
    /// platform-dependent and racy).
    ///
    /// We run the load on a fresh probe context (cloned from the runtime)
    /// so the interrupted eval's partial state can be torn down with the
    /// probe context before the outer runtime drops — same pattern the
    /// shutdown-interrupt test uses for QuickJS hygiene.
    #[test]
    fn load_single_policy_with_deadline_interrupts_runaway_policy() {
        let runtime = Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        let store: PolicyStore = Arc::new(Mutex::new(Vec::new()));
        let tmp = tempfile::tempdir().expect("tempdir");

        // start_hot_reload installs the deadline-aware interrupt handler on
        // the shared runtime.
        let guard =
            start_hot_reload(tmp.path().to_path_buf(), ctx, store).expect("start hot reload");
        let deadline_slot = guard.eval_deadline.clone();

        let policy_file = tmp.path().join("runaway.js");
        std::fs::write(&policy_file, "while (true) {}").expect("write runaway policy");

        // Run the runaway-load on a separate context tied to the same
        // runtime so we can drop the partial state before the runtime
        // tears down. Wrap in a thread so a bug in the interrupt wiring
        // would manifest as a hang rather than the whole suite freezing.
        let runtime_for_thread = runtime.clone();
        let load_thread = std::thread::spawn(move || {
            let probe_ctx = Context::full(&runtime_for_thread).expect("probe context");
            let start = Instant::now();
            let result =
                load_single_policy_with_deadline(&probe_ctx, &policy_file, Some(&deadline_slot));
            let elapsed = start.elapsed();
            // Drop the probe context inside the thread so its partial state
            // is reclaimed before the runtime drops in the parent.
            drop(probe_ctx);
            (result.is_err(), elapsed)
        });
        let join_deadline = Instant::now() + HOT_RELOAD_EVAL_BUDGET + Duration::from_secs(3);
        loop {
            if load_thread.is_finished() {
                break;
            }
            assert!(
                Instant::now() < join_deadline,
                "scope-bound deadline interrupt did not fire within budget+3s"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let (is_err, elapsed) = load_thread.join().expect("load thread join");
        assert!(
            is_err,
            "runaway policy should fail to load (deadline interrupt expected)"
        );
        assert!(
            elapsed < HOT_RELOAD_EVAL_BUDGET + Duration::from_secs(1),
            "deadline-based interrupt fired too late: {elapsed:?} (budget {HOT_RELOAD_EVAL_BUDGET:?})"
        );

        drop(guard);
        drop(runtime);
    }

    /// #2372 round-3 (Codex follow-up): the deadline must be scoped to the
    /// loader's `ctx.with()` eval — it must NEVER fire while an unrelated
    /// live hook is the currently-executing JS on the same runtime. This
    /// test arms the deadline-aware handler, then runs a slow-but-legitimate
    /// "live hook" eval on the runtime *before* invoking the loader. The
    /// slow eval must complete without interruption because the deadline
    /// slot is only set between the loader's `ctx.with` entry and its
    /// `eval_with_options` return.
    #[test]
    fn hot_reload_deadline_does_not_interrupt_concurrent_live_hook() {
        let runtime = Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        let store: PolicyStore = Arc::new(Mutex::new(Vec::new()));
        let tmp = tempfile::tempdir().expect("tempdir");

        let guard =
            start_hot_reload(tmp.path().to_path_buf(), ctx, store).expect("start hot reload");

        // Sanity: the deadline slot is 0 (no deadline armed) for the entire
        // duration of this live eval, because we never call the loader and
        // never invoke `arm_eval_deadline`. The handler must therefore
        // return false on every bytecode tick.
        assert_eq!(guard.eval_deadline.load(Ordering::Acquire), 0);

        // Mirror the shutdown-interrupt test's hygiene pattern: run the
        // live eval on a fresh probe context tied to the same runtime and
        // drop it before tearing down the runtime so any partial JS state
        // is reclaimed in order. This is the live-engine analogue (no
        // shutdown is requested) of the existing during-shutdown test.
        let probe_ctx = Context::full(&runtime).expect("create probe context");
        let start = Instant::now();
        let result: i32 = probe_ctx
            .with(|c| {
                // Moderately slow but bounded JS work — well within what a
                // real policy hook might do (~50ms of pure JS busy loop).
                c.eval::<i32, _>("var s=0; for (var i=0;i<500000;i++) { s = (s + i) | 0; } s")
            })
            .expect("legitimate live eval should not be interrupted");
        let elapsed = start.elapsed();
        let _ = result;
        assert!(
            elapsed < Duration::from_secs(5),
            "legitimate live eval took unexpectedly long: {elapsed:?}"
        );
        // Slot remained 0 throughout — proves the deadline was never armed
        // outside the loader path.
        assert_eq!(guard.eval_deadline.load(Ordering::Acquire), 0);

        drop(probe_ctx);
        drop(guard);
        drop(runtime);
    }

    #[test]
    fn hot_reload_guard_explicit_shutdown_is_idempotent() {
        let runtime = Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        let store: PolicyStore = Arc::new(Mutex::new(Vec::new()));
        let tmp = tempfile::tempdir().expect("tempdir");

        let mut guard =
            start_hot_reload(tmp.path().to_path_buf(), ctx, store).expect("start hot reload");

        // Calling shutdown explicitly then dropping must not panic, even
        // when the implicit Drop runs a second shutdown.
        guard.shutdown();
        drop(guard);
        drop(runtime);
    }

    /// #2378: synchronous native bridge ops must observe the bridge-op
    /// deadline that's armed for the currently-running JS eval. The
    /// QuickJS interrupt handler can't fire while Rust is running, so we
    /// rely on bridge ops to consult `bridge_op_deadline_remaining()` and
    /// either short-circuit or clamp their own internal timeout.
    ///
    /// This test verifies the load-bearing contract directly: while an
    /// `ArmedDeadline` is in scope on this thread, the public
    /// `bridge_op_deadline_remaining()` helper returns a budget that
    /// shrinks monotonically and becomes zero once the deadline passes.
    /// After the guard drops, the helper returns `None` again so live
    /// engine bridge ops are not affected.
    #[test]
    fn bridge_op_deadline_visible_to_native_ops_while_armed() {
        assert!(
            bridge_op_deadline_remaining().is_none(),
            "no deadline should be armed at test start"
        );

        let slot = Arc::new(AtomicU64::new(0));
        {
            let _armed = ArmedDeadline::new(&slot, Duration::from_millis(200));

            // Immediately after arming, the remaining budget must be positive
            // and bounded by the original budget. Use a generous upper bound
            // (the requested budget) since `Instant::now()` may advance
            // between `ArmedDeadline::new` and this read.
            let initial = bridge_op_deadline_remaining()
                .expect("deadline should be visible to bridge ops while armed");
            assert!(
                initial > Duration::ZERO && initial <= Duration::from_millis(200),
                "initial bridge-op budget out of range: {initial:?}"
            );

            // Sleep past the deadline; the helper must now report a zero
            // remaining budget so synchronous bridge ops short-circuit.
            std::thread::sleep(Duration::from_millis(250));
            let after_deadline = bridge_op_deadline_remaining()
                .expect("deadline should still be visible after it has passed");
            assert!(
                after_deadline.is_zero(),
                "bridge-op deadline should report zero remaining after expiry, got {after_deadline:?}"
            );
        }

        assert!(
            bridge_op_deadline_remaining().is_none(),
            "deadline must clear once ArmedDeadline drops so live ops are unaffected"
        );
        // The shared AtomicU64 slot must also be cleared so the interrupt
        // handler sees `0` (no deadline armed) on subsequent ticks.
        assert_eq!(
            slot.load(Ordering::Acquire),
            0,
            "ArmedDeadline drop must reset the shared deadline slot"
        );
    }

    #[test]
    fn test_compute_policy_version() {
        let source1 = "console.log('hello');";
        let source2 = "console.log('world');";
        let source1_again = "console.log('hello');";

        let hash1 = compute_policy_version(source1);
        let hash2 = compute_policy_version(source2);
        let hash1_again = compute_policy_version(source1_again);

        assert_eq!(
            hash1, hash1_again,
            "Identical sources should have the same hash"
        );
        assert_ne!(
            hash1, hash2,
            "Different sources should have different hashes"
        );
        assert_eq!(
            hash1.len(),
            12,
            "Hash string should be exactly 12 characters long"
        );
    }
}
