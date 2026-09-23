# Cluster execution evidence and central session control

Use one native AgentDesk artifact per OS/CPU target. Runtime roles select the
modules that start; a runner does not need a separate executable or scheduler.
See [execution only mode](worker-runtime-profile.md), [display terminology](node-terminology.md), and
[remote dashboard authentication](remote-dashboard-auth.md).

## Evidence and freshness

`GET /api/cluster/nodes` separates these signals:

| Signal | Evidence | Meaning |
| --- | --- | --- |
| `status` | PostgreSQL node heartbeat/lease | The node is recently online |
| `capabilities.execution_readiness` | Local execution probe | OS/architecture, CLI, credential metadata, logical repositories, tools and available backends |
| `execution_readiness.providers` | Fresh local evidence plus intake progress | Provider admission eligibility and explicit blocking reasons |
| `forwarding_diagnostics.configured` | Operator-owned trusted origin configuration | The hub has an explicit target origin |
| `forwarding_diagnostics.trust_validated` | Existing trusted-target validator | Advertisement agrees with allowed origin, address and transport rules |
| `forwarding_diagnostics.reachability_verified` | Authenticated request and matching node identity | The expected peer responded through that trusted target |

Local probes run sequentially every 30 seconds after the preceding probe ends.
Their evidence expires after 120 seconds, measured from collection start.
Heartbeats publish the existing timestamp and never extend it. An intake poller
records progress only after a successful claim query, including an empty queue;
its evidence expires after 30 seconds. A stalled or failed poller cannot renew
that evidence from the heartbeat task.

Provider version probes reuse the provider runtime's bounded `--version` check.
Credential fields are booleans for default and named local profiles. Neither
credential presence nor a CLI version proves remote authentication or quota;
both remain explicitly unverified. No paid model call runs on each heartbeat.
Diagnostics do not publish credentials or their hashes.

Repository evidence uses configured logical repository IDs and the existing
local resolver/origin check. A Windows path is never interpreted as a Mac path.
Extra executable names may be listed in `cluster.capabilities.tools`; only
simple executable names are probed, with a maximum of 32 configured tools.
`tmux` is an optional Unix capability, not a requirement for native process work.

Forwarding checks use the existing pinned `trusted_target` transport, including
its redirect/proxy/address protections and explicit consent for private HTTP.
They run with at most four concurrent requests, a three-second overall deadline
per peer and a two-second HTTP deadline. Results expire after 45 seconds; a
failed registry refresh does not refresh them. The protected
`GET /api/internal/node-probe` returns a protocol version and actual instance ID.
A response from a different node or an authentication failure is not success.

Legacy nodes without `execution_readiness_version` retain their pre-upgrade
admission contract. Updated nodes advertise the version before their first
probe and refuse new remote work until the required evidence is available.
Existing session ownership is retained when a probe fails: failure does not
authorize creating the same session on another node or killing an active turn.

## Backend-neutral output and cancellation

The Ops page polls node and shared session-owner data every five seconds. It
disables control when node data is stale, the peer is offline, or forwarding
verification is unavailable. The last displayed output remains visibly a
previous capture when a refresh fails. Browser credentials use the common
dashboard authentication and cache invalidation path.

`GET /api/sessions/{numeric_id}/output?lines=100` resolves the authoritative
session owner and uses the existing forwarding/fencing path. The old
`/tmux-output` endpoint is retained as an alias. Responses include `backend`,
`output_format`, `available`, `alive`, `unavailable_reason`, `captured_at_ms` and
`truncated`, alongside the existing session metadata and `recent_output`.

- Native process output is JSONL read from the file handle bound when the
  provider wrapper was created. The API accepts no file path and never reopens
  a replacement path. Captures are limited to 256 KiB and 2,000 lines.
- Unix tmux output uses the existing pane capture adapter and reports terminal
  text. Native process sessions are checked first on every OS.
- An unattached process after restart reports `session_output_not_attached`.
  Provider-native conversation resume and reattaching a live child process are
  separate capabilities. An empty tmux response is not fabricated on Windows.
- Stop uses the existing owner-fenced `POST /api/sessions/{session_key}/force-kill`
  with `retry:false` after an inline confirmation. An output read cannot cancel
  or transfer ownership.

The node panel labels `active_dispatch_count` as dispatch deliveries in progress.
It does not present that counter as the number of executing provider turns or
available execution slots.
