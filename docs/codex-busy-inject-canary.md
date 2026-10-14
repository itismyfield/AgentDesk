# Codex busy-turn injection canary

Codex busy-turn injection uses the existing `ADK_BUSY_INJECT_DISCORD_CHANNELS`
gate. The capability constant is enabled, and the requested channel must pass
that gate. No additional configuration file or environment key is used.

The gate is cached once per process. Changing its environment value requires
replacing the release process through the existing release procedure. There is
no hot reload or instantaneous revocation of an already pasted input.

## Channel scope

- An unset, empty or invalid list disables the gate. `*` admits every channel
  that otherwise meets the existing gate checks.
- For Codex capability, the gate receives the requested channel and no parent.
  Listing a parent admits its threads to Discord intake, but a Codex thread
  must independently pass the capability gate.
- Windows and Herdr exclusions remain in the existing gate. Native turn,
  full-access context, draft, modal, queue and holder checks still apply.
- Claude and the Claude-only queued-head capability keep their existing paths.
- External input modes do not bypass the Codex channel gate.

## Before a live canary

These are deployment checks for the coordinator; implementation tests do not
establish their live values.

1. Record the deployed SHA, process PID, start time and execution node.
2. Confirm the effective existing gate on every execution node. A one-channel
   canary requires a list other than `*` and no other active direct Codex
   channel covered by that list. Preserve existing channel IDs.
3. Confirm the target is direct tmux Codex with full-access native context,
   and record its session ID, native rollout path and starting offset.
4. Preserve the initial Discord snapshot and inspect health, lease and queue.
5. Require independently validated J8 measurement tooling before claiming a
   live J8 result. Unit and PG fixtures alone cannot establish that result.

## Sample and verdict

Start A through real Discord input, confirm its native model turn is busy,
then submit B once through Discord. Preserve native and Discord evidence for
every submitted B across at least ten samples.

`Injected` can also mean B opened a new turn after A ended. It does not prove
that B joined A. A J8 pass requires every sample to show `joined=true`, the
observed turn equal to the target, one accepted original input, one complete
final answer per target, the B marker once, and no duplicate answer, input
echo, synthetic placeholder or missing delivery. The final state must be
idle with an empty queue and no unconfirmed or unfinished sample.

Missing logs, native ranges or Discord pages make the result incomplete.
An observed `joined=false` is a failed join sample. Do not exclude vetoed,
new-turn or unconfirmed samples from the submitted sample set.

## Stop and rollback

Stop submitting inputs and preserve their nonce/turn mapping, native ranges,
logs and Discord snapshots. Do not resend an uncertain B.

Remove only the canary channel from the existing gate through the approved
release configuration procedure, preserve the other IDs, regenerate the
release service definition and replace the process. Confirm the new PID,
effective gate and unlisted-channel refusal. A restart that retains the old
loaded environment does not establish revocation.

The new process blocks further unlisted attempts before pane I/O. It does
not cancel an earlier paste or convert `Unconfirmed` into a resend. Returning
the capability constant to false is a separate code rollback if needed.
