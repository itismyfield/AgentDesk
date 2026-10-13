# TUI O writer canary runbook

This runbook covers the first O writer canary: one new channel moved to the O writer by
[`tui_o.writer.channels`](tui-o-writer-channels.md). It applies to a build whose `O_TUI_WRITER`
switch is `true`, as this build is. With an empty list every channel stays on Legacy.

The first N1 run uses Claude's completed warm-up and historical adoption path. Writer activation
and N1 turn-mode confirmation are separate gates. Code coordinates below refer to
`945aac2668ce010b92053fdb39f541ae51629e20`; recheck them for the deployed build.

Managed drain and handback to Legacy are not implemented. The canary is one way: a canary channel
is never removed from the list and never added again. The next canary uses a different new channel.

## 1. Pick the channel

Use a Discord channel created for the canary. On the gateway node all of these must hold, or the
channel is not adopted:

- The first N1 channel is registered in `agents[].channels` with a Claude TUI runtime.
- The gateway node is the O home: without clustering every node is; with clustering both
  `cluster.instance_id` and `cluster.gateway_preferred_instance_id` are set and equal on it. A
  clustered node with a non-empty list and either id unset refuses to boot.
- The provider's gateway runs on a node with the PG gateway lease. Without PG the channel is held.
- `intake_outbox` has no `pending`, `claimed`, `accepted`, `spawned` or `dispatched` row for it.
- `sessions` has no live row for it owned by another node (`status` other than `disconnected` or
  `aborted` with a different `instance_id`).
- There is no `/node` override for the channel and its agent has no `default_execution_node_id`.
- There is no Legacy inflight state for the channel, no pending TUI-direct start for it
  (`<runtime_root>/discord_tui_direct_pending_start`) and no retained terminal delivery custody
  record naming it (`<runtime_root>/discord_terminal_delivery_custody`). A custody record that does
  not parse also blocks adoption.
- The channel's binding log (`<runtime_root>/binding_events/<channel>.log`) binds at least one
  transcript and no bind is pending. The Claude transcript may hold output under the checks of §3.1.
- `<runtime_root>/o_store/<channel>` does not exist.

Codex writer activation requires an already bound, unchanged 0-byte source, but its N1 native
reader requires real session metadata. A normal native path providing both has not been verified.
Do not create or truncate a transcript to manufacture that state; Codex is excluded from the first
N1 run (P12: `src/services/tui_o/writer/activation.rs:244–276`,
`src/services/tui_o/writer/input_facts.rs:87–100`,
`src/services/codex_tui/rollout_index.rs:240–280`).

The TUI writes its transcript and the binding log binds it only at the first prompt, so a Claude
canary starts with a warm-up turn:

1. With the channel not in `tui_o.writer.channels`, send it one prompt. Legacy posts the reply.
2. Wait until that reply is posted and the turn is over. Then confirm the checks above for the
   channel: no Legacy inflight state, no pending TUI-direct start, no terminal delivery custody
   record and no open `intake_outbox` row.
3. Apply (§2) and confirm (§3). The `init` must list the transcript with `delivery_start` at its
   length at the restart.

Legacy's cursor lives only in memory (`tui_prompt_dedupe` state). At boot the rehydrate pass uses
the launch script's transcript length. Confirm the actual cursor, EOF, closed native turn and
authoritative delivery frontier; a restart alone does not establish these facts
(`src/services/discord/tui_prompt_relay/launch_script.rs:104–120`,
`src/services/tui_o/writer/adoption.rs:1–270`).

Only the canary must stay quiet from step 2 to the confirmation; other channels may keep working
through the restart. A message to the canary in that window lets its placement or first Legacy
body take the channel for that process: it stays on Legacy until the next restart judges it again.

## 2. Apply

1. Add the channel ID to `tui_o.writer.channels` in `agentdesk.yaml` on every node. All nodes must
   carry the same list. Saving the file only marks the change as restart-required.
   Separately select this channel in `tui_o.turn.channels`, keeping `tui_o.turn.all_owned: false`
   for the single-channel run. Its default is an empty list with `all_owned: false`, so writer
   selection alone does not enable N1. Verify the effective boot selection on every producer
   (`src/services/tui_o/turn_mode.rs:8–80`).
2. Restart the gateway node through the managed restart path. The new list applies at boot only.
3. Restart any standby or runner node the same way, so no node keeps the old list.

