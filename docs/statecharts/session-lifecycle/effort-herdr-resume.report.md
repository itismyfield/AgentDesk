# #8109 S5 (reduced): `/effort` on Herdr

Model: `EffortHerdrResume.tla`, written against `d6f5aa7b7a` plus the S5 change on `feat/i8109-s5n`.
It projects the Herdr branch of the S4 effort-restart model (`TmuxSession.tla`, `EffortRestart*.cfg`):
one `/effort` request, one possibly running turn A, a later user U, an optional `/clear`.

## Rules

| Rule | Kind | Meaning |
|---|---|---|
| `SameThread` | safety | A launch that owes an effort resume runs only the ended thread; a fresh launch only after an explicit clear, a refused request, or a row no restart ended |
| `LaunchedSaved` | safety | The new execution runs the saved level |
| `NoOptionsDrift` | safety | No prompt meets launch options its pane was not launched with (`herdr_turn.rs:418`) |
| `ClaudeNoRestart` | safety | A Herdr Claude pane is never ended or gated for `/effort` |
| `ContinuationOnlyForCut` | safety | A continuation exists only for a turn the restart cut |
| `RequestSettles` | liveness | Every request ends applied or refused with a reason |
| `CutContinues` | liveness | A cut turn's continuation is submitted |
| `UserServed` | liveness | A waiting user is served |

## Transitions and code

| Action | Code |
|---|---|
| `ClaudeApply` | `tui_passthrough.rs:99` → `herdr_settings.rs:236` (unchanged ready-pane overlay) |
| `CodexRequest` | `tui_passthrough.rs:94`; `effort_restart/herdr.rs:91-99` (Pending refused, Retired/vacant save only) |
| `Prepare` | `effort_restart/herdr.rs:136-186` (expected row, live thread, captured turn, owner intent before any end) |
| `TerminateOk` | `effort_restart/herdr.rs:220` exact terminate; postlude saves the level under the held exclusion; gate released at `:229` |
| `TerminateFails` | `effort_restart/herdr.rs:223` cancels the intent, nothing saved |
| `Admit` | `effort_restart/herdr.rs:237` → `tmux.rs` `advance` |
| `Launch` | `herdr_turn.rs:212,260` → `restart_intent.rs:61-75` |
| `Recover` | `effort_restart/recovery.rs:50-58` |
| `Clear` | `queued_head/v2/clear.rs:221` |
| `Followup` | `herdr_turn.rs:418` `options_kept` |

Value ranges are those of the entry census in the S5 report: row Bound/Pending/Retired, turn none/running,
O cursor naming the ended thread or another one.

## Results

`python3 scripts/ci/statecharts_tlc.py --root <only these configs> --jobs 4`: PASS 11/11, 18 s.

| Config | Expected breaks |
|---|---|
| `EffortHerdrDesign` (busy), `Idle`, `Starting`, `Retired`, `Claude` | none |
| `EffortHerdrOtherThread` | `CutContinues`, `UserServed`: the launch is refused rather than resuming another thread (fail-closed by design; the safety rules hold) |
| `EffortHerdrMutSaveFirst` | `NoOptionsDrift` |
| `EffortHerdrMutAnyThread` | `SameThread` |
| `EffortHerdrMutRecoverUngated` | `SameThread`, `CutContinues` |
| `EffortHerdrMutClearKeeps` | `UserServed` |
| `EffortHerdrMutClaudeRestart` | `ClaudeNoRestart` |

## Assumptions

- The O store's one live cursor names the thread the ended execution read (`restart_intent.rs:80-92`).
- `terminate_for_http` runs its postlude only after the exact retire and while the exclusion is held.
- The restart gate excludes owner delivery while it is held (`restart_gate.rs`).
