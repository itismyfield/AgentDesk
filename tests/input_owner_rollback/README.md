# Input-owner acceptance preparation

This is test-only groundwork at S4 4b `d4976406ba9f1a54f5772f0aaf3e0588f248ea91`.
It does not connect, activate, or predict the 4c–4f owner ingress API. S5 must
install the scopes and verify all actual ingress paths after 4f lands.

`test_support::input_owner::StrictOwnerEffectGuard` registers a physical target
for the entire fixture lifetime, including restart and observation. Its process
registry observes writes from threads that never inherited a scope. Scopes are
explicit guards in a process-wide thread map; a spawned task needs an explicit
scope on the thread that performs the transport operation. A scope must not span
an async suspension. Bind the final owner proof at that boundary after 4f.
Entry reachability, admission and content have separate counters. Call each at its actual boundary. `observe_durable` reads exact V2
persisted source generations, namespace, provider, root, host, physical target,
execution, nonce and the payload-start/bootstrap window. Bootstrap permits create-only effects. Content/argv requires either the explicit Started window or the existing warm arm record (MayHaveSent with no window and a deadline); no additional owner commit is invented. This is a fixture proof
reader, not a production capability. Registration remains active after scope
return. `rebind` changes a known execution descriptor on the same physical target without unregistering it; it refuses a live writer and invalidates old scopes. Guard lifetime must outlive every spawned writer and observer.

The hooks run before raw tmux output, timed output, create, and Herdr socket frame
write. Generic `TmuxHost` and direct Herdr text/paste/line converge there. Events
separate literal/paste/argv/text/line, Enter, create, capture, control and buffer
load. Count `Argv` in both create and content totals; `Line` includes one Enter as well as content. Register the exact cold
shell command and expected nonempty prompt with `expect_argv`; do not infer argv
content from a command name or the requested warm/fresh mode. S5 must also compare
the generated launch script/prompt digest at the final cold-helper boundary.
An unregistered create is create-only and cannot satisfy positive content.
Use `assert_owner_payload` with the actual prepared frame for acceptance: it compares per-attempt payload digests, so an underscore/probe cannot substitute for content.
The command/socket lifetime holds a writer interval; overlap is rejected. Content counts only a successful tmux result or a complete Herdr frame write. A failed command is an attempted trace, not proof of NotSent.

The bounded submission, Claude Pane and input actor workers explicitly capture and
restore the caller's test scopes and producer metadata. The shared bounded runner
observes before spawning, and its existing `OwnedChild` retains the Flight through
async cleanup and its destructor. Pinned stash commands declare their physical
target, operation, buffer and prepared payload before crossing the worker boundary.
The payload must match the successful buffer cache; unknown or mismatched bytes
cannot prove that payload. A successful `if-shell` exit counts content only with
its APPLIED response. HELD never counts.
An opaque shell without producer metadata is refused on a registered target.

Buffer-load attempts record requested bytes independently of the successful-buffer
cache. A load invalidates cached knowledge first; complete input and a successful
child exit are both required before publishing bytes for subsequent paste probes.
A failed load leaves the buffer unknown, even when a stateful fake proves the old
bytes remained. Such a paste cannot prove the requested new payload.

The preserved 4h fixture uses `Preserved4h(stable_origin)` on the same registered
physical TUI. Only its registered payload on that execution descriptor is admitted; rebind invalidates an old 4h registration as well as old owner scopes. Its content goes to a
separate counter and cannot satisfy target owner content. The actual 4h handler
is still pending; this fixture does not claim its integration is complete.

## Fixtures

Queue array/V1 use `snapshot_json`; V2 uses the current `Owner` writer, including
reserve/bootstrap/payload-start/retain. `.dispatch` uses the actual marker writer.
The marker's wall timestamp is captured from the writer and checked positive;
regeneration comparison excludes only this nondeterministic timestamp. Expected
source generations, author/bot, role, reply, voice identity and attachment refs
are pinned separately. `fixture://` references resolve to the adjacent immutable
asset bytes in isolated tests. They are not live paths or network URLs.
A common independent identity projection checks author/bot, channel, role, reply,
source generations, voice and assets in raw/V1/V2/head/retained/marker records,
including the original restored from V1. V2 attempt provider/namespace/nonce and
execution plus the legacy nonce are pinned independently. The legacy canonical
V1 head omits the role override (`queued_head.rs:123`); its null expectation is
separate from the queued item's role, which V1 adoption restores into original.
Golden regeneration cannot change these expectations.
Unknown and corrupt files are deliberately invalid and must retain their bytes.
Existing corrupt legacy clear behavior is unchanged: `queued_head/compat.rs:22`
on main `1fc4b85638e03a157c37e729a8947211372e644c` allows legacy mutation after a parse/read failure. This preparation adds
no cleanup authority and does not treat failed parsing as empty/unattempted.

Generate with the test binary or cargo filter
`s5_fixture_writer_goldens_verify_separately_from_generation` and
`AGENTDESK_S5_GENERATE_GOLDENS=1`. The generation run must FAIL after writing.
Unset it and run the same filter again to verify GREEN. No ignore/skip is used.

`fixtures/scenarios.json` defines all seven crash cuts, old/candidate/third/read
failure cases, terminal cutoff ±1ms and clock reversal, Pending/later-commit/
24h receipt cases, and a reproducible 5001-row source/nonce expectation table.
The current tests exercise real durable-probe failures and the terminal clock;
the 5001-row PG test verifies the isolated fixture table, not 5001 owner ingress
calls. S5 supplies actual boot/worker/completion/HTTP/routine calls and native CLI
execution. The existing `durable_probe::{Watch,Gate}` gives path-specific publish
steps and barriers; count successful queue `FileSync` events separately from
history/outbox/migration paths, and report failed attempts as well.

## Required selection

The current required Linux `rust_build`/`rust_tests` archive and plan select all
new IDs, including the PG fixture. Each worker starts its own PostgreSQL server
with `AGENTDESK_REQUIRE_PG=1`; absence of a real server cannot skip this fixture.
The Windows path gate covers `src/test_support/**`. All helpers are reachable
through cfg(test) parents and use the repository's `_tests.rs` classification.
The retired lib inventory and exception files are not recreated.
Regenerate `required-tests.json` with `python3 tests/input_owner_rollback/generate_required.py` for IDs, path gates,
run mode and platforms. Linux runs are required locally. Durable-writer and Unix transport tests
compile on Linux/macOS; registry/scenario tests also compile on Windows. Windows has no directory-flush guarantee, so it cannot run the writable-owner fixture path.
macOS/Windows execution and actual future 4f acceptance remain separate evidence.

## Rollback

`manifest.template.json` intentionally has UNMEASURED/null fingerprints and
counts. It is not an approved rollback manifest. Fill real forward/R5 binary
SHA256, source/tree/patchset, dependency fingerprint, target/profile and raw logs
after 4f. R5 means writable compatible rollback; it is not the read-only 4f0 R.
`scenarios::writable_r5_counts` rejects absent/zero boots, actual R5 admissions,
queue writes, content and forward recovery; the single ready fixture requires exactly one content effect. Those counts must come from the
non-test process/transport trace, never from fixture construction. Unknown-field
preservation and same-ticket Pending recovery must also be measured before
approving R5. No snapshot deletion, downgrade, terminal reset or readmission is
a rollback mechanism.

P2-1 (`catch_up_race` in L05) is transferred to the 5c brief.
