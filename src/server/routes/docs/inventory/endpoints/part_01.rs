use serde_json::json;

#[allow(unused_imports)]
use super::super::{EndpointDoc, ParamDoc, body_param, ep, header_param, path_param, query_param};

pub(super) fn endpoints() -> Vec<EndpointDoc> {
    vec![
        ep(
            "GET",
            "/api/health",
            "health",
            "Health check with `server_up` minimum readiness and `fully_recovered` startup recovery completion.",
        )
        .with_example(
            json!({}),
            json!({
                "status": "healthy",
                "server_up": true,
                "fully_recovered": true,
                "delivery_record_rollout": {
                    "shadow_enabled": false,
                    "authority_enabled": false,
                    "mode": "off",
                    "dedup_authority": "in_memory_committed_offset",
                    "same_turn_backward_write_enforcement": "observe_only",
                    "warning_count": 1
                },
                "intake_routing": {
                    "mode": "disabled",
                    "source": "yaml",
                    "yaml": {
                        "enabled": false,
                        "mode": "observe",
                        "forward_pre_claim_timeout_secs": 12,
                        "stale_claim_recovery_secs": 60
                    },
                    "env_override": null,
                    "warning_count": 0,
                    "configuration_warnings": []
                },
                "latest_startup_doctor": {
                    "available": true,
                    "status": "warned",
                    "artifact_path": "/Users/kunkun/.adk/release/runtime/doctor/startup/123-456.json",
                    "started_at": "2026-04-26T14:49:14+09:00",
                    "completed_at": "2026-04-26T14:49:17+09:00",
                    "boot_id": "123-456",
                    "summary": {"passed": 21, "warned": 3, "failed": 0, "total": 24},
                    "failed_count": 0,
                    "warned_count": 3,
                    "detail_endpoint": "/api/doctor/startup/latest"
                },
                "db": true,
                "dashboard": true,
                "degraded": false,
                "degraded_reasons": [],
                "deferred_hooks": 0,
                "queue_depth": 0,
                "watcher_count": 0,
                "outbox_age": 0,
                "recovery_duration": 0.12
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"status": "unhealthy", "server_up": false, "fully_recovered": false, "db": false, "degraded": true, "degraded_reasons": ["db_unavailable"], "error": "db connection failing"}),
        )
        .with_curl("curl http://localhost:8787/api/health"),
        ep(
            "GET",
            "/api/health/detail",
            "health",
            "Local/protected detailed health with provider diagnostics and latest startup doctor detail.",
        )
        .with_example(
            json!({}),
            json!({
                "status": "healthy",
                "server_up": true,
                "fully_recovered": true,
                "delivery_record_rollout": {
                    "shadow_enabled": true,
                    "authority_enabled": true,
                    "mode": "shadow_and_authority",
                    "dedup_authority": "durable_delivery_record_frontier",
                    "same_turn_backward_write_enforcement": "enforcing",
                    "warning_count": 0,
                    "configuration_warnings": []
                },
                "intake_routing": {
                    "mode": "observe",
                    "source": "yaml",
                    "yaml": {
                        "enabled": true,
                        "mode": "observe",
                        "forward_pre_claim_timeout_secs": 12,
                        "stale_claim_recovery_secs": 60
                    },
                    "env_override": null,
                    "warning_count": 0,
                    "configuration_warnings": []
                },
                "latest_startup_doctor": {
                    "available": true,
                    "status": "failed",
                    "artifact_path": "/Users/kunkun/.adk/release/runtime/doctor/startup/123-456.json",
                    "summary": {"passed": 21, "warned": 3, "failed": 1, "total": 25},
                    "failed_count": 1,
                    "warned_count": 3,
                    "detail_endpoint": "/api/doctor/startup/latest",
                    "run_context": "startup_once",
                    "non_fatal": true,
                    "failed_checks": [{"id": "dispatch_outbox", "status": "fail"}],
                    "warned_checks": [{"id": "disk_usage", "status": "warn"}],
                    "followup_context": "restart_followup"
                }
            }),
        )
        .with_error_example(
            403,
            json!({}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl http://localhost:8787/api/health/detail"),
        ep(
            "GET",
            "/api/dispatch-outbox/failed",
            "health",
            "List up to 100 failed dispatch_outbox rows that make the startup doctor dispatch_outbox check fail.",
        )
        .with_example(
            json!({}),
            json!({
                "ok": true,
                "count": 1,
                "rows": [{
                    "id": 42,
                    "dispatch_id": "dispatch-123",
                    "action": "notify",
                    "agent_id": "project-agentdesk",
                    "retry_count": 5,
                    "error": "delivery failed",
                    "dispatch_status": "completed"
                }]
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"ok": false, "error": "pg pool unavailable"}),
        )
        .with_error_example(
            401,
            json!({}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl http://localhost:8787/api/dispatch-outbox/failed"),
        ep(
            "POST",
            "/api/dispatch-outbox/failed",
            "health",
            "Acknowledge failed dispatch_outbox rows without deleting them. Acknowledged rows no longer count as permanent failures.",
        )
        .with_params([
            (
                "ids",
                body_param("array<integer>", true, "Failed dispatch_outbox row ids to acknowledge. Required unless dry_run is true."),
            ),
            (
                "reason",
                body_param("string", false, "Operator-visible acknowledgement reason"),
            ),
            (
                "dry_run",
                body_param("boolean", false, "When true, return matching rows without mutating them"),
            ),
        ])
        .with_example(
            json!({"body": {"ids": [42], "reason": "obsolete completed dispatch notification", "dry_run": false}}),
            json!({"ok": true, "acknowledged": 1, "dry_run": false, "acknowledged_ids": [42]}),
        )
        .with_error_example(
            400,
            json!({"body": {}}),
            json!({"ok": false, "error": "ids required unless dry_run is true"}),
        )
        .with_error_example(
            503,
            json!({"body": {"dry_run": true}}),
            json!({"ok": false, "error": "pg pool unavailable"}),
        )
        .with_error_example(
            401,
            json!({"body": {"ids": [42]}}),
            json!({"ok": false, "error": "auth_token required for non-loopback host"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/dispatch-outbox/failed -H 'Content-Type: application/json' -d '{\"dry_run\":true}'"),
        ep(
            "GET",
            "/api/prompt-manifest/retention",
            "monitoring",
            "Prompt-manifest storage stats and the boot-time retention config snapshot; retention config changes require process restart and are not hot-reloaded.",
        )
        .with_example(
            json!({}),
            json!({
                "total_stored_bytes": 1234,
                "total_original_bytes": 5678,
                "manifest_count": 42,
                "layer_count": 168,
                "truncated_count": 3,
                "oldest_full_content_at": "2026-04-04T01:23:45Z",
                "retention_horizon_at": "2026-04-04T01:23:45Z",
                "retention_days": 30,
                "per_layer_max_bytes_adk_provided": 65536,
                "per_layer_max_bytes_user_derived": 16384,
                "enabled": true,
                "restart_required_for_config_changes": true,
                "config_applied_at": "boot",
                "config_source": "agentdesk.yaml boot snapshot",
                "hot_reload": false
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres pool unavailable"}),
        )
        .with_curl("curl http://localhost:8787/api/prompt-manifest/retention"),
        ep(
            "GET",
            "/api/cluster/nodes",
            "cluster",
            "Protected multinode worker registry view with configured/effective role, heartbeat, labels, and capabilities.",
        )
        .with_example(
            json!({}),
            json!({
                "cluster": {
                    "enabled": true,
                    "configured_role": "auto",
                    "lease_ttl_secs": 30,
                    "heartbeat_interval_secs": 10
                },
                "nodes": [{
                    "instance_id": "mac-mini",
                    "hostname": "mac-mini",
                    "role": "auto",
                    "effective_role": "leader",
                    "status": "online",
                    "labels": ["mac-mini"],
                    "capabilities": {"providers": ["codex"]}
                }]
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres unavailable"}),
        )
        .with_curl("curl http://localhost:8787/api/cluster/nodes"),
        ep(
            "GET",
            "/api/cluster/sessions",
            "cluster",
            "Protected diagnostic readout of the in-memory SessionRegistry populated by SessionDiscovery (Epic #2285 / E2 / #2344).",
        )
        .with_example(
            json!({}),
            json!({
                "count": 1,
                "sessions": [{
                    "matched": {
                        "channel_id": "1234",
                        "agent_id": "td",
                        "provider": "codex",
                        "expected_session_name": "AgentDesk-codex-1234",
                        "expected_rollout_path": "/tmp/agentdesk-AgentDesk-codex-1234.jsonl"
                    },
                    "first_seen_at": "2026-05-17T03:00:00Z",
                    "last_seen_at": "2026-05-17T03:01:10Z"
                }]
            }),
        )
        .with_curl("curl http://localhost:8787/api/cluster/sessions"),
        ep(
            "GET",
            "/api/cluster/routing-diagnostics",
            "cluster",
            "Explain which multinode workers satisfy a required capability set and why excluded workers do not match.",
        )
        .with_example(
            json!({"required": "{\"labels\":[\"mac-book\"],\"providers\":[\"codex\"],\"mcp\":{\"filesystem\":{\"healthy\":true}}}"}),
            json!({
                "required": {
                    "labels": ["mac-book"],
                    "providers": ["codex"],
                    "mcp": {"filesystem": {"healthy": true}}
                },
                "decisions": [{
                    "instance_id": "mac-book-release",
                    "eligible": true,
                    "reasons": []
                }, {
                    "instance_id": "mac-mini-release",
                    "eligible": false,
                    "reasons": ["missing label 'mac-book'"]
                }]
            }),
        )
        .with_error_example(
            400,
            json!({"required": "{not-json"}),
            json!({"error": "invalid required JSON: expected object key"}),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres unavailable"}),
        )
        .with_curl("curl --get http://localhost:8787/api/cluster/routing-diagnostics --data-urlencode 'required={\"labels\":[\"mac-book\"],\"providers\":[\"codex\"]}'"),
        ep(
            "GET",
            "/api/cluster/resource-locks",
            "cluster",
            "List active multinode resource locks used to serialize exclusive worker resources such as Unreal editor/test execution.",
        )
        .with_example(
            json!({"include_expired": false}),
            json!({
                "default_ttl_secs": 900,
                "locks": [{
                    "lock_key": "unreal:project:CookingHeart",
                    "holder_instance_id": "mac-book-release",
                    "holder_job_id": "phase-compile",
                    "metadata": {"phase": "compile"},
                    "expires_at": "2026-05-01T06:25:00Z",
                    "heartbeat_at": "2026-05-01T06:10:00Z",
                    "created_at": "2026-05-01T06:10:00Z",
                    "updated_at": "2026-05-01T06:10:00Z"
                }]
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres unavailable"}),
        )
        .with_curl("curl http://localhost:8787/api/cluster/resource-locks"),
        ep(
            "POST",
            "/api/cluster/resource-locks/acquire",
            "cluster",
            "Acquire or renew a PG-backed exclusive resource lock. Conflicting active holders return 409 with the current holder.",
        )
        .with_example(
            json!({
                "lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-compile",
                "ttl_secs": 900,
                "metadata": {"phase": "compile"}
            }),
            json!({
                "acquired": true,
                "lock": {
                    "lock_key": "unreal:project:CookingHeart",
                    "holder_instance_id": "mac-book-release",
                    "holder_job_id": "phase-compile"
                },
                "current": null
            }),
        )
        .with_error_example(
            409,
            json!({
                "lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-mini-release",
                "holder_job_id": "phase-compile"
            }),
            json!({"acquired": false, "lock": null, "current": {"holder_instance_id": "mac-book-release"}}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/resource-locks/acquire -H 'content-type: application/json' -d '{\"lock_key\":\"unreal:project:CookingHeart\",\"holder_instance_id\":\"mac-book-release\",\"holder_job_id\":\"phase-compile\"}'"),
        ep(
            "POST",
            "/api/cluster/resource-locks/heartbeat",
            "cluster",
            "Extend a resource lock only when the same holder still owns the lock.",
        )
        .with_example(
            json!({
                "lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-compile",
                "ttl_secs": 900
            }),
            json!({"ok": true, "lock": {"lock_key": "unreal:project:CookingHeart"}}),
        )
        .with_error_example(
            409,
            json!({}),
            json!({"ok": false, "error": "lock is not held by requester or has expired"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/resource-locks/heartbeat -H 'content-type: application/json' -d '{\"lock_key\":\"unreal:project:CookingHeart\",\"holder_instance_id\":\"mac-book-release\",\"holder_job_id\":\"phase-compile\"}'"),
        ep(
            "POST",
            "/api/cluster/resource-locks/release",
            "cluster",
            "Release a resource lock only when lock key, holder instance, and holder job all match.",
        )
        .with_example(
            json!({
                "lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-compile"
            }),
            json!({"released": true}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/resource-locks/release -H 'content-type: application/json' -d '{\"lock_key\":\"unreal:project:CookingHeart\",\"holder_instance_id\":\"mac-book-release\",\"holder_job_id\":\"phase-compile\"}'"),
        ep(
            "POST",
            "/api/cluster/resource-locks/reclaim-expired",
            "cluster",
            "Delete expired resource locks so crashed workers do not permanently hold exclusive resources.",
        )
        .with_example(json!({}), json!({"reclaimed": 1}))
        .with_curl("curl -X POST http://localhost:8787/api/cluster/resource-locks/reclaim-expired"),
        ep(
            "GET",
            "/api/cluster/test-phase-runs",
            "cluster",
            "List deterministic test phase evidence records by phase, head SHA, and status for multinode merge gates.",
        )
        .with_example(
            json!({"phase_key": "unreal-smoke", "head_sha": "abc123", "status": "passed"}),
            json!({
                "runs": [{
                    "id": "tpr-123",
                    "idempotency_key": "unreal-smoke:abc123",
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "passed",
                    "required_capabilities": {"labels": ["mac-book"], "unreal": true},
                    "resource_lock_key": "unreal:project:CookingHeart",
                    "evidence": {"log": "passed"},
                    "completed_at": "2026-05-01T06:30:00Z"
                }]
            }),
        )
        .with_error_example(
            503,
            json!({}),
            json!({"error": "postgres unavailable"}),
        )
        .with_curl("curl --get http://localhost:8787/api/cluster/test-phase-runs --data-urlencode phase_key=unreal-smoke --data-urlencode head_sha=abc123"),
        ep(
            "POST",
            "/api/cluster/test-phase-runs/upsert",
            "cluster",
            "Create or update the idempotent evidence row for one test phase and commit head SHA.",
        )
        .with_example(
            json!({
                "phase_key": "unreal-smoke",
                "head_sha": "abc123",
                "status": "passed",
                "issue_id": "881",
                "card_id": "card-881",
                "required_capabilities": {"labels": ["mac-book"], "unreal": true},
                "resource_lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-unreal-smoke-abc123",
                "evidence": {"runner": "deterministic-phase-runner", "result": "passed"}
            }),
            json!({
                "run": {
                    "idempotency_key": "unreal-smoke:abc123",
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "passed"
                }
            }),
        )
        .with_error_example(
            400,
            json!({"phase_key": "", "head_sha": "abc123"}),
            json!({"error": "phase_key is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/test-phase-runs/upsert -H 'content-type: application/json' -d '{\"phase_key\":\"unreal-smoke\",\"head_sha\":\"abc123\",\"status\":\"passed\"}'"),
        ep(
            "POST",
            "/api/cluster/test-phase-runs/start",
            "cluster",
            "Acquire the required resource lock and mark a deterministic test phase as running.",
        )
        .with_example(
            json!({
                "phase_key": "unreal-smoke",
                "head_sha": "abc123",
                "resource_lock_key": "unreal:project:CookingHeart",
                "holder_instance_id": "mac-book-release",
                "holder_job_id": "phase-unreal-smoke-abc123",
                "ttl_secs": 900,
                "required_capabilities": {"labels": ["mac-book"], "unreal": true}
            }),
            json!({
                "started": true,
                "run": {
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "running"
                },
                "lock": {"lock_key": "unreal:project:CookingHeart"},
                "current_lock": null
            }),
        )
        .with_error_example(
            409,
            json!({}),
            json!({"started": false, "run": null, "lock": null, "current_lock": {"holder_instance_id": "mac-mini-release"}}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/test-phase-runs/start -H 'content-type: application/json' -d '{\"phase_key\":\"unreal-smoke\",\"head_sha\":\"abc123\",\"resource_lock_key\":\"unreal:project:CookingHeart\",\"holder_instance_id\":\"mac-book-release\",\"holder_job_id\":\"phase-unreal-smoke-abc123\"}'"),
        ep(
            "POST",
            "/api/cluster/test-phase-runs/complete",
            "cluster",
            "Record terminal phase evidence and optionally release the resource lock held by the runner.",
        )
        .with_example(
            json!({
                "phase_key": "unreal-smoke",
                "head_sha": "abc123",
                "status": "passed",
                "release_lock": true,
                "evidence": {"result": "passed", "log_path": "Saved/Logs/phase.log"}
            }),
            json!({
                "run": {
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "passed"
                },
                "lock_released": true
            }),
        )
        .with_error_example(
            400,
            json!({"phase_key": "unreal-smoke", "head_sha": "abc123", "status": "running"}),
            json!({"error": "complete requires status passed, failed, or canceled"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/test-phase-runs/complete -H 'content-type: application/json' -d '{\"phase_key\":\"unreal-smoke\",\"head_sha\":\"abc123\",\"status\":\"passed\",\"release_lock\":true}'"),
        ep(
            "GET",
            "/api/cluster/test-phase-runs/evidence",
            "cluster",
            "Fetch the latest passing evidence for a required phase/head SHA pair. Merge gates should use this shape before accepting phase evidence.",
        )
        .with_example(
            json!({"phase_key": "unreal-smoke", "head_sha": "abc123"}),
            json!({
                "ok": true,
                "run": {
                    "phase_key": "unreal-smoke",
                    "head_sha": "abc123",
                    "status": "passed",
                    "evidence": {"result": "passed"}
                }
            }),
        )
        .with_error_example(
            404,
            json!({"phase_key": "unreal-smoke", "head_sha": "missing"}),
            json!({"ok": false, "error": "passing evidence not found"}),
        )
        .with_curl("curl --get http://localhost:8787/api/cluster/test-phase-runs/evidence --data-urlencode phase_key=unreal-smoke --data-urlencode head_sha=abc123"),
        ep(
            "POST",
            "/api/cluster/task-dispatches/claim",
            "cluster",
            "Atomically claim pending task_dispatches for a worker with PG row locking, capability-match diagnostics, and named semaphore routing constraints. Named semaphore acquire happens in this claim transaction after route owner selection; dispatch terminal statuses release holdings, and expired holdings are reclaimed before each claim.",
        )
        .with_example(
            json!({
                "claim_owner": "mac-book-release",
                "ttl_secs": 600,
                "limit": 10,
                "dispatch_type": "implementation"
            }),
            json!({
                "claimed": [{
                    "id": "dispatch-123",
                    "claim_owner": "mac-book-release",
                    "required_capabilities": {
                        "required": {
                            "labels": ["mac-book"],
                            "semaphores": ["ue_editor"]
                        }
                    }
                }],
                "skipped": [{
                    "id": "dispatch-456",
                    "reasons": ["semaphore 'ue_editor' exhausted for per-node:mac-mini-release (1/1 active)"]
                }]
            }),
        )
        .with_error_example(
            400,
            json!({"claim_owner": ""}),
            json!({"error": "claim_owner is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/task-dispatches/claim -H 'content-type: application/json' -d '{\"claim_owner\":\"mac-book-release\",\"limit\":10}'"),
        ep(
            "GET",
            "/api/cluster/issue-specs",
            "cluster",
            "List parsed Issue-as-Spec contracts, including required phases consumed by merge gates.",
        )
        .with_example(
            json!({"card_id": "card-881"}),
            json!({
                "specs": [{
                    "issue_id": "881",
                    "card_id": "card-881",
                    "required_phases": ["unreal-smoke"],
                    "validation_errors": []
                }]
            }),
        )
        .with_curl("curl --get http://localhost:8787/api/cluster/issue-specs --data-urlencode card_id=card-881"),
        ep(
            "POST",
            "/api/cluster/issue-specs/upsert",
            "cluster",
            "Parse a GitHub issue body into acceptance criteria, test plan, DoD, and required phase keys.",
        )
        .with_example(
            json!({
                "issue_id": "881",
                "card_id": "card-881",
                "repo_id": "itismyfield/AgentDesk",
                "issue_number": 881,
                "head_sha": "abc123",
                "body": "## Acceptance Criteria\n- Evidence is persisted\n\n## Test Plan\n- Run regression\n\n## Definition of Done\n- Gate consumes evidence\n\n## Required Phases\n- Unreal Smoke"
            }),
            json!({
                "spec": {
                    "issue_id": "881",
                    "required_phases": ["unreal-smoke"],
                    "validation_errors": []
                }
            }),
        )
        .with_error_example(
            400,
            json!({"issue_id": "", "body": ""}),
            json!({"error": "issue_id is required"}),
        )
        .with_curl("curl -X POST http://localhost:8787/api/cluster/issue-specs/upsert -H 'content-type: application/json' -d '{\"issue_id\":\"881\",\"body\":\"## Acceptance Criteria\\n- Done\\n\\n## Test Plan\\n- Test\\n\\n## Definition of Done\\n- Ship\"}'")
    ]
}
