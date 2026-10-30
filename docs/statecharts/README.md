# State charts

Small TLA+ models of AgentDesk's state-based code, checked with TLC and drawn as Mermaid state charts.
They catch stuck states and broken invariants before a change lands. They are not tests of the code.

## When to add one

A node that creates or substantially changes state-based code draws a chart during its investigation/design leg.
Examples are sessions, turns, inflight rows, Herdr executions, gateway leases, queues, and the campaign FSM.
Skip code that the I/O migration is about to delete.
Every counterexample is replayed step by step against the code, and only the ones that the code confirms go to design review or acceptance tests.

## Layout

One folder per area: `<area>/<Model>.tla`, `.cfg`, `<name>.view.mjs` (labels in the reader's words),
plus the committed output `<name>.chart.mmd` and `<name>.report.md`.
Each report states the code SHA the model was written against.

## Run

Needs the `statechart` skill (`~/.claude/skills/statechart`, from cursor/plugins pstack), Node 23.6+, Java 11+, and `tla2tools.jar` 1.7.4.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar   # sha256 936a2620…050e88
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/session-lifecycle
~/.claude/skills/statechart/scripts/statechart TmuxSession.tla --config TmuxSession.cfg --view tmux.view.mjs --out /tmp/sc-tmux
```

The command exits 1 when any rule breaks, which is the expected result for a model that documents a known gap.

## Index

| Area | Code SHA | Rules that break | Disposition |
|---|---|---|---|
| `session-lifecycle` (tmux) | `e8d94425a0` | stale inflight row after a deferred boot probe (T-A/T-B), FIFO open race (T-C) | tracked by #7331 (I21 in `docs/relay-state-contract.md`) |
| `session-lifecycle` (Herdr) | `e8d94425a0` | Pending row with no pane after a lost create reply or crash (H-A/H-B), prompt to an exited Claude pane (H-C) | H-A/H-B fixed by #7563 |
| `host-migration` (tmux ↔ Herdr, #7503) | `db99888121` (branch `feat/i7503-claude-resume-both`) | old tmux Claude left running after a fresh forward launch (C1), which then blocks every Return turn (C2); id-only channel's queued input never starts Return (C3) | C1 already in main; C2 new, not raised by #7503 reviews; C3 is review B2 P1-1 (repair B-2) |
| `input-single-path` (#7272, landed + 4f design) | `0a9b3f99cf` + design-4f-r1 | unreadable composer pastes over a human draft (C1), MayHaveSent head after a restart never ticked (C2), Held input dropped or never re-woken (C4, unconfirmed), busy input lost by /stop (C5, unconfirmed) | C3 is carry P1-7; C1, C2, C4, C5 are 4f-b acceptance test candidates |
