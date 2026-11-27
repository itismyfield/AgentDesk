use super::{
    BTreeSet, Check, CheckGroup, CheckStatus, FixSafety, HealthSnapshot, SecurityExposure,
    Severity, Value, dcserver_log_hint, health, health_detail_endpoint, health_endpoint,
    highest_reason_fix_safety, highest_reason_severity, json, observation_checks,
};

pub(super) fn check_runtime_path() -> Check {
    let current = std::env::var("PATH").unwrap_or_default();
    match crate::services::platform::merged_runtime_path() {
        Some(merged) if merged == current => Check::ok(
            "runtime_path",
            CheckGroup::ProviderRuntime,
            "Runtime PATH",
            "current PATH already matches provider runtime PATH",
        )
        .with_expected_actual("runtime PATH resolved", "runtime PATH matches current PATH"),
        Some(merged) => {
            let entry_count = std::env::split_paths(&merged).count();
            Check::ok(
                "runtime_path",
                CheckGroup::ProviderRuntime,
                "Runtime PATH",
                format!(
                    "provider subprocesses will use merged login-shell PATH ({entry_count} entries)"
                ),
            )
            .with_expected_actual(
                "runtime PATH resolved",
                format!("merged runtime PATH with {entry_count} entries"),
            )
        }
        None => Check::fail(
            "runtime_path",
            CheckGroup::ProviderRuntime,
            "Runtime PATH",
            "unable to resolve provider runtime PATH",
            "Could not read the login shell PATH. Compare the service environment PATH with your shell PATH.",
        )
        .with_expected_actual("runtime PATH resolved", "runtime PATH resolution failed")
        .with_next_steps(vec!["echo $PATH".to_string()]),
    }
}

pub(super) fn check_server_running(snapshot: &HealthSnapshot) -> Check {
    match snapshot.body.as_ref() {
        Some(body) => {
            let ver = body
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let status = body
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let db = body.get("db").and_then(Value::as_bool).unwrap_or(false);
                    Some(if ok && db { "healthy" } else { "degraded" }.to_string())
                })
                .unwrap_or_else(|| "unknown".to_string());
            let detail = format!("{status} v{ver} on {}", snapshot.base);
            if status == "healthy" {
                Check::ok("server", CheckGroup::Core, "Server", detail)
                    .with_path(health_endpoint(&snapshot.base))
                    .with_expected_actual(
                        "reachable healthy health endpoint",
                        format!("status={status}"),
                    )
            } else {
                let observation = health::degraded_reasons(body);
                let reasons = observation.as_deref().unwrap_or(&[]);
                let reason_detail = if let Err(issue) = observation.as_ref() {
                    format!("{detail}; reasons={}", issue.describe())
                } else if reasons.is_empty() {
                    detail.clone()
                } else {
                    format!(
                        "{}; reasons={}",
                        detail,
                        reasons
                            .iter()
                            .map(|reason| reason.raw.clone())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                Check::fail(
                    "server",
                    CheckGroup::Core,
                    "Server",
                    reason_detail.clone(),
                    "The health endpoint responded, but the service status is not healthy. Check the subsystem for each degraded reason first.",
                )
                .with_subsystem("health")
                .with_severity(highest_reason_severity(&reasons))
                .with_fix_safety(highest_reason_fix_safety(&reasons))
                .with_security_exposure(SecurityExposure::OperationalMetadata)
                .with_evidence(match &observation {
                    Ok(reasons) => health::reasons_evidence(reasons),
                    Err(issue) => json!({"measurement_issue": issue}),
                })
                .with_path(health_endpoint(&snapshot.base))
                .with_expected_actual(
                    "reachable healthy health endpoint",
                    reason_detail,
                )
                .with_next_steps(vec![
                    format!("curl -s {}", health_detail_endpoint(&snapshot.base)),
                    format!("tail -n 200 {}", dcserver_log_hint()),
                ])
            }
        }
        None => {
            let error = snapshot
                .error
                .clone()
                .unwrap_or_else(|| "unknown error".to_string());
            let (status, severity, guidance) = if error.contains("(401)") || error.contains("(403)")
            {
                (
                    "unauthorized",
                    Severity::Error,
                    "check the auth token or /api/health/detail permissions.",
                )
            } else if error.contains("--allow-remote") {
                (
                    "blocked_remote_token",
                    Severity::Critical,
                    "non-loopback URL에 token을 보내려면 명시적으로 --allow-remote를 사용하세요.",
                )
            } else {
                (
                    "unreachable",
                    Severity::Error,
                    "Check if the dcserver/axum server is running and if the firewall/port is accessible.",
                )
            };
            Check::fail(
                "server",
                CheckGroup::Core,
                "Server",
                format!("{status} — {} ({error})", snapshot.base),
                guidance,
            )
            .with_subsystem("server")
            .with_severity(severity)
            .with_fix_safety(FixSafety::ExplicitRestartRequired)
            .with_security_exposure(SecurityExposure::OperationalMetadata)
            .with_path(health_detail_endpoint(&snapshot.base))
            .with_expected_actual("reachable health endpoint", error)
            .with_next_steps(vec![
                "agentdesk doctor --fix --allow-restart".to_string(),
                format!("curl -s {}", health_endpoint(&snapshot.base)),
            ])
        }
    }
}

