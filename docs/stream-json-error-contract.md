# StreamJson terminal errors

AGY can report `result.status=SUCCESS` with an empty response when a headless
tool request is denied. `AgyCodec` treats empty or whitespace-only assistant
output as an error. Permission diagnostics include the last failing step and
the bounded stderr prefix, including when the terminal event has no session ID.
An unrelated stderr warning does not hide a step's permission denial.

Text deltas are streamed immediately. A terminal aggregate response is used
only when no delta was emitted. `Done` is emitted once, after a `SUCCESS` result,
a non-empty conversation ID, non-blank assistant output, and a successful process
exit. A failed or signal-terminated process cannot turn partial text into success.
Explicit provider failures and premature EOF retain available diagnostics.

The shared runner drains stderr while retaining at most 16 KiB of input bytes.
Invalid UTF-8 and a character split at the cap are decoded lossily. Logs contain
exit status, stdout line count, and stderr length/presence; they do not include
the captured stderr body. Existing no-output watchdog and resume-reset markers
continue to apply.

No automatic permission bypass is added. A denied request remains a visible
provider error so the operator can configure an appropriately scoped project
permission rule.

Antigravity is registered as `antigravity` with alias `agy` and channel suffix
`-ag`. Both headless and intake turns use the shared provider dispatch boundary;
one-shot execution also selects the AGY dialect through the registry. Session,
model, system-prompt envelope, tool policy and cancellation identity are preserved.
Restricted tool policies and remote execution remain explicitly unsupported.
No credential location or permission capability is inferred.

Accepted StreamJson turns remain active until process exit, stream failure, or
explicit cancellation; silence only limits the initial startup handshake.
For a non-zero caller timeout, the handshake defaults to **60 seconds**.
Operators can change it per installation through `agentdesk.yaml`:

```yaml
runtime:
  stream_json_startup_output_timeout_secs: 300
```

An omitted or zero setting retains the 60-second default; positive values are
capped at 86,400 seconds. New launches read the live configuration snapshot.
The existing zero-caller-timeout handshake remains 90 seconds. This setting
does not impose a total duration limit after the first non-blank stdout line.

AgentDesk does not pass `--print-timeout` to Antigravity. Unlimited headless turns
require Antigravity CLI 1.2.6 or newer, whose
[versioned release notes](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.2.6)
changed the CLI default from five minutes to unlimited. Older CLI versions retain
their own default timeout.

Regression coverage: `cargo test --lib stream_json_cli`,
`cargo test --lib services::provider::`, and
`cargo test --lib agy_discord_dispatch`.
