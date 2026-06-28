use std::collections::BTreeSet;

use serde_json::Value;

use super::types::{
    DispatchTerminal, EntrySnapshot, EntryTerminal, PreflightReport, PreflightSnapshot, SlotId,
    TerminalStatus,
};

pub(crate) fn apply_snapshot_to_report(report: &mut PreflightReport, snapshot: &PreflightSnapshot) {
    report.slot_ids = observed_slot_ids(snapshot);
    report.phase_gate_state = snapshot.phase_gates.clone();
    report.terminal_status = TerminalStatus {
        run_status: snapshot.run_status.clone(),
        entries: snapshot
            .entries
            .iter()
            .map(|entry| EntryTerminal {
                id: entry.id.clone(),
                status: entry.status.clone(),
                dispatch_id: entry.dispatch_id.clone(),
                slot_index: entry.slot_index,
            })
            .collect(),
        dispatches: snapshot
            .dispatches
            .iter()
            .map(|dispatch| DispatchTerminal {
                id: dispatch.id.clone(),
                status: dispatch.status.clone(),
            })
            .collect(),
    };
    report.safety = snapshot.safety.clone();
}

pub(crate) fn validate_preflight_snapshot(snapshot: &PreflightSnapshot) -> Vec<String> {
    let mut failures = Vec::new();

    for dispatch in &snapshot.dispatches {
        if dispatch.status != "completed" {
            continue;
        }
        let matching_entries: Vec<&EntrySnapshot> = snapshot
            .entries
            .iter()
            .filter(|entry| entry.dispatch_id.as_deref() == Some(dispatch.id.as_str()))
            .collect();
        if matching_entries.is_empty() {
            failures.push(format!(
                "split-brain: task_dispatches.status=completed for {} but no matching auto_queue_entries row points at it",
                dispatch.id
            ));
            continue;
        }
        for entry in matching_entries {
            if entry.status != "done" {
                failures.push(format!(
                    "split-brain: task_dispatches.status=completed for {} but auto_queue_entries {} stayed {}",
                    dispatch.id, entry.id, entry.status
                ));
            }
        }
        if snapshot.run_status.as_deref() != Some("completed") {
            failures.push(format!(
                "split-brain: task_dispatches.status=completed for {} but auto_queue_runs {:?} stayed {:?}",
                dispatch.id, snapshot.run_id, snapshot.run_status
            ));
        }
    }

    for slot in &snapshot.reserved_slots {
        failures.push(format!(
            "slot remains reserved after terminal preflight: agent={} slot={}",
            slot.agent_id, slot.slot_index
        ));
    }
    for entry in &snapshot.entries {
        if entry.status == "dispatched" {
            failures.push(format!(
                "entry stuck dispatched after terminal preflight: {} dispatch={:?}",
                entry.id, entry.dispatch_id
            ));
        }
    }
    for gate in &snapshot.phase_gates {
        let status = gate.get("status").and_then(Value::as_str).unwrap_or("");
        if matches!(status, "pending" | "failed" | "blocked")
            && !phase_gate_has_visible_reason(gate)
        {
            failures.push(format!(
                "phase gate blocks without visible reason or correlation: {gate}"
            ));
        }
    }
    for diagnostics in &snapshot.diagnostics {
        failures.extend(validate_diagnostics_have_correlation_ids(diagnostics));
    }

    let safety = &snapshot.safety;
    if safety.production_card_count != 0 {
        failures.push(format!(
            "sandbox preflight touched non-fixture kanban cards: {}",
            safety.production_card_count
        ));
    }
    if safety.github_pr_tracking_count != 0 {
        failures.push(format!(
            "sandbox preflight created PR/branch tracking rows: {}",
            safety.github_pr_tracking_count
        ));
    }
    if safety.live_session_count != 0 {
        failures.push(format!(
            "sandbox preflight left live sessions: {}",
            safety.live_session_count
        ));
    }
    if safety.dispatch_delivery_sent_count != 0 {
        failures.push(format!(
            "sandbox preflight sent dispatch delivery events: {}",
            safety.dispatch_delivery_sent_count
        ));
    }
    if safety.message_outbox_count != 0 {
        failures.push(format!(
            "sandbox preflight wrote dispatch channel messages: {}",
            safety.message_outbox_count
        ));
    }
    if safety.dispatch_outbox_notify_count != 0 {
        failures.push(format!(
            "sandbox preflight enqueued dispatch notifications: {}",
            safety.dispatch_outbox_notify_count
        ));
    }
    if safety.worktree_or_branch_context_count != 0 {
        failures.push(format!(
            "sandbox preflight recorded worktree/branch context: {}",
            safety.worktree_or_branch_context_count
        ));
    }

    failures
}

pub(crate) fn validate_history_contains_run(history: &Value, run_id: &str) -> Vec<String> {
    let contains_run = history
        .get("runs")
        .and_then(Value::as_array)
        .is_some_and(|runs| {
            runs.iter()
                .any(|run| run.get("id").and_then(Value::as_str) == Some(run_id))
        });
    if contains_run {
        Vec::new()
    } else {
        vec![format!(
            "/api/queue/history did not include terminal preflight run {run_id}: {history}"
        )]
    }
}

fn observed_slot_ids(snapshot: &PreflightSnapshot) -> Vec<SlotId> {
    let mut slots = snapshot.reserved_slots.clone();
    let mut seen: BTreeSet<(String, i64)> = slots
        .iter()
        .map(|slot| (slot.agent_id.clone(), slot.slot_index))
        .collect();
    for entry in &snapshot.entries {
        if let Some(slot_index) = entry.slot_index {
            let agent_id = "entry-bound-slot".to_string();
            if seen.insert((agent_id.clone(), slot_index)) {
                slots.push(SlotId {
                    agent_id,
                    slot_index,
                });
            }
        }
    }
    slots
}

fn phase_gate_has_visible_reason(gate: &Value) -> bool {
    ["failure_reason", "dispatch_id", "verdict"]
        .iter()
        .any(|key| non_empty_json_string(gate.get(*key)))
}

fn validate_diagnostics_have_correlation_ids(diagnostics: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for diagnostic in diagnostics
        .get("slot_invariant_violations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !non_empty_json_string(diagnostic.get("run_id"))
            || diagnostic
                .get("entry_ids")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
            || diagnostic
                .get("dispatch_ids")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
        {
            failures.push(format!(
                "slot invariant diagnostic omits correlation ids: {diagnostic}"
            ));
        }
    }
    for diagnostic in diagnostics
        .get("entry_dispatch_delivery_mismatches")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let has_ids = non_empty_json_string(diagnostic.get("run_id"))
            && non_empty_json_string(diagnostic.get("entry_id"))
            && non_empty_json_string(diagnostic.get("dispatch_id"))
            && non_empty_json_string(diagnostic.get("card_id"))
            && diagnostic.get("slot_index").is_some();
        if !has_ids {
            failures.push(format!(
                "delivery mismatch diagnostic omits correlation ids: {diagnostic}"
            ));
        }
    }
    for diagnostic in diagnostics
        .get("run_timeout_overruns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !non_empty_json_string(diagnostic.get("run_id")) {
            failures.push(format!(
                "timeout diagnostic omits run correlation id: {diagnostic}"
            ));
        }
    }
    failures
}

fn non_empty_json_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}
