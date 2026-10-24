//! The dedicated Herdr config upkeep in the herdr health view. It reads only the upkeep's last
//! in-memory records, never a config file or a server socket.
use serde_json::{Value, json};

/// A local dedicated endpoint's upkeep record; its E7 reading replaces `never` once one ran.
pub(super) fn endpoint(key: &str, entry: &mut Value) {
    #[cfg(unix)]
    {
        use crate::services::session_host::managed_config::maintenance::records;
        let Some(record) = records().remove(key) else {
            return;
        };
        if record.last_e7 != "never" {
            entry["last_e7"] = record.last_e7.clone().into();
        }
        entry["managed_config"] = json!({
            "config": record.config,
            "backup": record.backup,
            "reason": record.cause,
            "observation_stale": record.stale_at(chrono::Utc::now()),
            "notice": record.notice,
            "observed_at": record.observed_at,
            "earliest_start_after": record.earliest_start_after,
        });
    }
    #[cfg(not(unix))]
    let _ = (key, entry);
}

/// The view's fixed upkeep reasons, only once some endpoint has an upkeep record; a stale
/// observation is listed beside the cause it keeps, never in its place.
pub(super) fn summarize(view: &mut Value) {
    let Some(endpoints) = view["endpoints"].as_object() else {
        return;
    };
    let upkept: Vec<&Value> = endpoints
        .values()
        .filter_map(|entry| entry.get("managed_config"))
        .collect();
    if upkept.is_empty() {
        return;
    }
    let mut reasons: Vec<&str> = upkept
        .iter()
        .flat_map(|upkeep| {
            let stale = upkeep["observation_stale"].as_bool() == Some(true);
            [
                upkeep["reason"].as_str(),
                stale.then_some("herdr_config_observation_stale"),
            ]
        })
        .flatten()
        .collect();
    reasons.sort_unstable();
    reasons.dedup();
    view["managed_config_reasons"] = json!(reasons);
}

#[cfg(all(test, unix))]
#[path = "managed_herdr_tests.rs"]
mod tests;
