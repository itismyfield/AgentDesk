# Runbook — auditing dispatched intake-outbox rows

Use this procedure to inventory `intake_outbox` rows whose status is
`dispatched`. It gathers facts only; it does not change a row or release the
row's channel route.

## Run the audit

On a host configured for the target PostgreSQL database, run:

```sh
agentdesk intake-outbox dispatched-audit
```

An empty population prints `(no dispatched intake_outbox rows)` and exits
successfully. Otherwise the command prints one tab-separated row per database
row, ordered by `dispatched_at` with NULL clocks first and then by `id`.

Record the complete output in the incident. The identity fields are `id`,
`channel_id`, `user_msg_id`, `attempt_no`, and `parent_outbox_id`; the remaining
facts are `dispatched_at`, `claim_owner`, `provider`, and `provider_nonempty`.
A `-` represents a database NULL.

`provider_nonempty` means exactly that `provider.trim()` is non-empty, matching
the provider guard used by the operator force-fail implementation. It does not
establish worker availability, capability, feature support, labels, placement
ownership, generation, or freedom from route and attempt conflicts. A
dispatched row is still refused by that force-fail command.

## Scope and limits

The command issues only unlocked reads. It does not determine whether Discord
delivery occurred, repair a NULL clock, resolve an open route, or authorize a
retry. Correlate the printed row identity with the incident's independently
collected delivery and receipt evidence before choosing any later response.
Run this audit on demand; this procedure does not define a schedule or batch
worker.
