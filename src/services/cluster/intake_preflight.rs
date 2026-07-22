//! Fail-closed target-readiness gate for planned intake owner handoff.
//!
//! The handoff coordinator supplies source expectations plus the target node's
//! latest probe snapshot. This module is deliberately pure: collecting remote
//! credentials and host-resource evidence belongs to the node health probe,
//! while this gate makes the transfer decision deterministic and testable.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::intake_worker_capabilities::node_supports_intake_provider;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttachmentReadiness {
    Portable,
    TextOnlyPilot,
    Unsupported,
}

impl Default for AttachmentReadiness {
    fn default() -> Self {
        Self::Unsupported
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TargetPreflightPolicy {
    pub provider: String,
    pub source_release_sha: String,
    pub source_config_schema: String,
    pub source_provider_binary_version: String,
    pub expected_workspace_head: String,
    pub expected_workspace_branch: String,
    pub require_clean_workspace: bool,
    pub minimum_disk_free_bytes: u64,
    pub minimum_memory_available_bytes: u64,
    pub maximum_recent_db_pool_errors: u64,
    pub require_standby_relay: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct TargetProbeSnapshot {
    pub release_sha: String,
    pub config_schema: String,
    pub provider_binary_version: String,
    pub credentials_valid: bool,
    pub quota_available: bool,
    pub token_rest_access: bool,
    pub workspace_exists: bool,
    pub workspace_head: String,
    pub workspace_branch: String,
    pub workspace_clean: bool,
    pub disk_free_bytes: u64,
    pub memory_available_bytes: u64,
    pub recent_db_pool_errors: u64,
    pub worker_poller_ready: bool,
    pub terminal_relay_ready: bool,
    pub standby_relay_ready: bool,
    pub intake_outbox_operator_ready: bool,
    #[serde(default)]
    pub attachments: AttachmentReadiness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PreflightReasonCode {
    TargetOffline,
    ProviderIntakeUnavailable,
    WorkerPollerUnavailable,
    ReleaseShaMismatch,
    ConfigSchemaMismatch,
    ProviderBinaryVersionMismatch,
    ProviderCredentialsInvalid,
    ProviderQuotaUnavailable,
    ProviderAccessProbeFailed,
    WorkspaceMissing,
    WorkspaceHeadMismatch,
    WorkspaceBranchMismatch,
    WorkspaceDirty,
    InsufficientDisk,
    InsufficientMemory,
    DbPoolErrorThresholdExceeded,
    TerminalRelayUnavailable,
    StandbyRelayUnavailable,
    IntakeOutboxOperatorUnavailable,
    AttachmentsUnsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PreflightFailure {
    pub code: PreflightReasonCode,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TargetPreflightReport {
    pub target_instance_id: String,
    pub provider: String,
    pub passed: bool,
    pub attachment_readiness: AttachmentReadiness,
    pub attachment_notice: Option<String>,
    pub failures: Vec<PreflightFailure>,
}

impl TargetPreflightReport {
    pub(crate) fn require_ready(&self) -> Result<(), PreflightBlocked> {
        if self.passed {
            Ok(())
        } else {
            Err(PreflightBlocked {
                target_instance_id: self.target_instance_id.clone(),
                failures: self.failures.clone(),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreflightBlocked {
    pub target_instance_id: String,
    pub failures: Vec<PreflightFailure>,
}

/// Runs an owner mutation only after every required target check passes.
///
/// Planned-handoff code should use this boundary immediately around its
/// generation-fenced transfer call. A failed report never invokes `transfer`.
pub(crate) fn transfer_if_target_ready<T>(
    report: &TargetPreflightReport,
    transfer: impl FnOnce() -> T,
) -> Result<T, PreflightBlocked> {
    report.require_ready()?;
    Ok(transfer())
}

fn push_failure(
    failures: &mut Vec<PreflightFailure>,
    condition: bool,
    code: PreflightReasonCode,
    detail: impl FnOnce() -> String,
) {
    if !condition {
        failures.push(PreflightFailure {
            code,
            detail: detail(),
        });
    }
}

fn nonempty_equal(actual: &str, expected: &str) -> bool {
    !actual.trim().is_empty() && actual.trim() == expected.trim()
}

/// Evaluates the target node registry record and its latest `intake_preflight`
/// capability snapshot. Missing or malformed evidence fails closed.
pub(crate) fn evaluate_target_preflight(
    target_node: &Value,
    policy: &TargetPreflightPolicy,
) -> TargetPreflightReport {
    let target_instance_id = target_node
        .get("instance_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let snapshot = target_node
        .pointer("/capabilities/intake_preflight")
        .cloned()
        .and_then(|value| serde_json::from_value::<TargetProbeSnapshot>(value).ok())
        .unwrap_or_default();
    let mut failures = Vec::new();

    push_failure(
        &mut failures,
        target_node.get("status").and_then(Value::as_str) == Some("online"),
        PreflightReasonCode::TargetOffline,
        || "target worker-node lease is not online".to_string(),
    );
    push_failure(
        &mut failures,
        node_supports_intake_provider(target_node, &policy.provider),
        PreflightReasonCode::ProviderIntakeUnavailable,
        || {
            format!(
                "target does not advertise {} intake capability",
                policy.provider
            )
        },
    );
    push_failure(
        &mut failures,
        snapshot.worker_poller_ready,
        PreflightReasonCode::WorkerPollerUnavailable,
        || "target intake worker poller probe failed".to_string(),
    );
    push_failure(
        &mut failures,
        nonempty_equal(&snapshot.release_sha, &policy.source_release_sha),
        PreflightReasonCode::ReleaseShaMismatch,
        || {
            format!(
                "target SHA '{}' differs from source SHA '{}'",
                snapshot.release_sha, policy.source_release_sha
            )
        },
    );
    push_failure(
        &mut failures,
        nonempty_equal(&snapshot.config_schema, &policy.source_config_schema),
        PreflightReasonCode::ConfigSchemaMismatch,
        || {
            format!(
                "target config schema '{}' differs from source '{}'",
                snapshot.config_schema, policy.source_config_schema
            )
        },
    );
    push_failure(
        &mut failures,
        nonempty_equal(
            &snapshot.provider_binary_version,
            &policy.source_provider_binary_version,
        ),
        PreflightReasonCode::ProviderBinaryVersionMismatch,
        || {
            format!(
                "target provider binary '{}' differs from source '{}'",
                snapshot.provider_binary_version, policy.source_provider_binary_version
            )
        },
    );
    push_failure(
        &mut failures,
        snapshot.credentials_valid,
        PreflightReasonCode::ProviderCredentialsInvalid,
        || "provider credential probe failed".to_string(),
    );
    push_failure(
        &mut failures,
        snapshot.quota_available,
        PreflightReasonCode::ProviderQuotaUnavailable,
        || "provider quota probe failed".to_string(),
    );
    push_failure(
        &mut failures,
        snapshot.token_rest_access,
        PreflightReasonCode::ProviderAccessProbeFailed,
        || "provider token/REST access probe failed".to_string(),
    );
    push_failure(
        &mut failures,
        snapshot.workspace_exists,
        PreflightReasonCode::WorkspaceMissing,
        || "target AgentDesk workspace/repository is missing".to_string(),
    );
    push_failure(
        &mut failures,
        nonempty_equal(&snapshot.workspace_head, &policy.expected_workspace_head),
        PreflightReasonCode::WorkspaceHeadMismatch,
        || {
            format!(
                "workspace HEAD '{}' differs from expected '{}'",
                snapshot.workspace_head, policy.expected_workspace_head
            )
        },
    );
    push_failure(
        &mut failures,
        nonempty_equal(
            &snapshot.workspace_branch,
            &policy.expected_workspace_branch,
        ),
        PreflightReasonCode::WorkspaceBranchMismatch,
        || {
            format!(
                "workspace branch '{}' differs from expected '{}'",
                snapshot.workspace_branch, policy.expected_workspace_branch
            )
        },
    );
    if policy.require_clean_workspace {
        push_failure(
            &mut failures,
            snapshot.workspace_clean,
            PreflightReasonCode::WorkspaceDirty,
            || "target workspace is dirty".to_string(),
        );
    }
    push_failure(
        &mut failures,
        snapshot.disk_free_bytes >= policy.minimum_disk_free_bytes,
        PreflightReasonCode::InsufficientDisk,
        || {
            format!(
                "disk free {} is below required {} bytes",
                snapshot.disk_free_bytes, policy.minimum_disk_free_bytes
            )
        },
    );
    push_failure(
        &mut failures,
        snapshot.memory_available_bytes >= policy.minimum_memory_available_bytes,
        PreflightReasonCode::InsufficientMemory,
        || {
            format!(
                "memory available {} is below required {} bytes",
                snapshot.memory_available_bytes, policy.minimum_memory_available_bytes
            )
        },
    );
    push_failure(
        &mut failures,
        snapshot.recent_db_pool_errors <= policy.maximum_recent_db_pool_errors,
        PreflightReasonCode::DbPoolErrorThresholdExceeded,
        || {
            format!(
                "recent DB pool errors {} exceed threshold {}",
                snapshot.recent_db_pool_errors, policy.maximum_recent_db_pool_errors
            )
        },
    );
    push_failure(
        &mut failures,
        snapshot.terminal_relay_ready,
        PreflightReasonCode::TerminalRelayUnavailable,
        || "terminal relay probe failed".to_string(),
    );
    if policy.require_standby_relay {
        push_failure(
            &mut failures,
            snapshot.standby_relay_ready,
            PreflightReasonCode::StandbyRelayUnavailable,
            || "standby relay probe failed".to_string(),
        );
    }
    push_failure(
        &mut failures,
        snapshot.intake_outbox_operator_ready,
        PreflightReasonCode::IntakeOutboxOperatorUnavailable,
        || "intake-outbox operator surface probe failed".to_string(),
    );
    push_failure(
        &mut failures,
        snapshot.attachments != AttachmentReadiness::Unsupported,
        PreflightReasonCode::AttachmentsUnsupported,
        || "target cannot accept the configured attachment contract".to_string(),
    );

    let attachment_notice = match snapshot.attachments {
        AttachmentReadiness::Portable => None,
        AttachmentReadiness::TextOnlyPilot => Some(
            "This routed session is text-only; attachments must be rejected before handoff."
                .to_string(),
        ),
        AttachmentReadiness::Unsupported => {
            Some("Attachments are unsupported and target preflight is blocked.".to_string())
        }
    };

    TargetPreflightReport {
        target_instance_id,
        provider: policy.provider.trim().to_ascii_lowercase(),
        passed: failures.is_empty(),
        attachment_readiness: snapshot.attachments,
        attachment_notice,
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> TargetPreflightPolicy {
        TargetPreflightPolicy {
            provider: "claude".to_string(),
            source_release_sha: "abc123".to_string(),
            source_config_schema: "7".to_string(),
            source_provider_binary_version: "2.1.0".to_string(),
            expected_workspace_head: "abc123".to_string(),
            expected_workspace_branch: "main".to_string(),
            require_clean_workspace: true,
            minimum_disk_free_bytes: 100,
            minimum_memory_available_bytes: 200,
            maximum_recent_db_pool_errors: 1,
            require_standby_relay: true,
        }
    }

    fn ready_node() -> Value {
        json!({
            "instance_id": "mac-mini-release",
            "status": "online",
            "capabilities": {
                "intake_worker": {"enabled": true, "providers": ["claude"]},
                "intake_preflight": {
                    "release_sha": "abc123",
                    "config_schema": "7",
                    "provider_binary_version": "2.1.0",
                    "credentials_valid": true,
                    "quota_available": true,
                    "token_rest_access": true,
                    "workspace_exists": true,
                    "workspace_head": "abc123",
                    "workspace_branch": "main",
                    "workspace_clean": true,
                    "disk_free_bytes": 100,
                    "memory_available_bytes": 200,
                    "recent_db_pool_errors": 1,
                    "worker_poller_ready": true,
                    "terminal_relay_ready": true,
                    "standby_relay_ready": true,
                    "intake_outbox_operator_ready": true,
                    "attachments": "text_only_pilot"
                }
            }
        })
    }

    #[test]
    fn ready_target_allows_transfer_and_reports_text_only_notice() {
        let report = evaluate_target_preflight(&ready_node(), &policy());
        let mut owner = "mac-book-release";
        let result = transfer_if_target_ready(&report, || owner = "mac-mini-release");

        assert!(result.is_ok());
        assert_eq!(owner, "mac-mini-release");
        assert!(report.passed);
        assert!(report.attachment_notice.is_some());
    }

    #[test]
    fn each_required_failure_preserves_owner() {
        let mutations: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
            ("offline", Box::new(|n| n["status"] = json!("offline"))),
            (
                "provider",
                Box::new(|n| n["capabilities"]["intake_worker"]["providers"] = json!(["codex"])),
            ),
            (
                "poller",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["worker_poller_ready"] = json!(false)
                }),
            ),
            (
                "sha",
                Box::new(|n| n["capabilities"]["intake_preflight"]["release_sha"] = json!("wrong")),
            ),
            (
                "schema",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["config_schema"] = json!("wrong")
                }),
            ),
            (
                "binary",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["provider_binary_version"] =
                        json!("wrong")
                }),
            ),
            (
                "credentials",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["credentials_valid"] = json!(false)
                }),
            ),
            (
                "quota",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["quota_available"] = json!(false)
                }),
            ),
            (
                "access",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["token_rest_access"] = json!(false)
                }),
            ),
            (
                "workspace",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["workspace_exists"] = json!(false)
                }),
            ),
            (
                "head",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["workspace_head"] = json!("wrong")
                }),
            ),
            (
                "branch",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["workspace_branch"] = json!("wrong")
                }),
            ),
            (
                "dirty",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["workspace_clean"] = json!(false)
                }),
            ),
            (
                "disk",
                Box::new(|n| n["capabilities"]["intake_preflight"]["disk_free_bytes"] = json!(99)),
            ),
            (
                "memory",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["memory_available_bytes"] = json!(199)
                }),
            ),
            (
                "db",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["recent_db_pool_errors"] = json!(2)
                }),
            ),
            (
                "terminal_relay",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["terminal_relay_ready"] = json!(false)
                }),
            ),
            (
                "standby_relay",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["standby_relay_ready"] = json!(false)
                }),
            ),
            (
                "outbox",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["intake_outbox_operator_ready"] =
                        json!(false)
                }),
            ),
            (
                "attachments",
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["attachments"] = json!("unsupported")
                }),
            ),
        ];

        for (name, mutate) in mutations {
            let mut node = ready_node();
            mutate(&mut node);
            let report = evaluate_target_preflight(&node, &policy());
            let mut owner = "mac-book-release";
            let result = transfer_if_target_ready(&report, || owner = "mac-mini-release");
            assert!(result.is_err(), "{name} unexpectedly passed");
            assert_eq!(owner, "mac-book-release", "{name} mutated owner");
        }
    }

    #[test]
    fn missing_or_malformed_snapshot_fails_closed() {
        let mut node = ready_node();
        node["capabilities"]["intake_preflight"] = json!({"release_sha": 42});
        let report = evaluate_target_preflight(&node, &policy());

        assert!(!report.passed);
        assert!(report.failures.len() > 1);
    }

    #[test]
    fn structured_report_serializes_reason_codes() {
        let mut node = ready_node();
        node["capabilities"]["intake_preflight"]["release_sha"] = json!("wrong");
        let report = evaluate_target_preflight(&node, &policy());
        let encoded = serde_json::to_value(report).unwrap();

        assert_eq!(encoded["passed"], false);
        assert_eq!(encoded["failures"][0]["code"], "release_sha_mismatch");
    }
}
