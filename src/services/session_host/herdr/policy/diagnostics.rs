//! Read-only observations never grant authority or revalidate a peer from health/doctor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use super::{NextRestartProof, ProofCache, Unverified};

#[derive(Default)]
struct Observed {
    policy: BTreeMap<PathBuf, Value>,
    checker: BTreeMap<PathBuf, Value>,
    server: BTreeMap<PathBuf, String>,
}
static OBSERVED: OnceLock<Mutex<Observed>> = OnceLock::new();

pub(crate) fn checker(socket: &Path, stdout: &[u8], stderr: &[u8]) {
    if let Ok(mut observed) = OBSERVED.get_or_init(Mutex::default).lock() {
        let capped = |bytes| {
            String::from_utf8_lossy(bytes)
                .chars()
                .take(4096)
                .collect::<String>()
        };
        observed.checker.insert(
            socket.into(),
            json!({"stdout": capped(stdout), "stderr": capped(stderr),
                "last_error_line": String::from_utf8_lossy(stderr).lines()
                    .rfind(|line| !line.trim().is_empty()).unwrap_or("")
                    .chars().rev().take(256).collect::<String>().chars().rev().collect::<String>()}),
        );
    }
}

/// The version and protocol the socket's last pong reported, shown beside every reason.
pub(crate) fn server(socket: &Path, version: &str, protocol: u32) {
    if let Ok(mut observed) = OBSERVED.get_or_init(Mutex::default).lock() {
        let shown = version.chars().take(64).collect::<String>();
        observed.server.insert(
            socket.into(),
            format!("herdr {shown} / protocol {protocol}"),
        );
    }
}

/// Keeps the last observation for health and doctor and returns it, so every surface shows the
/// same reason and action.
pub(crate) fn record(
    socket: &Path,
    cache: &ProofCache,
    result: &Result<NextRestartProof, Unverified>,
) -> Value {
    let (ready, startup, reason, action, witness) = match result {
        Ok(proof) => {
            let startup = cache.startup_for(proof.witness()).ok().flatten().is_some();
            (
                true,
                if startup {
                    "verified"
                } else {
                    "awaiting_server_restart"
                },
                "next_restart_ready".to_owned(),
                "Fresh creation is ready; the server need not restart. Existing execution permits remain exact-witness scoped.",
                format!("{:?}", proof.witness()),
            )
        }
        Err(reason) => (
            false,
            "unknown",
            match reason {
                Unverified::ResumeEnabled => "next_restart_resumes".into(),
                Unverified::Ambiguous => "next_restart_policy_ambiguous".into(),
                #[cfg(target_os = "macos")]
                Unverified::ImageChangedSinceStart => {
                    "next_restart_image_changed_since_start".into()
                }
                #[cfg(target_os = "macos")]
                Unverified::ImageTimeAmbiguous => "next_restart_image_time_ambiguous".into(),
                #[cfg(target_os = "macos")]
                Unverified::ImageFilesystemUnsupported => {
                    "next_restart_image_filesystem_unsupported".into()
                }
                #[cfg(target_os = "macos")]
                Unverified::ImageLayoutUnsupported => {
                    "next_restart_image_layout_unsupported".into()
                }
                _ => format!("next_restart_policy_unverified:{reason:?}"),
            },
            match reason {
                Unverified::Peer | Unverified::Changed => {
                    "The herdr peer on this socket could not be verified. See `autostart` for server presence and start eligibility; AgentDesk never stops or restarts a live server."
                }
                Unverified::Unsupported => {
                    "Use a stable herdr release of protocol 22, version 0.9.3 or newer, on the execution node, then retry; `server` shows what the socket reported."
                }
                Unverified::Identity if !super::peer::SUPPORTED => {
                    "This platform has no loaded-image adapter, so a user-default server cannot be verified here; keep legacy_dedicated."
                }
                Unverified::Identity => {
                    "Verify the execution node's loaded executable identity; this platform may remain Unverified. A pathname stat is insufficient."
                }
                #[cfg(target_os = "macos")]
                Unverified::ImageChangedSinceStart => {
                    "The herdr executable changed after this server process started (install, upgrade, permission or xattr change). Waiting, consent retry or an AgentDesk restart cannot clear this for the same server; the execution node needs a herdr server started after the executable's last change. AgentDesk never restarts it; held inputs stay preserved."
                }
                #[cfg(target_os = "macos")]
                Unverified::ImageTimeAmbiguous => {
                    "The herdr executable's change time falls inside the server's start microsecond, so its order is unknown; this is not proof of a later change. A herdr server started after the executable's last change clears it; AgentDesk never restarts it; held inputs stay preserved."
                }
                #[cfg(target_os = "macos")]
                Unverified::ImageFilesystemUnsupported => {
                    "The herdr executable is on a filesystem other than APFS, whose change times cannot prove it is unchanged since the server started. Install herdr on APFS or keep legacy_dedicated."
                }
                #[cfg(target_os = "macos")]
                Unverified::ImageLayoutUnsupported => {
                    "The herdr server is not a native arm64 single-architecture image (universal/fat or x86_64 are not supported yet). Use a native arm64 herdr build or keep legacy_dedicated."
                }
                Unverified::Ambiguous => {
                    "Normalize HOME/XDG/HERDR_CONFIG_PATH on the execution node so the actual default config and selected socket are unambiguous, then retry."
                }
                Unverified::ResumeEnabled => {
                    "The execution user's herdr resumes agents on restore. Owner consent may offer a card if its preview succeeds, or explicitly set session.resume_agents_on_restore=false in that config, then retry."
                }
                _ => {
                    "Read the bounded checker diagnostics, repair the execution user's full config or permissions, then retry. No automatic restart or config fix is performed."
                }
            },
            String::new(),
        ),
    };
    let mut value = json!({"next_restart_ready": ready,
    "startup_proof": startup, "reason": reason, "action": action, "witness": witness,
    "observation": "last_check_only; a fresh create rechecks at its fence, while existing input mutations compare only the recorded server witness"});
    if let Ok(mut observed) = OBSERVED.get_or_init(Mutex::default).lock() {
        if let Some(server) = observed.server.get(socket) {
            value["server"] = server.clone().into();
        }
        observed.policy.insert(socket.into(), value.clone());
    }
    value
}

