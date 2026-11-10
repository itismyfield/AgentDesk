use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// A Legacy body on a TUI binding fails stage 1 even when its runtime or source is unknown.
pub(super) fn is_violation(event: &Value) -> bool {
    let tag = &event["census"];
    tag["binding_mode"] == "TUI"
        && tag["body_owner"] == "Legacy"
        && matches!(
            tag["output_class"].as_str(),
            Some("LegacyBody" | "RecoveredLegacyBody")
        )
        && matches!(event["op"].as_str(), Some("POST" | "EDIT"))
        && matches!(
            event["phase"].as_str(),
            Some("begin" | "result" | "not_sent")
        )
}

fn error(errors: &mut BTreeSet<String>, reason: &str) {
    errors.insert(reason.to_owned());
}
fn number(value: &Value) -> Option<u64> {
    value.as_u64()
}
fn static_evidence(value: &Value, census: &str) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 5
            && value["schema_version"].as_u64() == Some(1)
            && value["complete"] == true
            && object.get("manifest_error").is_some_and(Value::is_null)
            && value["pending"].as_u64() == Some(0)
            && value["census_hash"] == census
    })
}
fn list(value: &Value) -> Option<BTreeSet<u64>> {
    let rows = value.as_array()?;
    let values = rows
        .iter()
        .map(Value::as_u64)
        .collect::<Option<BTreeSet<_>>>()?;
    (values.len() == rows.len()).then_some(values)
}
fn within(event: &Value, start: i64, end: i64) -> bool {
    start > end
        || event["at_ms"]
            .as_i64()
            .is_none_or(|at| at >= start && at <= end)
}

pub(super) fn host(data: &Value, end: i64, build: &str, census: &str) -> Value {
    inspect_host(
        data,
        end,
        data["coverage_start"]["started_at_ms"]
            .as_i64()
            .unwrap_or(i64::MAX),
        build,
        census,
    )
}

