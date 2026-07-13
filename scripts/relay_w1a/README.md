# #4254 standalone W1-A executable contract

This directory is intentionally independent of the AgentDesk Rust module tree.
It is a side-effect-free executable specification, not runtime recovery
authority.

## Contract boundaries

The model covers four fail-closed contracts:

- stall authority uses typed, episode-bound evidence; silence, elapsed time,
  repeated repair failure, and the compatibility boolean termination flag never
  prove producer death;
- repair accounting uses an exact attempt CAS and a `Reserved -> EffectPending
  -> Settled` ledger. An unresolved reservation or effect cannot be overwritten
  or replayed, and only a delivery-effect proof can rearm a same-episode circuit;
- bounded batching durably disposes every source range as delivered,
  summarized, or policy-omitted. Oversized tool bulk advances a bounded prefix
  and exposes an explicit suffix instead of requiring the whole group to fit;
- reconciliation recomputes range, expected-body, Discord-body, marker, and
  committed-frontier facts from external pins and actual UTF-8 source bytes.

`DeliveryProof` pins the episode, source id and generation, durable anchor,
source and committed frontiers, typed queue disposition, and action-time clock.
The observation and proof must agree at settlement time. `TerminationProof`
additionally pins the exact termination id, finalizer commit, stopped watcher,
safe queue disposition, and action-time reprobe. Unknown enum values, stale or
future clocks, impossible `u64` counters/frontiers, missing anchors, and
unrecognized ledger states fail closed.

## Oracle inputs

An evidence bundle is not self-authenticating. `validate_evidence_bundle`
requires these inputs from outside the bundle:

- the exact manifest/schema/template pin;
- actual source id and actual source bytes;
- Discord channel id and exclusive message-id cutoff.

The oracle accepts only raw UTF-8 byte coordinates, exact disposition/reason
pairs, contiguous non-duplicated ranges, a durable advancing anchor, complete
Discord revision history after the cutoff, and unrelated-session evidence from
both Claude and Codex. Declared source hashes, markers, normalized bodies,
expected-body hashes, and frontiers are always recomputed or cross-checked.
Every serialized disposition carries both its aggregate `source_sha256` and an
ordered `source_range_sha256s` list, including multi-range tool summaries.
Discord evidence ids remain typed decimal strings; arbitrary values are never
coerced. Marker uniqueness is derived from actual source bytes independently of
nullable range metadata.

`commit_dispositions` and `advance_committed_frontier` are side-effect-free, so
their Discord message-id uniqueness checks cover only the dispositions supplied
in that call. They do not claim process-global or durable uniqueness. Cross-batch
reconciliation must provide the evidence oracle with every disposition and
Discord observation in the externally pinned audit extent; the oracle rejects a
message id reused anywhere in that extent.

## Fixtures and tests

`fixtures/incidents.json` contains the authority matrix, executable ledger and
backpressure fault cases, the continuous #4104 E-22
`Missing -> ReappearedSame -> PresentStable` sequence, and a complete oracle
example with separately supplied pins. The suite invokes the model for every
fault fixture; fixture names alone are not treated as evidence.

Run the isolated standard-library suite from the repository root:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s scripts/relay_w1a/tests -p 'test_*.py' -v
```

Production wiring remains blocked on the dependency and sole-writer gates in
the corrected #4254 V2 design. Files in this directory must never import
AgentDesk runtime modules or perform network, Discord, tmux, process, or
filesystem-state mutation.
