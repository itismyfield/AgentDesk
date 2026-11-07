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
| `session-lifecycle` (Herdr RootReplaced, #7788 S2) | `61bc2876e3` + S2 | an early old-binding claim retains current inflight that blocks quiet; missing completion wake leaves the same original Held | preclaim before claim and one wake for a process-seen completion close both progress gaps; same-root and nonapplicable-row controls pass; see `docs/statecharts/session-lifecycle/herdr-root-s2.report.md` |
| `host-migration` (tmux ↔ Herdr, #7503) | `db99888121` (branch `feat/i7503-claude-resume-both`) | old tmux Claude left running after a fresh forward launch (C1), which then blocks every Return turn (C2); id-only channel's queued input never starts Return (C3) | C1 already in main; C2 new, not raised by #7503 reviews; C3 is review B2 P1-1 (repair B-2) |
| `input-single-path` (#7272, landed + 4f design) | `0a9b3f99cf` + design-4f-r1 | unreadable composer pastes over a human draft (C1), MayHaveSent head after a restart never ticked (C2), Held input dropped or never re-woken (C4, unconfirmed), busy input lost by /stop (C5, unconfirmed) | C3 is carry P1-7; C1, C2, C4, C5 are 4f-b acceptance test candidates |
| `input-single-path` owner input stop (#7755) | `cbcad30ec3739d543c5da0e1d932f0acef9e87d4` + receipt, N1/N2, birth-child, restored-child decision-2 and live missing-source contract | before: missing owner/receipt authority; absorption-before: remove-only own input is ignored or foreign input is missed; summary-before: normal auto-compaction prevents stop; episode-before: a ledger child bypasses its rejected birth episode/parent; restored-before: a restored token trusts a ledger child without persisted membership; source-before: a live accepted root is refused before RuntimeReady fills its durable source | fixed permits a live accepted root whose durable output path is not yet filled while rejecting confirmed source conflicts, and rejects unproven restored children while keeping live claim-gap and tokenless authority, rejects birth mismatches without treating a direct native pin as birth proof, reads durable Command or absorption evidence and keeps verified compact summaries neutral; pure external relay claims retain their authority; see [report](input-single-path/native-stop.report.md) |
| `input-single-path` stop before first key (#7754) | `e85948c2c3` + branch `feat/i7754-stop-gate` | without the writer's NoKeyYet gate, or with `Cancelled{0}` held as NotSent, an input a stop took before its first key still reaches Claude | the gate config holds every rule; the two mutant configs document the guarded gaps |
| `input-single-path` ownership (#7676 A2+D) | `0b0fbcbfbe`, `524adb6190`, `e6d97a192f`, `4def1c6289`, `6f568ed680` + slice C | main: busy input retained (retry required), queued busy input answered by nobody; main and first A2+D: turn/start on a busy pane dropped, held input re-woken by a timer, unparked draft untold, repeated body answered by nobody; first repair: a reader's declaration lost after 10 s or taken from a same-body twin, an absorbed input's reader posting a second body, a held input never woken once its screen recovers; second repair: one failed read of the held pane, or an old watch's end, leaves that input unwatched; all before C: idle pane without observer waits for a wake; a2d4: a front held by a lock, buffer load or missing transcript waits once it clears | slice C wakes a waiting front when a turn-mode turn AgentDesk did not start ends, finds the idle observer's transcript as the live path does, watches a refusing cause even when it cleared before the first read and ends that watch when its pane is gone, relaunches a live pane under another account instead of writing into it, tells a busy buffer load failure (watched) from a frame ledger refusal (left for the turn's end wake), and keeps an input an older binary already asked to resend |
| `execution-authority` (#7731) | `b66f40ba19` | main: one transient PG read failure (or an owner change) on a channel hosted under the gateway lease holds live input forever, because the refresh waits for a host that never retires | fixed by #7731 (`fix1`); the `nowait` mutant breaks the permit-host fence |
| `execution-authority` observation (#7768) | `6249d8796f` | custody read closes admission during a held output; snapshot read preserves it | `ExecutionAuthorityCustody` expects the counterexample; `ExecutionAuthoritySnapshot` passes; see [report](execution-authority/snapshot.report.md) |
| `input-single-path` ownership, slice F (#7676) | `a9d59259e5` | main after C: a restart-carried row whose transcript has an input absorbed mid-turn without a `queued_command` row never closes, so every owner start is refused for good (CF); an outside copy of a waiting input is seen twice (FOutside) | F plan: settle an absorbed input at its `remove` record and drop its dequeue credit; the outside copy stays a documented gap |
| `input-single-path` consumed waiting row (#7807) | `0f27c5335a` + impl-7 contract | Before: channel debt or a missing busy-observer pin keeps B pending; a separate RED config assigns N's cancellation to orphan A | After orders real busy return → Submitted settlement → actual-user observation → verified pin; orphan A defaults to Completed and honors its own stamp. O-only marker consumption, unconditional shutdown refresh, current native idle, notice20 independence and successor guards remain. NonO/Unknown and invalid evidence have safety-only configs. See [report](input-single-path/consumed-waiting.report.md). |
| `input-single-path` superseded carried row (#7896 B) | `b5e0f2115f` + repair | `SupersededBefore`: an unproven carried row bound to the session the pane left keeps B pending forever (#7889 D) | `SupersededAfter` closes it on the existing native end, O drain and lock proof with one deduplicated notice; `SupersededSameSession` keeps the #7828 gap; `SupersededNoDedupe` breaks `UnprovenCloseToldOnce`. See [report](input-single-path/superseded-carried.report.md). |
| `input-single-path` owner observer rebind (#7896 B) | `b5e0f2115f` + repair | `ManagedObserverRebindBefore`: an observer built while `/clear`'s session is pending tails the old session, so the managed turn never closes (#7891 review P1-1) | `ManagedObserverRebindAfter` follows the verified session after the submit unless the old one grew. See [report](input-single-path/superseded-carried.report.md). |
| `input-single-path` F7 recovery and transition release (#7676) | `1c19e5058f` + F7 repair | `F7RecoveryMain` and `F7TransitionMain`: a refused input is not woken after active reconcile or /resume·clear release; `F7StandbyWake`: standby alone injects | fixed recovery wakes adopted owners; /resume·clear completion releases the guard before waking. `F7ShortTransition` and `F7ShortTransitionMain` keep short intake/Herdr/O-writer locks unchanged: the original is delivered once after A ends, with no loss. |
| `input-single-path` duplicate intake (#7857) | `0bf73d0e2f` + repair | Before: live/catch-up retry after delivery warns because the receipt survives its original | Fixed uses the existing committed Discord ticket; both arrival orders and restart preserve one delivery per message; see [report](input-single-path/duplicate-intake.report.md). |
| `input-single-path` reset ownership (#7835) | `435710f6a2` + repair | External reset overwrites the adopted snapshot: live and catch-up CAS reject it; a Pending original needs catch-up for its first commit | Fixed and Pending configs pass; raw writer and always-rejected Pending catch-up mutants expose the named safety/liveness counterexamples. See [report](input-single-path/reset.report.md). |
| `input-single-path` StartFailure (#7787 S0) | `b0bab6ebf6` | `StartFailureBefore`: repeated starts/error cards; `OpaqueNoWakeBefore`: silent recovery without a wake; `HoldCountedBefore`: scheduling holds miscounted; `TokenWakeBefore`: grant before exact row cleanup; `EarlyRetainBefore`: premature retention; `ReserveResetBefore`: failed reserve reset | exact row removal + token release wake policy, early policy deletion comparison RED, independent early-clear delivery and reserve-reset checks; live repair pinned in [report](input-single-path/start-failure.report.md) |
| `turn-presence` (#7769) | `97b7f6b352` | Main/M1: false Unknown and notice, unstable Busy; M2: publication starvation; M3: wrong turn admission/publication | [Fixed identity projection](turn-presence/turn-presence.report.md) checks safety and both liveness properties |
| `tui-o-writer` (#7845) | `7a0c819e12` + park-and-run contract | main: sessionless missing source repeatedly reboots and holds input forever; a superseded missing predecessor prevents the new source's first reply | park keeps the actor ready, replays retained spool debt and readmits input; Present/Unknown and capture/derivation debt remain halted. See [report](tui-o-writer/writer.report.md). |
| `tui-o-writer` current-source classification (#7845 delta) | `7a0c819e12` + reviewed Current contract | birth ordering hides current A loss after A→B→A; Unknown can park without absence, ready precedes a valid new bind, parked rebind keeps running | full-log last valid Bound identifies historical sources; ready follows applicable bind application; post-view parked rebind/refinement halts. Includes rotation-before-checkpoint interruption. See [report](tui-o-writer/rebound.report.md). |
| `tui-o-writer` parked old alias (#7845 repair 3) | old `34822f3bf1`, fixed `1e2763de57` | stored empty-session A0 is parked, but raw old A1 is reopened before fresh B attaches; first reply stalls and hosts repeat | only an established unique parked old graph reference uses A0 without reopen; new/parent/unseen/ambiguous aliases keep strict checks and parked new keeps C6. See [report](tui-o-writer/parked-alias.report.md). |

F7 transition-release configs cover only `/resume` and the common clear function (`/clear` and `!clear`).
They require one busy paste after those operations release their guard. Other short transition locks
may defer an input until the running turn A ends; their configs require one delivery after that
existing boundary wake. Every F7 fixed config checks duplicate delivery, and the short-transition
configs also check that the original is not silently lost. Recovery completion is an active-runtime
event; standby marking reconcile complete never wakes an owner.

The #7768 execution observation pair uses `ReadMode = custody | snapshot` on the #7731 `fix3` baseline.
`ExecutionAuthorityCustody` expects `OutputDelayKeepsAdmission` to break: an output holds custody,
then refresh times out and suspends admission without an independent DB failure.
`ExecutionAuthoritySnapshot` preserves admission while output waits. Both check
`OutputSerializesOwner` and the existing permit-host safety rules. Existing historical configs
keep custody reads and their existing properties; this pair adds no liveness property.
The model abstracts the full raw wait into `output = held`; the real guard has an effective
upper bound of about 65 seconds from the server `idle_in_transaction_session_timeout`.
Routing/staging can still wait for that guard; admission here does not mean durable staging has completed.
