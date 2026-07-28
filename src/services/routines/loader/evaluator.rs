use super::discovery::script_ref;
use super::{LoadedRoutineScript, RoutineTickContext};
use crate::engine::loader::compute_policy_version;
use anyhow::{Result, anyhow};
use rquickjs::function::Opt;
use rquickjs::{Array, Context, Function, Object, Runtime};
use serde_json::{Map, Number, Value};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Display;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

const ROUTINE_QUICKJS_TIMEOUT: Duration = Duration::from_secs(5);
const ROUTINE_QUICKJS_MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ROUTINE_JSON_DEPTH: usize = 128;
const MAX_ROUTINE_JSON_ARRAY_LENGTH: usize = 16_384;
const MAX_ROUTINE_JSON_NODES: usize = 32_768;
const MAX_ROUTINE_JSON_CONVERTED_BYTES: usize = ROUTINE_QUICKJS_MEMORY_LIMIT_BYTES;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(super) fn load_single_routine_script(root: &Path, path: &Path) -> Result<LoadedRoutineScript> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("read routine script {}: {e}", path.display()))?;
    load_single_routine_script_from_source(root, path, source)
}

pub(super) fn load_single_routine_script_from_source(
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

    let validation = validate_routine_script_source(&source, &fallback_name, &script_ref, path)?;

    Ok(LoadedRoutineScript {
        name: validation.name,
        script_ref,
        file: path.to_path_buf(),
        script_version,
        metadata: validation.metadata,
        source,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ValidatedRoutineSource {
    pub(super) name: String,
    pub(super) metadata: Value,
}

/// Validate one stable source snapshot with the same QuickJS contract used at runtime.
pub(super) fn validate_routine_script_source(
    source: &str,
    fallback_name: &str,
    script_ref: &str,
    path: &Path,
) -> Result<ValidatedRoutineSource> {
    with_bounded_quickjs_context(|ctx| -> Result<ValidatedRoutineSource> {
        let registration =
            capture_registered_routine(ctx.clone(), source, fallback_name, script_ref, path)?;
        Ok(ValidatedRoutineSource {
            name: registration.name,
            metadata: registration.metadata,
        })
    })
}

pub(super) fn evaluate_tick_action(
    script: &LoadedRoutineScript,
    tick_context: &RoutineTickContext,
) -> Result<Value> {
    let fallback_name = script
        .file
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    with_bounded_quickjs_context(|ctx| -> Result<Value> {
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
        if action_value.is_promise() {
            return Err(anyhow!(
                "routine script {} tick(ctx) returned a Promise; async tick is not supported",
                script.script_ref
            ));
        }
        js_value_to_json(
            action_value,
            "routine action",
            &registration.json_container_policy,
        )
    })
}

fn create_bounded_quickjs_context() -> Result<(Runtime, Context)> {
    let runtime =
        Runtime::new().map_err(|e| anyhow!("routine QuickJS runtime creation failed: {e}"))?;
    // QuickJS requires the allocator limit to be installed before the context and
    // its intrinsic objects allocate from the runtime heap.
    runtime.set_memory_limit(ROUTINE_QUICKJS_MEMORY_LIMIT_BYTES);
    install_interrupt_handler(&runtime, ROUTINE_QUICKJS_TIMEOUT);
    let context = Context::full(&runtime)
        .map_err(|e| anyhow!("routine QuickJS context creation failed: {e}"))?;
    Ok((runtime, context))
}

fn with_bounded_quickjs_context<T>(
    operation: impl for<'js> FnOnce(rquickjs::Ctx<'js>) -> Result<T>,
) -> Result<T> {
    let (runtime, context) = create_bounded_quickjs_context()?;
    let result = context.with(operation);
    // Drop every context-owned reference before collecting cycles. In particular,
    // revoked Proxy internals can otherwise survive until JS_FreeRuntime asserts.
    drop(context);
    runtime.run_gc();
    result
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

fn conversion_js_error(
    ctx: &rquickjs::Ctx<'_>,
    operation: impl Display,
    error: &rquickjs::Error,
) -> anyhow::Error {
    let detail = quickjs_exception_detail(ctx, error);
    anyhow!("{operation}: {detail}")
}

struct CapturedRoutineRegistration<'js> {
    name: String,
    tick: Function<'js>,
    metadata: Value,
    json_container_policy: JsonContainerPolicy<'js>,
}

struct RegistrationCapture<'js> {
    invocation_count: usize,
    registration: Option<Result<CapturedRoutineRegistration<'js>>>,
    tick_classifier: Option<Function<'js>>,
    json_container_policy: Option<JsonContainerPolicy<'js>>,
}

#[derive(Clone, Copy)]
struct JsonContainerClassIds {
    object: rquickjs::qjs::JSClassID,
    array: rquickjs::qjs::JSClassID,
}

#[derive(Clone)]
struct JsonContainerPolicy<'js> {
    class_ids: JsonContainerClassIds,
    plain_object_classifier: Function<'js>,
}

