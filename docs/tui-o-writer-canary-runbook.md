# TUI O writer canary runbook

This runbook covers the first O writer canary: one new, empty channel moved to the O writer by
[`tui_o.writer.channels`](tui-o-writer-channels.md). It applies to a build whose `O_TUI_WRITER`
switch is `true`, as this build is. With an empty list every channel stays on Legacy.

Managed drain and handback to Legacy are not implemented. The canary is one way: a canary channel
is never removed from the list and never added again. The next canary uses a different new channel.

## 1. Pick the channel

Use a Discord channel created for the canary. Do not use a channel that already carries a
conversation. On the gateway node all of these must hold, or the channel is not adopted:

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
  transcript, every transcript it names is still empty (0 bytes, same file), and no bind is pending.
- `<runtime_root>/o_store/<channel>` does not exist.

The TUI session must be running on the gateway with an empty transcript before the restart. Only
the canary must stay quiet; other channels may keep working through the restart. Do not send a
message to the canary before activation: its placement or first Legacy body cancels the adoption
for that process, the channel stays on Legacy, and its grown transcript blocks later adoptions.

## 2. Apply

1. Add the channel ID to `tui_o.writer.channels` in `agentdesk.yaml` on every node. All nodes must
   carry the same list. Saving the file only marks the change as restart-required.
2. Restart the gateway node through the managed restart path. The new list applies at boot only.
3. Restart any standby or runner node the same way, so no node keeps the old list.

## 3. Confirm activation

- The release log has `[tui_o] writer host created the channel's init` for the channel, once.
- `<runtime_root>/o_store/o_era` exists (first canary only) and
  `<runtime_root>/o_store/<channel>/init` exists and lists the empty transcript with
  `delivery_start` 0.
- The release log has no `[tui_o] writer host held the channel` line for the channel, and
  `/api/health` has no `tui_o:halted:<channel>` reason. `tui_o:paused_no_gateway:<channel>` is
  expected only until the gateway lease is owned.
- The next restart logs no new init line: the stored init is recovered, never written again.

Activation waits, without a hold, until the gateway lease is owned; facts read while the lease was
lost are read again once it is owned. With clustering it uses `cluster.instance_id` and stops if
the cluster bootstrap published another id; without clustering it waits up to 10 seconds for the
published id and stops with `this node's instance id is not published yet` otherwise.

When activation stops, the log line and `tui_o:halted:<channel>` name the reason, for example
`first activation: 1 open intake rows` or `first activation: source ... already holds N bytes`.
A stop before any store write releases the channel: Legacy keeps its output for this process. A
stop after a store write holds it: output stays withheld and Legacy does not take it over. Do not
delete store files or edit the list to retry. Leave the channel in the list and start again with
another new channel.

A held store is never initialized again: an era channel whose `init` is missing or damaged, an
`init` without `o_era`, or a channel directory without `init` all hold.

### Expanding to a channel that already carries a conversation

After the first canary, a Claude TUI channel whose transcript already holds output may be added the
same way. Codex channels with output are not adopted: they stay on Legacy. O starts at the cursor
Legacy reads that transcript from in the new process, so neither writer skips or repeats a byte.
Every check of §1 still applies except the empty-transcript one; in its place all of these must
hold after the restart, or the channel stays on Legacy for that process with
`adoption held: <reason>`:

- Legacy's first rehydrate pass ran within 60 seconds (`legacy cursor not established` otherwise),
  and it holds a cursor on the transcript the binding log bound last.
- The transcript ends at that cursor on a line boundary, and its last turn is closed: no user or
  assistant record follows the last turn end.
- The delivery record is authoritative and its frontier covers that turn end without passing the
  cursor (`frontier F is outside T..=R`).
- Earlier transcripts the log bound total at most 64 files and 128 MiB (`past sources exceed
  budget`).
- Nothing moved before the `init`: the log, the transcripts' length and mtime, and no Legacy
  response tail runs for the session.

Before editing the list, run both cross-node checks below by hand and keep their output in the lane
log. If either fails, stop the expansion; neither may be skipped.

1. Panes (F4): on every node, list the tmux sessions and the Legacy binding for each channel being
   added. A pane for it on any node other than the O home stops the expansion.
2. Selection (F5): diff `tui_o.writer.channels`, `cluster.gateway_preferred_instance_id` and
   `cluster.instance_id` across the nodes. Refuse the deploy if any node's list drops a channel
   another node selects. After the restart, every channel with `<runtime_root>/o_store/<channel>/init`
   on the O home must still be in its list.

Confirm as in the first canary, except that the channel's `init` lists its current transcript with
`delivery_start` at Legacy's cursor, which is that transcript's length when it was adopted, and each
earlier transcript at its length. Replace these manual checks with the automated check once it
lands.

## 4. Emergency stop

When output must stop without a drain, stop the provider's gateway process through the managed
stop path. Shutdown closes the O ownership gate before the gateway lease is released, so no new O
POST starts; in-flight results are settled from the O ledger on the next start.

- Keep the channel in `tui_o.writer.channels`. Removing it would hand the channel to Legacy on the
  next boot; that handback is not supported.
- Do not start Legacy delivery for the channel by any other means.
- A node other than the O home adopts nothing. If it takes the gateway lease it holds new messages
  for the canary with `served only on O home <id>`; keep no TUI session for the canary there,
  since such a session's output would go through Legacy.

## 5. Observe

Watch the canary for at least 72 hours and 50 turns with an oracle independent of the writer:

- Silent missing: every assistant text unit in the transcript appears in the channel, or its
  withheld state is visible as an alarm.
- Unapproved duplicates: no unit is posted twice without an ambiguity alarm.
- Legacy body effect: Legacy posts no body text in the canary channel (count 0).

Other channels stay on Legacy throughout. Check that their delivery is unchanged.

## 6. Forbidden during the canary

- Removing the canary channel from the list, or adding a removed channel back.
- Deleting `o_store`, `o_era` or a channel directory. A fully deleted store cannot be told apart
  from a first install; restore it from an external backup instead.
- Running nodes with different lists.
- Deploying an external binary (`AGENTDESK_DEPLOY_BINARY`) while the source switch is `true`, or
  rolling back to a build whose manifest does not record `o_tui_writer` as `true`. The deploy
  script refuses both.