To select every TUI channel at once, set `tui_o.writer.all_tui: true` and leave
`tui_o.writer.channels` empty or absent; a file with both refuses to load. Every binding in
`agents[].channels` that resolves to a Claude or Codex TUI is selected and the rest are skipped.
Apply it as above: the same file on every node, then managed restarts. No redeploy is needed. Each
TUI channel is still adopted only through §1 and §3.1; the others stay on Legacy for that process.
The selection is taken from `agents[].channels` at boot, so every node needs the same agent
bindings as well.

Every node must keep an adopted channel selected for as long as it exists. On the O home a channel
with `<runtime_root>/o_store/<channel>/init` (or named by `o_era`) stays on O even when the
selection drops it, but that only protects the home's own restart. A standby whose selection
dropped it serves it as an ordinary Legacy channel once it takes the gateway lease, and an empty
selection (no list and no `all_tui`, or `all_tui` with no TUI binding) turns O off everywhere,
committed channels included. Both are unsupported handbacks.

If the home logs `[tui_o] committed channel missing from writer selection` and `/api/health` shows
`tui_o:selection_missing:<channel>`, the channel is still on O but its selection is wrong:

1. Put the channel back in the selection (the list, or a TUI binding under `all_tui`) on every
   node, so every node selects the same set again. Do not delete its store.
2. Restart the standby and runner nodes first, then the home, through the managed path. Restarting
   the home while a standby still lacks the channel lets that standby serve it through Legacy.
3. After the restart the reason is gone and the log line does not repeat.

A startup refused with `o_store: channel <id> is not registered` or `is not TUI` means a committed
channel lost its TUI binding. Restore the binding; do not delete the store to get past it.

## 3. Confirm activation

These checks establish writer activation. Before measuring N1, also require effective turn
selection, successful retirement of the channel's synthetic population, and a same-boot,
same-channel `n1_mode_confirmed` event with `actual_confirmed_at_us` inside its confirmation
window (§5). A failed or partial retirement leaves Legacy turn-mode active. An `init` alone
does not authorize N1 measurement (P33: `src/services/tui_o/turn_mode.rs:8–80`,
`src/services/discord/tui_direct_pending_start/turn_retirement.rs:172–240`,
`src/services/tui_o/n1_observation.rs:93–100`).

- The release log has `[tui_o] writer host created the channel's init` for the channel, once.
- `<runtime_root>/o_store/o_era` exists (first canary only) and
  `<runtime_root>/o_store/<channel>/init` exists and lists the current transcript with
  `delivery_start` at Legacy's cursor: 0 for an empty transcript, its length at the restart after
  a warm-up.
- The release log has no `[tui_o] writer host held the channel` or `left the channel to Legacy`
  line for the channel, and `/api/health` has no `tui_o:halted:<channel>` or
  `tui_o:released:<channel>` reason. `tui_o:paused_no_gateway:<channel>` is
  expected only until the gateway lease is owned.
- The next restart logs no new init line: the stored init is recovered, never written again.

Activation waits, without a hold, until the gateway lease is owned; facts read while the lease was
lost are read again once it is owned. With clustering it uses `cluster.instance_id` and stops if
the cluster bootstrap published another id; without clustering it waits up to 10 seconds for the
published id and stops with `this node's instance id is not published yet` otherwise.

When activation stops, the log line and the health reason name why, for example
`first activation: 1 open intake rows`, `adoption held: <reason>` (§3.1) or, for Codex,
`first activation: source ... already holds N bytes`.
A stop before any store write releases the channel as `tui_o:released:<channel>`: Legacy keeps
its output for this process and the deploy health gate does not block on it. A stop after a store
write holds it as `tui_o:halted:<channel>`: output stays withheld, Legacy does not take it over,
and deploys block. Do not delete store files or edit the list to retry. Leave the channel in the
list. A Claude channel released before any store write is judged again at the next restart (§3.1);
otherwise start again with another new channel.

A held store is never initialized again: an era channel whose `init` is missing or damaged, an
`init` without `o_era`, or a channel directory without `init` all hold.

### 3.1 A channel whose transcript already holds output

