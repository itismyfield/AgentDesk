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
    pub minimum_disk_free_bytes: u64,
    pub minimum_memory_available_bytes: u64,
    pub maximum_recent_db_pool_errors: u64,
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
    SourceReleaseShaMissing,
    SourceConfigSchemaMissing,
    SourceProviderBinaryVersionMissing,
    ExpectedWorkspaceHeadMissing,
    ExpectedWorkspaceBranchMissing,
    ProviderUnsupported,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PreflightVerdict {
    Pass,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TargetPreflightReport {
    pub target_instance_id: String,
    pub provider: String,
    pub verdict: PreflightVerdict,
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

fn equal_when_expected(actual: &str, expected: &str) -> bool {
    expected.trim().is_empty() || nonempty_equal(actual, expected)
}

fn supported_provider(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "claude" | "codex"
    )
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
        !policy.source_release_sha.trim().is_empty(),
        PreflightReasonCode::SourceReleaseShaMissing,
        || "source release SHA expectation is missing".to_string(),
    );
    push_failure(
        &mut failures,
        !policy.source_config_schema.trim().is_empty(),
        PreflightReasonCode::SourceConfigSchemaMissing,
        || "source config schema expectation is missing".to_string(),
    );
    push_failure(
        &mut failures,
        !policy.source_provider_binary_version.trim().is_empty(),
        PreflightReasonCode::SourceProviderBinaryVersionMissing,
        || "source provider binary version expectation is missing".to_string(),
    );
    push_failure(
        &mut failures,
        !policy.expected_workspace_head.trim().is_empty(),
        PreflightReasonCode::ExpectedWorkspaceHeadMissing,
        || "expected workspace HEAD is missing".to_string(),
    );
    push_failure(
        &mut failures,
        !policy.expected_workspace_branch.trim().is_empty(),
        PreflightReasonCode::ExpectedWorkspaceBranchMissing,
        || "expected workspace branch is missing".to_string(),
    );
    push_failure(
        &mut failures,
        supported_provider(&policy.provider),
        PreflightReasonCode::ProviderUnsupported,
        || {
            format!(
                "provider '{}' is outside the Claude/Codex preflight contract",
                policy.provider
            )
        },
    );
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
        equal_when_expected(&snapshot.release_sha, &policy.source_release_sha),
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
        equal_when_expected(&snapshot.config_schema, &policy.source_config_schema),
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
        equal_when_expected(
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
        equal_when_expected(&snapshot.workspace_head, &policy.expected_workspace_head),
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
        equal_when_expected(
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
    push_failure(
        &mut failures,
        snapshot.workspace_clean,
        PreflightReasonCode::WorkspaceDirty,
        || "target workspace is dirty".to_string(),
    );
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
    push_failure(
        &mut failures,
        snapshot.standby_relay_ready,
        PreflightReasonCode::StandbyRelayUnavailable,
        || "standby relay probe failed".to_string(),
    );
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

    let verdict = if failures.is_empty() {
        PreflightVerdict::Pass
    } else {
        PreflightVerdict::Fail
    };
    TargetPreflightReport {
        target_instance_id,
        provider: policy.provider.trim().to_ascii_lowercase(),
        verdict,
        passed: verdict == PreflightVerdict::Pass,
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
            minimum_disk_free_bytes: 100,
            minimum_memory_available_bytes: 200,
            maximum_recent_db_pool_errors: 1,
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
    fn ready_target_requires_no_execution_callback() {
        let report = evaluate_target_preflight(&ready_node(), &policy());

        assert!(report.require_ready().is_ok());
        assert!(report.passed);
        assert!(report.attachment_notice.is_some());
    }

    type NodeMutation = (&'static str, PreflightReasonCode, Box<dyn Fn(&mut Value)>);

    fn required_failure_mutations() -> Vec<NodeMutation> {
        vec![
            (
                "offline",
                PreflightReasonCode::TargetOffline,
                Box::new(|n| n["status"] = json!("offline")),
            ),
            (
                "provider",
                PreflightReasonCode::ProviderIntakeUnavailable,
                Box::new(|n| n["capabilities"]["intake_worker"]["providers"] = json!(["codex"])),
            ),
            (
                "poller",
                PreflightReasonCode::WorkerPollerUnavailable,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["worker_poller_ready"] = json!(false)
                }),
            ),
            (
                "sha",
                PreflightReasonCode::ReleaseShaMismatch,
                Box::new(|n| n["capabilities"]["intake_preflight"]["release_sha"] = json!("wrong")),
            ),
            (
                "schema",
                PreflightReasonCode::ConfigSchemaMismatch,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["config_schema"] = json!("wrong")
                }),
            ),
            (
                "binary",
                PreflightReasonCode::ProviderBinaryVersionMismatch,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["provider_binary_version"] =
                        json!("wrong")
                }),
            ),
            (
                "credentials",
                PreflightReasonCode::ProviderCredentialsInvalid,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["credentials_valid"] = json!(false)
                }),
            ),
            (
                "quota",
                PreflightReasonCode::ProviderQuotaUnavailable,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["quota_available"] = json!(false)
                }),
            ),
            (
                "access",
                PreflightReasonCode::ProviderAccessProbeFailed,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["token_rest_access"] = json!(false)
                }),
            ),
            (
                "workspace",
                PreflightReasonCode::WorkspaceMissing,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["workspace_exists"] = json!(false)
                }),
            ),
            (
                "head",
                PreflightReasonCode::WorkspaceHeadMismatch,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["workspace_head"] = json!("wrong")
                }),
            ),
            (
                "branch",
                PreflightReasonCode::WorkspaceBranchMismatch,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["workspace_branch"] = json!("wrong")
                }),
            ),
            (
                "dirty",
                PreflightReasonCode::WorkspaceDirty,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["workspace_clean"] = json!(false)
                }),
            ),
            (
                "disk",
                PreflightReasonCode::InsufficientDisk,
                Box::new(|n| n["capabilities"]["intake_preflight"]["disk_free_bytes"] = json!(99)),
            ),
            (
                "memory",
                PreflightReasonCode::InsufficientMemory,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["memory_available_bytes"] = json!(199)
                }),
            ),
            (
                "db",
                PreflightReasonCode::DbPoolErrorThresholdExceeded,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["recent_db_pool_errors"] = json!(2)
                }),
            ),
            (
                "terminal_relay",
                PreflightReasonCode::TerminalRelayUnavailable,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["terminal_relay_ready"] = json!(false)
                }),
            ),
            (
                "standby_relay",
                PreflightReasonCode::StandbyRelayUnavailable,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["standby_relay_ready"] = json!(false)
                }),
            ),
            (
                "outbox",
                PreflightReasonCode::IntakeOutboxOperatorUnavailable,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["intake_outbox_operator_ready"] =
                        json!(false)
                }),
            ),
            (
                "attachments",
                PreflightReasonCode::AttachmentsUnsupported,
                Box::new(|n| {
                    n["capabilities"]["intake_preflight"]["attachments"] = json!("unsupported")
                }),
            ),
        ]
    }

    #[test]
    fn each_required_failure_is_independently_fail_closed() {
        for (name, expected_reason, mutate) in required_failure_mutations() {
            let mut node = ready_node();
            mutate(&mut node);
            let report = evaluate_target_preflight(&node, &policy());
            assert!(
                report.require_ready().is_err(),
                "{name} unexpectedly passed"
            );
            assert_eq!(
                report.failures.len(),
                1,
                "{name} did not fail independently"
            );
            assert_eq!(report.failures[0].code, expected_reason, "{name} reason");
        }
    }

    #[test]
    fn missing_source_expectations_fail_closed() {
        let cases: Vec<(
            &str,
            PreflightReasonCode,
            Box<dyn Fn(&mut TargetPreflightPolicy)>,
        )> = vec![
            (
                "release_sha",
                PreflightReasonCode::SourceReleaseShaMissing,
                Box::new(|policy| policy.source_release_sha.clear()),
            ),
            (
                "config_schema",
                PreflightReasonCode::SourceConfigSchemaMissing,
                Box::new(|policy| policy.source_config_schema.clear()),
            ),
            (
                "provider_binary",
                PreflightReasonCode::SourceProviderBinaryVersionMissing,
                Box::new(|policy| policy.source_provider_binary_version.clear()),
            ),
            (
                "workspace_head",
                PreflightReasonCode::ExpectedWorkspaceHeadMissing,
                Box::new(|policy| policy.expected_workspace_head.clear()),
            ),
            (
                "workspace_branch",
                PreflightReasonCode::ExpectedWorkspaceBranchMissing,
                Box::new(|policy| policy.expected_workspace_branch.clear()),
            ),
        ];

        for (name, expected_reason, mutate) in cases {
            let mut policy = policy();
            mutate(&mut policy);
            let report = evaluate_target_preflight(&ready_node(), &policy);

            assert!(
                report.require_ready().is_err(),
                "{name} unexpectedly passed"
            );
            assert_eq!(
                report.failures.len(),
                1,
                "{name} did not fail independently"
            );
            assert_eq!(report.failures[0].code, expected_reason, "{name} reason");
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

    async fn production_state_snapshot(pool: &sqlx::PgPool) -> Value {
        let sessions: Value = sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY s.id), '[]'::jsonb) FROM sessions s",
        )
        .fetch_one(pool)
        .await
        .expect("snapshot sessions");
        let intake_outbox: Value = sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(to_jsonb(o) ORDER BY o.id), '[]'::jsonb) FROM intake_outbox o",
        )
        .fetch_one(pool)
        .await
        .expect("snapshot intake_outbox");
        let intake_session_owners: Value = sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(to_jsonb(o) ORDER BY o.id), '[]'::jsonb) FROM intake_session_owners o",
        )
        .fetch_one(pool)
        .await
        .expect("snapshot intake_session_owners");
        json!({
            "sessions": sessions,
            "intake_outbox": intake_outbox,
            "intake_session_owners": intake_session_owners,
        })
    }

    #[tokio::test]
    async fn evaluation_does_not_mutate_production_handoff_state_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query("INSERT INTO agents (id, name) VALUES ('preflight-agent', 'Preflight')")
            .execute(&pool)
            .await
            .expect("seed agent");
        sqlx::query(
            "INSERT INTO sessions
                (session_key, agent_id, provider, status, channel_id, instance_id)
             VALUES
                ('preflight-session', 'preflight-agent', 'claude', 'idle',
                 'preflight-channel', 'mac-book-release')",
        )
        .execute(&pool)
        .await
        .expect("seed session");
        sqlx::query(
            "INSERT INTO intake_session_owners
                (provider, raw_channel_id, owner_instance_id, generation, status)
             VALUES ('claude', 'preflight-channel', 'mac-book-release', 41, 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed intake owner");
        sqlx::query(
            "INSERT INTO intake_outbox
                (target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
                 request_owner_id, user_text, turn_kind, agent_id, status,
                 owner_generation, owner_instance_id)
             VALUES
                ('mac-book-release', 'mac-mini-release', 'preflight-channel',
                 'preflight-message', 'preflight-user', 'hello', 'message',
                 'preflight-agent', 'failed_pre_accept', 41, 'mac-book-release')",
        )
        .execute(&pool)
        .await
        .expect("seed intake outbox");

        let before = production_state_snapshot(&pool).await;
        let passing = evaluate_target_preflight(&ready_node(), &policy());
        assert!(passing.require_ready().is_ok());
        for (name, expected_reason, mutate) in required_failure_mutations() {
            let mut node = ready_node();
            mutate(&mut node);
            let report = evaluate_target_preflight(&node, &policy());
            assert!(
                report.require_ready().is_err(),
                "{name} unexpectedly passed"
            );
            assert_eq!(report.failures.len(), 1, "{name} failure count");
            assert_eq!(report.failures[0].code, expected_reason, "{name} reason");
            assert_eq!(
                production_state_snapshot(&pool).await,
                before,
                "{name} mutated production handoff state"
            );
        }
        let after = production_state_snapshot(&pool).await;

        assert_eq!(after, before);
        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn claude_and_codex_emit_structured_pass_and_fail_evidence() {
        for provider in ["claude", "codex"] {
            let mut policy = policy();
            policy.provider = provider.to_string();
            let mut node = ready_node();
            node["capabilities"]["intake_worker"]["providers"] = json!([provider]);

            let passing = serde_json::to_value(evaluate_target_preflight(&node, &policy)).unwrap();
            assert_eq!(passing["provider"], provider);
            assert_eq!(passing["verdict"], "pass");
            assert_eq!(passing["passed"], true);
            assert_eq!(passing["failures"], json!([]));

            node["capabilities"]["intake_preflight"]["credentials_valid"] = json!(false);
            let failing = serde_json::to_value(evaluate_target_preflight(&node, &policy)).unwrap();
            assert_eq!(failing["provider"], provider);
            assert_eq!(failing["verdict"], "fail");
            assert_eq!(failing["passed"], false);
            assert_eq!(
                failing["failures"][0]["code"],
                "provider_credentials_invalid"
            );
            assert!(failing["failures"][0]["detail"].is_string());
        }
    }

    #[test]
    fn unsupported_provider_fails_closed() {
        let mut policy = policy();
        policy.provider = "gemini".to_string();
        let mut node = ready_node();
        node["capabilities"]["intake_worker"]["providers"] = json!(["gemini"]);
        let report = evaluate_target_preflight(&node, &policy);

        assert_eq!(report.verdict, PreflightVerdict::Fail);
        assert_eq!(
            report.failures[0].code,
            PreflightReasonCode::ProviderUnsupported
        );
        assert!(report.require_ready().is_err());
    }
}
