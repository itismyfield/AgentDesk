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

CI (`.github/workflows/statecharts-tlc.yml`, PRs touching this folder and nightly, alert only) runs every `.cfg` with plain TLC through `scripts/ci/statecharts_tlc.py` and fails when the broken rules differ from that config's row in `expected.tsv`; run it locally with `TLA2TOOLS` and `JAVA` set as above.
A change that adds a config, breaks a rule or closes a known gap updates `expected.tsv` and the report in the same PR.
The first three TSV fields retain their meaning (config, module, expected invariant/deadlock violations).
An optional fourth field lists expected liveness violations; use comma-separated PROPERTY names or `-`.
An unlisted violation or an expected violation that now holds fails the check.

`PROPERTY` and `PROPERTIES` require `SPECIFICATION <name>` instead of `INIT`/`NEXT`.
Before model checking, the same TLC jar interprets the selected Spec and must find at least one applied temporal assumption.
Unused fairness definitions and comments do not count. This checks that an assumption is present, not that it is sufficient.
`SYMMETRY` together with PROPERTY is refused because TLC can miss liveness violations with symmetry reduction.
Invariant-only configs can still use symmetry.
Safety rules run first, then each temporal property runs alone: TLC 1.7.4 reports a temporal violation without its name.
A temporal run must report temporal checking and a clean finish or exit 13 with a temporal violation.
Constant or state-only PROPERTY formulas that TLC optimizes away from temporal checking are refused; use INVARIANT for state predicates.
ExecutionAuthorityMain expects `AdmissionResumes` to break, while ExecutionAuthority expects it to hold.

Add a new model only with small constants that finish within the CI limits (5 minutes per config, 15 minutes per job).
The 300-second config budget includes the fairness precheck, every safety rerun and every property; timeout always fails.
The precheck uses a Java 11+ source-file launcher and adds one JVM per config that declares PROPERTY.
Measure the full checker and its per-config times before and after adding liveness.
If a config exceeds that budget or the suite approaches 15 minutes, reduce its PR bounds and move the larger cfg
to a separate nightly-only root with its own `expected.tsv` and explicit timeout/job budget.
Wire that root into the scheduled workflow before moving it; do not omit a config from the current manifest to hide a timeout.

## Index

| Area | Code SHA | Rules that break | Disposition |
|---|---|---|---|
| `session-lifecycle` (tmux) | `e8d94425a0` | stale inflight row after a deferred boot probe (T-A/T-B), FIFO open race (T-C) | tracked by #7331 (I21 in `docs/relay-state-contract.md`) |
| `session-lifecycle` (Herdr) | `e8d94425a0` | Pending row with no pane after a lost create reply or crash (H-A/H-B), prompt to an exited Claude pane (H-C) | H-A/H-B fixed by #7563 |
| `session-lifecycle` (Herdr RootReplaced, #7788 S0/S1) | `d508839828` + S1 | owned channel retains a replaced Bound row when current startup policy evidence is never produced; early input can also leave current-generation inflight blocking quiet | S1 produces evidence before a new target; GREEN assumes input after the first reader pass, with preclaim/wake race reserved for S2; see `session-lifecycle/herdr-root.report.md` |
| `host-migration` (tmux ↔ Herdr, #7503) | `db99888121` (branch `feat/i7503-claude-resume-both`) | old tmux Claude left running after a fresh forward launch (C1), which then blocks every Return turn (C2); id-only channel's queued input never starts Return (C3) | C1 already in main; C2 new, not raised by #7503 reviews; C3 is review B2 P1-1 (repair B-2) |
| `input-single-path` (#7272, landed + 4f design) | `0a9b3f99cf` + design-4f-r1 | unreadable composer pastes over a human draft (C1), MayHaveSent head after a restart never ticked (C2), Held input dropped or never re-woken (C4, unconfirmed), busy input lost by /stop (C5, unconfirmed) | C3 is carry P1-7; C1, C2, C4, C5 are 4f-b acceptance test candidates |
| `input-single-path` ownership (#7676 A2+D) | `0b0fbcbfbe`, `524adb6190`, `e6d97a192f`, `4def1c6289`, `6f568ed680` + slice C | main: busy input retained (retry required), queued busy input answered by nobody; main and first A2+D: turn/start on a busy pane dropped, held input re-woken by a timer, unparked draft untold, repeated body answered by nobody; first repair: a reader's declaration lost after 10 s or taken from a same-body twin, an absorbed input's reader posting a second body, a held input never woken once its screen recovers; second repair: one failed read of the held pane, or an old watch's end, leaves that input unwatched; all before C: idle pane without observer waits for a wake; a2d4: a front held by a lock, buffer load or missing transcript waits once it clears | slice C wakes a waiting front when a turn-mode turn AgentDesk did not start ends, finds the idle observer's transcript as the live path does, watches a refusing cause even when it cleared before the first read and ends that watch when its pane is gone, relaunches a live pane under another account instead of writing into it, tells a busy buffer load failure (watched) from a frame ledger refusal (left for the turn's end wake), and keeps an input an older binary already asked to resend |
| `execution-authority` (#7731) | `b66f40ba19` | main: one transient PG read failure (or an owner change) on a channel hosted under the gateway lease holds live input forever, because the refresh waits for a host that never retires | fixed by #7731 (`fix1`); the `nowait` mutant breaks the permit-host fence |
| `input-single-path` ownership, slice F (#7676) | `a9d59259e5` | main after C: a restart-carried row whose transcript has an input absorbed mid-turn without a `queued_command` row never closes, so every owner start is refused for good (CF); an outside copy of a waiting input is seen twice (FOutside) | F plan: settle an absorbed input at its `remove` record and drop its dequeue credit; the outside copy stays a documented gap |
