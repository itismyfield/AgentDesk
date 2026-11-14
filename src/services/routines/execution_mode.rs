//! Routine execution modes: request validation, the legacy (null-mode) projection and the
//! definition snapshot pinned on every claimed run.
use serde::Serialize;
use serde_json::{Value, json};

pub const DEFAULT_HEADLESS_FAILURE_NOTICE_LIMIT: i32 = 3;
const HEADLESS_FAILURE_NOTICE_LIMIT_MAX: i32 = 100;
const RUN_SPEC_VERSION: i64 = 1;

/// DB constraints that back the API checks when a concurrent writer races past them.
const DEFINITION_CONSTRAINTS: [&str; 4] = [
    "routines_execution_mode_check",
    "routines_runner_shape_check",
    "routines_explicit_mode_fresh_check",
    "routines_headless_failure_notice_limit_check",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineExecutionMode {
    Headless,
    Thread,
    Channel,
    Script,
}

impl RoutineExecutionMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "headless" => Some(Self::Headless),
            "thread" => Some(Self::Thread),
            "channel" => Some(Self::Channel),
            "script" => Some(Self::Script),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headless => "headless",
            Self::Thread => "thread",
            Self::Channel => "channel",
            Self::Script => "script",
        }
    }

    /// Only modes with a runtime path may be stored; thread waits until its backend comes from
    /// provider capability rather than the legacy `headless_fresh_agents` flag.
    fn is_available(self) -> bool {
        false
    }
}

/// An explicit definition to store: the mode and its canonical runner JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionDefinition {
    pub mode: RoutineExecutionMode,
    pub runner: Value,
}

/// Stored definition columns, as read from a `routines` row.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct StoredExecutionDefinition {
    pub id: String,
    pub execution_mode: Option<String>,
    pub runner: Option<Value>,
    pub execution_strategy: String,
    pub script_ref: String,
    pub headless_failure_notice_limit: i32,
}

/// API projection of a routine's execution definition.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExecutionProjection {
    pub execution_mode: Option<&'static str>,
    pub effective_execution_mode: Option<&'static str>,
    pub execution_compatibility: &'static str,
    pub runner: Value,
    pub headless_failure_notice_limit: i32,
}

impl StoredExecutionDefinition {
    fn explicit_mode(&self) -> Option<RoutineExecutionMode> {
        self.execution_mode
            .as_deref()
            .and_then(RoutineExecutionMode::parse)
    }

    /// A null-mode fresh row runs as a thread; a null-mode persistent row keeps its legacy
    /// behaviour as a read-only compatibility state rather than being promoted.
    pub fn projection(&self) -> ExecutionProjection {
        let explicit = self.explicit_mode();
        let (effective, compatibility) = match explicit {
            Some(mode) => (Some(mode.as_str()), "explicit"),
            None if self.execution_strategy == "fresh" => (Some("thread"), "legacy_fresh"),
            None => (None, "legacy_persistent"),
        };
        ExecutionProjection {
            execution_mode: explicit.map(RoutineExecutionMode::as_str),
            effective_execution_mode: effective,
            execution_compatibility: compatibility,
            runner: self
                .runner
                .clone()
                .unwrap_or_else(|| quickjs_runner(&self.script_ref)),
            headless_failure_notice_limit: self.headless_failure_notice_limit,
        }
    }
}

fn quickjs_runner(script_ref: &str) -> Value {
    json!({"kind": "quickjs", "script_ref": script_ref})
}

fn definition(
    mode: &str,
    runner: Option<&Value>,
    script_ref: &str,
) -> Result<ExecutionDefinition, String> {
    let parsed = RoutineExecutionMode::parse(mode).ok_or_else(|| {
        format!("unsupported execution_mode '{mode}'; expected headless, thread, channel or script")
    })?;
    if !parsed.is_available() {
        return Err(format!(
            "execution_mode '{mode}' is not available on this server yet"
        ));
    }
    let runner = match runner {
        None => quickjs_runner(script_ref),
        Some(runner) => validate_runner(runner, script_ref)?,
    };
    Ok(ExecutionDefinition {
        mode: parsed,
        runner,
    })
}

fn validate_runner(runner: &Value, script_ref: &str) -> Result<Value, String> {
    match runner.get("kind").and_then(Value::as_str) {
        Some("quickjs") => match runner.get("script_ref") {
            None | Some(Value::Null) => Ok(quickjs_runner(script_ref)),
            Some(Value::String(value)) if value.trim().replace('\\', "/") == script_ref => {
                Ok(quickjs_runner(script_ref))
            }
            Some(_) => Err(format!(
                "runner.script_ref must match the routine script_ref '{script_ref}'"
            )),
        },
        Some("command") => Err("runner kind 'command' is not available on this server yet".into()),
        _ => Err("runner.kind must be quickjs or command".into()),
    }
}