A Claude TUI channel whose transcript already holds output, the warm-up canary included, is added
the same way. Codex channels with output are not adopted: they stay on Legacy. O starts at the
cursor Legacy reads that transcript from in the new process, so neither writer repeats a byte.
Records Legacy left undelivered before that cursor are posted by neither: the adoption reports
their range once as `tui_o:abandoned:<channel>` (`Abandoned { source, from, to }`). Every check of
§1 still applies; in addition all of these must hold after the restart, or the
channel stays on Legacy for that process with `adoption held: <reason>`:

- Legacy's first rehydrate pass ran within 60 seconds (`legacy cursor not established` otherwise),
  and it holds a cursor on the transcript the binding log bound last.
- The transcript ends at that cursor on a line boundary, and its last turn is closed: no user or
  assistant record follows the last turn end.
- The delivery record is authoritative and its frontier ends a record at or before the cursor
  (`frontier F ends no record`). Legacy's reader can end a turn at its `stop_hook_summary`, so
  turn ends and TUI bookkeeping after it are not undelivered: `turn_duration`, `informational`,
  `last-prompt`, `ai-title`, `mode`, `permission-mode`, `atis-latch`, `cost-state`,
  `file-history-snapshot` and `hook_success` attachments. The first prompt or other record there
  starts the abandoned range; it no longer holds the channel.
- Earlier transcripts are pinned newest first within a 64-source scan and cumulative 128 MiB
  budget. Missing past sources are skipped and the byte budget stops further pinning; exceeding
  it does not reject the whole adoption. Inspect the actual pinned set rather than assuming it
  covers all history (P27/P28: `src/services/tui_o/writer/adoption.rs:592–620`).
- Nothing moved before the `init`: the log, the transcripts' length and mtime, and no Legacy
  response tail runs for the session.

An open last turn, delivery debt, lagging cursor, custody or other transient Legacy state can defer
adoption; sessions on another node or a node override release it first. The channel stays on Legacy, logged as
`adoption waits for Legacy`, and its writer host looks again every 5 seconds without a restart. It
reads the transcript again only once the reason it waited on may have cleared: the transcript
changed, Legacy's cursor reached its end, the frontier moved or the binding log moved. It adopts
once all of these hold, logged as `deferred adoption committed`:

- The transcript has not changed for 10 seconds.
- Legacy holds no inflight row, custody, pending start, response tail, mailbox turn, queued
  intervention or pending dispatch for the channel, and is not emitting a terminal delivery.
- Every check above passes, and in the normal clean branch nothing past Legacy's frontier is left
  undelivered. This branch abandons nothing; the stall-EOF branch below is a separate outcome.

A deferred channel is released for good, with `adoption held: <reason>`, if the reason is final
(sessions on another node, a node override or a non-authoritative delivery record) or if
the binding log bound another transcript while it waited (`was bound while the adoption
waited`), since Legacy may still owe output it read from that one.

Deferred adoption does not promise an unlimited wait. After 40 minutes without delivery progress
(soft stall), or the 80-minute frontier-progress cap (hard stall), the guarded non-late path may
handoff at EOF. The log is `Legacy delivered nothing through the stall; O starts at the source's
end`. Preserve the stall kind, source, frontier and abandoned ranges. Do not approve that sample
as a clean N1 baseline until the historical debt/abandon evidence is established; elapsed time or
`deferred adoption committed` alone is insufficient (P31:
`src/services/tui_o/writer/deferred.rs:19–28,325–428`).

Before editing the list, run both cross-node checks below by hand and keep their output in the lane
log. If either fails, stop the expansion; neither may be skipped.

1. Panes (F4): on every node, list the tmux sessions and the Legacy binding for each channel being
   added. A pane for it on any node other than the O home stops the expansion.
2. Selection (F5): compare writer and turn selections, the TUI bindings in `agents[].channels`
   and `cluster.gateway_preferred_instance_id` across the nodes. Check each node's distinct local
   `cluster.instance_id` and the home relationship; local IDs need not be identical. Refuse the deploy if any node's list drops a channel
   another node selects. After the restart, every channel with `<runtime_root>/o_store/<channel>/init`
   on the O home must still be in its selection: the home has no `tui_o:selection_missing` reason.

Confirm as in §3. The channel's `init` lists its current transcript with `delivery_start` at
Legacy's cursor, which is that transcript's length when it was adopted, and each pinned earlier
transcript at its length. Unpinned history is not covered by these entries: a later bind to an
unpinned source holds it Pending (`src/services/tui_o/writer/past_bind.rs:9–40`). Replace these
manual checks with the automated check once it lands.

