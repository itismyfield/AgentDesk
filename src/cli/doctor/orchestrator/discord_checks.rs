use super::{
    BTreeSet, Check, CheckGroup, HealthSnapshot, SecurityExposure, Value, dcserver_log_hint,
    health, health_detail_endpoint, health_endpoint, highest_reason_fix_safety,
    highest_reason_severity,
};

pub(super) fn discord_bot_check_from_health(base: &str, body: &Value) -> Check {
    if let Some(providers) = body.get("providers").and_then(Value::as_array) {
        let total = providers.len();
        let connected: Vec<String> = providers
            .iter()
            .filter(|provider| provider.get("connected").and_then(Value::as_bool) == Some(true))
            .filter_map(|provider| provider.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let disconnected: Vec<String> = providers
            .iter()
            .filter(|provider| provider.get("connected").and_then(Value::as_bool) != Some(true))
            .filter_map(|provider| provider.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let overall = body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if total > 0 && connected.len() == total && overall == "healthy" {
            return Check::ok(
                "discord_bot",
                CheckGroup::Core,
                "Discord Bot",
                format!(
                    "{}/{} connected — {}",
                    connected.len(),
                    total,
                    connected.join(", ")
                ),
            )
            .with_path(health_endpoint(base))
            .with_expected_actual(
                "all registered providers connected",
                format!("{}/{} connected", connected.len(), total),
            );
        }
        let observation = health::degraded_reasons(body);
        let reasons = observation.as_deref().unwrap_or(&[]);
        let provider_reasons: Vec<_> = reasons
            .iter()
            .filter(|reason| reason.subsystem == "provider_runtime")
            .cloned()
            .collect();
        if total > 0 && connected.len() == total && !provider_reasons.is_empty() {
            let detail = provider_reasons
                .iter()
                .map(|reason| reason.summary.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Check::warn(
                "discord_bot",
                CheckGroup::Core,
                "Discord Bot",
                format!("overall={overall}, connected={total}/{total}; {detail}"),
                "All providers are connected; check degraded reasons instead of token/offline guidance.",
            )
            .with_subsystem("provider_runtime")
            .with_severity(highest_reason_severity(&provider_reasons))
            .with_fix_safety(highest_reason_fix_safety(&provider_reasons))
            .with_security_exposure(SecurityExposure::OperationalMetadata)
            .with_evidence(health::reasons_evidence(&provider_reasons))
            .with_path(health_detail_endpoint(base))
            .with_expected_actual("all providers connected and no provider degraded reasons", detail)
            .with_next_steps(
                provider_reasons
                    .iter()
                    .map(|reason| reason.next_step.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            );
        }
        if total == 0 {
            return Check::warn(
                "discord_bot",
                CheckGroup::Core,
                "Discord Bot",
                format!("no providers registered in unified health payload — {base}"),
                "dcserver may not have registered providers yet. Check startup logs and bot token configuration.",
            )
            .with_path(health_endpoint(base))
            .with_expected_actual("provider registry populated", "providers=0")
            .with_next_steps(vec![
                format!("tail -n 200 {}", dcserver_log_hint()),
                "agentdesk doctor --fix".to_string(),
            ]);
        }
        return Check::warn(
            "discord_bot",
            CheckGroup::Core,
            "Discord Bot",
            format!(
                "overall={overall}, connected={}/{}, offline={}{}",
                connected.len(),
                total,
                if disconnected.is_empty() {
                    "-".to_string()
                } else {
                    disconnected.join(", ")
                },
                observation.as_ref().err().map(|issue| format!("; reasons={}", issue.describe())).unwrap_or_default()
            ),
            "Check Discord tokens, gateway connection status, and dcserver stdout logs for offline providers.",
        )
        .with_subsystem("provider_runtime")
        .with_security_exposure(SecurityExposure::OperationalMetadata)
        .with_path(health_endpoint(base))
        .with_expected_actual(
            "all registered providers connected",
            format!(
                "overall={overall}, connected={}/{}, offline={}",
                connected.len(),
                total,
                if disconnected.is_empty() {
                    "-".to_string()
                } else {
                    disconnected.join(", ")
                }
            ),
        )
        .with_next_steps(vec![
            format!("tail -n 200 {}", dcserver_log_hint()),
            "agentdesk doctor --fix".to_string(),
        ]);
    }

    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let db = body.get("db").and_then(Value::as_bool).unwrap_or(false);
    if ok && db {
        Check::warn(
            "discord_bot",
            CheckGroup::Core,
            "Discord Bot",
            format!("standalone health only — provider status unavailable at {base}"),
            "The server responds, but the Discord provider health registry is empty. Check if running in standalone mode.",
        )
        .with_path(health_endpoint(base))
        .with_expected_actual("unified provider registry available", "standalone health payload only")
        .with_next_steps(vec![
            format!("curl -s {}", health_endpoint(base)),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ])
    } else {
        Check::fail(
            "discord_bot",
            CheckGroup::Core,
            "Discord Bot",
            format!("server unhealthy or provider data missing: ok={ok} db={db}"),
            "Discord provider initialization may have failed even if the server is running. Check dcserver stdout logs.",
        )
        .with_path(health_endpoint(base))
        .with_expected_actual("healthy server with provider registry", format!("ok={ok} db={db}"))
        .with_next_steps(vec![
            "agentdesk doctor --fix".to_string(),
            format!("tail -n 200 {}", dcserver_log_hint()),
        ])
    }
}

pub(super) fn check_discord_bot(snapshot: &HealthSnapshot) -> Check {
    match snapshot.body.as_ref() {
        Some(body) => discord_bot_check_from_health(&snapshot.base, body),
        None => Check::fail(
            "discord_bot",
            CheckGroup::Core,
            "Discord Bot",
            format!(
                "unreachable ({})",
                snapshot
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".to_string())
            ),
            "Ensure dcserver is running and /api/health is reachable.",
        )
        .with_path(health_endpoint(&snapshot.base))
        .with_expected_actual("reachable health endpoint", "health endpoint unreachable")
        .with_next_steps(vec![
            "agentdesk doctor --fix".to_string(),
            format!("curl -s {}", health_endpoint(&snapshot.base)),
        ]),
    }
}
