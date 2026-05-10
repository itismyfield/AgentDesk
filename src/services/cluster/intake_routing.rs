//! Pure routing decision logic for intake-node-routing
//! (docs/design/intake-node-routing.md). Given a snapshot of
//! `worker_nodes` and the agent's `preferred_intake_node_labels`, picks a
//! `target_instance_id` to forward a Discord intake message to — or
//! returns `Local` if the leader should keep handling it.
//!
//! No DB access, no async, no Discord types: easy to unit-test exhaustively
//! and reason about under contention. Phase 4 wires this into the leader
//! intake hook.

use serde_json::Value;

/// Decision returned by `pick_intake_target`. The caller persists the
/// chosen target into `intake_outbox.target_instance_id` (when forwarded)
/// or runs the turn locally (when `Local`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRouteTarget {
    /// Run on the local leader. Either the agent has no preference
    /// (`preferred_intake_node_labels` empty), no eligible worker node
    /// matched, or the only eligible target is the leader itself.
    Local { reason: LocalRouteReason },
    /// Forward to the specified worker instance. The leader inserts a
    /// row into `intake_outbox` with `target_instance_id = instance_id`
    /// and the worker's poll loop claims it.
    Worker { instance_id: String },
}

/// Diagnostic enum explaining why `Local` was picked. Phase 4 records
/// this into the observability log so operators can spot mis-configured
/// label preferences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LocalRouteReason {
    /// Agent did not opt in (`preferred_intake_node_labels` empty).
    NoPreference,
    /// Preferences were set but no worker node had matching labels +
    /// online status. The intake stays on leader as a safety fallback.
    NoEligibleWorker,
    /// The only eligible target IS the leader itself (e.g., leader's
    /// own labels happen to match the preference).
    LeaderIsOnlyEligible,
}

/// Inputs needed by the routing decision. Decoupled from `worker_nodes`
/// JSON shape so the routing fn can be unit-tested without DB fixtures.
#[derive(Clone, Debug)]
pub(crate) struct CandidateNode {
    pub instance_id: String,
    pub labels: Vec<String>,
    /// "online" / "offline" / "stale" — strings used by the existing
    /// dispatch routing pipeline. Only "online" is eligible for intake
    /// forwarding (round-2 P0 #2 — never claim through offline nodes).
    pub status: String,
}

/// Build `CandidateNode` rows from the JSON snapshot returned by
/// `crate::server::cluster::list_worker_nodes`. Defensive against missing
/// fields — bad rows simply do not become candidates.
///
/// `list_worker_nodes` emits the staleness-corrected status under JSON
/// key `"status"` (the underlying SQL column is `computed_status`). We
/// read `"status"` first; the legacy `"computed_status"` key is also
/// accepted as a fallback for tests / non-canonical JSON producers.
pub(crate) fn candidates_from_worker_nodes_json(nodes: &[Value]) -> Vec<CandidateNode> {
    nodes
        .iter()
        .filter_map(|node| {
            let instance_id = node.get("instance_id")?.as_str()?.to_string();
            let status = node
                .get("status")
                .and_then(Value::as_str)
                .or_else(|| node.get("computed_status").and_then(Value::as_str))
                .unwrap_or("offline")
                .to_string();
            let labels = node
                .get("labels")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            Some(CandidateNode {
                instance_id,
                labels,
                status,
            })
        })
        .collect()
}

/// Picks the routing target for a Discord intake message.
///
/// Algorithm (matches the design doc §B routing rules):
/// 1. If `preferred_labels` is empty, return `Local { NoPreference }`.
/// 2. Filter candidates to those whose `status == "online"` AND whose
///    `labels` are a superset of `preferred_labels`.
/// 3. If `leader_instance_id` is present in the eligible set AND the
///    leader is the only eligible node, return
///    `Local { LeaderIsOnlyEligible }`.
/// 4. If no eligible candidates remain, return `Local { NoEligibleWorker }`.
/// 5. Otherwise, deterministically pick the eligible candidate (excluding
///    the leader) with the lexicographically smallest `instance_id`.
///    Deterministic ordering keeps the decision stable under retries
///    and during sweep races.
pub(crate) fn pick_intake_target(
    candidates: &[CandidateNode],
    preferred_labels: &[String],
    leader_instance_id: &str,
) -> IntakeRouteTarget {
    if preferred_labels.is_empty() {
        return IntakeRouteTarget::Local {
            reason: LocalRouteReason::NoPreference,
        };
    }

    let eligible: Vec<&CandidateNode> = candidates
        .iter()
        .filter(|c| c.status == "online" && labels_satisfy(&c.labels, preferred_labels))
        .collect();

    if eligible.is_empty() {
        return IntakeRouteTarget::Local {
            reason: LocalRouteReason::NoEligibleWorker,
        };
    }

    let non_leader: Vec<&&CandidateNode> = eligible
        .iter()
        .filter(|c| c.instance_id != leader_instance_id)
        .collect();

    if non_leader.is_empty() {
        return IntakeRouteTarget::Local {
            reason: LocalRouteReason::LeaderIsOnlyEligible,
        };
    }

    let chosen = non_leader
        .into_iter()
        .min_by(|a, b| a.instance_id.cmp(&b.instance_id))
        .expect("non_leader non-empty");

    IntakeRouteTarget::Worker {
        instance_id: chosen.instance_id.clone(),
    }
}