## 4. Emergency stop

When output must stop without a drain, stop the provider's gateway process through the managed
stop path. Shutdown closes the O ownership gate before the gateway lease is released, so no new O
POST starts; in-flight results are settled from the O ledger on the next start.

- Keep the channel selected on every node and do not change the selection (the list or `all_tui`)
  while stopped. The O home keeps a committed channel its selection dropped, with the
  `tui_o:selection_missing:<channel>` reason, but a standby that takes the lease without it and
  an empty selection both hand it to Legacy; that handback is not supported.
- Do not start Legacy delivery for the channel by any other means.
- A node other than the O home adopts nothing. If it takes the gateway lease it holds new messages
  for the canary with `served only on O home <id>`; keep no TUI session for the canary there,
  since such a session's output would go through Legacy.

## 5. Observe

### 5.1 Establish capture paths before starting (R1, R2, R4)

Keep a capture plan with the real raw input path/API, producer/normalizer command, completeness
check and retained hash for every row below. Run R1, R2 and R4 before the first measured turn and
retain a baseline from the confirmed boot. If a production provenance, Legacy, residue, binding
normalizer or historical-debt producer cannot be identified and demonstrated, stop before
measurement. The raw binding and N1 logs alone do not supply these captures. Fixtures and manual
empty lists or zero counters cannot substitute for operational evidence (P44:
`scripts/n1_canary/manifest.py:11–53`, `scripts/n1_canary/evidence.py:107–130`,
`scripts/n1_canary/audit.py:14–60`; capture fields are in
[`scripts/n1_canary/README.md`](../scripts/n1_canary/README.md)).

All commands in this preflight read operational state. Save output only in a separate evidence
directory; do not write configuration, DB rows, stores, cursors or transcripts. Variables name
already established runtime root `R`, release root `REL`, boot configuration `CFG`, PID `PID`,
channel string `C`, API base `BASE` and agent `AGENT`. Use existing read-only authentication where
required; 401/403 and read failures do not mean absence.

R1 checks the node, deployed source provenance and actual running executable:

```sh
uname -s
ps -p "$PID" -o pid=,lstart=,comm=
jq '{repo_head, repo_dirty, build_profile, o_tui_writer,
     o_tui_writer_type: (.o_tui_writer | type)}' "$REL/runtime/release-source.json"
readlink "/proc/$PID/exe"
sha256sum "/proc/$PID/exe"
rg -n '^[[:space:]]*(tui_o|writer|turn|channels|all_tui|all_owned|cluster|instance_id|gateway_preferred_instance_id|default_execution_node_id):' "$CFG"
```

Check PID start identity before and after capture to detect restart/PID reuse. Current config text
is not proof of what that PID applied: connect it to effective boot policy and retirement logs.
The current oracle accepts Linux `/proc/<pid>/exe` proof only; another OS cannot use an installed
file path as its substitute. `init.build_digest` is not the build Git SHA
(`scripts/n1_canary/manifest.py:100–117`, `scripts/n1_canary/evidence.py:107–115`).

R2 reads health, the watcher and the N1 snapshot. Verify that the agent's default turn target is C:

```sh
curl -fsS "$BASE/api/health" | jq '{status, fully_recovered, degraded_reasons}'
curl -fsS "$BASE/api/channels/$C/watcher-state"
curl -fsS "$BASE/api/agents/$AGENT/turn" |
  python3 -c '
import json, sys
body = json.load(sys.stdin)
snap = (body.get("turn_presence") or {}).get("n1_observation") or {}
if snap.get("status") != "available" or str(snap.get("channel_id")) != sys.argv[1]:
    raise SystemExit("REFUSE: unavailable or wrong-channel snapshot")
print(json.dumps(snap, ensure_ascii=False))
' "$C"
rg -n '\[tui_o\].*(first activation|adoption|turn mode|Legacy delivered nothing through the stall|writer alarm)|delivery_record_authority: enabled' "$R"/logs/dcserver.stdout.log*
find "$R/logs" -maxdepth 1 -type f -name 'n1-observation.jsonl*' -print
```

