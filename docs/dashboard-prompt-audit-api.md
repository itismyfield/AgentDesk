# Dashboard prompt audit and session anchors

The dashboard uses the existing API authentication for these read-only endpoints. Both successful responses set `Cache-Control: no-store`. Their examples appear at `GET /api/docs/agents/agents`.

Both endpoints take a registered agent ID and an optional `provider=claude|codex` query parameter. Omitting the provider selects the agent's primary binding. An unknown agent returns 404, an unavailable binding returns a 4xx response, and an unavailable PostgreSQL pool returns 503.

## GET /api/agents/{id}/prompt-audit

| Field | Type | Meaning |
| --- | --- | --- |
| `agent_id`, `provider` | string | Selected agent and provider. |
| `session_id` | string or null | Current or saved provider-session ID, when available. |
| `origin` | enum | `current_session` for a captured current session, `latest_saved` for the newest scoped capture when there is no current binding, or `canon_reassembled` when the selected capture is missing. |
| `session_started_at` | RFC 3339 timestamp or null | Observed launch time of the saved session. |
| `canon_modified_at` | RFC 3339 timestamp or null | Most recent modification time among available canonical prompt files. |
| `masked_count` | nonnegative integer | Total credential matches across all sections. |
| `sections[]` | array | Sections in launch-text order, or configured canon sections when reassembled. |

Each section has `name`, `source` (`canon_file`, `memento`, `runtime`, or `conditional`), a nullable logical `path`, `chars` (Unicode scalar count of the displayed text), `text`, and `masked_count`. The session-start manifest identifies sections whose full text matches the captured launch text. Unattributed text is grouped into runtime sections. Reassembly uses the current shared and provider-role prompt files as a canon preview.

Credential patterns cover key, token, secret, password and authorization assignments, credential command arguments and folded headers, YAML values with indented continuation lines, sensitive URL query values, PEM private keys, and recognized standalone access tokens. A truncated PEM key is masked through the end of the input. Each matching item contributes one to the count and is replaced with `***` in displayed text.

## GET /api/agents/{id}/anchors

| Field | Type | Meaning |
| --- | --- | --- |
| `agent_id`, `provider` | string | Selected agent and provider. |
| `session_id` | string or null | Selected provider-session identity. |
| `session_started_at` | RFC 3339 timestamp or null | Observed launch time of the saved session. |
| `source` | enum | `current_session`, `latest_saved`, or `unavailable`. |
| `memento` | object | `status` is `connected`, `disconnected`, `unknown`, or `unconfigured`. `checked_at` is the timestamp of the last recorded probe or null. |
| `anchor_count` | integer | Recorded number of injected anchors. |
| `anchors[]` | array | Copy of the anchors captured during session launch. |

Each anchor has nullable `id`, `type`, `scope`, and `limit` fields, plus `chars`, `text`, `masked_count`, `overlap_candidates[]`, and `receiving_agents[]`. The limit is the upstream record's character limit when available. The receiving-agent list contains the selected agent. Overlap candidates identify a zero-based anchor index (`{"source":"anchor","index":0}`) or logical canonical path (`{"source":"canon_file","path":"agents/_shared.prompt.md"}`).

Memento status comes from the server's existing health observation. A disconnect still yields the captured session copy. Older snapshots recover anchor text from the delimited injection block. They can return one block when item boundaries are ambiguous. Metadata unavailable at capture is null.
