# TUI O writer canary runbook

This runbook covers the first O writer canary: one new, empty channel moved to the O writer by
[`tui_o.writer.channels`](tui-o-writer-channels.md). It applies only to a build whose
`O_TUI_WRITER` switch is `true`. While the switch is `false`, listing a channel changes nothing.

Managed drain and handback to Legacy are not implemented. The canary is one way: a canary channel
is never removed from the list and never added again. The next canary uses a different new channel.

## 1. Pick the channel

Use a Discord channel created for the canary. Do not use a channel that already carries a
conversation. On the gateway node all of these must hold, or activation holds the channel:

- The channel is registered in `agents[].channels` with a Claude or Codex TUI runtime.
- The provider's gateway runs on a node with the PG gateway lease. Without PG the channel is held.
- `intake_outbox` has no `pending`, `claimed`, `accepted`, `spawned` or `dispatched` row for it.
- `sessions` has no live row for it owned by another node (`status` other than `disconnected` or
  `aborted` with a different `instance_id`).
- There is no `/node` override for the channel and its agent has no `default_execution_node_id`.
- There is no Legacy inflight state for the channel and no retained terminal delivery custody
  record naming it (`<runtime_root>/discord_terminal_delivery_custody`). A custody record that does
  not parse also holds activation.
- The channel's binding log (`<runtime_root>/binding_events/<channel>.log`) binds at least one
  transcript, every transcript it names is still empty (0 bytes, same file), and no bind is pending.
- `<runtime_root>/o_store/<channel>` does not exist.

The TUI session must be running on the gateway with an empty transcript before the restart. Do
not send a message to the channel before activation: its first turn would grow the transcript and
activation would hold.

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

When activation holds, the log line and `tui_o:halted:<channel>` name the reason, for example
`first activation: 1 open intake rows` or `first activation: source ... already holds N bytes`.
Output of that channel stays withheld; Legacy does not take it over. Do not delete store files or
edit the list to retry. Leave the channel in the list and start again with another new channel.

A held store is never initialized again: an era channel whose `init` is missing or damaged, an
`init` without `o_era`, or a channel directory without `init` all hold.

## 4. Emergency stop

When output must stop without a drain, stop the provider's gateway process through the managed
stop path. Shutdown closes the O ownership gate before the gateway lease is released, so no new O
POST starts; in-flight results are settled from the O ledger on the next start.

- Keep the channel in `tui_o.writer.channels`. Removing it would hand the channel to Legacy on the
  next boot; that handback is not supported.
- Do not start Legacy delivery for the channel by any other means.
- A standby that takes the gateway lease has no store for the channel and holds it.

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
