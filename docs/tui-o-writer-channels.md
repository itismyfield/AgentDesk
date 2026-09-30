# TUI writer channel selection

`agentdesk.yaml` is the only membership source:

```yaml
tui_o:
  writer:
    channels: []
```

An omitted section or empty list selects no channels. IDs must be unsigned integers other than zero. Duplicate IDs become one member. Every selected ID must have a registered `agents[].channels` binding to a Claude or Codex TUI runtime; invalid runtime settings and conflicting provider bindings reject startup.

The process validates and captures this membership at boot. Reloading the file reports `tui_o.writer.channels` as restart-required and keeps the applied membership unchanged, including when the pending file is saved again. Reverting the file to the applied list clears that pending change. The snapshot is the shared policy input for output ownership and for the gateway writer host, which creates a new channel's first store only when the checks in the [canary runbook](tui-o-writer-canary-runbook.md) pass.

Output responsibility is `O_TUI_WRITER && channels.contains(destination) && TUI(kind)`. The destination is the actual Discord channel, independent of any watcher lease owner. The build switch is on: an omitted or empty list leaves every channel on Legacy, and a listed channel moves to O only through the checks in the canary runbook.

This setting does not provide managed drain, lossless handback, or safe reuse of a previous canary channel. The initial canary uses a disposable new channel; see the [canary runbook](tui-o-writer-canary-runbook.md).
