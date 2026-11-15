# CodexHerdrBootstrap

Model written against AgentDesk `3fa2ab95c8068bb0c4a58bc742f65f0db80c5a71` (2026-11-15, issue #8181 design, revision 2).
Counterexamples were replayed against that code; dispositions are below.

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
with its listing (`herdr_launch.rs:482-492`) and the marker publication (`:493-508`), keeps the listing
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
  ready writer the ordinary launch has no vacancy check at all (`herdr_launch.rs:497` stamps before the
  `:502` check), a pre-existing gap outside this design (design F1).
- Not modelled: the headless turn/start path (`turn_host.rs:191` executor_not_wired, issue #8125), non-canary
  channels, Legacy or missing rows, late hooks of an older nonce (`codex_hook.rs:333` checks the listed
  nonce; covered by test T6), and more than one front.

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
a ready edge (`host.rs:998-1012` wakes on `ready && !was_ready`). A V2 owner front is not re-driven by time
(`drive.rs:96-123`). Confirmed.

## Design1Listing: the retired nonce's listing refuses forever

CAS and `install_herdr_execution` list (n2, false) (`herdr_launch.rs:482-492`); the marker write fails and
removes its torn file (`tmux_session_files.rs:326-355`); `retire_unsent` returns the row to Retired
(`herdr_launch.rs:506-508,588-630`); nothing removes the listing (`adopt_skip.rs:203-260`). Revision 1's
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