Retain the actual snapshot object as `baseline`, including schema, boot, sequence high-water,
all nine counters, loss/error, open-attempt state and confirmation window. Require status
`available`, dropped events and sink errors 0, a healthy observer, `open_attempts_complete: true`
and no open attempt. Join `n1_mode_confirmed` and `actual_confirmed_at_us` to that same boot/channel.
Do not replace unavailable/null/incomplete state with 0. Preserve all needed rotations, not only
stdout grep or the latest JSONL file (queue 1024, 16 MiB rotation, three archives:
`src/services/tui_o/n1_observation/sink.rs:1–115`,
`src/services/tui_o/n1_observation.rs:110–175,240–500`). Health 200 or `fully_recovered: true` does
not replace inspecting the channel's degraded reasons (`src/services/tui_o/alarm.rs:197–400`).

R4 runs on every related node to establish panes, complete binding history, stores and source
identity. Listing panes grants no permission to send keys or stop sessions:

```sh
tmux list-panes -a -F '#{session_name} #{pane_id} #{pane_pid} #{pane_current_command}'
cat "$R/binding_events/$C.log"
find "$R/o_store/$C" -maxdepth 1 -type f -print
python3 -m json.tool "$R/o_store/$C/init"
```

Read `init` only if an existing store is present; distinguish missing from unreadable or damaged.
Use the binding's resolved native path as `TRANSCRIPT`. Read-only `os.fstat` on its open descriptor
must capture path, dev, ino, length and mtime; validate full JSONL boundaries and native turn
completion. A tail or first-record check is only an identity probe. Reconcile Legacy delivery
receipts/frontier and every historical/abandoned range with the pinned source set. No DB session
count alone establishes that another node has no pane.

The 16 manifest inputs and their required operational sources are:

| Input | Raw source and production/completeness check |
|---|---|
| `process_proof` | R1 PID/start identity, release build attestation and proc executable hash; normalize exactly to runtime fields plus `executable_path`. |
| `executable` | Bytes captured from the same stable `/proc/<pid>/exe`; hash agrees with proof and approved producer pair. |
| `transcript` | R4 resolved native file, full SourceId and exact prefix/window through closure; no generated or truncated native file. |
| `bindings` | Full R4 binding history, with a demonstrated normalizer to seq/boot/channel/provider/source/identity/status; all intervening transitions included. |
| `baseline` | R2 available snapshot after actual confirmation, before the first measured native turn; preserve its full counters and boundary. |
| `end_snapshot` | Same R2 API after the 50th turn and closure; same boot/channel, matching log high-water. Produced after measurement. |
| `log` | Dedicated R2 N1 JSONL plus rotations covering confirmation through closure, without sequence gaps or mixed boots. |
| `o_init` | R4 real `o_store/<C>/init`, including pinned sources and starts; preserve native store format. |
| `o_ledger` | R4 complete `ledger.jsonl` through closure, including historical and unresolved attempts; preserve native ledger format. |
| `legacy` | Actual input provenance ledger and authoritative delivery records, observed owner and allowed panel evidence; capture schema 1, full scoped inventory. |
| `inflight` | Real Legacy inflight population with all relevant records, independently inventoried through closure; scoped schema 1. |
| `pending` | Real pending TUI-direct population and its completeness evidence through closure; scoped schema 1. |
| `abort` | Actual abort/residue population and boundary evidence through closure; scoped schema 1. |
| `debt` | Historical sources, undelivered/abandoned ranges, individually checked baseline messages and full ledger hash; scoped schema 1. |
| `pg` | SELECT-only capture of TEXT-channel intake and lifecycle rows for the exact interval; collector normalizes channel to an integer. External captures must use that same form. |
| `discord` | Read-only history with proven page closure and visibility, plus individual Posted/baseline message lookups; one page or an empty response does not prove absence. |

Demonstrate the capture producers on real preflight data and record the baseline before starting.
The final `end_snapshot`, closure scopes and file hashes are completed afterward. Do not label a
partial preflight capture `complete: true`; every capture's final scope must bind boot, channel,
provider, SourceId, closing seq and closing offset. Positive per-physical-observation origin proof
must bind execution identity, native record digest and input kind. Missing PG rows do not prove
TTY/autonomous origin (`scripts/n1_canary/audit.py:14–60`).

### 5.2 Register the deployed producer before using PASS

The shipped `producer_capabilities` list is intentionally empty until a verified deployed pair
is registered. The coordinator registers the actual `945aac266` deployment only after deployment;
this lane does not invent an executable hash or enable unconditional support. Keep rejection of
every unregistered build/hash, old schema or false capability (P43:
`scripts/n1_canary/manifest.py:74–87`).

