# TUI writer channel selection

`agentdesk.yaml` is the only membership source:

```yaml
tui_o:
  writer:
    channels: []
```

An omitted section or empty list selects no channels, unless `all_tui` is set. IDs must be unsigned integers other than zero. Duplicate IDs become one member. Every selected ID must have a registered `agents[].channels` binding to a Claude or Codex TUI runtime; invalid runtime settings and conflicting provider bindings reject startup.

`all_tui: true` selects every `agents[].channels` binding that resolves to a Claude or Codex TUI runtime, by the same provider and runtime rules; other bindings are skipped, not rejected. It cannot be combined with a non-empty `channels` list, and a binding whose runtime is invalid, whose provider settings are ambiguous, or that is bound both as a TUI and as something else rejects startup. The selection is the TUI bindings at boot, so adding or retyping an agent channel changes it only at the next restart.

The process validates and captures this membership at boot. Reloading the file reports `tui_o.writer.channels` as restart-required whenever the selection changes, in either mode, and keeps the applied membership unchanged, including when the pending file is saved again. Reverting the file to the applied list clears that pending change. The snapshot is the shared policy input for output ownership and for the gateway writer host, which creates a new channel's first store only when the checks in the [canary runbook](tui-o-writer-canary-runbook.md) pass.

Output responsibility is `O_TUI_WRITER && channels.contains(destination) && TUI(kind) && adopted(destination)`, where on the O home `channels` is the selection plus every channel whose `o_store/<channel>/init` exists or that `o_era` names. Such a channel left out of a non-empty selection stays on O with the `[tui_o] committed channel missing from writer selection` log and the `tui_o:selection_missing:<channel>` health reason, and it must still be registered as a TUI or startup is rejected. This protects only the home's own restart: other nodes and an empty selection do not add it, so every node must keep it selected (see the runbook, §2). The destination is the actual Discord channel, independent of any watcher lease owner. Only the O home (`cluster.gateway_preferred_instance_id`, or every node without clustering) adopts: a channel whose store exists at boot is adopted, and a new one is adopted by its first `init` unless a placement or a Legacy body reached it first in that process. The build switch is on: an omitted or empty list leaves every channel on Legacy, and a listed channel moves to O only through the checks in the canary runbook.

This setting does not provide managed drain, lossless handback, or safe reuse of a previous canary channel. The initial canary uses a disposable new channel; see the [canary runbook](tui-o-writer-canary-runbook.md).

## Transcript turn mode

```yaml
tui_o:
  turn:
    channels: []
```

`tui_o.turn.channels` (or `all_owned: true`) selects channels whose direct and autonomous TUI turns stop creating Legacy synthetic turns: no inflight row, pending start, prompt anchor or lease, only the prompt notice, and a `/stop` with no Discord turn interrupts the session's open transcript turn. Discord-originated turns stay on Legacy. The list is read at boot; an empty or omitted list changes nothing. A selected channel is confirmed only when O owns its output: at boot for a channel whose store was committed, otherwise right after its first activation commits. Confirmation first retires the channel's synthetic rows, pending starts and abort markers; a channel whose retirement fails or leaves residue keeps Legacy turns until the next restart and logs `[tui_o] turn mode refused; Legacy keeps turns`. To turn it off, remove the channel and restart.
