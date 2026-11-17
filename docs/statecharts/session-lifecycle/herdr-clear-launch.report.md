# Herdr clear and one-shot provider resume fence

Investigation baseline: `2f32ed6e232a98612e276b7e27c7df19cb8e2188`.
This model checks the adopted one-shot token design. Model GREEN does not mean
production implementation, Rust argv tests or deployment verification are complete.

The observed clear completed through the ordinary Retired path, not Held.
Background RootReplaced recovery reported Held separately. The second launch's
journal seq21 was written synchronously by `pin_retired_resume` while choosing
its provider session; it was not a late hook. Held is included because the
requested clear contract also requires its later launch to start fresh.

## Values, actions and code correspondence

| Model | Code / contract | Meaning |
|---|---|---|
| `ClearRetired` | `control.rs:561` / `clear_teardown.rs:183`; `herdr_recovery.rs:88` exact Retired cleanup | Clear selector and install a token for the exact canonical row |
| `ClearHeld`, `OldExecutionEnds` | `herdr_recovery.rs:88`, `:132`; existing confirmed-end recovery | Commit conversation reset independently of teardown; no authority to kill a held execution |
| `clearedMemory`, `LoadSelector` | `pre_admission_control.rs:108`; `intake_turn/session_preparation.rs:30` | Router consumes a volatile flag; restart loses it, so DB original may be restored |
| `fence`, `ReadFence` | Adopted `sessions.provider_resume_fence` contract; `launched_source` entry | NULL is ordinary policy; active token bypasses both `judged()` calls |
| `Judge`, `RepinRetired` | `claude/herdr_turn/handoff.rs:74`, `:95`, `:267`, `:329` | Durable native binding can override original None; seq21-like repinning writes the journal |
| `Select`, `Launch`, `Launch2` | `handoff.rs:100`, `:132`; `claude_tui/session.rs:442` | Chosen ID and resume bit reach provider command construction |
| `LaunchWritesSelector`, `ReadApi` | Provider SessionStart hook / selector API | Startup can rewrite selector before release; active fence still makes API return null |
| `FreshVerified`, `Release` | Verified SessionStart (`handoff.rs:138`), captured-token CAS | Only verified fresh startup releases that same token |
| `ClearAgain`, `WrongTokenRelease` | Second clear before the earlier release | New token survives stale release |
| `SecondRestart`, `Rollover` | Existing restart/native-binding policy | After release, resume verified S or its legitimate later session transition |

Coordinates above identify the investigated call chain; new implementation must
be verified separately. The fence has only NULL and active-token states. Tokens
`t1`/`t2` abstract distinct UUIDs. `set_at` and `cause=user_clear` are metadata and
are not used for authorization. Transcript boundary generation is not a fence.
There is no allowed-session list or explicit Herdr `/resume` authorization here.
`epoch`/`firstPending` are history oracles for the required launch property, not
additional persistence fields.

## Six rules

- `ClearNeverResumesPrior`: the first launch after each clear is fresh and does
  not select the pre-clear session, including after a second clear.
- `FreshRestartKeepsSession`: after verified fresh S and release, the next restart
  resumes S, or the current binding after `Rollover`; it does not keep starting fresh.
- `UnclearedRestartKeepsResume`: a channel without clear retains ordinary resume.
- `WrongTokenReleaseNoEffect`: a stale token cannot remove a newer clear fence.
- `NoRepinUnderFence`: a fenced launch does not call the journal-writing judge.
- `ApiSelectorDoesNotExposePrior`: active fence masks even a selector rewritten
  by startup; before a first verified start, API must not expose the old ID.

`NoDeadlock` is also checked. Preservation rules are conditional: the uncleared
configuration exercises ordinary restart, fixed Retired/Held exercise verified
fresh restart and rollover, and FixedRaceClear exercises token replacement.
Claims for those rules do not rely only on configurations where their antecedents
never occur.

## TLC results and previous model correction

TLC reports **2.19, revision `5a47802`**, using one worker. The jar's distribution
filename contains 1.7.4; the engine version above comes from its actual stdout.
Ten configurations took twenty complete checking runs, including reruns that
remove each expected failing invariant. All expected sets matched; every final
rerun finished without errors, including NoDeadlock. The final clean rerun's
state count is shown below. No rule was skipped or weakened to obtain GREEN.

