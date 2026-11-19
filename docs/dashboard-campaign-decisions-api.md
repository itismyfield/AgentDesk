# Dashboard campaign decisions API

The full `GET /api/server-campaigns` and `GET /api/server-campaigns/{id}` responses
include these fields in `view.nodes[].runtime` alongside the saved runtime.

| Field | JSON type | Source |
| --- | --- | --- |
| `pending_gate` | object or null | `{gate, binding}` for the scheduler's next gate. `gate` is `start`, `implementation`, `review`, `landing`, `merge`, or `deploy`. `binding` is the current `{run_id,node_id,gate,definition_revision,target,policy_hash}` when opened, or null while the gate is still being set up. |
| `wait_started_at` | RFC 3339 string or null | Observed time of the latest opening event at or before the view sequence for that exact binding. Null when the matching event is missing or the gate is unopened. |
| `evidence_url` | string or null | Reserved for a decision request permalink. Currently null. |
| `reminder_interval_seconds` | integer or null | Reserved for the effective reminder cadence. Currently null. |
| `human_status` | string or null | Current dashboard status, described below. |

- `decision_required` (판단 필요) means the scheduler requires a gate and proposes opening it.
- `awaiting_approval` (승인 대기) means the scheduler is waiting for the current open gate's approval.
- `awaiting_deploy` (배포 대기) means a deploy action is planned or claimed and has a current binding on a node awaiting an attempt or evidence.
- `awaiting_start` (착수 대기) means the scheduler proposes assigning work. The view's existing readiness and command guards apply.
- Null means the snapshot supplies none of these statuses. `view=summary` omits nodes.

## Recording an approval

Using the dashboard, the coordinator verifies an explicit approval in the
Decision forum and copies the following fields from that decision and response.

| Evidence field | Value |
| --- | --- |
| `decision_ref` | Stable ID or slug of the decision post, copied exactly as recorded. It must contain a non-whitespace character. |
| `response_ref` | Canonical HTTPS permalink to the specific approving response, including its message or comment ID where applicable. Use a link without credentials or access tokens. The server checks for an HTTPS scheme and a host. |
| `response_at` | The response's actual timestamp as an RFC 3339 string. |

If any of those inputs is unavailable, the dashboard keeps approval disabled.
On submission to `POST /api/server-campaigns/{id}/commands`, set
`command.command` to `approve` and copy the open gate's `binding` into the
command. Map `run_id` to `expected_run` and `definition_revision` to
`expected_definition_revision`. Send the evidence under `command.evidence`.
Use the same idempotency key and payload for retries of the same submission.
On a stale binding, reload the view and verify the response against the current
gate before submitting again.

The server checks the binding against its stored current gate and records the
submission as `coordinator_recorded_forum`. The coordinator verifies who
responded and what they approved. Example read and write schemas are at
`GET /api/docs/automation/campaigns`.
