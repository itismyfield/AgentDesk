# Release a stuck turn lease

After independently confirming that the provider finished its turn, capture the
lease from the dcserver that owns the channel:

```sh
agentdesk turn-lease inspect --provider codex --channel-id 1479671301387059200
agentdesk turn-lease release --expected '<exact JSON object from inspect>' --reason 'Operator verified the provider finished this turn'
```

The explicit release preserves the TUI, tmux process, provider session and
conversation context. It preserves queued message contents, ordering and pending
dispatch claims, then uses the existing completion listener to resume queue drain.
No elapsed-time or pane-text heuristic authorizes this operation. The operator's
reason is recorded as `operator_turn_lease_released`; this does not assert that
output delivery succeeded.

`inspect` returns a JSON identity containing provider, channel, runtime token hash
(not the token), process generation, message ID, episode nonce and start version.
It returns `null` when the mailbox actor confirms there is no active lease.
An unregistered mailbox or a failed actor request returns HTTP 409 with
`context.status` set to `unobservable` for both inspect and release. A missing
nonce, mismatched inflight identity, planned restart or rebind marker returns an
error.

Pass the exact object to `release`. A changed runtime or episode is rejected.
An observed empty mailbox returns `released: false` and `status: already_released`.
The mailbox actor checks the message ID, nonce and monotonic start boundary
before any cleanup. Late
terminals for that released episode cannot remove a successor; a successor's
normal terminal carries its own captured nonce. Filesystem cleanup failure exits
with an error reporting partial recovery, while preserving the provider.

This implements requirement 4 of #5754. It does not diagnose cold-start watcher
attachment, automatically detect stalls, or repair #5707 transcript persistence.