| Configuration | Prior simplified model | States | Runs | Revised model result |
|---|---|---:|---:|---|
| BeforeRetired | ClearNever RED | 396 | 3 | RED ClearNever, API selector |
| BeforeHeld | ClearNever + API RED | 441 | 3 | RED ClearNever, API selector |
| BoundaryOnlyHeld | ClearNever RED | 404 | 3 | RED ClearNever, API selector |
| FixedRetired | Proposal GREEN | 322 | 1 | All six rules + NoDeadlock GREEN |
| FixedHeld | Proposal GREEN | 330 | 1 | All six rules + NoDeadlock GREEN |
| UnclearedRestart | Proposal GREEN | 272 | 1 | All six rules + NoDeadlock GREEN |
| FixedRaceClear | Not modeled | 142 | 1 | All six rules + NoDeadlock GREEN |
| NoRelease | Not modeled | 134 | 2 | RED FreshRestartKeepsSession |
| ReleaseWithoutToken | Not modeled | 154 | 3 | RED WrongTokenReleaseNoEffect, ClearNever |
| FilterAfterJudge | Not modeled | 342 | 2 | RED NoRepinUnderFence |

Short names ClearNever/API in the table mean `ClearNeverResumesPrior` and
`ApiSelectorDoesNotExposePrior`. All unlisted rules hold in that configuration.
The old model stopped after one launch and could not detect repeated fresh
launches, stale release, selector rewrite or synchronous repinning. Its API
rule missed the selector written by the bad launch. The revised API RED for
BeforeRetired and BoundaryOnlyHeld exposes that omitted behavior.

The Retired shortest ClearNever counterexample has 7 actions: clear → consume
volatile clear flag → read no provider fence → judge native source → synchronously
repin retired resume → choose the old ID → launch with resume. Its API violation
adds startup selector write and API read (9 actions). Held adds the old execution's
later confirmed end. `FilterAfterJudge` still chooses fresh, but the earlier
repinning write violates the no-journal-write rule. `NoRelease` selects fresh1,
verifies it, leaves t1 active and starts fresh2 after restart. `ReleaseWithoutToken`
clears t2 using captured t1, allowing fresh1 to be resumed after the second clear.

## Bounds, omissions and evidence

The model includes one optional restart before first launch, one NotSent retry,
one verified-start restart, optional session rollover, and one racing second
clear. DB publication, transcript identity, account authorization and verified
SessionStart evidence are abstract successful ports. Read failure/fail-closed,
publication failure, unbounded retries, multi-channel isolation, third clear,
post-create crash before verification and real pane process races are not
proved here. No liveness claim is made: API reads and completed states can
stutter, so NoDeadlock alone is not a progress proof. Release is ordered after
verification by construction; an early
release mutant is an implementation-test obligation, not one of these cfgs.

Every checker invocation retains its actual reduced cfg, raw `-tool` stdout and
exit metadata under `i8208/logs/impl2-statechart-<case>-run<N>.*`. The six-rule model
snapshot, custom capture runner, aggregate results and runtime version are also
retained there. The runner calls the repository's `check_cfg` unchanged and wraps
its `run_tlc` only to preserve stdout/config before temporary cleanup. Model SHA256:
`824b7b96c316ebdee9c1c5d631fcf4f866b5fe8a1eca84f66dcbd663f0f75a87`.
These are model results; the implementation report must attach real argv RED/GREEN
and PG results before claiming production repair.

## Reproduce and diagrams

From the repository root, with `TLA2TOOLS` and `JAVA` selecting the validated
engine and Java 11+, use the statechart skill's executable:

```sh
statechart docs/statecharts/session-lifecycle/HerdrClearLaunch.tla \
  --config docs/statecharts/session-lifecycle/HerdrClearLaunchBeforeRetired.cfg \
  --view docs/statecharts/session-lifecycle/herdr-clear-launch.view.mjs \
  --out /tmp/herdr-clear-before-new
statechart docs/statecharts/session-lifecycle/HerdrClearLaunch.tla \
  --config docs/statecharts/session-lifecycle/HerdrClearLaunchFixedHeld.cfg \
  --view docs/statecharts/session-lifecycle/herdr-clear-launch.view.mjs \
  --out /tmp/herdr-clear-fixed-new
```

[Before](herdr-clear-launch-before.chart.mmd) and
[fixed design](herdr-clear-launch-proposal.chart.mmd) diagrams include both launches.
The view retains the rule marks and folds only steps within the same visible state;
no action is explicitly hidden. These bounded models are broader than the prior
one-launch chart, so the diagrams have more than twenty arrows.