fn capture_json_container_class_ids(ctx: rquickjs::Ctx<'_>) -> Result<JsonContainerClassIds> {
    let object = Object::new(ctx.clone())
        .map_err(|e| anyhow!("failed to create routine JSON object sample: {e}"))?;
    let array =
        Array::new(ctx).map_err(|e| anyhow!("failed to create routine JSON array sample: {e}"))?;
    // SAFETY: Both samples are live object values owned by this QuickJS context.
    // QuickJS class IDs are runtime-stable Copy identifiers, so retaining only
    // the IDs cannot outlive or alias the sample JS values.
    Ok(unsafe {
        JsonContainerClassIds {
            object: rquickjs::qjs::JS_GetClassID(object.as_raw()),
            array: rquickjs::qjs::JS_GetClassID(array.as_raw()),
        }
    })
}

fn create_plain_object_classifier<'js>(ctx: rquickjs::Ctx<'js>) -> Result<Function<'js>> {
    ctx.eval(
        r#"
        (() => {
            "use strict";
            const getPrototypeOf = Object.getPrototypeOf;
            const objectPrototype = Object.prototype;

            return function isPlainDataObject(value) {
                const prototype = getPrototypeOf(value);
                return prototype === null || prototype === objectPrototype;
            };
        })()
        "#,
    )
    .map_err(|e| anyhow!("failed to create routine plain-object classifier: {e}"))
}

fn create_tick_function_classifier<'js>(ctx: rquickjs::Ctx<'js>) -> Result<Function<'js>> {
    ctx.eval(
        r#"
        (() => {
            "use strict";
            const apply = Reflect.apply;
            const evaluate = eval;
            const functionToString = Function.prototype.toString;
            const getPrototypeOf = Object.getPrototypeOf;
            const ownKeys = Reflect.ownKeys;
            const asyncFunctionPrototype = getPrototypeOf(async function () {});
            const asyncGeneratorPrototype = getPrototypeOf(async function* () {});

            return function classifyTick(value) {
                const source = apply(functionToString, value, []);
                let rebuilt;
                try {
                    rebuilt = evaluate("(" + source + ")");
                } catch (_) {
                    try {
                        const holder = evaluate("({" + source + "})");
                        const keys = ownKeys(holder);
                        if (keys.length !== 1) return -1;
                        rebuilt = holder[keys[0]];
                    } catch (_) {
                        return -1;
                    }
                }

                if (typeof rebuilt !== "function") return -1;
                const prototype = getPrototypeOf(rebuilt);
                return prototype === asyncFunctionPrototype || prototype === asyncGeneratorPrototype
                    ? 1
                    : 0;
            };
        })()
        "#,
    )
    .map_err(|e| anyhow!("failed to create routine tick classifier: {e}"))
}