fn reject_strategy_conflict(strategy: Option<&str>) -> Result<(), String> {
    match strategy {
        None | Some("fresh") => Ok(()),
        Some(other) => Err(format!(
            "execution_strategy '{other}' conflicts with an explicit execution_mode"
        )),
    }
}

/// POST: an omitted mode keeps the legacy (null-mode) contract.
pub fn definition_for_create(
    mode: Option<&str>,
    runner: Option<&Value>,
    strategy: Option<&str>,
    script_ref: &str,
) -> Result<Option<ExecutionDefinition>, String> {
    let Some(mode) = mode else {
        if runner.is_some() {
            return Err("runner requires execution_mode".into());
        }
        return Ok(None);
    };
    reject_strategy_conflict(strategy)?;
    definition(mode, runner, script_ref).map(Some)
}

/// PATCH: omitted fields keep the stored value; an explicit null never clears a mode or runner.
pub fn definition_for_patch(
    mode: Option<Option<&str>>,
    runner: Option<Option<&Value>>,
    strategy: Option<&str>,
    current: &StoredExecutionDefinition,
) -> Result<Option<ExecutionDefinition>, String> {
    if matches!(mode, Some(None)) {
        return Err("execution_mode cannot be cleared".into());
    }
    if matches!(runner, Some(None)) {
        return Err("runner cannot be cleared".into());
    }
    let mode = mode.flatten();
    let runner = runner.flatten();
    let current_mode = current.explicit_mode();
    if mode.is_some() || current_mode.is_some() {
        reject_strategy_conflict(strategy)?;
    }
    match (mode, current_mode) {
        (Some(mode), _) => definition(mode, runner, &current.script_ref).map(Some),
        (None, Some(current_mode)) if runner.is_some() => {
            definition(current_mode.as_str(), runner, &current.script_ref).map(Some)
        }
        (None, None) if runner.is_some() => Err("runner requires execution_mode".into()),
        _ => Ok(None),
    }
}

pub fn notice_limit_for_create(limit: Option<i32>) -> Result<i32, String> {
    match limit {
        None => Ok(DEFAULT_HEADLESS_FAILURE_NOTICE_LIMIT),
        Some(value) => checked_notice_limit(value),
    }
}

pub fn notice_limit_for_patch(limit: Option<Option<i32>>) -> Result<Option<i32>, String> {
    match limit {
        None => Ok(None),
        Some(None) => Err("headless_failure_notice_limit cannot be null".into()),
        Some(Some(value)) => checked_notice_limit(value).map(Some),
    }
}

fn checked_notice_limit(value: i32) -> Result<i32, String> {
    if (1..=HEADLESS_FAILURE_NOTICE_LIMIT_MAX).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "headless_failure_notice_limit must be between 1 and {HEADLESS_FAILURE_NOTICE_LIMIT_MAX}"
        ))
    }
}

pub fn is_definition_constraint_violation(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    DEFINITION_CONSTRAINTS
        .iter()
        .any(|name| message.contains(name))
}

/// Routine fields a run reads after claim; pinned so a later PATCH applies from the next claim.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ClaimDefinitionRow {
    #[sqlx(flatten)]
    pub definition: StoredExecutionDefinition,
    pub agent_id: Option<String>,
    pub fallback_agent_id: Option<String>,
    pub max_retries: i32,
    pub discord_thread_id: Option<String>,
    pub timeout_secs: Option<i32>,
}