/// Returns true when every label in `required` is present in `available`.
/// Order-insensitive; duplicate entries on either side are tolerated.
fn labels_satisfy(available: &[String], required: &[String]) -> bool {
    required.iter().all(|r| available.iter().any(|a| a == r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(instance: &str, status: &str, labels: &[&str]) -> CandidateNode {
        CandidateNode {
            instance_id: instance.to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            status: status.to_string(),
        }
    }

    #[test]
    fn empty_preference_routes_local_with_no_preference_reason() {
        let nodes = vec![node("worker-1", "online", &["unreal"])];
        let result = pick_intake_target(&nodes, &[], "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Local {
                reason: LocalRouteReason::NoPreference
            }
        );
    }

    #[test]
    fn no_matching_worker_routes_local_with_no_eligible_reason() {
        let nodes = vec![
            node("worker-1", "online", &["api"]),
            node("worker-2", "offline", &["unreal"]),
        ];
        let preferred = vec!["unreal".to_string()];
        let result = pick_intake_target(&nodes, &preferred, "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Local {
                reason: LocalRouteReason::NoEligibleWorker
            }
        );
    }

    #[test]
    fn single_matching_online_worker_is_picked() {
        let nodes = vec![
            node("worker-1", "online", &["unreal", "macbook"]),
            node("leader-1", "online", &["mini"]),
        ];
        let preferred = vec!["unreal".to_string()];
        let result = pick_intake_target(&nodes, &preferred, "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Worker {
                instance_id: "worker-1".to_string()
            }
        );
    }

    #[test]
    fn multiple_eligible_workers_are_picked_deterministically() {
        let nodes = vec![
            node("worker-zeta", "online", &["unreal"]),
            node("worker-alpha", "online", &["unreal"]),
            node("worker-mid", "online", &["unreal"]),
        ];
        let preferred = vec!["unreal".to_string()];
        let result = pick_intake_target(&nodes, &preferred, "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Worker {
                instance_id: "worker-alpha".to_string()
            }
        );
    }

    #[test]
    fn offline_worker_is_excluded_even_when_labels_match() {
        let nodes = vec![
            node("worker-1", "offline", &["unreal"]),
            node("worker-2", "online", &["api"]),
        ];
        let preferred = vec!["unreal".to_string()];
        let result = pick_intake_target(&nodes, &preferred, "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Local {
                reason: LocalRouteReason::NoEligibleWorker
            }
        );
    }

    #[test]
    fn leader_only_eligible_returns_leader_is_only_eligible() {
        let nodes = vec![
            node("leader-1", "online", &["unreal"]),
            node("worker-1", "online", &["api"]),
        ];
        let preferred = vec!["unreal".to_string()];
        let result = pick_intake_target(&nodes, &preferred, "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Local {
                reason: LocalRouteReason::LeaderIsOnlyEligible
            }
        );
    }

    #[test]
    fn multi_label_requirement_needs_all_to_match() {
        let nodes = vec![
            node("worker-1", "online", &["unreal", "macbook"]),
            node("worker-2", "online", &["unreal"]),
        ];
        let preferred = vec!["unreal".to_string(), "macbook".to_string()];
        let result = pick_intake_target(&nodes, &preferred, "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Worker {
                instance_id: "worker-1".to_string()
            }
        );
    }

    #[test]
    fn candidates_from_worker_nodes_json_skips_malformed_rows() {
        // `list_worker_nodes` emits the staleness-corrected status under
        // the JSON key "status" (SQL alias was `computed_status` — we
        // assert both shapes work below).
        let json_nodes = vec![
            json!({"instance_id": "worker-1", "status": "online", "labels": ["unreal"]}),
            json!({"status": "online"}), // missing instance_id
            json!({"instance_id": "worker-2", "status": "offline", "labels": []}),
            json!({"instance_id": "worker-3", "labels": ["api"]}), // missing status defaults to offline
        ];
        let candidates = candidates_from_worker_nodes_json(&json_nodes);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].instance_id, "worker-1");
        assert_eq!(candidates[0].status, "online");
        assert_eq!(candidates[0].labels, vec!["unreal".to_string()]);
        assert_eq!(candidates[2].status, "offline");
    }

    #[test]
    fn candidates_from_worker_nodes_json_accepts_legacy_computed_status_key() {
        // Some test fixtures and non-canonical JSON producers still emit
        // the field as `computed_status`. The adapter falls back so we
        // do not silently classify them as offline.
        let json_nodes = vec![json!({
            "instance_id": "worker-legacy",
            "computed_status": "online",
            "labels": ["unreal"],
        })];
        let candidates = candidates_from_worker_nodes_json(&json_nodes);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].status, "online");
    }

    #[test]
    fn pick_intake_target_routes_to_worker_for_real_list_worker_nodes_shape() {
        // Regression for the round-1 codex blocker: ensure the JSON shape
        // emitted by `server::cluster::list_worker_nodes` (which uses the
        // key `"status"`, not `"computed_status"`) actually drives the
        // routing decision instead of falling through to NoEligibleWorker.
        let json_nodes = vec![
            json!({
                "instance_id": "leader-1",
                "status": "online",
                "labels": ["mini"],
            }),
            json!({
                "instance_id": "worker-mac-book",
                "status": "online",
                "labels": ["unreal", "macbook"],
            }),
        ];
        let candidates = candidates_from_worker_nodes_json(&json_nodes);
        let preferred = vec!["unreal".to_string()];
        let result = pick_intake_target(&candidates, &preferred, "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Worker {
                instance_id: "worker-mac-book".to_string()
            }
        );
    }

    #[test]
    fn worker_with_extra_labels_still_satisfies_subset_requirement() {
        let nodes = vec![node(
            "worker-1",
            "online",
            &["unreal", "macbook", "gpu", "metal"],
        )];
        let preferred = vec!["unreal".to_string()];
        let result = pick_intake_target(&nodes, &preferred, "leader-1");
        assert_eq!(
            result,
            IntakeRouteTarget::Worker {
                instance_id: "worker-1".to_string()
            }
        );
    }
}
