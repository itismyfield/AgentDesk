# Idle-kill owner recovery

Idle-kill skips remote session owners whose trusted origin is not configured or
whose worker heartbeat has expired. Each affected owner emits one warning while
the condition persists. The next tick rechecks configuration and heartbeat;
recovery restores eligibility and resets the warning for a later outage. Owners
without idle candidates leave the warning cache, so the cache stays bounded by
the candidate-owner count. Filtering happens before the 50-session batch limit.

`GET /api/cluster/nodes` exposes `cluster.local_instance_id` and
`cluster.configured_forward_owner_ids`. The latter reports only the presence of
an explicit, nonempty configured origin; it is not a forwarding authorization.
The kill-tmux route still checks capability, exact origin agreement, address and
transport restrictions, and current ownership. Registry `api_base_url` values
never become trusted configuration.

## Preparing operator configuration

[Source of truth](../source-of-truth.md) assigns policies to the repository and
the runtime baseline to `~/.adk/release/config/agentdesk.yaml`. Release policy
mirrors, the root-level legacy YAML, and vault YAML copies are not edit targets.

Before adding an owner under `cluster.nodes`, verify the peer's operator-owned
`cluster.instance_id`, `cluster.api_base_url`, listener port, and transport. An
SSH hostname or known Tailscale IP alone does not establish the API origin. A
registry advertisement alone is also insufficient. Once verified, merge the
following shape into the canonical YAML, preserving existing cluster entries:

```yaml
cluster:
  nodes:
    <verified-instance-id>:
      trusted_forward_origin: "<verified-api-origin>"
      allow_private_forwarding: true
      # Set separately only for a verified private/Tailscale HTTP origin:
      # allow_insecure_http_forwarding: true
```

Do not permanently exclude a temporarily offline peer. Configured-origin
presence and a fresh heartbeat make that peer eligible again automatically once
the updated configuration is loaded by the running server. Cluster settings are
boot-bound (`src/config_live_reload.rs`); a YAML edit requires a server restart
to update both this preflight and the forwarding authority.

## Remaining operational verification

Code fixtures cannot establish the peer's live API origin or demonstrate a
remote tmux kill. Completing #5714 still requires authorized canonical YAML
installation and deployment, checking `/api/cluster/nodes`, observing 24 hours
without repeated `trusted_forward_origin_missing` idle-kill errors, and one
successful forwarded kill after the peer is restarted. Use a disposable idle
session explicitly designated for that final check.
