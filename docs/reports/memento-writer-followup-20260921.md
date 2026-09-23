# Memento writer follow-up review and release gate

This follow-up supersedes the initial implementation's receipt behavior in
[memento-writer-recurrence-20260921.md](memento-writer-recurrence-20260921.md).
The user explicitly authorized independent review, repairs, targeted verification,
commit, and safe deployment. Existing sessions must not be terminated, existing
memory/database data must not be changed, and AnchorMind remains upstream-owned.

## Source and independent review

- Worktree: `~/.adk/release/worktrees/memento-writer-novelty-20260921`.
- Branch: `fix/memento-writer-novelty-20260921`.
- Refreshed `origin/main` to `a0b85e9839` and fast-forwarded this independent
  branch before repair. Its six newer commits did not overlap the writer changes.
  The original project worktree and branch were not checked out or modified.
- A new, read-only reviewer (`fresh_writer_review`), distinct from all authors,
  reviewed actual code and tests. Initial findings: stale receipts after mutation,
  definitive rejection poisoning, and implicit session-scope collision.
- Second review found missing consolidation invalidation and the documented
  `SYMBOLIC_POLICY_VIOLATION` rejection. Both were repaired.
- Final independent verdict: PASS, no new blocking code finding. This is separate
  from runtime readiness and does not substitute for release/CI gates.

## Final behavior

The internal client uses locked durable receipts. Content, scope, status and other
semantic metadata distinguish new writes; source/importance-only changes do not
constitute new knowledge for this guard. A confirmed duplicate skips the remote
call; in-flight/ambiguous results do not masquerade as success. Connect/initialization,
401/403/429, standard JSON-RPC dispatch rejection, and documented policy rejection
release the claim for a later retry. Unknown errors remain conservative.

Both client and hooks read an endpoint-wide generation. Observed amend, forget,
consolidation, and session rotation invalidate prior suppression; authentication
and scope remain part of each fingerprint. Mutation hooks always pass, and the
weekly consolidation client invalidates before and after its call, including an
ambiguous result. Master-key maintenance also invalidates ordinary-key receipts.
An older in-flight completion cannot restore suppression in a newer generation.

Direct Claude/Codex hooks block confirmed duplicates only. They preserve exact
content bytes, session boundaries, new family facts, changed assertions/scopes,
and ordinary amend flow. Missing metadata or local observation failure does not
become an arbitrary tool ban. Missing/ambiguous PostToolUse and concurrent first
calls remain allowed; this limitation is deliberate to avoid losing new facts.

Automatic transcript capture/reflect remain disabled. Card retrospective audit
records remain local while their automatic Memento mirror is disabled. Hygiene
no longer rewrites old errors into new procedure fragments.

The derived Memento guidance was also corrected: it previously restricted every
Full-profile session, including family counseling, to technical decisions and
configuration. It now permits confirmed new personal/family facts and preferences,
requires zero writes without new knowledge, and directs corrections to amend.
The file backend text is unchanged. This delta received independent review PASS.

The changed transport was extracted to `memento_transport.rs`, shrinking the
existing giant file. New tests were added to the source-derived library test
inventory; no ratchet baseline or mandatory check was relaxed.

## Verification

Targeted Rust tests, `cargo check`, changed-file formatting, lane coverage,
Clippy allow and hotfile ratchets, shell syntax, and diff whitespace are checked.
The writer/maintenance selection passed 55 tests (0 failed). The final `cargo check` and `cargo build --bin agentdesk` passed. The final actual
CLI consumer run passed all 94 checks (47 Claude, 47 Codex), with all 94 hook
observations received by the loopback relay. All eight memory-guidance tests passed, including the new family-fact test and
existing file-backend behavior: **63 distinct targeted Rust tests, 0 failures**.

`scripts/verify_memento_writer_consumption.py --binary <absolute-binary>` executes
actual Claude/Codex hook-relay CLI processes against synthetic observations and a
loopback relay. It uses a temporary runtime with dummy credentials, waits for its
own detached relay workers, and never executes a real memory tool or contacts the
operational DB/Discord/MCP service. It checks positive duplicate denial, new facts,
assertion/scope/session changes, mutation invalidation, concurrent first calls,
response loss, and unrelated tool behavior. It proves AgentDesk hook consumption,
not provider enforcement or live upstream storage.

The existing deployed binary failed the positive duplicate-denial control as
expected: three isolated hook observations arrived and the old CLI returned only
`{"suppressOutput":true}` for the repeated confirmed write. Evidence is retained
in `target/memento-deployed-negative-control.json`.

## Shared policy and runtime audit

