# TUI O writer canary runbook

This runbook covers the first O writer canary: one new channel moved to the O writer by
[`tui_o.writer.channels`](tui-o-writer-channels.md). It applies to a build whose `O_TUI_WRITER`
switch is `true`, as this build is. With an empty list every channel stays on Legacy.

Managed drain and handback to Legacy are not implemented. The canary is one way: a canary channel
is never removed from the list and never added again. The next canary uses a different new channel.

## 1. Pick the channel

Use a Discord channel created for the canary. On the gateway node all of these must hold, or the
channel is not adopted:

- The channel is registered in `agents[].channels` with a Claude or Codex TUI runtime.
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
  transcript and no bind is pending. A Codex channel's transcripts must all still be empty (0 bytes,
  same file). A Claude channel's may hold output under the checks of §3.1.
- `<runtime_root>/o_store/<channel>` does not exist.

The TUI writes its transcript and the binding log binds it only at the first prompt, so a Claude
canary starts with a warm-up turn:

1. With the channel not in `tui_o.writer.channels`, send it one prompt. Legacy posts the reply.
2. Wait until that reply is posted and the turn is over. Then confirm the checks above for the
   channel: no Legacy inflight state, no pending TUI-direct start, no terminal delivery custody
   record and no open `intake_outbox` row.
3. Apply (§2) and confirm (§3). The `init` must list the transcript with `delivery_start` at its
   length at the restart.

Legacy's cursor lives only in memory (`tui_prompt_dedupe` state). At boot the rehydrate pass sets
it from the launch script to the transcript's length, and O starts there.

Only the canary must stay quiet from step 2 to the confirmation; other channels may keep working
through the restart. A message to the canary in that window lets its placement or first Legacy
body take the channel for that process: it stays on Legacy until the next restart judges it again.

## 2. Apply

1. Add the channel ID to `tui_o.writer.channels` in `agentdesk.yaml` on every node. All nodes must
   carry the same list. Saving the file only marks the change as restart-required.
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
- Earlier transcripts the log bound total at most 64 files and 128 MiB (`past sources exceed
  budget`).
- Nothing moved before the `init`: the log, the transcripts' length and mtime, and no Legacy
  response tail runs for the session.

An open last turn does not end the adoption, unless the channel also has sessions on another node
or a node override (those release it first). The channel stays on Legacy, logged as
`adoption waits for Legacy`, and its writer host looks again every 5 seconds without a restart. It
reads the transcript again only once the reason it waited on may have cleared: the transcript
changed, Legacy's cursor reached its end, the frontier moved or the binding log moved. It adopts
once all of these hold, logged as `deferred adoption committed`:

- The transcript has not changed for 10 seconds.
- Legacy holds no inflight row, custody, pending start, response tail, mailbox turn, queued
  intervention or pending dispatch for the channel, and is not emitting a terminal delivery.
- Every check above passes, and in addition nothing past Legacy's frontier is left undelivered:
  a deferred adoption abandons nothing. While a record is past the frontier it keeps waiting.

A deferred channel is released for good, with `adoption held: <reason>`, if the reason is final
(sessions on another node, a node override, a non-authoritative delivery record, the budget) or if
the binding log bound another transcript while it waited (`was bound while the adoption
waited`), since Legacy may still owe output it read from that one. A channel whose debt never clears stays on Legacy
until the next restart, which adopts it as above.

Before editing the list, run both cross-node checks below by hand and keep their output in the lane
log. If either fails, stop the expansion; neither may be skipped.

1. Panes (F4): on every node, list the tmux sessions and the Legacy binding for each channel being
   added. A pane for it on any node other than the O home stops the expansion.
2. Selection (F5): diff `tui_o.writer.channels`, `tui_o.writer.all_tui`, the TUI bindings in
   `agents[].channels`, `cluster.gateway_preferred_instance_id` and `cluster.instance_id` across
   the nodes. Refuse the deploy if any node's list drops a channel
   another node selects. After the restart, every channel with `<runtime_root>/o_store/<channel>/init`
   on the O home must still be in its selection: the home has no `tui_o:selection_missing` reason.

Confirm as in §3. The channel's `init` lists its current transcript with `delivery_start` at
Legacy's cursor, which is that transcript's length when it was adopted, and each earlier transcript
at its length. Replace these manual checks with the automated check once it lands.

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

Watch the canary for 50 turns with an oracle independent of the writer:

- Silent missing: every assistant text unit in the transcript appears in the channel, or its
  withheld state is visible as an alarm.
- Unapproved duplicates: no unit is posted twice without an ambiguity alarm.
- Legacy body effect: Legacy posts no body text in the canary channel (count 0).

When all three hold across the 50 turns, move to the expansion step without further waiting.

Other channels stay on Legacy throughout. Check that their delivery is unchanged.

## 6. Forbidden during the canary

- Removing the canary channel from the list, or adding a removed channel back.
- Emptying the selection, or turning `all_tui` off, once a channel it selected was adopted.
- Deleting `o_store`, `o_era` or a channel directory. A fully deleted store cannot be told apart
  from a first install; restore it from an external backup instead.
- Running nodes with different lists.
- Deploying an external binary (`AGENTDESK_DEPLOY_BINARY`) while the source switch is `true`, or
  rolling back to a build whose manifest does not record `o_tui_writer` as `true`. The deploy
  script refuses both.
