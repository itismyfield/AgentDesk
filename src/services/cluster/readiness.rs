//! Fresh local execution evidence. Heartbeats publish this snapshot without
//! extending its lifetime; an online registry row is not execution readiness.
use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Config;
use crate::services::{platform, provider::ProviderKind, provider_auth, provider_auth_profile};

const PROBE_TTL_MS: i64 = 120_000;
const POLLER_TTL_MS: i64 = 30_000;
static SNAPSHOT: LazyLock<RwLock<Option<ExecutionProbe>>> = LazyLock::new(|| RwLock::new(None));
static POLLERS: LazyLock<RwLock<BTreeMap<String, i64>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));
static BOOT_ID: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().to_string());

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProviderEvidence {
    pub cli_usable: bool,
    pub version: Option<String>,
    pub failure: Option<String>,
    pub credential_profiles: BTreeMap<String, bool>,
    // Credential metadata is evidence of configuration, not successful remote
    // authentication or available quota. Neither is fabricated from presence.
    pub authentication_verified: bool,
    pub quota_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ExecutionProbe {
    pub schema: u32,
    pub boot_id: String,
    pub observed_at_ms: i64,
    pub expires_at_ms: i64,
    pub os: String,
    pub arch: String,
    pub runtime_profile: crate::config::RuntimeProfile,
    pub release: Value,
    pub providers: BTreeMap<String, ProviderEvidence>,
    pub tools: BTreeMap<String, bool>,
    pub repositories: BTreeMap<String, bool>,
    pub backends: Vec<String>,
    pub disk_free_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ReadinessReport {
    pub eligible: bool,
    pub reasons: Vec<String>,
}

pub(crate) fn evidence(node: &Value, now: i64) -> Result<ExecutionProbe, &'static str> {
    let probe: ExecutionProbe = node
        .pointer("/capabilities/execution_readiness")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or("execution_evidence_missing")?;
    if probe.schema != 1 || probe.boot_id.is_empty() {
        return Err("execution_evidence_protocol");
    }
    if probe.observed_at_ms > now.saturating_add(5_000)
        || probe.expires_at_ms <= now
        || probe.expires_at_ms.saturating_sub(probe.observed_at_ms) > PROBE_TTL_MS
    {
        return Err("execution_evidence_stale");
    }
    Ok(probe)
}

pub(crate) fn local_node() -> Value {
    let mut capabilities = serde_json::Map::new();
    publish(&mut capabilities);
    json!({"status":"online", "instance_id":super::node_registry::resolve_self_instance_id_without_config(),
        "capabilities":capabilities})
}

pub(crate) fn evaluate(
    node: &Value,
    provider: &str,
    auth_profile: &str,
    now: i64,
) -> ReadinessReport {
    let mut reasons = Vec::new();
    if node.get("status").and_then(Value::as_str) != Some("online") {
        reasons.push("node_offline".into());
    }
    match evidence(node, now) {
        Err(reason) => reasons.push(reason.into()),
        Ok(probe) => match probe.providers.get(provider) {
            None => reasons.push("provider_not_probed".into()),
            Some(p) => {
                if !p.cli_usable {
                    reasons.push("provider_cli_unavailable".into());
                }
                if p.credential_profiles.get(auth_profile) != Some(&true) {
                    reasons.push("provider_credentials_missing".into());
                }
            }
        },
    }
    let poll = node
        .pointer("/capabilities/intake_poller")
        .and_then(|v| v.get(provider))
        .and_then(Value::as_i64);
    if !poll.is_some_and(|at| {
        at <= now.saturating_add(5_000) && now.saturating_sub(at) <= POLLER_TTL_MS
    }) {
        reasons.push("intake_poller_stale".into());
    }
    ReadinessReport {
        eligible: reasons.is_empty(),
        reasons,
    }
}

pub(crate) fn expected_auth_profile(provider: &str, channel: &str, agent: &str) -> String {
    let bindings = crate::services::discord::org_schema::list_profile_bindings();
    bindings
        .iter()
        .find(|b| b.provider == provider && b.channel_id.as_deref() == Some(channel))
        .or_else(|| {
            bindings
                .iter()
                .find(|b| b.provider == provider && b.agent_id == agent && b.channel_id.is_none())
        })
        .map(|b| b.profile_id.clone())
        .or_else(|| {
            crate::services::discord::org_schema::provider_auth_primary_profiles()
                .get(provider)
                .cloned()
        })
        .unwrap_or_else(|| "default".into())
}

/// Rolling upgrades keep legacy nodes on their existing contract. A modern
/// node advertises the protocol even while its first probe is still pending.
pub(crate) fn evaluate_declared(node: &Value, provider: &str, profile: &str) -> ReadinessReport {
    if node
        .pointer("/capabilities/execution_readiness_version")
        .is_none()
    {
        return ReadinessReport {
            eligible: true,
            reasons: Vec::new(),
        };
    }
    with_forwarding(
        evaluate(
            node,
            provider,
            profile,
            chrono::Utc::now().timestamp_millis(),
        ),
        node,
    )
}

fn with_forwarding(mut report: ReadinessReport, node: &Value) -> ReadinessReport {
    if let Some(reason) = crate::services::session_forwarding::probe::readiness_reason(node) {
        report.reasons.push(reason);
        report.eligible = false;
    }
    report
}

pub(crate) fn attach_diagnostics(node: &mut Value) {
    let providers = node
        .pointer("/capabilities/intake_worker/providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let reports: BTreeMap<_, _> = providers
        .iter()
        .filter_map(Value::as_str)
        .map(|p| {
            (
                p.to_owned(),
                with_forwarding(
                    evaluate(
                        node,
                        p,
                        &expected_auth_profile(p, "", ""),
                        chrono::Utc::now().timestamp_millis(),
                    ),
                    node,
                ),
            )
        })
        .collect();
    node["execution_readiness"] =
        json!({"providers":reports, "credential_evidence":"local_metadata"});
}

/// Called only after a successful claim query, including an empty queue. A
/// stuck/erroring poller cannot refresh this evidence via the node heartbeat.
pub(crate) fn record_poller_progress(provider: &str) {
    if let Ok(mut pollers) = POLLERS.write() {
        pollers.insert(provider.to_owned(), chrono::Utc::now().timestamp_millis());
    }
}

pub(crate) fn publish(capabilities: &mut serde_json::Map<String, Value>) {
    capabilities.insert("execution_readiness_version".into(), json!(1));
    capabilities.insert(
        "execution_readiness".into(),
        SNAPSHOT
            .read()
            .ok()
            .and_then(|s| s.as_ref().and_then(|s| serde_json::to_value(s).ok()))
            .unwrap_or(Value::Null),
    );
    capabilities.insert(
        "intake_poller".into(),
        POLLERS
            .read()
            .ok()
            .and_then(|p| serde_json::to_value(&*p).ok())
            .unwrap_or(Value::Null),
    );
}

pub(crate) fn spawn_probe(config: Config) {
    tokio::spawn(async move {
        loop {
            let config = config.clone();
            // Exactly one collector at a time. Provider --version probes have
            // existing deadlines; a hung filesystem probe cannot accumulate
            // detached collectors or turn old evidence into a new success.
            match tokio::task::spawn_blocking(move || collect(&config)).await {
                Ok(snapshot) => {
                    if let Ok(mut cached) = SNAPSHOT.write() {
                        *cached = Some(snapshot);
                    }
                }
                Err(error) => tracing::warn!(%error, "cluster execution probe failed"),
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

fn collect(config: &Config) -> ExecutionProbe {
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    let catalog = crate::services::discord::org_schema::provider_auth_catalog();
    let mut providers = BTreeMap::new();
    for id in super::intake_worker_capabilities::active_intake_worker_providers() {
        let Some(kind) = ProviderKind::from_str(&id) else {
            continue;
        };
        let Some(probe) = kind.probe_runtime() else {
            continue;
        };
        let mut profiles = BTreeMap::from([("default".into(), probe.credential_present)]);
        if let Some(entry) = kind.registry_entry() {
            for (profile_id, profile) in &catalog {
                if profile.provider != id {
                    continue;
                }
                let present =
                    provider_auth_profile::resolve(kind.clone(), Some(profile_id), None, &catalog)
                        .ok()
                        .is_some_and(|overlay| {
                            provider_auth::detect_provider_credentials_with_overlay(
                                &id,
                                &entry.auth,
                                Some(&overlay),
                            )
                            .credential_present
                        });
                profiles.insert(profile_id.clone(), present);
            }
        }
        providers.insert(
            id,
            ProviderEvidence {
                cli_usable: probe.version.is_some() && probe.probe_failure_kind.is_none(),
                version: probe.version.map(|v| v.chars().take(256).collect()),
                failure: probe.probe_failure_kind,
                credential_profiles: profiles,
                authentication_verified: false,
                quota_verified: false,
            },
        );
    }
    let mut tool_names = vec!["git", "tmux"];
    if let Some(configured) = config
        .cluster
        .capabilities
        .get("tools")
        .and_then(Value::as_array)
    {
        tool_names.extend(configured.iter().filter_map(Value::as_str).take(32));
    }
    let tools: BTreeMap<_, _> = tool_names
        .into_iter()
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        })
        .map(|name| {
            (
                name.to_owned(),
                platform::binary_resolver::resolve_binary_with_login_shell(name).is_some(),
            )
        })
        .collect();
    let repositories = config
        .github
        .repo_dirs
        .keys()
        .take(64)
        .map(|id| {
            let ready = crate::services::git::resolve_repo_dir_for_id(Some(id))
                .ok()
                .flatten()
                .is_some_and(|path| {
                    std::path::Path::new(&path).is_dir()
                        && crate::services::git::repo_resolver::repo_id_for_dir(&path).as_deref()
                            == Some(id)
                });
            (id.clone(), ready)
        })
        .collect();
    let mut backends = vec!["process".into()];
    if cfg!(unix) && platform::tmux::is_available() {
        backends.push("tmux".into());
    }
    let root = crate::config::runtime_root().unwrap_or_else(|| config.data.dir.clone());
    ExecutionProbe {
        schema: 1,
        boot_id: BOOT_ID.clone(),
        observed_at_ms,
        expires_at_ms: observed_at_ms.saturating_add(PROBE_TTL_MS),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        runtime_profile: config.cluster.runtime_profile,
        release: crate::services::release_source::health_json(false),
        providers,
        tools,
        repositories,
        backends,
        disk_free_bytes: crate::services::disk_monitor::probe(&root).map(|p| p.free_bytes),
    }
}

#[cfg(test)]
mod tests;
