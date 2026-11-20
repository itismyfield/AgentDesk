# Dashboard pipeline editor API

The editor uses the dashboard's existing authenticated `/api/server-campaigns` routes. Requests are JSON and share the server campaign 2 MiB body limit. The interactive contract is at `/api/docs/automation/campaigns`.

| Method and path under `/api/server-campaigns` | Request | Success response |
| --- | --- | --- |
| GET `/pipeline/condition-fields` | Query `kind`, optional `executor` and `executor_version` | `{vocabulary_version, fields}`. Each item supplies `field`, `type`, `operators`, `enum_values`, `source`, `subject_binding`, `freshness` and `available`. |
| GET `/pipeline/templates` | | `{templates: [{id, version, design_enabled, execution_readiness}]}` |
| GET `/pipeline/templates/{template_id}/{version}` | | `{template, execution_hash, pipeline}` |
| POST `/pipeline/validate` | `{pipeline}` | `{errors, warnings}` |
| GET `/{id}/pipeline` | | `{pipeline, definition_revision, execution_hash, source, editable, migration_available, execution}` |
| POST `/{id}/pipeline/validate` | `{pipeline}` | `{errors, warnings}`, including current occupied states |
| PUT `/{id}/pipeline` | `{idempotency_key, expected_definition_revision, pipeline}` | `{result}` with the stored definition revision, run ID, control version, event sequence and control state |
| POST `/{id}/pipeline/migrate` | `{idempotency_key, expected_definition_revision}` | Same command result as PUT |

## Validation and revisions

A validation request with a decoded graph returns HTTP 200. Each error supplies a code, a JSON Pointer relative to the graph and stable IDs when applicable.

```json
{
  "errors": [{
    "code": "TRANSITION_TARGET_MISSING",
    "path": "/transitions/0/to",
    "state_id": "missing-target",
    "transition_id": "finish"
  }],
  "warnings": []
}
```

The validator checks a single existing start, reachability, transition/timeout/event references, legal exits, bounded cycles, success and terminal reachability, and typed conditions for the source state and executor. A retry consumes a finite shared budget on its normal edge. Its exhausted edge remains in the graph for the unbounded-cycle check.

Writes repeat validation and occupied-state checks inside the campaign transaction. A campaign must be prepared or stopped and have no unresolved execution work. For an existing run, the check includes saved cursors and active or recoverable work bound to legacy stages. The full definition POST and PUT also validate supplied graphs. A full PUT that omits a previously saved pipeline or sends it as null returns `definition_schema_upgrade_required`.

| Status | Meaning |
| --- | --- |
| 400 | Invalid JSON envelope, unknown fields/state kinds or invalid path/query parameters |
| 404 | Campaign or template missing |
| 409 | Stale revision, ineligible control, occupied state or request key reused for different input |
| 422 | Invalid graph. Diagnostics are in `context.definition_error.pipeline_errors`. |
| 503 | PostgreSQL unavailable for a campaign-specific operation |

Retries with the same key and body receive the original receipt, including a recorded rejection. A corrected request uses a fresh key.

## Template and migration

The immutable `campaign-development-9` version 1 has nine business states and one failure end. Its IDs are requirements, design, design_review, implementation, review, landing, main_verification, final_verification, complete and failed. Design defaults to disabled. The template uses a separate one-retry design budget and a three-retry quality budget shared by implementation and review.

A legacy server campaign without a saved graph exposes the template as `source: "legacy_projection"`. Migration attaches a copy as a new definition revision for prepared campaigns without a run and stopped, fully settled campaigns. Successful or running campaigns can still read the projection. The older `/api/campaigns` ledger is a separate API.

`execution` has `mode: "legacy_stage"`, a nullable `run_id` and `state_ids` keyed by node ID. The IDs describe the legacy stage, with null for nodes in a campaign that has no run. Graph migration preserves the current executor, run pin, recorded attempts, actions, cursor and acceptance facts. Editing the saved graph does not enable execution. The saved graph hash identifies the graph and is distinct from an active run pin.
