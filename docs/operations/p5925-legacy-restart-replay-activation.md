# Legacy restart replay activation (#5925 B)

The coordinate-verified legacy restart replay is dormant until its activation slice lands.
This procedure is the operations gate for that activation. It does not replace the in-code
source proof: every replay still has to pass policy, session, native-turn and prefix proof
before it installs anything or causes any external effect.

## Boundary

Stop every writer that uses the same runtime root, then restart them all on one binary built
from the approved source SHA. A rolling deploy that mixes binaries is not an activation path.
A binary that predates the coordinate marker must not be started against a root that a
newer binary has already written to.

The writer set includes every process that can change the root's inflight rows, its normalized
output, or its binding and marker files:

- dcserver primary and standby
- workers
- repair runners
- AgentDesk wrappers and normalizers
- the supervisors that restart any of the above

Codex processes that only produce raw rollouts are recorded separately. A process does not
count as a non-writer just because it has no file open right now.

## Survey and check

1. **Preparation.**
   - Record the approved full source SHA, and the SHA-256 of every build artifact built from it.
   - List the declared hosts and roots. For each root, record its realpath and filesystem id.
   - List known unrecoverable rows separately as open delivery obligations:
     - missing native turn id
     - permanent prefix mismatch
     - suspected past raw restamp
2. **Stop.**
   - Stop the root's writers and their supervisors.
   - Survey again. For each stopped writer, record:
     - host, role, PID/PGID and start time
     - supervisor label, or confirmation that it has no supervisor
     - that it is neither alive nor restartable
3. **Restart.**
   - Start only approved artifacts.
   - For each restarted writer, record its executable realpath, executable SHA-256, start time
     and root realpath.
4. **Sources.** For each Codex source, record:
   - the policy verdict from the tmux source authority, with its incarnation
   - session id
   - the opened raw path and its file identity
   - for a row selected for recovery: native turn id, nonce, BODY message id, and proof result
5. **Run the check:**

   ```bash
   python3 scripts/check_p5925_activation.py survey.json
   ```

   - Only `P5925_ACTIVATION_OK` (rc 0) approves the restart.
   - `P5925_ACTIVATION_REFUSED` (rc 1) lists one `REFUSE` line per gap.
   - An unreadable survey returns rc 2.
   - Empty sections, unsurveyed hosts, unresolved or aliased roots, mixed SHAs, unapproved
     running executables, surviving writers, `VerifiedUnavailable` sources and env-only policy
     verdicts are all refused.

## Acceptance

Run a restart test against real raw byte ranges and Discord message ids. It passes only if all
three hold:

- no new POST for OLD or FROZEN
- the BODY message id is preserved
- the TAIL is visible exactly once

On failure, do not declare activation complete: stop the root's writers again. A deploy
script's success string or a fail-open smoke run is not this acceptance.