/// One next-restart check of `socket` with a fresh cache, and whether a server answers there and
/// could be started, as a dry run; it writes no config or file and starts no server.
#[cfg(unix)]
pub(crate) async fn probe(
    socket: &Path,
    env: super::super::autostart::Environment,
) -> Result<Value, String> {
    let source = super::super::launch_host::probe_source(socket)?;
    let cache = ProofCache::default();
    let result = cache.next_restart(source, &super::LocalChecker).await;
    let mut value = record(socket, &cache, &result);
    value["socket"] = socket.display().to_string().into();
    value["observation"] = "probe_once; nothing outlives this process".into();
    value["autostart"] = super::super::autostart::assess(socket, env).await;
    Ok(value)
}

pub(crate) fn snapshot(socket: &Path) -> Value {
    let Ok(observed) = OBSERVED.get_or_init(Mutex::default).lock() else {
        return json!({"next_restart_ready": false, "startup_proof": "unknown", "reason": "policy_cache_unavailable"});
    };
    let mut value = observed.policy.get(socket).cloned().unwrap_or_else(|| json!({
        "next_restart_ready": false, "startup_proof": "unknown", "reason": "not_observed",
        "action": "On the execution node, inspect the consent/launch refusal and retry; health performs no server or config IO."
    }));
    if let Some(checker) = observed.checker.get(socket) {
        value["checker"] = checker_excerpt(checker).into();
    }
    #[cfg(unix)]
    if let Some(autostart) = super::super::autostart::last(socket) {
        value["autostart"] = autostart;
    }
    value
}

fn checker_excerpt(checker: &Value) -> String {
    let tail = checker["last_error_line"].as_str().unwrap_or("");
    let suffix = if tail.is_empty() {
        String::new()
    } else {
        format!("\nlast error: {tail}")
    };
    let prefix = format!(
        "stdout={}; stderr={}",
        checker["stdout"].as_str().unwrap_or(""),
        checker["stderr"].as_str().unwrap_or("")
    );
    let remaining = 512usize.saturating_sub(suffix.chars().count());
    prefix
        .chars()
        .take(remaining)
        .chain(suffix.chars())
        .collect()
}

pub(crate) fn doctor_evidence(config: &crate::config::Config) -> Value {
    use crate::config::session_hosts::{EndpointMode, with_boot};
    if !config
        .session_hosts
        .herdr
        .endpoints
        .values()
        .any(|endpoint| endpoint.mode == EndpointMode::UserDefault)
    {
        return json!({});
    }
    // Observations use the selected boot socket, including execution-user defaults.
    with_boot(|boot| checker_evidence(boot.map_or(&config.session_hosts, |boot| boot.config())))
}

fn checker_evidence(hosts: &crate::config::session_hosts::SessionHostsConfig) -> Value {
    use crate::config::session_hosts::EndpointMode;
    let endpoints: Vec<_> = hosts
        .herdr
        .endpoints
        .iter()
        .filter(|(_, endpoint)| endpoint.mode == EndpointMode::UserDefault)
        .collect();
    if endpoints.is_empty() {
        return json!({});
    }
    let Ok(observed) = OBSERVED.get_or_init(Mutex::default).lock() else {
        return json!({"reason": "policy_cache_unavailable"});
    };
    let mut evidence = serde_json::Map::new();
    for (key, endpoint) in endpoints {
        if let Some(checker) = observed.checker.get(&endpoint.socket_path) {
            evidence.insert(key.clone(), checker.clone());
        }
    }
    Value::Object(evidence)
}

pub(crate) fn checker_detail(socket: &Path) -> String {
    let value = snapshot(socket);
    format!(
        "{}; repair the full user config and retry",
        value["checker"]
            .as_str()
            .unwrap_or("checker output unavailable")
    )
}

#[cfg(test)]
#[path = "diagnostics_tests.rs"]
mod tests;
