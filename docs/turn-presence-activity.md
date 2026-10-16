# Turn presence: Full, Tail, and Unbound

Turn activity is an observation used by the existing input and automatic queue
gates. An Idle observation does not bypass routing, host readiness, mailbox
ownership, pending/lease persistence, input fences, external inflight rows, or
queue eligibility. It never deletes a transcript, clears an execution row, or
replaces a session.

## Tail switch and size boundary

`ADK_TURN_PRESENCE_TAIL` is read once per process. Unset and exactly `1` enable
Tail; exactly `0` disables it. Other values, including invalid UTF-8, disable Tail
and produce a warning. Changing the value requires a process restart. Direct
reads and supervisor startup use the same frozen value.

A cold transcript at or below 256 MiB uses Full. A cold transcript above that
boundary uses a 32 MiB observation window when Tail is enabled. Tail disabled
leaves that cold source Unknown. A Full reader already installed continues
reading when its file grows past 256 MiB.

Tail discards an initially cut record through its LF and preserves the final
partial record. Its bootstrap snapshot is not the current EOF. Effect-point
polls read at most 4 MiB of new bytes, so a growing transcript may stay
`catching_up` across several reads. A typed resume carries its mode, evidence,
and remaining bootstrap budget and validates the old observation before
handing its evidence to the new reader.

Codex Tail needs an observed opener and its matching closer. A closer alone,
a foreign closer, an anonymous boundary, or a replaced source cannot reuse an
older opener. Claude retains the existing strict end-envelope rule. Neither
provider uses a ready pane to turn an unproven Tail into Idle.

## Unbound and expected Unknown results

Watcher absence first invalidates the old observation and pending worker.
Unbound then collects channel-name candidates, binding/inflight evidence,
host evidence, and an inventory of the namespace a start would use. A missing
watcher alone is insufficient evidence of absence.

A local first start requires a complete inventory: a verified missing socket,
a verified server absence, or a complete session list whose ownership can be
accounted for. A registered local target with no name additionally requires an
empty inventory and no previous execution evidence. Missing binaries, timeouts,
unreadable sockets, ambiguous ownership, and contradictory host evidence hold
the channel. Herdr first-create and Retired cases use their host evidence and
retain related tmux/owner conflicts.

Completed Unbound observations have a publication stamp. Before publication,
the adapter rechecks its file/host inputs, exact and canonical DB reads, and a
fresh inventory in the same namespace, then checks the Watch generation and
revision. Runtime-kind restoration keeps canonical-first, legacy-on-NotFound
reads; host-kind stays canonical-only. Historical Process, foreign host, or
unreadable marker evidence remains a hold. Superseded work
cannot publish or install an old result. An Unbound result supplies no source
identity or Busy effect permission.

Expected Unknown reasons include:

- `catching_up`: the reader has not reached the current EOF.
- `facts_resumed`: a typed handoff is awaiting sufficient new evidence.
- `no_turn_boundary`: Tail cannot prove the current turn closed.
- `facts_halted`: replacement, shrink, corruption, or failed continuity.
- `watcher_unbound`: related execution evidence still exists.
- `session_identity_unresolved`: candidates, ownership, or host inputs disagree.
- `session_probe_failed`: absence could not be observed reliably.
- `superseded`: a newer observation or changed input invalidated this reading.

These holds preserve the input or queued head. Elapsed time alone does not
convert them to Idle. `no_session` may describe verified local absence, Herdr
first-create, or Herdr Retired; the diagnostic classification distinguishes them.

## Verification before deployment

The coordinator must record both Macs separately. Passing WSL tests or one Mac
does not establish the other Mac's binary, namespace, or host configuration.
Use the final PR's named T11/T16 tests and raw evidence; a filter selecting zero
tests, a skipped PG case, or `Started` without the selected execution I/O count
is not a passing result.