pub(super) fn check_runtime_root() -> Check {
    match crate::config::runtime_root() {
        Some(path) if path.exists() && path.is_dir() => Check::ok(
            "runtime_root",
            CheckGroup::Core,
            "Runtime Root",
            format!("{}", path.display()),
        )
        .with_path(path.display().to_string())
        .with_expected_actual("runtime root exists", "runtime root exists"),
        Some(path) => Check::fail(
            "runtime_root",
            CheckGroup::Core,
            "Runtime Root",
            format!("{} — missing", path.display()),
            "you can create the default runtime directory with agentdesk doctor --fix.",
        )
        .with_path(path.display().to_string())
        .with_expected_actual("runtime root exists", "runtime root missing")
        .with_next_steps(vec!["agentdesk doctor --fix".to_string()]),
        None => Check::fail(
            "runtime_root",
            CheckGroup::Core,
            "Runtime Root",
            "unable to determine runtime root",
            "check AGENTDESK_ROOT_DIR or the default ~/.adk/release path.",
        )
        .with_expected_actual(
            "runtime root path resolvable",
            "runtime root path unresolved",
        ),
    }
}

pub(super) fn check_degraded_reasons(snapshot: &HealthSnapshot) -> Check {
    let Some(body) = snapshot.body.as_ref() else {
        return Check::fail(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            snapshot
                .error
                .clone()
                .unwrap_or_else(|| "health endpoint unavailable".to_string()),
            "could not classify degraded reasons because the health endpoint is unavailable.",
        )
        .with_subsystem("health")
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_fix_safety(FixSafety::NotFixable);
    };
    let reasons = match health::degraded_reasons(body) {
        Ok(reasons) => reasons,
        Err(issue) => {
            return observation_checks::unmeasured_check("health_degraded_reasons", &issue);
        }
    };
    if reasons.is_empty() {
        return Check::ok(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            "no degraded reasons",
        )
        .with_subsystem("health")
        .with_path(health_detail_endpoint(&snapshot.base))
        .with_expected_actual("degraded_reasons classified", "none");
    }

    let detail = reasons
        .iter()
        .map(|reason| reason.summary.clone())
        .collect::<Vec<_>>()
        .join("; ");
    let next_steps = reasons
        .iter()
        .map(|reason| reason.next_step.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let status = if reasons
        .iter()
        .any(|reason| matches!(reason.severity, Severity::Error | Severity::Critical))
    {
        CheckStatus::Fail
    } else {
        CheckStatus::Warn
    };
    let mut check = match status {
        CheckStatus::Pass => unreachable!(),
        CheckStatus::Warn => Check::warn(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            detail.clone(),
            "check health degraded reasons by subsystem.",
        ),
        CheckStatus::Fail => Check::fail(
            "health_degraded_reasons",
            CheckGroup::Core,
            "Health Reasons",
            detail.clone(),
            "resolve error/critical degraded reasons first.",
        ),
    };
    check = check
        .with_subsystem(
            reasons
                .first()
                .map(|reason| reason.subsystem)
                .unwrap_or("health"),
        )
        .with_severity(highest_reason_severity(&reasons))
        .with_evidence(health::reasons_evidence(&reasons))
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_fix_safety(highest_reason_fix_safety(&reasons))
        .with_expected_actual("no degraded reasons", detail)
        .with_next_steps(next_steps);
    check
}