1. Retain the build's clean source attestation (`release-source.json` `repo_head` and
   `repo_dirty: "false"`) and the build/deployment record linking that SHA to the exact artifact.
   A current checkout SHA or edited JSON alone does not attest a binary's build.
2. In a separate producer verification run of those exact executable bytes, capture real schema 2
   `n1_outcome_unknown` and successful `n1_mode_confirmed` events with actual confirmation time
   and its window. Preserve that run's process proof and complete raw verification log/hash.
   The original log may interleave channels. Sequence increases per boot/channel, including
   across provider changes; preserve those rows without manual filtering. The observer owns
   sequence per channel (`src/services/tui_o/n1_observation.rs:128–142,205–268`).
   Both events must name the same boot/channel/provider; the unknown witness belongs to this
   verification run, not the clean 50-turn canary. Do not induce an unknown event in the canary
   to demonstrate the capability. Review the build's producer paths at
   `src/services/tui_o/n1_observation.rs:57–100,240–365` and
   `src/services/discord/tui_direct_pending_start/turn_retirement.rs:172–240`.
3. Run the read-only candidate tool against the captured proc executable. It checks SHA256,
   build/proc proof, schema and positive evidence for both capabilities and prints one JSON
   object; it neither executes the binary nor modifies the support table:

   ```sh
   PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$REPO/scripts" \
     python3 -m n1_canary.producer \
       --process-proof "$PRODUCER_PROOF" --executable "$CAPTURED_EXE" \
       --release-source "$RELEASE_SOURCE" --verification-log "$PRODUCER_LOG"
   ```

4. The coordinator reviews the retained attestation and verification captures, then inserts the
   printed object into `scripts/n1_canary/compatibility.json` `producer_capabilities` for the
   deployed oracle. Keep every other guard. Archive the approved table and recompute the oracle
   bundle hash in the manifest, because the table and tool are part of that hash. A rebuild with
   different bytes needs a new entry even at the same Git SHA. The registration/removal regression
   proves PASS with a verified entry and rejection after removal in isolated test copies.

The tool checks consistency of supplied captures; trusted build attestation and operational log
provenance remain required review evidence. Missing positive capability evidence keeps the run
blocked; toggling booleans by hand is not verification.

### 5.3 Sample and close the run

“50 turns” means 50 distinct completed native logical turns whose non-Discord input provenance
(`tty`, `monitor` or `autonomous`) is positively proved for every physical observation. Discord
round trips, aborted turns, rereads and the raw `closed_events` counter cannot fill the sample.
Pin `fiftieth_offset` to the 50th eligible turn's exact end and retain evidence through
`closure_offset`; do not replace that boundary with a later EOF (P40:
`scripts/n1_canary/audit.py:14–60`). Preserve the same boot/channel/source and continuous binding
and observation sequences from confirmation through closure.

Run the independent oracle on the finalized 16-input manifest:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 "$REPO/scripts/n1_turn_canary_oracle.py" --manifest "$MANIFEST"
```

Expand only on exit 0 / `PASS`: native/origin, delivery, effects, reverse Discord comparison and
historical-debt audit stages must all be complete, with no reasons or failures. Missing/duplicate
pieces, Legacy body effects, synthetic/placeholder attempts, unresolved historical debt or
unknown outcomes cannot be excused by an alarm or time spent waiting. Exit 1 / `FAIL` or 2 / `[U]`
stops expansion (`scripts/n1_canary/audit.py:220–313`,
`scripts/n1_turn_canary_oracle.py:79–127`). An approved producer entry alone is insufficient.

Other channels remain control samples: separately confirm their Legacy delivery. The target's
PASS does not prove their behavior. Broad `all_tui`/`all_owned` selection expands the operational
scope beyond this single-channel procedure (`src/services/tui_o/turn_mode.rs:8–80`).

## 6. Forbidden during the canary

- Removing the canary channel from the list, or adding a removed channel back.
- Emptying the selection, or turning `all_tui` off, once a channel it selected was adopted.
- Deleting `o_store`, `o_era` or a channel directory. A fully deleted store cannot be told apart
  from a first install; restore it from an external backup instead.
- Running nodes with different lists.
- Deploying an external binary (`AGENTDESK_DEPLOY_BINARY`) while the source switch is `true`, or
  rolling back to a build whose manifest does not record `o_tui_writer` as `true`. The deploy
  script refuses both.

## 7. Recover a piece Discord refused

A POST refused with 400, 403 or 404 blocks the channel's O output with a `Blocked` alarm. A restart
alone keeps it blocked. After the cause is fixed, one explicit approval lets the next writer send
the original piece once, then the output that followed. Run these on the O home, in order:

1. Note the channel and serial from the `Blocked` log.
2. `agentdesk o status --channel <CHANNEL_ID>` lists every refused piece with its latest attempt
   and approval (`absent`, `unconsumed` or `consumed by serial <n> <outcome>`). It writes nothing,
   but it holds the ledger lock while it reads, so do not run it during a restart.
3. Fix the cause of the refusal, for example the channel permission. Do not send a test POST.
4. `agentdesk o resume --channel <CHANNEL_ID> --rejected-serial <SERIAL> --reason "<what was
   fixed>"`. `--operator` defaults to `operator`. Only the latest refused attempt of a piece, with
   no POST in flight, can be approved. The approval sends nothing and does not check the cause.