| Check | Mac 1 | Mac 2 |
|---|---|---|
| Actual tmux absolute path/version and `-N` support | pending | pending |
| UID and default/explicit namespace match actual start | pending | pending |
| C-locale missing socket and stale socket classifications | pending | pending |
| Probe leaves an absent socket uncreated; no server starts | pending | pending |
| Local first start reaches selected execution I/O exactly once | pending | pending |
| Exact-local key, canonical row, marker, locator, host configuration agree | pending | pending |
| Herdr first-create and Retired select Herdr I/O once, tmux I/O zero | pending | pending |
| Related live/restored-owner conflicts preserve input and queue | pending | pending |

T11 must cover both an explicit isolated namespace and a fresh default namespace
under an isolated `TMUX_TMPDIR`, with `TMUX` and `TMUX_PANE` unset for the default
case. Observation and actual start must use the same binary, UID, namespace, and
target name. Observe before and after the probe, then verify creation occurs
only at the start boundary. The client may create the private parent directory;
the socket and server are the creation witnesses.

Run the local first-start proof with the pipeline skill installed on that host:

```sh
unset TMUX TMUX_PANE
export CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none
bash "$PIPELINE_SKILL/scripts/pg_throwaway.sh" cargo test --lib \
  s1_real_missing_namespace_reaches_selected_tmux_start_pg -- --nocapture
```

Require a nonzero selection and the selected driver I/O count, in addition to
`Started`. The fixture uses a private socket and FIFO-controlled session lifetime;
it sends no keys to an operational pane. Keep the full command, binary checksum,
counts, environment, and raw log with the PR evidence.

T16 uses actual queued kickoff, intake, and host selection. Human headless
delivery has an existing configured-Herdr refusal, so it is not the Herdr
positive path. Keep the existing injection and queued-head policies unchanged.
Record the positive execution count beside every negative zero-effect case.

```sh
unset TMUX TMUX_PANE
export CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none
bash "$PIPELINE_SKILL/scripts/pg_throwaway.sh" cargo test --lib \
  s1_herdr_first_create_and_retired_use_herdr_pg -- --nocapture
```

The automated fixture uses the production Herdr executor and native Unix peer
provenance over a private scripted socket. It installs the boot registry on the
actual blocking worker, with consistent O ownership and writer-readiness
fixtures. First-create and Retired each require one pane input and no tmux
execution. Related live, restored-owner, historical, Pending/Bound, and marker
conflicts preserve the queued head with no pane input. The socket fixture does
not run a provider process; the two Mac checks still require the deployed Herdr
server and provider runtime.

For a large operational transcript, use read-only metadata and existing activity
diagnostics to record SourceId, binding seq, length, window start, read-through,
current EOF, and whether a new opener or partial EOF remains. Do not change the
file, binding, queue, or execution row to manufacture Idle. Do not dump transcript
or pane contents into the verification record.

I/O evidence has three independent parts: initial/resume whole-prefix hash
count zero, observed tail/guard/partial and bounded Codex-header read ranges,
and no bootstrap-budget refill on resume. Keep the Full hash positive control.
Do not claim the file header is never read or total physical I/O is exactly
32 MiB; guards, partial revalidation, and the Codex parent header are separate.

## Queue progress and rollback

After both Mac checks, the coordinator verifies AC7 on an eligible large Idle
channel: the former size hold clears, the exact eligible head progresses, and
new human input reaches its selected execution boundary. Also check capped
eligibility, pending/lease state, active tokens, external rows, routing/recovery
and host holds, and already answered entries. Success does not mean all Unknown
results disappear or that duplicate queue entries are removed.

To disable cold Tail, the coordinator sets `ADK_TURN_PRESENCE_TAIL=0` in the
existing startup environment and restarts the process. Unbound protection,
verified first-start behavior, host separation, stale-reading rejection, and
installed Full readers remain active. Tail off is not a rollback of all S1.
Deployment with Tail off still requires the T11/T16 checks. If the activation
itself is at fault, the coordinator assesses reverting S1 as a unit.