impl ClaimDefinitionRow {
    /// `execution_backend` stays null until the first provider start pins it. An explicit-mode
    /// run never starts a second turn once its first was submitted; a null-mode run still may.
    pub fn run_spec(&self) -> Value {
        let projection = self.definition.projection();
        json!({
            "version": RUN_SPEC_VERSION,
            "execution_mode": projection.execution_mode,
            "effective_execution_mode": projection.effective_execution_mode,
            "execution_compatibility": projection.execution_compatibility,
            "execution_strategy": self.definition.execution_strategy,
            "runner": projection.runner,
            "agent_id": self.agent_id,
            "fallback_agent_id": self.fallback_agent_id,
            "max_retries": self.max_retries,
            "discord_thread_id": self.discord_thread_id,
            "timeout_secs": self.timeout_secs,
            "headless_failure_notice_limit": projection.headless_failure_notice_limit,
            "execution_backend": Value::Null,
            "retry_after_start": projection.execution_mode.is_none(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(mode: Option<&str>, strategy: &str) -> StoredExecutionDefinition {
        StoredExecutionDefinition {
            id: "routine-1".into(),
            execution_mode: mode.map(str::to_string),
            runner: mode.map(|_| quickjs_runner("daily.js")),
            execution_strategy: strategy.into(),
            script_ref: "daily.js".into(),
            headless_failure_notice_limit: DEFAULT_HEADLESS_FAILURE_NOTICE_LIMIT,
        }
    }

    #[test]
    fn null_mode_rows_project_without_promotion() {
        let cases = [
            (None, "fresh", None, Some("thread"), "legacy_fresh"),
            (None, "persistent", None, None, "legacy_persistent"),
            (
                Some("thread"),
                "fresh",
                Some("thread"),
                Some("thread"),
                "explicit",
            ),
        ];
        for (mode, strategy, explicit, effective, compatibility) in cases {
            let projection = stored(mode, strategy).projection();
            assert_eq!(projection.execution_mode, explicit, "{mode:?}/{strategy}");
            assert_eq!(projection.effective_execution_mode, effective);
            assert_eq!(projection.execution_compatibility, compatibility);
            assert_eq!(projection.runner, quickjs_runner("daily.js"));
            assert_eq!(projection.headless_failure_notice_limit, 3);
        }
    }

    fn reason<T: std::fmt::Debug>(result: Result<T, String>) -> String {
        result.expect_err("request must be refused")
    }

    #[test]
    fn patch_keeps_omitted_and_refuses_each_definition_change_for_its_own_reason() {
        let legacy = stored(None, "persistent");
        let explicit = stored(Some("thread"), "fresh");
        let runner = quickjs_runner("daily.js");

        assert_eq!(definition_for_patch(None, None, None, &legacy), Ok(None));
        assert_eq!(
            definition_for_patch(None, None, Some("persistent"), &legacy),
            Ok(None)
        );
        let cases = [
            (
                definition_for_patch(Some(Some("thread")), None, None, &legacy),
                "not available",
            ),
            (
                definition_for_patch(Some(Some("fresh")), None, None, &legacy),
                "unsupported",
            ),
            (
                definition_for_patch(None, Some(Some(&runner)), None, &legacy),
                "requires execution_mode",
            ),
            (
                definition_for_patch(None, None, Some("persistent"), &explicit),
                "conflicts",
            ),
            (
                definition_for_patch(Some(Some("thread")), None, Some("persistent"), &legacy),
                "conflicts",
            ),
        ];
        for (result, expected) in cases {
            let message = reason(result);
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }

    #[test]
    fn create_refuses_every_explicit_mode_until_its_runtime_exists() {
        assert_eq!(
            definition_for_create(None, None, None, "daily.js"),
            Ok(None)
        );
        for mode in ["headless", "thread", "channel", "script"] {
            let message = reason(definition_for_create(
                Some(mode),
                None,
                Some("fresh"),
                "daily.js",
            ));
            assert!(message.contains("not available"), "{mode}: {message}");
        }
        let cases = [
            (
                definition_for_create(None, Some(&quickjs_runner("daily.js")), None, "daily.js"),
                "requires execution_mode",
            ),
            (
                definition_for_create(Some("thread"), None, Some("persistent"), "daily.js"),
                "conflicts",
            ),
        ];
        for (result, expected) in cases {
            let message = reason(result);
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }

    #[test]
    fn runner_must_be_a_quickjs_alias_of_the_routine_script() {
        assert_eq!(
            validate_runner(
                &json!({"kind": "quickjs", "script_ref": " daily.js "}),
                "daily.js"
            ),
            Ok(quickjs_runner("daily.js"))
        );
        for runner in [
            json!({"kind": "quickjs", "script_ref": "other.js"}),
            json!({"kind": "command", "program": "/bin/true"}),
            json!({}),
        ] {
            assert!(validate_runner(&runner, "daily.js").is_err(), "{runner}");
        }
    }

    #[test]
    fn notice_limit_defaults_to_three_and_rejects_null_or_out_of_range() {
        assert_eq!(notice_limit_for_create(None), Ok(3));
        assert_eq!(notice_limit_for_create(Some(1)), Ok(1));
        assert_eq!(notice_limit_for_create(Some(100)), Ok(100));
        assert_eq!(notice_limit_for_patch(None), Ok(None));
        assert_eq!(notice_limit_for_patch(Some(Some(7))), Ok(Some(7)));
        for rejected in [
            notice_limit_for_create(Some(0)),
            notice_limit_for_create(Some(101)),
            notice_limit_for_patch(Some(None)).map(|_| 0),
            notice_limit_for_patch(Some(Some(0))).map(|_| 0),
        ] {
            assert!(rejected.is_err(), "{rejected:?}");
        }
    }
}