fn snapshot_registered_routine<'js>(
    ctx: rquickjs::Ctx<'js>,
    registered: Option<rquickjs::Value<'js>>,
    fallback_name: &str,
    script_ref: &str,
    tick_classifier: &Function<'js>,
    json_container_policy: &JsonContainerPolicy<'js>,
) -> Result<CapturedRoutineRegistration<'js>> {
    let routine_obj = registered
        .and_then(|value| value.into_object())
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
    let tick_kind: i32 = tick_classifier.call((tick.clone(),)).map_err(|e| {
        let detail = quickjs_exception_detail(&ctx, &e);
        anyhow!("routine script {script_ref} tick classification failed: {detail}")
    })?;
    match tick_kind {
        0 => {}
        1 => {
            return Err(anyhow!(
                "routine script {script_ref} tick must be synchronous"
            ));
        }
        _ => {
            return Err(anyhow!(
                "routine script {script_ref} tick must be a directly inspectable synchronous function"
            ));
        }
    }

    let metadata = routine_obj
        .get::<_, rquickjs::Value>("metadata")
        .ok()
        .filter(|value| !value.is_null() && !value.is_undefined())
        .map(|value| js_value_to_json(value, "routine metadata", json_container_policy))
        .transpose()?
        .unwrap_or(Value::Null);

    Ok(CapturedRoutineRegistration {
        name,
        tick,
        metadata,
        json_container_policy: json_container_policy.clone(),
    })
}

fn capture_registered_routine<'js>(
    ctx: rquickjs::Ctx<'js>,
    source: &str,
    fallback_name: &str,
    script_ref: &str,
    path: &Path,
) -> Result<CapturedRoutineRegistration<'js>> {
    let globals = ctx.globals();
    let tick_classifier = create_tick_function_classifier(ctx.clone())?;
    let json_container_policy = JsonContainerPolicy {
        class_ids: capture_json_container_class_ids(ctx.clone())?,
        plain_object_classifier: create_plain_object_classifier(ctx.clone())?,
    };
    let capture = Rc::new(RefCell::new(RegistrationCapture {
        invocation_count: 0,
        registration: None,
        tick_classifier: Some(tick_classifier),
        json_container_policy: Some(json_container_policy),
    }));
    let callback_capture = Rc::clone(&capture);
    let callback_fallback_name = fallback_name.to_string();
    let callback_script_ref = script_ref.to_string();
    let register = Function::new(
        ctx.clone(),
        move |callback_ctx: rquickjs::Ctx<'js>, Opt(registered): Opt<rquickjs::Value<'js>>| {
            let invocation = {
                let mut capture = callback_capture.borrow_mut();
                capture.invocation_count = capture.invocation_count.saturating_add(1);
                capture.invocation_count
            };
            if invocation == 1 {
                // Drop the RefCell borrow before invoking user-controlled getters in the
                // registration snapshot. A getter may re-enter register(), and the nested
                // callback must increment the count instead of panicking on borrow_mut().
                let tick_classifier = { callback_capture.borrow().tick_classifier.clone() };
                let json_container_policy =
                    { callback_capture.borrow().json_container_policy.clone() };
                let registration = tick_classifier
                    .zip(json_container_policy)
                    .ok_or_else(|| anyhow!("routine value classifiers were unavailable"))
                    .and_then(|(tick_classifier, json_container_policy)| {
                        snapshot_registered_routine(
                            callback_ctx,
                            registered,
                            &callback_fallback_name,
                            &callback_script_ref,
                            &tick_classifier,
                            &json_container_policy,
                        )
                    });
                callback_capture.borrow_mut().registration = Some(registration);
            }
        },
    )
    .map_err(|e| anyhow!("failed to create routine register capture: {e}"))?;
    let routines = Object::new(ctx.clone())
        .map_err(|e| anyhow!("failed to create agentdesk.routines: {e}"))?;
    routines
        .prop("register", register)
        .map_err(|e| anyhow!("failed to protect agentdesk.routines.register: {e}"))?;
    let agentdesk =
        Object::new(ctx.clone()).map_err(|e| anyhow!("failed to create agentdesk: {e}"))?;
    agentdesk
        .prop("routines", routines)
        .map_err(|e| anyhow!("failed to protect agentdesk.routines: {e}"))?;
    globals
        .prop("agentdesk", agentdesk)
        .map_err(|e| anyhow!("failed to protect global agentdesk: {e}"))?;

    let mut eval_opts = rquickjs::context::EvalOptions::default();
    eval_opts.strict = false;
    let eval_result: rquickjs::Result<rquickjs::Value> =
        ctx.eval_with_options(source.as_bytes().to_vec(), eval_opts);
    let eval_error = eval_result
        .err()
        .map(|error| quickjs_exception_detail(&ctx, &error));
    let (invocation_count, registration) = {
        let mut capture = capture.borrow_mut();
        capture.tick_classifier = None;
        capture.json_container_policy = None;
        (capture.invocation_count, capture.registration.take())
    };
    if let Some(exception_detail) = eval_error {
        return Err(anyhow!(
            "JS eval error in routine script {}: {exception_detail}",
            path.display()
        ));
    }

    if invocation_count == 0 {
        return Err(anyhow!(
            "routine script {} did not call agentdesk.routines.register()",
            path.display()
        ));
    }
    if invocation_count != 1 {
        return Err(anyhow!(
            "routine script {} must call agentdesk.routines.register() exactly once (got {})",
            path.display(),
            invocation_count
        ));
    }

    registration.ok_or_else(|| {
        anyhow!(
            "routine script {} registration capture was unavailable",
            path.display()
        )
    })?
}