Read-only inspection on mac-book and mac-mini found stable launchd release
binaries at `~/.adk/release/bin/agentdesk`, version 0.1.3, source `f9890a73`.
The mac-book has five AgentDesk tmux sessions; mac-mini is cluster standby.
No session was killed, no raw launchctl restart was used, and no runtime mirror
was hand-edited.

Both current vaults already contain the approved `ed4e003` Memory policy:

- `## Memory` (41 lines): SHA256
  `776e147e9ab3a0dea0c6cf39fc40fcdf1fd8339bda7f71b15783ecf08d18e9af`.
- `adk-config/shared/memento-rules.md`: SHA256
  `be3127b95f0a58cfa98f98d29113760d5a20c68a2b7e812a1553d3cad4f00fea`.

Other current vault edits differ from that commit and must be preserved. Both
runtime shared-prompt mirrors still contain the old policy. The sanctioned path
is deploy-release.sh's vault staging and atomic prompt promotion, with the normal
local source or its documented `AGENTDESK_OBSIDIAN_AGENTS_SRC` override. No separate
standalone prompt-sync operation was found in the canonical procedures.

Actual bounded inspection of the long-lived Codex rollout's developer message
also confirmed old policy consumption (39-line Memory SHA256
`1df50998cc8a61c5911469c9c62e9a83f2824310d91ee215dfa19d1ebafad8a6`).
A mirror update alone would not prove refresh of this thread: warm resume keeps
the original developer instructions. A safe new session/launch must be inspected
without terminating this existing session. No such live session was created here.

## Deployment gate

The canonical skills are `agentdesk-runtime-ops` and `adk-release`. They require
the safe deploy path, normal review/checks, stable launchd paths, and preservation
of AgentDesk work sessions. The script's default durable restart-persistence gate
can preserve live turns; active turns alone are not asserted to block deployment.

At 2026-09-21 05:10 UTC, applying the actual `health_json_is_ready` function from
`scripts/_defaults.sh` with the deploy arguments to current mac-book health
returned `DEPLOY_READINESS_BLOCK`: db/server/dashboard true, fully_recovered true,
status degraded, and `provider:codex:pending_queue_depth:4` alongside unknown Codex
relay verdict. The existing Codex tmux is alive but its relay is degraded.
This is not the allowed relay-only degraded/standby case. No force/skip flag or
session cancellation is used to turn this into a pass.

The final deployment decision must retain this safety distinction: a committed,
tested candidate is not a deployed binary or a verified new policy consumer.

## Remaining boundaries

- Semantic paraphrases, unmanaged/unknown-identity MCP callers, other runtime
  roots, and unobserved upstream mutations are not universally deduplicated.
- Backend and hook receipt formats remain separate; they share invalidation,
  not cross-transport duplicate confirmation.
- An ambiguous internal write requires read-only reconciliation before recovery.
  Invalidation intentionally permits a repeat instead of suppressing a legitimate
  restoration after a mutation.
- Hook concurrency and response loss remain retryable. A successful hook output
  is not proof the provider will enforce it, nor proof of live database storage.
- Local disk failure can prevent mutation observation; no unconditional
  exactly-once or permanent semantic-novelty guarantee is claimed.

Final candidate binary SHA256: `4af1180557bca8ce12341d805ca981160cfa14b5b67e00f138ef13aa003526bc`.
Detailed CLI evidence: `target/memento-final-consumption.json`.

Deployment status: **not deployed on either host**. The final read-only readiness
recheck still returned BLOCK with pending_queue_depth:4. Both launchd services
remain on source f9890a73, mac-book PID 93813 and mac-mini PID 83777, and all five
mac-book AgentDesk work sessions remain. No safe wrapper restart, release binary
swap, runtime prompt promotion, DB migration, or fragment mutation was attempted.
Required remote CI/main integration and release gates remain prerequisites of a
later rollout; they were not bypassed with non-main, skip, or force overrides.

Reproduction from this worktree (all tests use synthetic data):

```sh
python3 scripts/build_token.py -- cargo check
python3 scripts/build_token.py -- cargo build --bin agentdesk
python3 scripts/build_token.py -- cargo test --lib -- \
  services::memory::memento:: services::memory::memento_writer_guard:: \
  services::retrospectives::tests services::claude_tui::memento_writer_hook:: \
  services::maintenance::jobs::memento_consolidation::receipt_tests \
  memento_writer_hook_identity memento_turn_plan_never_schedules_raw_capture \
  --test-threads=1
python3 scripts/build_token.py -- cargo test --lib \
  services::discord::prompt_builder::memory_guidance::tests -- --test-threads=1
python3 scripts/verify_memento_writer_consumption.py \
  --binary "$PWD/target/debug/agentdesk" --output-json /tmp/memento-consumption.json
```
