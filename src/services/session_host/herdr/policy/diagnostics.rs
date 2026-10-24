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
            json!({"stdout": capped(stdout), "stderr": capped(stderr)}),
        );
    }
}

pub(crate) fn record(
    socket: &Path,
    cache: &ProofCache,
    result: &Result<NextRestartProof, Unverified>,
) {
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
                _ => format!("next_restart_policy_unverified:{reason:?}"),
            },
            match reason {
                Unverified::Peer => {
                    "On the execution node, start bare herdr if absent and recheck the configured socket; AgentDesk never starts or restarts it."
                }
                Unverified::Unsupported => {
                    "Use a verified herdr 0.9.3 / protocol 22 server on the execution node, then retry."
                }
                Unverified::Identity => {
                    "Verify the execution node's loaded executable identity; this platform may remain Unverified. A pathname stat is insufficient."
                }
                Unverified::Ambiguous => {
                    "Normalize HOME/XDG/HERDR_CONFIG_PATH on the execution node so the actual default config and selected socket are unambiguous, then retry."
                }
                Unverified::ResumeEnabled => {
                    "Review the owner consent card or explicitly set session.resume_agents_on_restore=false in the execution user's config, then retry."
                }
                _ => {
                    "Read the bounded checker diagnostics, repair the execution user's full config or permissions, then retry. No automatic restart or config fix is performed."
                }
            },
            String::new(),
        ),
    };
    if let Ok(mut observed) = OBSERVED.get_or_init(Mutex::default).lock() {
        observed.policy.insert(
            socket.into(),
            json!({"next_restart_ready": ready,
            "startup_proof": startup, "reason": reason, "action": action, "witness": witness,
            "observation": "last_check_only; revalidated at each mutation/create fence"}),
        );
    }
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
        value["checker"] = checker.clone();
    }
    value
}

pub(crate) fn checker_detail(socket: &Path) -> String {
    let value = snapshot(socket);
    let excerpt = |key| {
        value["checker"][key]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(512)
            .collect::<String>()
    };
    format!(
        "stdout={:?}; stderr={:?}; repair the full user config and retry",
        excerpt("stdout"),
        excerpt("stderr")
    )
}

#[cfg(test)]
#[path = "diagnostics_tests.rs"]
mod tests;