fn js_value_to_json<'js>(
    value: rquickjs::Value<'js>,
    label: &'static str,
    json_container_policy: &JsonContainerPolicy<'js>,
) -> Result<Value> {
    js_value_to_json_with_byte_budget(
        value,
        label,
        json_container_policy,
        MAX_ROUTINE_JSON_CONVERTED_BYTES,
    )
}

struct JsonConversionByteBudget {
    maximum: usize,
    remaining: usize,
}

impl JsonConversionByteBudget {
    fn new(maximum: usize) -> Self {
        Self {
            maximum,
            remaining: maximum,
        }
    }

    fn charge(&mut self, label: &'static str, bytes: usize) -> Result<()> {
        let Some(remaining) = self.remaining.checked_sub(bytes) else {
            return Err(anyhow!(
                "{label} exceeds maximum converted JSON size {} bytes",
                self.maximum
            ));
        };
        self.remaining = remaining;
        Ok(())
    }
}

fn copy_json_string_with_budget<'js>(
    value: rquickjs::String<'js>,
    label: &'static str,
    kind: &'static str,
    byte_budget: &mut JsonConversionByteBudget,
) -> Result<String> {
    let ctx = value.ctx().clone();
    let value = value.to_cstring().map_err(|e| {
        conversion_js_error(&ctx, format_args!("{label} {kind} conversion failed"), &e)
    })?;
    // SAFETY: `value` owns a non-null QuickJS buffer of exactly `value.len()`
    // bytes for this scope. Treat it as bytes first because QuickJS preserves
    // unmatched UTF-16 surrogates using byte sequences that are not Rust UTF-8.
    let bytes = unsafe { std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len()) };
    let value = std::str::from_utf8(bytes)
        .map_err(|error| anyhow!("{label} {kind} conversion failed: invalid UTF-8: {error}"))?;
    // Debit the aggregate budget before creating the retained Rust String.
    byte_budget.charge(label, value.len())?;
    Ok(value.to_owned())
}

fn js_value_to_json_with_byte_budget<'js>(
    value: rquickjs::Value<'js>,
    label: &'static str,
    json_container_policy: &JsonContainerPolicy<'js>,
    maximum_converted_bytes: usize,
) -> Result<Value> {
    let mut active = HashSet::new();
    let mut remaining_nodes = MAX_ROUTINE_JSON_NODES;
    let mut byte_budget = JsonConversionByteBudget::new(maximum_converted_bytes);
    js_value_to_json_inner(
        value,
        label,
        json_container_policy,
        0,
        &mut active,
        &mut remaining_nodes,
        &mut byte_budget,
    )
}

