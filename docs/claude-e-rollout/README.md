# claude-e rollout (historical)

The claude-e rollout has ended; #5706 option c removes the runtime. Its E2E
cell and dedicated witness scenario were already retired. Units 0–1 remove
the remaining dedicated E2E expectations, rollout procedures and toolchain entry.
This is an intermediate removal state: production Rust still contains the adapter.

Remaining order: activate refusal of retired runtime settings, preserve old
inflight rows without execution or rewriting, remove the executor, then remove
its handoff metadata. Final source-link cleanup removes this directory.

This tombstone and the [decision log](decision-log.md) remain for existing source
references. Shared [Claude TUI follow-up retry guidance](../tui-o-writer-channels.md#claude-tui-follow-up-retry-boundary)
now lives in the current TUI documentation.
