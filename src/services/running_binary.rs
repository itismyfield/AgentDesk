use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::process_boot;

pub(crate) fn attach(health: &mut Value, detailed: bool) {
    if !detailed {
        return;
    }
    let Some(source) = option_env!("ADK_RUNNING_SOURCE_SHA").filter(|value| !value.is_empty())
    else {
        return;
    };
    let Some(target) = option_env!("ADK_RUNNING_SOURCE_TARGET").filter(|value| !value.is_empty())
    else {
        return;
    };
    health["running_binary"] = witness(
        source,
        target,
        process_boot::initialize().started_at_ms,
        Utc::now(),
    );
}

fn witness(
    source: &str,
    target: &str,
    boot_started_at_ms: i64,
    observed_at: DateTime<Utc>,
) -> Value {
    json!({
        "source_sha": source,
        "target": target,
        "boot_started_at_ms": boot_started_at_ms,
        "observed_at": observed_at,
    })
}
