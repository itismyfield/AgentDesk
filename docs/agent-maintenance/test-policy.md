# Test Authoring Policy

Write tests that keep working when the implementation is rewritten with the same
behavior. Ask of every new test: "Would this break if the code were rewritten to
do the same thing?" If yes, do not write it.

## Rules

1. Assert behavior at a public boundary: function results, HTTP responses, DB
   state, outgoing Discord messages.
2. Do not assert on source text. Do not read `.rs` files (`include_str!`,
   `read_to_string`, tree walks) to pin strings, call counts, call order, `use`
   lines or log fields. Express structural rules with visibility
   (`pub(in …)`), types, or an existing CI script.
3. Do not commit test switches in production code (`*_mutant()`, env-enabled
   branches). Prove mutants inside the lane and revert before committing. When a
   test needs a situation, inject it through an argument.
4. Name tests and files after behavior (`queued_followup_survives_restart`).
   Issue and stage numbers (`_7441`, `s2a_`, `f4r2_`) belong in PRs and commits.
   Rename an existing test only when you are already changing it for another
   reason.
5. Keep operational boundary tests: restart, loss, duplication, exactly-once,
   ordering, crash replay, and module contracts (relay state, API DTOs, DB
   migrations and state, provider output format, Discord output boundary,
   server↔lane receipts). When cleaning these up, only merge them into tables.
6. Variants that differ only by channel, provider or flag are one table-driven
   test.
7. Add a regression test only for a defect that actually reproduced, once, at
   the owning boundary. Do not write defensive tests for states no caller can
   produce.
8. Do not build infrastructure for tests (lexers, censuses, probes, tests of
   tests).
9. Do not synchronize with fixed sleeps. Use `tokio::time::pause`/`advance` or
   polling with a deadline.
10. When you delete code, delete the tests that target only that code in the
    same PR, and add a reason line to
    `scripts/ci/test-removal-justifications.txt`.
11. Do not add `std::env::set_var` calls, new env-var test seams, new
    process-wide `static Mutex<()>` locks or test-hook statics. Where existing
    code still reads an env var, use the shared `TestEnvVarGuard` until it takes
    the value as an argument.
12. Keep the test diff within the production diff: added test lines should not
    exceed changed production lines. State the reason in the PR when they do (a
    reproduced defect, replacing a deleted guard). An exception to rule 8 (new
    test infrastructure) needs coordinator approval.
13. Every wait in a test goes through the capped helpers in `test_support::wait`
    (`join_within`, `recv_within`, `await_within`, default cap `WAIT_CAP`; a
    longer cap is an explicit argument). A bare `.join()`/`.recv()` in test code
    fails `scripts/ci/rust_test_sources.py check`; the allowlist
    `scripts/ci/test_wait_allowlist.txt` only shrinks.

## Scope

This is an authoring policy for humans and agents. Rule 3 is enforced on added
lines by `scripts/audit_state_lint_hardening.py` for `*_MUTANT`/`*_MUTATION` env
names and `*mutant` hook functions; a switch under another name is left to review.
Existing process-global environment dependencies (raw counts in `src/`: about
350 lines containing `set_var(` and 980 containing `shared_test_env_lock()`) are
cleaned up in #7819. Reusing the shared `TestEnvVarGuard` stays allowed until a
dependency is replaced by an injected value.
Existing tests are cleaned up in the #7344 slices; do not bulk-rewrite them outside
that work.
