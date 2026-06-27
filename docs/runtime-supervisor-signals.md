# Runtime Supervisor Signals

Follow-up status for #210/#398: Rust owns the runtime supervisor decision log and
orphan recovery action path, but the broad signal surface is intentionally
partial. Signals that do not have Rust recovery semantics are accepted only as
explicit audit-only escalations.

| Signal | Support state | Emit requirement | Runtime action |
| --- | --- | --- | --- |
| `OrphanCandidate` | `implemented_action` | normal evidence with `dispatch_id` | probes confirmed orphan dispatches and may roll them back to a dispatchable card state |
| `DeadlockCandidate` | `audit_only` | evidence must include `supervisor_audit_only: true` | records `Escalate` with `executed=false`; no recovery action |
| `ResumeCandidate` | `audit_only` | evidence must include `supervisor_audit_only: true` | records `Escalate` with `executed=false`; no recovery action |
| `StaleInflight` | `audit_only` | evidence must include `supervisor_audit_only: true` | records `Escalate` with `executed=false`; no recovery action |

`runtime_decisions.evidence_json` includes these supervisor-owned fields:

- `supervisor_signal_support`: `implemented_action` or `audit_only`
- `supervisor_chosen_action`: the stored action name, such as `Escalate`
- `supervisor_action_executed`: `true` only when Rust actually executed a
  recovery action

Unknown or reserved signal names are rejected at the public bridge boundary.