5. Wait for the command to exit, then run the managed restart. The new writer sends the original
   piece, which gets a new serial, and continues with later output. Check the new serial's result.

`o resume` exits with:

| Exit | Meaning | Next step |
|---|---|---|
| 0 | This command wrote the approval and it is durable | Managed restart |
| 1 | No runtime root, or no `o_store` under it; nothing was written | Run it on the O home |
| 3 | The serial already had an approval, possibly from a command that ran at the same time; the first one stands, with its state | Nothing more to record |
| 4 | The store refused the request or the ledger check: not the latest 400/403/404 attempt, a POST in flight, blank input, a ledger violation or an unfinished tail; nothing was written | Run `o status`; an unfinished ledger tail is recovered by the next writer start |
| 5 | The ledger is locked by a writer or another operator; nothing was written | Retry once the other command or the restart is done |
| 6 | Any other error, such as store damage, a missing channel ledger or an I/O error, with the rollback floor confirmed absent, so this command appended no approval | Fix the cause the message names and retry |
| 7 | Any other error while the floor exists or cannot be checked, so the command cannot tell whether an approval is durable. It does not mean the append was reached | Run `o status`, then the same command again |

- A writer that starts while the ledger is locked holds the channel; the next managed restart
  recovers it.
- If the retry is refused again, its new serial needs a new approval. A retry whose result is
  uncertain is recorded and alarmed as any other POST, and the output continues.
- A piece Discord refuses with 400 again has no exit in this build: its payload cannot be changed
  or discarded. Escalate it as a separate fix.
- The first approval creates `<runtime_root>/o_store/operator_resume.floor`. From then on the deploy
  script refuses a source or a rollback target that does not record `o_ledger_operator_resume` as
  `true`. Fix forward instead, and never delete approval lines from the ledger to get past it.

Enable re-post for a canary with both `tui_o.repost.enabled: true` and an explicit
`tui_o.repost.channels: [<CHANNEL_ID>]` allowlist. An empty or missing `channels` list denies
all re-post work; other hosted channels continue normal fresh original delivery. Validated reloads
apply to the next grant and wire attempt, including a 429 retry, while an HTTP request whose first
poll already left may finish.

An enabled re-post turn reports retained uncertain backlog with
`[tui_o] re-post backlog retained`: `channel`, `count`, and `pieces` contain the serial, unit key,
piece index and reason, without the payload. The sidecar marks each serial before the log is
emitted; repeated turns, sidecar reopening and later activations do not repeat that report.
These pieces do not enter the shared re-post budget and are not automatically re-posted.
`[tui_o] re-post backlog report failed` blocks that re-post turn until the sidecar can be reopened
and the report recorded. A missing log line is not proof of an empty backlog: a serial may already
be marked reported, or the reporting step may have failed.

## 8. Roll back after re-post activation

An older binary that cannot read the `not_sent` ledger tag stops the channel writer with ledger
damage if it encounters that tag. Turning re-post OFF does not end an existing request or prevent
its terminal ledger append: a request waiting after HTTP 429 can append `not_sent` after OFF.
Follow these steps in order before selecting that older binary:

1. Confirm effective OFF.
   Establish the complete list of channels that have ever had re-post enabled, including channels
   no longer selected for activation. Confirm the effective re-post switch is OFF on every target
   node, using its validated live value rather than a default or a file edit alone. Keep it OFF on
   all nodes through the rollback decision.
