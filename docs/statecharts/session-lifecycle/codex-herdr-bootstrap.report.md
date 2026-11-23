# CodexHerdrBootstrap

Model written against AgentDesk `3fa2ab95c8068bb0c4a58bc742f65f0db80c5a71` (2026-11-15, issue #8181 design, revision 2).
Counterexamples were replayed against that code; dispositions are below. Rechecked against the S1
implementation on base `3cbd2b04f9`: `LaunchRefused` now ends the turn, as the launch's refusal does in
code, instead of holding and arming the front (see "Implementation check").

The Codex canary channel (`codex_tui::canary`, channel 1509350778043895902) runs on Herdr. Its hosted row
is Retired, its spawn nonce marker is gone, and the binding log still ends its codex ownership records with
a tmux-era `verified` context. Herdr launches write no ownership record (`source_policy` None), so that pin
is permanent.

## Why a new module

`HerdrSession` (Claude pane and row lifecycle) and `WriterWake` (writer park, boot custody, restart end) share
one `vars` tuple across 26 and 11 configs. This behaviour needs the verified pin, the spawn nonce marker, the
in-memory Herdr listing, O writer resumption, owner front holds and wakes, and the turn host gate together,
and touches neither model's existing variables. A separate module keeps every existing config unchanged. The
`StartLaunch` edge of `HerdrSession` (Retired row CAS to Pending) is the edge this model gates; see
`herdr.report.md`.

## What the model covers and what it does not

The model checks the admission rules against one front at a time. It splits the launch into the Pending CAS
with its listing (`herdr_launch.rs:484-494`) and the marker publication (`:493-508`), keeps the listing
relative to the row's nonce, and lets a held front move only on a real wake: the writer-ready edge, or the
bootstrap probe of a held_watch. A green config is a statement about these abstractions, not about the
implementation; the design's tests T1-T11 and their mutants are still required in full.

- Liveness assumes `SF` on Resume, WatchWake and Unplaced (the actor loop, the held_watch look and the
  resolver read again and again) and `WF(NextInput)` (a later input comes). `DesignNoNextFair` drops the
  last one: a launch that fails with no later input leaves the writer blocked (`WriterReady` breaks). The
  actor polls once a second; a resume window that opens and closes between polls is not proven to be seen
  (design residual R1).
- The restart witness is assumed readable (`actor.rs:338-356` Ok); design [U3] measures it.
- `TmuxAppears` is limited to the bootstrap window (no new incarnation, no marker, writer not ready). With a
  ready writer the ordinary launch has no vacancy check at all (`herdr_launch.rs:499` stamps before the
  `:502` check), a pre-existing gap outside this design (design F1).
- `LaunchRefused` abstracts a refusal before the Pending CAS only for safety (no second execution, no
  row change). It does not prove what happens to the front afterwards: whether that front is kept or
  settled, and whether it resumes on its own, is left to the implementation tests.
- Not modelled: the headless turn/start path (`turn_host.rs:191` executor_not_wired, issue #8125), non-canary
  channels, Legacy or missing rows, late hooks of an older nonce (`codex_hook.rs:333` checks the listed
  nonce; covered by test T6), and more than one front (see `CodexHerdrBootstrapFront` below).

## Implementation check

Rechecked against the S1 implementation. The rules above are the ones the code applies: placement
admits on the pin, the vacant target and the live actor (`intake_route/input.rs`); the turn host
plans a bootstrap only for the owner's exact retired row, an absent or withheld listing of that
nonce, a released hold and a seeded store (`turn_host.rs` `bootstrap_eligible`); the launch reads
the vacancy again inside `prepare`, under the fence and before the CAS (`codex/herdr_turn.rs`); a held
front is armed with the turn host's own judgement at the three refusal points (`owner_input.rs`): a
writer hold reads the channel's turn key, a turn host refusal reads the key that refusal used.
Two code facts sit outside the model and are covered by tests instead. The attach admits the new
execution and then reads the row again, withholding a retired one, so a Terminate between the
reconcile and the admission leaves no admitted retired execution. `retire_confirmed` withholds only
Codex executions; Claude's move back releases its own listing.

## Configs

Fix selects the rules. `design1` is revision 1 (old condition 4, no probe), `design` is revision 2.

| cfg | Fix | scope | result |
|---|---|---|---|
| `Before` | before (base code) | full | HeldStarts, WriterReady break: permanent refusal |
| `Design1WakeA` | design1 | cleanup pending at start, no failure, no restart | HeldStarts, WriterReady break: cleanup finishes after the refusal, no wake |
| `Design1WakeB` | design1 | writer booting at start, no failure, no restart | HeldStarts, WriterReady break: the actor registers after the hold, no wake |
| `Design1Listing` | design1 | one marker failure, no restart | HeldStarts, WriterReady break: the withheld listing of the retired nonce refuses every later bootstrap |
| `Design` | design | full | all rules hold |
| `DesignLive` | design | tmux may run at start or appear in the window | all safety rules hold |
| `DesignNoNextFair` | design | full, no later input assumed | WriterReady breaks (expected limit) |
| `MutCond4` | design with the old condition 4 | full | HeldStarts, WriterReady break (listing path, probe armed but never ready) |
| `MutNoWatch` | design without the probe | full | HeldStarts, WriterReady break (boot order path) |
| `MutNoVacancy` | design without the admission and launch vacancy checks | tmux may run at start | NoDualExecution, SourceInBoundary break |
| `MutNoRecheck` | design without the launch recheck only | tmux may appear in the window | NoDualExecution, SourceInBoundary break: admission, tmux appears, CAS |
| `MutPinDrop` | ignore the pin on a Herdr channel (rejected alternative) | tmux may run at start | NoDualExecution, ResumedOnlyWithEvidence, SourceInBoundary break |

`scripts/ci/statecharts_tlc.py --base 3fa2ab95c8` matched `expected.tsv` for all twelve.
Design and DesignLive reach 1,783 and 2,117 states, over the 500-state chart budget, so they have no chart.

## Before: HeldStarts and WriterReady break

Boot reaches first_drain, the writer stays blocked by the old pin, the input is held forever. Replay:
`actor.rs:357-361` resumes only while `sources_allowed()`; with `owed_delivery_ready` false that is
`deliver.rs:140-148`; `verified_panes` adds the canary pane (`codex_policy.rs:330-342`); `disposition` with an
absent marker returns VerifiedUnavailable from the durable pin (`:42-58`). Placement holds the input
(`intake_route/input.rs:97-112`, `fresh_evidence_missing`). Confirmed against the live log.

## Design1WakeA and Design1WakeB: no re-judging event

A: placement passes, the turn host refuses on the retired hold (OWriterNotReady is not a confirmed failure,
`start_failure.rs:16-25`), the front stays; cleanup then releases the hold, but old Retired cleanup wakes no
owner (`herdr_reader.rs:271-322`, test `rr_wake_old_retired_rows_at_boot_do_not_wake_owner_pg`). B: the first
drive holds the front before the actor registers (`owner_input.rs:173-191`); registering a blocked actor is not
a ready edge (`host.rs:893-907` wakes on `ready && !was_ready`). A V2 owner front is not re-driven by time
(`drive.rs:96-123`). Confirmed.

## Design1Listing: the retired nonce's listing refuses forever

CAS and `install_herdr_execution` list (n2, false) (`herdr_launch.rs:484-494`); the marker write fails and
removes its torn file (`tmux_session_files.rs:326-355`); `retire_unsent` returns the row to Retired
(`herdr_launch.rs:508-510,590-632`); nothing removes the listing (`adopt_skip.rs:203-260`). Revision 1's
condition "listed nonce differs from the row nonce" then refuses every later bootstrap in the process.
Confirmed.

## MutNoRecheck and MutNoVacancy

MutNoRecheck keeps the admission vacancy check and drops only the launch recheck: a tmux that appears between
the turn host and the CAS gets a second execution. MutNoVacancy drops both checks and fails from the start
state; it does not by itself prove the recheck.

## MutPinDrop

Ignoring the pin when the channel is Herdr-configured makes the writer ready with no current evidence, lets a
running tmux source publish, and lets the ordinary launch path start a second execution. This is why the
design keeps the pin and adds an admission instead.

## CodexHerdrBootstrapFront: a snapshot front ahead of a person input

Written against `c1ad7d1c7b` after review: a scheduled snapshot front keyed off its own label (Ks, no Herdr
row) ahead of a later person input (K, the exact Retired row). The main module keeps one front; this module
models the owner queue's order instead, so the twelve configs above are unchanged.

Key entry points (the only sources of the probe's key and of the start's key):

| what | where | key |
|---|---|---|
| person start, turn host judgement | `adk_session.rs:95` `turn_session_key`, `intake_turn.rs:926` | K |
| kept start's headless judgement | `headless_turn.rs:148-152` `headless_session_basis`, `entrypoints.rs:76-108` normalisation and `owner_start_session_key` | Ks for a snapshot or a fresh routine label, else K |
| writer hold probe | `owner_input.rs:179` (owner_drive), `:988` (person queued intake `DeferredWriterBoot`) | K |
| turn host refusal probe | `owner_input.rs:956` `bootstrap_probe(.., kept)` | the start's own key |
| queue rotation | `owner_input.rs:249-265` `prefer_person` on START_WAITS and BOOTSTRAP_WAITS, `v2.rs:125-151` | - |

`NoFutileRedrive` fails when a probe wakes a front whose own judgement is unchanged; `PersonStarts` asks that
a person input behind the front reaches its start or is removed; `SnapshotSettles` asks that the snapshot
settles once that person's bootstrap made the writer ready (it fails as ExecutorNotWired, #8125).

| cfg | Fix | result |
|---|---|---|
| `FrontBefore` | `c1ad7d1c7b`: probe on K, no rotation on BOOTSTRAP_WAITS | NoFutileRedrive, PersonStarts break: K reads Ready, the snapshot is driven again every look and the person never leads |
| `FrontDesign` | probe on the start's key, rotation on BOOTSTRAP_WAITS | all rules hold |
| `FrontMutKey` | rotation only | NoFutileRedrive breaks: with no person yet, the K probe re-drives the snapshot |
| `FrontMutPrefer` | front key only | PersonStarts breaks: the snapshot waits on Ks forever and the person stays behind it |
| `FrontMutPlaceFront` | design, but the writer hold probe also reads the front's key | PersonStarts breaks: a snapshot held by the writer hold never wakes when placement clears, and WRITER_HELD does not rotate |

`FrontMutPlaceFront` is why the writer hold sites keep K: that hold is the channel's placement, which lifts
when K could bootstrap; the snapshot's next drive then stops at its own turn host refusal and rotates.
`Remove` (Cancel, Terminate, clear, stop) removes either input at any point and `Restart` keeps the durable
order and drops the watch; both are in every config.