fn inspect_host(data: &Value, end: i64, start: i64, build: &str, census: &str) -> Value {
    let mut errors: BTreeSet<String> = data["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let mut failures = BTreeSet::new();
    let (mut events, mut buckets) = (Vec::new(), BTreeMap::new());
    let (mut headers, mut doctor_ids) = (BTreeMap::new(), BTreeSet::new());
    let anchor = &data["coverage_start"];
    let epoch = anchor["started_at_ms"].as_i64().unwrap_or(i64::MAX);
    if data["errors"]
        .as_array()
        .is_none_or(|rows| rows.iter().any(|r| !r.is_string()))
    {
        error(&mut errors, "READ_STATUS_UNKNOWN");
    }
    if data["schema_version"] != 1
        || data["policy_version"] != 1
        || data["process_role"] != "server"
        || data["host"].as_str().is_none_or(str::is_empty)
        || start > end
        || epoch > start
        || data["cut_ms"].as_i64().is_none_or(|cut| cut < end)
        || data["evidence_truncated"] != false
    {
        error(&mut errors, "OBSERVATION_INCOMPLETE");
    }
    if data["build_sha"] != build
        || data["census_hash"] != census
        || build.len() != 40
        || census.len() != 64
    {
        error(&mut errors, "BUILD_OR_CENSUS_UNKNOWN");
    }
    if !static_evidence(&data["static_instrumentation"], census) {
        error(&mut errors, "STATIC_INSTRUMENTATION_UNKNOWN");
    }
    if data["build_sha"] == build
        && data["census_hash"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64 && hash != census)
    {
        failures.insert("STATIC_CENSUS_DRIFT");
    }
    let mut restarts = 0;
    let mut bindings_seen = BTreeSet::<u64>::new();
    if !data["boot_files"].is_array() {
        error(&mut errors, "BOOT_INVENTORY_MISSING");
    }
    let files = data["boot_files"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let live = &data["live_journal"];
    let live_valid = data["current_boot_id"]
        .as_str()
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .is_some_and(|id| {
            live["boot_id"] == id
                && live["invalid"] == false
                && live["enqueued_cut"].as_u64().is_some_and(|cut| {
                    files
                        .iter()
                        .find(|file| file["header"]["boot_id"] == id)
                        .and_then(|file| file["window"]["drained"].as_u64())
                        .is_some_and(|drained| drained >= cut)
                })
        });
    if !live_valid {
        error(&mut errors, "LIVE_CUT_UNVERIFIED");
    }
    let parent = files
        .iter()
        .find(|file| file["header"]["boot_id"] == anchor["boot_id"])
        .map(|file| &file["header"]["predecessor_boot_id"]);
    let boundary = files.iter().map(|file| &file["header"]).find(|header| {
        parent == Some(&header["boot_id"])
            && header["started_at_ms"]
                .as_i64()
                .is_some_and(|at| at < epoch)
    });
    let mut end_available = !files.is_empty();
    let mut drain_available = !files.is_empty();
    for file in files {
        let h = &file["header"];
        let Some(id) = h["boot_id"].as_str() else {
            error(&mut errors, "HEADER_INVALID");
            continue;
        };
        let at = h["started_at_ms"].as_i64();
        let Some(at) = at else {
            error(&mut errors, "HEADER_TIME_UNKNOWN");
            continue;
        };
        let outside = at < epoch;
        if at <= end && !outside {
            if headers.insert(id.to_owned(), h.clone()).is_some() {
                error(&mut errors, "DUPLICATE_BOOT");
            }
            if h["boot_id"] != anchor["boot_id"] && at >= start {
                restarts += 1;
            }
        }
        if file["end"].is_object()
            && (file["end"]["outputs_drained"] != true
                || file["end"]["ended_at_ms"]
                    .as_i64()
                    .is_none_or(|end| end < at))
        {
            error(&mut errors, "OUTPUT_DRAIN_UNKNOWN");
        }
        let header_known = h["schema_version"] == 1
            && h["observation_policy_version"] == 1
            && h["process_role"] == "server"
            && h["host"].as_str().is_some_and(|host| !host.is_empty());
        let w = &file["window"];
        let drained = w["cut"].as_u64().is_some()
            && w["cut"] == w["drained"]
            && w["invalid"] == false
            && w["boot_id"] == id
            && (w["closed"] == true || h["boot_id"] == data["current_boot_id"])
            && w["status"] == "COMPLETE";
        let clock_valid = file["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|event| {
                event["at_ms"].as_i64().is_none_or(|time| {
                    time >= at
                        && data["cut_ms"]
                            .as_i64()
                            .is_some_and(|cut| time.saturating_sub(cut) <= 5000)
                        && (file["end"]["outputs_drained"] != true
                            || file["end"]["ended_at_ms"]
                                .as_i64()
                                .is_none_or(|end| time <= end))
                })
            });
        if !clock_valid {
            error(&mut errors, "EVENT_CLOCK_INVALID");
        }
        let end_verified = header_known
            && drained
            && clock_valid
            && file["targets"].is_array()
            && file["buckets"].is_array()
            && file["end"]["outputs_drained"] == true
            && file["end"]["ended_at_ms"]
                .as_i64()
                .is_some_and(|end| end >= at);
        drain_available &= header_known && drained;
        if h["boot_id"] != data["current_boot_id"] {
            end_available &= end_verified;
        }
        let ended_before = end_verified
            && file["end"]["ended_at_ms"]
                .as_i64()
                .is_some_and(|at| at < start);
        if (at > end && header_known) || ended_before {
            continue;
        }
        if h["schema_version"] != 1
            || h["observation_policy_version"] != 1
            || h["process_role"] != "server"
            || h["host"] != data["host"]
            || h["build_sha"] != build
            || h["census_hash"] != census
            || !static_evidence(&h["static_instrumentation"], census)
            || h["pending_probes"] != 0
        {
            error(&mut errors, "BOOT_EVIDENCE_UNKNOWN");
        }
        if !file["targets"].is_array() || !file["buckets"].is_array() {
            error(&mut errors, "FILE_EVIDENCE_UNKNOWN");
        }
        if !drained {
            error(&mut errors, "DRAIN_OR_EXIT_UNKNOWN");
        }
        if let Some(doctor) = h["doctor_boot_id"].as_str() {
            if !doctor_ids.insert(doctor.to_owned()) {
                error(&mut errors, "DOCTOR_BOOT_ALIAS");
            }
        } else {
            error(&mut errors, "DOCTOR_BOOT_UNKNOWN");
        }
        let selection = &file["selection"];
        let bindings = list(&selection["tui_bindings"]);
        if selection["installed"].as_bool().is_none() {
            error(&mut errors, "SELECTION_UNKNOWN");
        }
        if selection["installed"] == false
            && bindings
                .as_ref()
                .is_some_and(|bindings| !bindings.is_empty())
        {
            failures.insert("TUI_SELECTION_INCOMPLETE");
        }
        match (
            bindings,
            list(&selection["effective"]),
            list(&selection["dormant"]),
        ) {
            (Some(bindings), _, _) if bindings.is_empty() => {}
            (Some(bindings), Some(selected), Some(dormant)) => {
                bindings_seen.extend(bindings.iter().copied());
                if !bindings.is_empty()
                    && (!bindings.is_subset(&selected) || !bindings.is_disjoint(&dormant))
                {
                    failures.insert("TUI_SELECTION_INCOMPLETE");
                }
            }
            _ => error(&mut errors, "SELECTION_UNKNOWN"),
        }
        for event in file["targets"].as_array().into_iter().flatten() {
            if !within(event, start, end) {
                continue;
            }
            if event["at_ms"].as_i64().is_none() {
                error(&mut errors, "EVENT_TIME_UNKNOWN");
            }
            let tag = &event["census"];
            let class = tag["output_class"].as_str();
            if !matches!(
                event["phase"].as_str(),
                Some("begin" | "result" | "not_sent")
            ) || !matches!(event["op"].as_str(), Some("POST" | "EDIT"))
            {
                error(&mut errors, "TARGET_SCHEMA_UNKNOWN");
            }
            if class != Some("NonBody")
                && (class == Some("UnknownBody")
                    || !matches!(tag["binding_mode"].as_str(), Some("TUI" | "NON_TUI"))
                    || tag["actual_runtime"]
                        .as_str()
                        .is_none_or(|r| r.is_empty() || r == "unknown")
                    || tag["source_incarnation"].is_null()
                    || tag["body_owner"] != "Legacy")
            {
                error(&mut errors, "BODY_IDENTITY_UNKNOWN");
            }
            if !matches!(
                class,
                Some(
                    "LegacyBody"
                        | "RecoveredLegacyBody"
                        | "LegacyBodyRefresh"
                        | "NonBody"
                        | "UnknownBody"
                )
            ) {
                error(&mut errors, "BODY_CLASS_UNKNOWN");
            }
            if is_violation(event) {
                failures.insert("LEGACY_TUI_BODY");
            }
            events.push(event);
        }
        for bucket in file["buckets"].as_array().into_iter().flatten() {
            let (Some(begin), Some(finish)) = (
                bucket["bucket_start_ms"].as_i64(),
                bucket["bucket_end_ms"].as_i64(),
            ) else {
                error(&mut errors, "DENOMINATOR_INVALID");
                continue;
            };
            if bucket["complete"] != true || begin < start || finish > end {
                continue;
            }
            if begin.checked_add(60_000) != Some(finish) || begin.rem_euclid(60_000) != 0 {
                error(&mut errors, "DENOMINATOR_INVALID");
                continue;
            }
            buckets.insert((id, begin), bucket);
        }
    }
    if !super::window::valid_chain(&headers, anchor, end, boundary, &mut 0) {
        error(&mut errors, "BOOT_CHAIN_INCOMPLETE");
    }
    let starts = data["doctor_starts"].as_array();
    if data["inventory_source"]["source"] != "doctor/startup"
        || data["inventory_source"]["version"] != 1
        || data["inventory_source"]["available"] != true
        || data["inventory_source"]["from_ms"]
            .as_i64()
            .is_none_or(|from| from > epoch)
        || data["inventory_source"]["to_ms"]
            .as_i64()
            .is_none_or(|to| to < end)
        || starts.is_none_or(Vec::is_empty)
    {
        error(&mut errors, "START_INVENTORY_UNAVAILABLE");
    }
    let mut inventory_ids = BTreeSet::new();
    for doc in starts.into_iter().flatten() {
        let (Some(id), Some(at)) = (
            doc["doctor_boot_id"].as_str(),
            doc["started_at_ms"].as_i64(),
        ) else {
            error(&mut errors, "START_INVENTORY_INVALID");
            continue;
        };
        if !inventory_ids.insert(id.to_owned()) {
            error(&mut errors, "START_INVENTORY_DUPLICATE");
        }
        if at >= start
            && at <= end
            && !files
                .iter()
                .any(|file| file["header"]["doctor_boot_id"] == id)
        {
            error(&mut errors, "MISSING_BOOT");
        }
        if at > data["cut_ms"].as_i64().unwrap_or(i64::MIN) {
            error(&mut errors, "CLOCK_SKEW");
        }
    }
    if !doctor_ids.is_subset(&inventory_ids) {
        error(&mut errors, "START_INVENTORY_GAP");
    }
    if restarts < 1 {
        error(&mut errors, "RESTART_EVIDENCE_MISSING");
    }
    let mut turns = Some(0u64);
    let mut ack = Some(0u64);
    let mut opportunities: BTreeMap<String, Option<u64>> = BTreeMap::new();
    if buckets.is_empty() {
        turns = None;
        ack = None;
    }
    for bucket in buckets.values() {
        turns = turns.and_then(|v| number(&bucket["tui_turns"]).and_then(|n| v.checked_add(n)));
        ack = ack.and_then(|v| number(&bucket["o_ack"]).and_then(|n| v.checked_add(n)));
        if let Some(sites) = bucket["opportunities"].as_object() {
            for (site, count) in sites {
                let total = opportunities.entry(site.clone()).or_insert(Some(0));
                *total = total.and_then(|v| count.as_u64().and_then(|n| v.checked_add(n)));
            }
        }
    }
    if turns.is_none() || ack.is_none() {
        error(&mut errors, "DENOMINATOR_UNAVAILABLE");
    }
    if is_violation(&data["known_violation"]) && within(&data["known_violation"], start, end) {
        failures.insert("LEGACY_TUI_BODY");
    }
    let no_bindings = bindings_seen.is_empty();
    if no_bindings
        && (!end_available
            || turns.is_some_and(|n| n > 0)
            || ack.is_some_and(|n| n > 0)
            || events.iter().any(|event| {
                event["census"]["binding_mode"] == "TUI"
                    && event["census"]["output_class"] != "NonBody"
            }))
    {
        error(&mut errors, "NO_TUI_BINDING_UNPROVEN");
    }
    let status = if !failures.is_empty() {
        "FAIL"
    } else if !errors.is_empty() {
        "INCONCLUSIVE"
    } else if no_bindings {
        "NO_TUI_BINDING"
    } else if turns.is_some_and(|n| n < 10) || ack == Some(0) {
        "NO_TRAFFIC"
    } else {
        "PASS"
    };
    let mut sites = BTreeMap::new();
    let names: BTreeSet<_> = events
        .iter()
        .filter_map(|event| event["census"]["site"].as_str())
        .chain(opportunities.keys().map(String::as_str))
        .chain(["W11", "W13", "W33", "W35"])
        .collect();
    for site in names {
        // Historical refreshes do not fail stage 1, but still prevent deleting their site.
        let body_events = events
            .iter()
            .filter(|event| {
                event["census"]["site"] == site
                    && (is_violation(event)
                        || (event["census"]["output_class"] == "LegacyBodyRefresh"
                            && event["census"]["binding_mode"] == "TUI"))
            })
            .count();
        let pending = site == "W01" || site == "W01g" || site.starts_with("W20");
        let opportunity = opportunities.get(site).copied().flatten();
        sites.insert(site, json!({"status":if status == "PASS" && !pending && opportunity.is_some_and(|n| n > 0) && body_events == 0 { "DELETE_READY" } else { "DELETE_NOT_READY" },
            "opportunities":opportunity,"body_events":body_events,"provenance_pending":pending,
            "reason":if pending { "PROVENANCE_PENDING" } else if opportunity == Some(0) && body_events == 0 { "NO_TRAFFIC" } else { "EVIDENCE_OR_TRAFFIC_REQUIRED" }}));
    }

    let mut traffic = BTreeMap::<String, u64>::new();
    for event in &events {
        let key = format!(
            "{}:{}:{}",
            event["census"]["output_class"]
                .as_str()
                .unwrap_or("unknown"),
            event["phase"].as_str().unwrap_or("unknown"),
            event["outcome"].as_str().unwrap_or("none")
        );
        *traffic.entry(key).or_default() += 1;
    }
    json!({"traffic":traffic,"termination":{"source":"drain_closed_or_current_live","available":drain_available,"process_end_available":end_available},"status":status,"complete":errors.is_empty(),"reasons":errors,"violations":failures,"restarts":restarts,
        "tui_turns":turns,"o_ack":ack,"denominator_available":turns.is_some() && ack.is_some(),"sites":sites})
}

pub(crate) fn evaluate(
    expected: [&str; 2],
    observations: &[Value],
    now: i64,
    build: &str,
    census: &str,
) -> Value {
    let mut errors = BTreeSet::new();
    let mut reports = BTreeMap::new();
    let mut known_failure = false;
    let end = observations
        .iter()
        .filter_map(|o| o["cut_ms"].as_i64())
        .min()
        .unwrap_or(i64::MIN);
    let start = observations
        .iter()
        .filter_map(|o| o["coverage_start"]["started_at_ms"].as_i64())
        .min()
        .unwrap_or(i64::MAX);
    if expected[0] == expected[1] || observations.len() != 2 {
        error(&mut errors, "HOST_INVENTORY_INCOMPLETE");
    }
    for observation in observations {
        let report = host(observation, end, build, census);
        known_failure |= report["status"] == "FAIL";
        let Some(name) = observation["host"].as_str() else {
            error(&mut errors, "HOST_IDENTITY_UNKNOWN");
            continue;
        };
        if !expected.contains(&name) || reports.contains_key(name) {
            error(&mut errors, "HOST_IDENTITY_DUPLICATE_OR_FOREIGN");
        }
        if observation["cut_ms"]
            .as_i64()
            .is_none_or(|cut| now < cut || now.saturating_sub(cut) > 120_000)
        {
            error(&mut errors, "STALE_OR_SKEWED_SNAPSHOT");
        }
        let clock_valid = observation["served_at_ms"].as_i64().is_some_and(|served| {
            served.abs_diff(now) <= 5000
                && observation["monotonic_age_ms"].as_u64().is_some_and(|age| {
                    age <= 120_000
                        && observation["cut_ms"].as_i64().is_some_and(|cut| {
                            u64::try_from(served.saturating_sub(cut))
                                .ok()
                                .is_some_and(|wall_age| wall_age.abs_diff(age) <= 5000)
                        })
                })
        });
        if !clock_valid {
            error(&mut errors, "RESPONSE_CLOCK_UNKNOWN");
        }
        reports.insert(name, report);
    }
    if !expected.iter().all(|name| reports.contains_key(name)) {
        error(&mut errors, "HOST_MISSING");
    }
    let status = if known_failure || reports.values().any(|r| r["status"] == "FAIL") {
        "FAIL"
    } else if !errors.is_empty() || reports.values().any(|r| r["status"] == "INCONCLUSIVE") {
        "INCONCLUSIVE"
    } else if reports.values().any(|r| r["status"] == "NO_TRAFFIC") {
        "NO_TRAFFIC"
    } else if reports.values().all(|r| r["status"] == "NO_TUI_BINDING") {
        "NO_TUI_BINDING"
    } else {
        "PASS"
    };
    let mut sites = BTreeMap::new();
    let names: BTreeSet<_> = reports
        .values()
        .flat_map(|r| {
            r["sites"]
                .as_object()
                .into_iter()
                .flat_map(|s| s.keys().cloned())
        })
        .collect();
    for name in names {
        let mut count = Some(0u64);
        let mut body_events = 0u64;
        let mut pending = false;
        for r in reports.values() {
            let s = &r["sites"][&name];
            count = count.and_then(|n| {
                s["opportunities"]
                    .as_u64()
                    .and_then(|value| n.checked_add(value))
            });
            body_events = body_events.saturating_add(s["body_events"].as_u64().unwrap_or(0));
            pending |= s["provenance_pending"] == true;
        }
        sites.insert(name, json!({"status":if status == "PASS" && reports.values().all(|r| r["status"] == "PASS") && count.is_some_and(|n| n>0) && body_events == 0 && !pending {"DELETE_READY"} else {"DELETE_NOT_READY"},"opportunities":count,"body_events":body_events,"provenance_pending":pending}));
    }
    json!({"sites":sites,"stage1":status,"complete":errors.is_empty() && reports.values().all(|r| r["complete"] == true),"coverage_start_ms":start,"cut_ms":end,"hosts":reports,"reasons":errors})
}
