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
An empty selection prints `NO_TARGETS`, runs no TLC, and returns nonzero; it is not a checked result.

`PROPERTY` and `PROPERTIES` require `SPECIFICATION <name>` instead of `INIT`/`NEXT`.
Before model checking, the same TLC jar interprets the selected Spec and must find at least one applied temporal assumption.
Unused fairness definitions and comments do not count. This checks that an assumption is present, not that it is sufficient.
`SYMMETRY` together with PROPERTY is refused because TLC can miss liveness violations with symmetry reduction.
Invariant-only configs can still use symmetry.
Safety rules run first, then each temporal property runs alone: TLC 1.7.4 reports a temporal violation without its name.
A temporal run must report temporal checking and a clean finish or exit 13 with a temporal violation.
Constant or state-only PROPERTY formulas that TLC optimizes away from temporal checking are refused; use INVARIANT for state predicates.
ExecutionAuthorityMain expects `AdmissionResumes` to break, while ExecutionAuthority expects it to hold.

Add a new model only with small constants that finish within the CI limits (5 minutes per config, 30 minutes per PR job).
The 300-second config budget includes the fairness precheck, every safety rerun and every property; timeout always fails.
The precheck uses a Java 11+ source-file launcher and adds one JVM per config that declares PROPERTY.
Measure the full checker and its per-config times before and after adding liveness.
If a config exceeds that budget or the suite approaches 30 minutes, reduce its PR bounds and move the larger cfg
to a separate nightly-only root with its own `expected.tsv` and explicit timeout/job budget.
Wire that root into the scheduled workflow before moving it; do not omit a config from the current manifest to hide a timeout.

## Index

Browse the [index fragments](index.d/) for model areas, code revisions, results, and reports.
Render the full table locally from the repository root:

```sh
python3 scripts/ci/statechart_index.py --output docs/statecharts/index.generated.md
```

Open the generated index alongside this README for working relative links. The PR and main
contracts jobs publish the full tables and an index change summary as an artifact named `statechart-index-*`.

The [clear-authorities integration row](index.d/650-input-single-path-clear-authorities.md) records the
historical **41ace S2-before** public-Ready/invalid-certificate scenario and DualCoupled
counterexample. Latest B1 public readiness requires a current certificate,
and its separate CurrentClear family includes a proof-observed-then-invalidated TOCTOU case.

The #8055 implementation code snapshot is `7f7e47d0b397d97527e5fefd92e48ac03fdfb9a4`; the index Code SHA records the frozen Current baseline. The #8055 `InputPathBootDesign` and `InputPathBootDesignTwo` GREEN configurations exceed the tool's 500-state chart budget, so they have complete reports but no generated chart. Their absence is not a skipped check; the [Writer design chart](tui-o-writer/i8055-writer-design.chart.mmd) shows the shared custody/ready flow. The scoped models leave normal V1 local execution unchanged and outside their proof.

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

The [Claude Herdr watcher completion row](index.d/690-input-single-path-herdr-watcher-completion.md) checks held input delivery after the same-episode soft terminal, including strict deadline deferral, restart, and nonce and file boundaries.

The Claude readiness refinement also checks terminal-free self-compact completion: [before](input-single-path/InputPathReadyCompactNoCompletion.cfg) breaks next-input liveness; [after](input-single-path/InputPathReadyCompactCompletion.cfg) satisfies it. Source, generation, reservation and Unknown-transfer guards remain caller obligations documented in [the readiness report](input-single-path/transcript-readiness.report.md).

AwaitingClear's authenticated, one-use source retirement is refined in
[input readiness](input-single-path/transcript-readiness.design.md#awaitingclear-adoption),
including restart, forged proof, cursor loss and split-publication counterexamples.

The #8080/#8114 readiness merge keeps clear retirement/consumption and pre-source Pending/modal-watch as separate Mode projections. The bounded integration check preserves both parent result sets (42 and 40 cfg) in 49 merged ReadinessSpec cfg: 27 GREEN and 22 expected RED, with no expected-rule changes. This check does not replace the wider changed-model CI selection; the separate 83 Herdr busy/source/reobserve/cold-Idle cfg and their expected rows remain byte-identical.

The resumed-start readiness refinement starts a `--resume` execution over the transcript its earlier run left: [before](input-single-path/InputPathReadyResumeMetaBefore.cfg) a queued input is held forever; [after](input-single-path/InputPathReadyResumeMetaAfter.cfg) its own SessionStart admits it while no turn of this run exists, and [the mutant](input-single-path/InputPathReadyResumeNoQuiet.cfg) without that condition submits into a running turn. [Evidence](input-single-path/resume-history.report.md). A daemon restart over that idle session drops the start: [before](input-single-path/InputPathReadyResumeRestartBefore.cfg) the queued input is held forever; [after](input-single-path/InputPathReadyResumeRestartAfter.cfg) the readiness budget ends it retained with one resend ask.

The managed idle-stash readiness refinement keeps a person draft intact through the existing C-s round trip: [before](input-single-path/InputPathReadyManagedStashBefore.cfg) breaks queued-input liveness; [after](input-single-path/InputPathReadyManagedStashAfter.cfg) preserves the draft and sends once after current source readiness. [Evidence](input-single-path/readiness-managed-stash.report.md).

The clear-authority integration adds six direct `ClaudeReadiness` / `Spec` configs to the merged dependency selection of 68 (including both managed-stash configs). All 74 selected configs ran: 37 GREEN and 37 expected RED, with all 68 pre-extension result sets and 574 existing expected rows preserved. Public source readiness and disk-certificate adoption remain separate: invalid disk proof can retain a public-Ready input before its first key. [Report and charts](input-single-path/clear-authorities.report.md). The earlier 49-config paragraph above records its historical bounded run.

The six `ClaudeReadinessDual*` configs and their independent-public-readiness result above are historical **S2-before at 41ace13c**, not a claim about the latest B1 source. Latest B1 requires a current durable certificate in public readiness. The separate `ClaudeReadinessCurrentClear*` family checks valid/invalid current proof, certificate invalidation after observation before the first key, and the missing-current-certificate guard mutant. Its safety rules and expected rows are distinct; the old DualCoupled RED is not a latest-B1 counterexample. The common clear claim/replay/source-retention projection follows B1 082c77aa. Actual bounded TLC checked S2-before74 (37 GREEN/37 expected RED), B1-before73 (37/36), and the merged readiness selection83 (44/39). Every B1 result set and the six historical Dual result sets are preserved. PermanentUnread is explicitly RED in S2-before and GREEN under the latest B1 common model; its row is not silently retained as an obsolete RED.
