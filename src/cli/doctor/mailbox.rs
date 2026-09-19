use serde_json::{Value, json};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MailboxFinding {
    pub(crate) id: &'static str,
    pub(crate) detail: String,
    pub(crate) evidence: Value,
    pub(crate) live_work_present: bool,
}

fn value_bool(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn value_usize(value: &Value, key: &str) -> usize {
    value.get(key).and_then(Value::as_u64).unwrap_or(0) as usize
}

/// #5071 relay-tail S1 (I-4): the frontier provenance the health entry
/// publishes, read out by name for the doctor's evidence.
///
/// Design §2.3 routes the two witnesses to `cli/doctor` and the r1 review
/// (legA P1-2) measured that no CLI code named them — the whole mailbox
/// snapshot was already being embedded verbatim, so the fields were present and
/// unread, which is the same thing as absent for anyone reading doctor output.
/// This names them.
///
/// DISPLAY ONLY. Nothing here classifies: no finding is produced from these
/// values, no finding is suppressed by them, and no `fix_safety` or severity
/// consults them. `Value::Null` per field when the entry predates it, so an
/// older dcserver reports "not reported" rather than a fabricated reading.
pub(crate) fn frontier_provenance_evidence(snapshot: &Value) -> Value {
    let provenance = snapshot.get("frontier_provenance");
    let field = |key: &str| {
        provenance
            .and_then(|value| value.get(key))
            .cloned()
            .unwrap_or(Value::Null)
    };
    json!({
        "coord_observation": field("coord_observation"),
        "durable_observation": field("durable_observation"),
        "hypothesis": field("hypothesis"),
    })
}

pub(crate) fn classify_mailbox_snapshot(snapshot: &Value) -> Option<MailboxFinding> {
    let channel_id = snapshot.get("channel_id").and_then(Value::as_u64);
    let has_cancel_token = value_bool(snapshot, "has_cancel_token");
    let queue_depth = value_usize(snapshot, "queue_depth");
    let watcher_attached = value_bool(snapshot, "watcher_attached");
    let inflight_state_present = value_bool(snapshot, "inflight_state_present");
    let tmux_present = value_bool(snapshot, "tmux_present");
    let process_present = value_bool(snapshot, "process_present");
    let session_active_dispatch_present = snapshot
        .get("session_active_dispatch_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    let active_dispatch_present =
        value_bool(snapshot, "active_dispatch_present") || session_active_dispatch_present;
    let session_record_present = value_bool(snapshot, "session_record_present");
    let session_status = snapshot
        .get("session_status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let agent_turn_status = snapshot
        .get("agent_turn_status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    // #5996 S3a: depth is not evidence of live work. A queue stuck behind a dead
    // turn reads identically to one a live turn is draining, so this term
    // suppressed the finding for exactly the shape that needed it. `tmux_present`
    // stays: the route's graded tmux gate is the decider for that shape and is
    // not wired yet, so widening into it here would pre-empt the route.
    let live_work_present = tmux_present || process_present || active_dispatch_present;

    if has_cancel_token && !live_work_present {
        return Some(MailboxFinding {
            id: "mailbox_busy_without_active_turn",
            detail: format!(
                "channel {} has mailbox cancel token without live tmux/process/dispatch evidence",
                channel_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            evidence: json!({
                "mailbox": snapshot,
                // #5071 relay-tail S1 (I-4): read-only, decides nothing.
                "frontier_provenance": frontier_provenance_evidence(snapshot),
                "turn_state_sources": {
                    "agent_turn_status": agent_turn_status,
                    "queue_depth": queue_depth,
                    "tmux_present": tmux_present,
                    "process_present": process_present,
                    "watcher_attached": watcher_attached,
                    "inflight_state_present": inflight_state_present,
                    "active_dispatch_present": active_dispatch_present
                },
                "session": {
                    "record_present": session_record_present,
                    "status": session_status,
                    "active_dispatch_present": session_active_dispatch_present
                }
            }),
            live_work_present,
        });
    }

    // #5996 S3b: `health/mailbox.rs::mailbox_agent_turn_status` is four-valued —
    // "active" from a bare cancel token, "residual"/"residual_held" for a
    // finished turn still holding something, "idle" only otherwise — and reads
    // "unknown" here on a dcserver predating the field. Demanding "idle" thus
    // declined on the anchor's mere existence, on every residual state, and on
    // every older server. Dropping the term only widens the candidate set; the
    // route still decides. What is left in the three arms below is structural.
    if queue_depth == 0 && !watcher_attached && inflight_state_present {
        return Some(MailboxFinding {
            id: "stale_watcher_inflight_without_active_turn",
            detail: format!(
                "channel {} has inflight watcher state with no watcher attached and an empty queue",
                channel_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            evidence: json!({
                "mailbox": snapshot,
                // #5071 relay-tail S1 (I-4): read-only, decides nothing.
                "frontier_provenance": frontier_provenance_evidence(snapshot),
                "turn_state_sources": {
                    "agent_turn_status": agent_turn_status,
                    "queue_depth": queue_depth,
                    "tmux_present": tmux_present,
                    "process_present": process_present,
                    "watcher_attached": watcher_attached,
                    "inflight_state_present": inflight_state_present,
                    "active_dispatch_present": active_dispatch_present
                },
                "session": {
                    "record_present": session_record_present,
                    "status": session_status,
                    "active_dispatch_present": session_active_dispatch_present
                }
            }),
            live_work_present,
        });
    }

    if queue_depth == 0
        && session_record_present
        && matches!(session_status, "turn_active" | "working")
        && !tmux_present
        && !active_dispatch_present
    {
        return Some(MailboxFinding {
            id: "tmux_missing_with_session_record",
            detail: format!(
                "channel {} has a working session record but no live tmux/process evidence",
                channel_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            evidence: json!({
                "mailbox": snapshot,
                // #5071 relay-tail S1 (I-4): read-only, decides nothing.
                "frontier_provenance": frontier_provenance_evidence(snapshot),
                "turn_state_sources": {
                    "agent_turn_status": agent_turn_status,
                    "queue_depth": queue_depth,
                    "tmux_present": tmux_present,
                    "process_present": process_present,
                    "watcher_attached": watcher_attached,
                    "inflight_state_present": inflight_state_present,
                    "active_dispatch_present": active_dispatch_present,
                    "session_status": session_status,
                    "session_record_present": session_record_present
                }
            }),
            live_work_present,
        });
    }

    // `tmux_present` keeps `live_work_present` true here, so the orchestrator
    // reports this arm as SKIPPED rather than posting. That is still the repair:
    // doctor names the stuck channel instead of staying silent. Carrying a
    // tmux-present channel to the route is the graded gate's job, not the CLI's.
    if tmux_present && !watcher_attached && inflight_state_present {
        return Some(MailboxFinding {
            id: "completed_output_not_relayed",
            detail: format!(
                "channel {} has a tmux session and stale inflight state but no active watcher",
                channel_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            evidence: json!({
                "mailbox": snapshot,
                // #5071 relay-tail S1 (I-4): read-only, decides nothing.
                "frontier_provenance": frontier_provenance_evidence(snapshot),
                "turn_state_sources": {
                    "agent_turn_status": agent_turn_status,
                    "queue_depth": queue_depth,
                    "tmux_present": tmux_present,
                    "process_present": process_present,
                    "watcher_attached": watcher_attached,
                    "inflight_state_present": inflight_state_present,
                    "active_dispatch_present": active_dispatch_present,
                    "session_status": session_status,
                    "session_record_present": session_record_present
                },
                "delivery_completed": false,
                "rebind_spawned": snapshot
                    .get("rebind_spawned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            }),
            live_work_present,
        });
    }

    None
}

pub(crate) fn classify_mailbox_findings(body: &Value) -> Vec<MailboxFinding> {
    let mut findings = body
        .get("mailboxes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(classify_mailbox_snapshot)
        .collect::<Vec<_>>();

    let global_active = value_usize(body, "global_active");
    let actual_active_turns = body
        .get("mailboxes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|snapshot| {
            value_bool(snapshot, "has_cancel_token")
                || snapshot.get("agent_turn_status").and_then(Value::as_str) == Some("active")
        })
        .count();
    if global_active > actual_active_turns {
        findings.push(MailboxFinding {
            id: "global_active_without_active_turn",
            detail: format!(
                "global_active={} exceeds actual active mailbox turns={}",
                global_active, actual_active_turns
            ),
            evidence: json!({
                "turn_state_sources": {
                    "global_active": global_active,
                    "actual_active_turns": actual_active_turns
                }
            }),
            live_work_present: true,
        });
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mailbox entry as `MailboxHealthSnapshot` serializes it, trimmed to the
    /// keys these predicates read plus the provenance block S1 publishes.
    fn mailbox_with_provenance(frontier_provenance: Value) -> Value {
        json!({
            "channel_id": 5_071_000_000_000_042u64,
            "has_cancel_token": true,
            "queue_depth": 0,
            "agent_turn_status": "active",
            "watcher_attached": false,
            "inflight_state_present": false,
            "tmux_present": false,
            "process_present": false,
            "active_dispatch_present": false,
            "frontier_provenance": frontier_provenance,
        })
    }

    fn e2_provenance() -> Value {
        json!({
            "coord_observation": {"kind": "absent"},
            "durable_observation": {
                "kind": "generation_unresolved",
                "relayed_start": 4_096,
                "row_generation_ns": 7,
                "live_generation_ns": Value::Null,
            },
            "counterpart_coord_observation": Value::Null,
            "hypothesis": "coord_entry_absent_with_durable_row",
        })
    }

    /// #5071 relay-tail S1 (I-4), design §2.3's "어디서" clause: the two
    /// witnesses and the hypothesis reach `cli/doctor`. Before this the CLI
    /// named none of them (r1 review, legA P1-2).
    #[test]
    fn doctor_evidence_carries_both_frontier_witnesses_and_the_hypothesis() {
        let finding = classify_mailbox_snapshot(&mailbox_with_provenance(e2_provenance()))
            .expect("a cancel token without live work is an existing finding");
        let provenance = &finding.evidence["frontier_provenance"];

        assert_eq!(provenance["coord_observation"]["kind"], "absent");
        assert_eq!(
            provenance["durable_observation"]["kind"],
            "generation_unresolved"
        );
        assert_eq!(provenance["durable_observation"]["relayed_start"], 4_096);
        assert_eq!(
            provenance["hypothesis"],
            "coord_entry_absent_with_durable_row"
        );
    }

    /// An entry that predates the field reports the witnesses as unreported —
    /// never as a reading the poll did not make.
    #[test]
    fn doctor_evidence_reports_missing_provenance_as_null() {
        let mut snapshot = mailbox_with_provenance(e2_provenance());
        snapshot
            .as_object_mut()
            .expect("object")
            .remove("frontier_provenance");
        let finding = classify_mailbox_snapshot(&snapshot)
            .expect("the finding does not depend on provenance");

        assert!(finding.evidence["frontier_provenance"]["coord_observation"].is_null());
        assert!(finding.evidence["frontier_provenance"]["durable_observation"].is_null());
        assert!(finding.evidence["frontier_provenance"]["hypothesis"].is_null());
    }

    /// The wiring is display only: which finding fires, whether it fires at
    /// all, and its `live_work_present` gate are identical with and without the
    /// provenance block, and identical across two provenances that name
    /// opposite hypotheses.
    #[test]
    fn frontier_provenance_changes_no_doctor_verdict() {
        let healthy_provenance = json!({
            "coord_observation": {"kind": "advanced", "offset": 8_192},
            "durable_observation": {"kind": "row_absent"},
            "counterpart_coord_observation": Value::Null,
            "hypothesis": "indeterminate",
        });
        let mut without = mailbox_with_provenance(e2_provenance());
        without
            .as_object_mut()
            .expect("object")
            .remove("frontier_provenance");

        let verdicts = [
            classify_mailbox_snapshot(&mailbox_with_provenance(e2_provenance())),
            classify_mailbox_snapshot(&mailbox_with_provenance(healthy_provenance)),
            classify_mailbox_snapshot(&without),
        ]
        .map(|finding| finding.map(|f| (f.id, f.live_work_present)));

        assert_eq!(
            verdicts[0],
            Some(("mailbox_busy_without_active_turn", false))
        );
        assert_eq!(verdicts[0], verdicts[1]);
        assert_eq!(verdicts[0], verdicts[2]);
    }

    /// #5996 S3a: the reported shape — the mailbox still anchors a turn and the
    /// queue is not empty, with nothing else live. `queue_depth` alone used to
    /// read as live work, so `classify_mailbox_snapshot` returned `None` and
    /// `agentdesk doctor` said nothing at all about the wedged channel.
    #[test]
    fn queue_depth_alone_no_longer_suppresses_the_busy_mailbox_finding() {
        let mut snapshot = mailbox_with_provenance(e2_provenance());
        snapshot["queue_depth"] = json!(3);

        let finding = classify_mailbox_snapshot(&snapshot)
            .expect("a queued channel whose turn may be dead is a finding, not silence");

        assert_eq!(finding.id, "mailbox_busy_without_active_turn");
        // False is what carries the candidate to the route. The route decides.
        assert!(!finding.live_work_present);
    }

    /// The subtraction is bounded to `queue_depth`. Each structural term still
    /// suppresses on its own, so the CLI does not widen into the shapes the
    /// route's own gates still own.
    #[test]
    fn live_tmux_process_or_dispatch_still_suppresses_the_busy_mailbox_finding() {
        for key in ["tmux_present", "process_present", "active_dispatch_present"] {
            let mut snapshot = mailbox_with_provenance(e2_provenance());
            snapshot["queue_depth"] = json!(3);
            snapshot[key] = json!(true);

            if let Some(finding) = classify_mailbox_snapshot(&snapshot) {
                assert!(
                    finding.live_work_present,
                    "{key} must still count as live work, got finding {}",
                    finding.id
                );
            }
        }
    }

    /// #5996 S3b, and what it does NOT buy.
    ///
    /// Every fixture here is one a dcserver can actually publish, which
    /// constrains them: `health/snapshot.rs` derives `has_cancel_token` and
    /// `agent_turn_status` from one binding, and `residual_occupancy` reaches
    /// `matches_observed_owner`, which requires the token. So "active",
    /// "residual" and "residual_held" each imply a held token, and "idle" is the
    /// only value that does not. A fixture pairing a non-idle status with an
    /// absent token would be a shape no server emits, and an assertion on it
    /// would restate the predicate instead of pinning behaviour.
    ///
    /// The consequence is the point. Holding the token keeps the first arm from
    /// returning only when `live_work_present` is true, so on a current server
    /// these findings are REPORTED, not posted: the repair set grows by nothing
    /// and doctor stops being silent. The one place a candidate is really
    /// carried to the route is a server old enough not to publish the field at
    /// all, where the retired precondition compared its "unknown" default
    /// against "idle".
    #[test]
    fn a_non_idle_agent_turn_status_no_longer_suppresses_the_shape_findings() {
        fn verdict(mutate: impl FnOnce(&mut Value)) -> (&'static str, bool) {
            let mut snapshot = mailbox_with_provenance(e2_provenance());
            mutate(&mut snapshot);
            let finding =
                classify_mailbox_snapshot(&snapshot).expect("a named finding, not silence");
            (finding.id, finding.live_work_present)
        }

        // Token held (so the status is reachable), live evidence elsewhere.
        for status in ["active", "residual_held"] {
            assert_eq!(
                verdict(|snapshot| {
                    snapshot["agent_turn_status"] = json!(status);
                    snapshot["process_present"] = json!(true);
                    snapshot["inflight_state_present"] = json!(true);
                }),
                ("stale_watcher_inflight_without_active_turn", true),
                "{status} must reach the second arm and report as live work"
            );
        }

        // The queue is what keeps this out of the arm above; the tmux half of
        // the reported shape. Also report-only.
        assert_eq!(
            verdict(|snapshot| {
                snapshot["agent_turn_status"] = json!("active");
                snapshot["queue_depth"] = json!(3);
                snapshot["tmux_present"] = json!(true);
                snapshot["inflight_state_present"] = json!(true);
            }),
            ("completed_output_not_relayed", true)
        );

        // A dcserver predating the field publishes no `agent_turn_status` key.
        // This is the only arm whose candidate actually reaches the route, so
        // `live_work_present` is false here.
        assert_eq!(
            verdict(|snapshot| {
                snapshot
                    .as_object_mut()
                    .expect("object")
                    .remove("agent_turn_status");
                snapshot["has_cancel_token"] = json!(false);
                snapshot["session_record_present"] = json!(true);
                snapshot["session_status"] = json!("working");
            }),
            ("tmux_missing_with_session_record", false)
        );
    }

    /// Pins the production call site these predicates hang from:
    /// `classify_mailbox_findings` is what `apply_stale_mailbox_fixes` and
    /// `check_mailbox_consistency` call, and it reaches the per-mailbox verdict
    /// through one `filter_map`. Deleting that reaches this test.
    ///
    /// It does not reach the two call sites above it, which need a live
    /// `HealthSnapshot` and an HTTP client; that wiring predates this change and
    /// stays unpinned.
    #[test]
    fn classify_mailbox_findings_carries_the_per_mailbox_verdict() {
        let mut wedged = mailbox_with_provenance(e2_provenance());
        wedged["queue_depth"] = json!(3);
        let body = json!({ "mailboxes": [wedged], "global_active": 0 });

        let ids = classify_mailbox_findings(&body)
            .iter()
            .map(|finding| finding.id)
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["mailbox_busy_without_active_turn"]);
    }

    /// The one prohibition on this slice. The CLI identifies candidates; the
    /// route decides. `unread_bytes` is the tail term the route's liveness gate
    /// is being rebuilt around, so no verdict here may move with it — a CLI that
    /// judged progress for itself would be a second decider, which is the
    /// category error #5996 reports rather than a repair of it.
    #[test]
    fn no_unread_progress_field_changes_a_doctor_verdict() {
        let verdict_with = |unread: Value| {
            let mut snapshot = mailbox_with_provenance(e2_provenance());
            snapshot["queue_depth"] = json!(3);
            snapshot["unread_bytes"] = unread;
            classify_mailbox_snapshot(&snapshot)
                .map(|finding| (finding.id, finding.live_work_present))
        };

        let drained = verdict_with(json!(0));
        assert_eq!(drained, Some(("mailbox_busy_without_active_turn", false)));
        assert_eq!(drained, verdict_with(json!(8_192)));
        assert_eq!(drained, verdict_with(Value::Null));
    }
}