2. Wait for in-flight requests and writer quiescence.
   Wait for existing requests to end and their terminal ledger appends to complete, then confirm
   the affected writer tasks have ended and joined with no ledger writes still possible. An HTTP
   in-flight count of zero alone does not prove writer quiescence. Keep the writers quiescent from
   the ledger inspection through the decision; do not restart them between those steps.
3. Inspect all activated channel ledgers.
   Read every ledger in the complete activation list at
   `<runtime_root>/o_store/<CHANNEL_ID>/ledger.jsonl`, using the runtime root that holds that channel.
   Run this check for each ledger and retain every result:

   ```sh
   rg -n '"type"[[:space:]]*:[[:space:]]*"not_sent"' '<runtime_root>/o_store/<CHANNEL_ID>/ledger.jsonl'
   ```

   Exit 0 means the tag is present; exit 1 means it is absent in that file; exit 2 or higher means
   the read failed. One tag-free file is not evidence for the other activated channels. Confirm
   each ledger was read completely and that every channel in the activation list was checked.
4. Decide whether the older binary may run.
   REFUSE the older binary if any ledger contains `not_sent`, any ledger cannot be read completely,
   or the full activation list cannot be established. Only complete, successful reads with zero
   `not_sent` tags across all activated ledgers allow this reader compatibility check to pass.
   The existing O writer rollback restrictions in §6 and operator-resume floor in §7 still apply.
   OFF does not remove existing tags. Do not delete or automatically convert `not_sent` records,
   or infer conversions from old Prepared/NotFound records. Keep a binary that supports the tag
   until a separately verified compatible recovery procedure is available.

This is a manual rollback procedure. A re-post rollback preflight in `scripts/deploy-release.sh`
is follow-up work and is not implemented here.

## 9. Resolve an unseen resume boundary

An in-session resume can bind a transcript the O writer has never seen. If that source has no
stored cursor or rotation link and no verified parent, its boundary is `Pending { candidates: [0] }`
and the writer raises `BoundaryPending`. The candidate `0` is not an approved delivery start.
While Pending, the source spools both history and new output but posts neither; its spool is
ineligible for GC. The alarm appears in `/api/health` as `tui_o:boundary_pending:<channel>` and
raises health to at least degraded. It reports withheld output, not proof of loss or duplication.

Run the following procedure on the O home:

1. Identify the channel and resumed source from the `BoundaryPending` alarm. Inspect the transcript
   and establish the first record that should be delivered after resume. Keep its exact byte start
   or unique record UUID, excluding the historical prefix while retaining all new owed output.
2. Record that start using the source's spool key or transcript path:

   ```sh
   agentdesk o boundary resolve --channel <CHANNEL_ID> --source '<KEY_OR_PATH>' --from '<BYTE_OFFSET_OR_UUID>' --operator '<NAME>'
   ```

   `--operator` defaults to `operator`. A `--from` that parses as an unsigned integer is a byte
   offset; otherwise it is a UUID. The source selector must match exactly one stored source,
   whose boundary must be Pending and whose file identity must still match. A numeric `--from`
   validates a line-start byte offset, not the existence of a complete record: newline-aligned
   EOF, offset 0 in an empty file, and the start of an incomplete trailing line can pass. UUID
   lookup requires exactly one matching complete JSON line. An already recorded resolution is
   refused with an instruction to restart the writer (`src/cli/o.rs:48–83`,
   `src/services/tui_o/store/rotation.rs:220–285`).
   Neither check proves the resume boundary; leave the source Pending unless the first owed
   position can be established.
3. Wait for the command to succeed, then use the managed restart path. The command only appends
   and fsyncs `BoundaryResolved`; it sends no output and applies at the next writer start. That
   start changes Pending to `Owed { from }` and replays the retained spool. Confirm that the
   historical range `[0, from)` is not posted and the owed output at or after `from` is delivered.

Automatic resume byte boundaries and automatic re-evaluation of existing resume Pending states
are not provided. Hook publication time, elapsed time and current EOF do not prove the resume
position. Reopening automatic boundary work requires a provider signal proving the exact resume
byte position or a separately verified boundary contract.

See the [TUI O resume contract](relay-state-contract.md#appendix-tui-o-unseen-resume-boundary)
for current-main code coordinates, acceptance-test assertions and the health follow-up.