fn js_value_to_json_inner<'js>(
    value: rquickjs::Value<'js>,
    label: &'static str,
    json_container_policy: &JsonContainerPolicy<'js>,
    depth: usize,
    active: &mut HashSet<rquickjs::Value<'js>>,
    remaining_nodes: &mut usize,
    byte_budget: &mut JsonConversionByteBudget,
) -> Result<Value> {
    if *remaining_nodes == 0 {
        return Err(anyhow!(
            "{label} exceeds maximum value count {MAX_ROUTINE_JSON_NODES}"
        ));
    }
    *remaining_nodes -= 1;
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
            return Err(anyhow!("{label} contains non-finite number"));
        };
        return Ok(Value::Number(number));
    }
    if let Some(value) = value.as_string() {
        return Ok(Value::String(copy_json_string_with_budget(
            value.clone(),
            label,
            "string",
            byte_budget,
        )?));
    }
    if value.is_promise() {
        return Err(anyhow!("{label} contains unsupported Promise"));
    }
    if value.is_function() {
        return Err(anyhow!("{label} contains unsupported function"));
    }
    if depth >= MAX_ROUTINE_JSON_DEPTH {
        return Err(anyhow!(
            "{label} exceeds maximum nesting depth {MAX_ROUTINE_JSON_DEPTH}"
        ));
    }
    if !value.is_object() {
        return Err(anyhow!("{label} contains unsupported JavaScript value"));
    }
    // SAFETY: JS_GetClassID requires an object-tagged live JSValue. The check
    // above establishes that invariant, and `value` remains owned for this call.
    let class_id = unsafe { rquickjs::qjs::JS_GetClassID(value.as_raw()) };
    let is_array = class_id == json_container_policy.class_ids.array;
    if !is_array && class_id != json_container_policy.class_ids.object {
        return Err(anyhow!(
            "{label} contains unsupported non-plain JavaScript object"
        ));
    }
    let identity = value.clone();
    if !active.insert(identity.clone()) {
        return Err(anyhow!("{label} contains cyclic object graph"));
    }
    if is_array {
        let array = value
            .into_array()
            .ok_or_else(|| anyhow!("{label} array conversion failed"));
        let result = array.and_then(|array| {
            let length = array.len();
            if length > MAX_ROUTINE_JSON_ARRAY_LENGTH {
                return Err(anyhow!(
                    "{label} array length {length} exceeds maximum {MAX_ROUTINE_JSON_ARRAY_LENGTH}"
                ));
            }
            if length > *remaining_nodes {
                return Err(anyhow!(
                    "{label} exceeds maximum value count {MAX_ROUTINE_JSON_NODES}"
                ));
            }
            let mut out = Vec::with_capacity(length);
            for index in 0..length {
                let item: rquickjs::Value = array.get(index).map_err(|e| {
                    conversion_js_error(
                        array.ctx(),
                        format_args!("{label} array[{index}] conversion failed"),
                        &e,
                    )
                })?;
                out.push(js_value_to_json_inner(
                    item,
                    label,
                    json_container_policy,
                    depth + 1,
                    active,
                    remaining_nodes,
                    byte_budget,
                )?);
            }
            Ok(Value::Array(out))
        });
        active.remove(&identity);
        return result;
    }
    if class_id == json_container_policy.class_ids.object {
        let is_plain_data_object: bool = json_container_policy
            .plain_object_classifier
            .call((value.clone(),))
            .map_err(|e| {
                conversion_js_error(
                    json_container_policy.plain_object_classifier.ctx(),
                    format_args!("{label} object classification failed"),
                    &e,
                )
            })?;
        if !is_plain_data_object {
            active.remove(&identity);
            return Err(anyhow!(
                "{label} contains unsupported non-plain JavaScript object"
            ));
        }
        let object = value
            .into_object()
            .ok_or_else(|| anyhow!("{label} object conversion failed"));
        let result = object.and_then(|object| {
            let mut out = Map::new();
            for key in object.keys::<rquickjs::String>() {
                let key = key.map_err(|e| {
                    conversion_js_error(
                        object.ctx(),
                        format_args!("{label} object key conversion failed"),
                        &e,
                    )
                })?;
                if *remaining_nodes == 0 {
                    return Err(anyhow!(
                        "{label} exceeds maximum value count {MAX_ROUTINE_JSON_NODES}"
                    ));
                }
                let key = copy_json_string_with_budget(key, label, "object key", byte_budget)?;
                let item: rquickjs::Value = object.get(key.as_str()).map_err(|e| {
                    conversion_js_error(
                        object.ctx(),
                        format_args!("{label} field {key} conversion failed"),
                        &e,
                    )
                })?;
                out.insert(
                    key,
                    js_value_to_json_inner(
                        item,
                        label,
                        json_container_policy,
                        depth + 1,
                        active,
                        remaining_nodes,
                        byte_budget,
                    )?,
                );
            }
            Ok(Value::Object(out))
        });
        active.remove(&identity);
        return result;
    }
    active.remove(&identity);

    Err(anyhow!("{label} contains unsupported JavaScript value"))
}
