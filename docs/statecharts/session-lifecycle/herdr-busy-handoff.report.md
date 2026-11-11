# Claude busy forward handoff — design 1 (#8077)

Code SHA: **`1fe3a57d266e1e7fe384796d22a925076f1d99fc`**. This is a design model and a baseline predicate investigation; production code is unchanged. Full design, production entry census, source/owner classification, estimates and test/mutant plan: [herdr-busy-handoff.design.md](herdr-busy-handoff.design.md).

## 원인

`session_handoff.rs:115` runs `/exit` through `herdr_turn/composer.rs:111`. `observation` at `composer.rs:50` checks readiness and, for an exit, additionally requires `draft_sighting == Settled` at `:75`. The supplied 46×39 capture has an empty composer (ownership.rs:74 → screen.rs:326) but an unmeasured clipped footer (screen.rs:157/175, claude_footer.rs:5) and an unmeasured update row above the composer (screen.rs:298). Both leave the stash unproven. `ownership.rs:26` returns Unsettled even in the synthetic idle capture. Correcting only the footer still fails; correcting the update row as well makes the focused contrast Settled.

The actual marker predicate is true on both supplied busy and synthetic idle screens. Busy is vetoed separately by `input.rs:1285`/`prompt_readiness.rs:216`; on idle, the exit-only stash guard itself refuses immediately. The visible ready prompt is therefore not sufficient. `composer.rs:130` maps both immediate refusal and timeout to Unknown; `session_handoff.rs:69` maps it to earlier Claude kept running. The generic opaque start-failure path (`provider_dispatch/start_failure.rs:226`, `queued_head/v2/start_failure.rs:95`) retains after its existing threshold. It is not correct to say every first opaque failure immediately retains.

Existing 46-column fixtures and busy paste tests are present (`busy_inject/screen_tests.rs:78`, `busy_inject_tests.rs:596`); the exit-specific test at `herdr_turn/composer_tests.rs:285` uses a wider measured layout. The original 14:17 idle capture is unavailable; the idle reproduction removes the spinner and Tip rows from the supplied 14:24 busy capture. This is an explicit assumption, not an operating-screen observation.

The proposed path stops only an exact verified old local Claude execution, after a visible draft-discard notice, without typing into the composer. It preserves the existing resume/auth/source/nonce checks. An unsuccessful stop either selects a freshly verified old execution for the same original claim, or waits in active custody for evidence; it never converts a handoff refusal into retry-required retention. Old fallback cannot be interrupted again until the exact input has actually been processed. The production plan is **17 files / net +1150 code lines**, including contingency and the Claude-only boot census described below.

## 상태 차트

`SC_RUN=mac-needed`: `/home/adk/.claude/skills/statechart/scripts/statechart` is absent. The `.chart.mmd` is explicitly hand-authored, not a generated state-space rendering. The `.view.mjs` is ready for a later Mac frontend run.

The WSL does have Java 21 and the verified TLC 1.7.4 jar. The repository's official CI checker ran the new 15 configs in an isolated root: **15 expected sets matched; 52 TLC runs; 28.0 seconds; exit 0**. The frontend absence remains reported as requested. No model/cfg change followed the final run. Modeling finished within the 30-minute limit.

| Config suffix (`HerdrBusyHandoff*.cfg`) | Result | Broken rules |
|---|---|---|
| BeforeBusy, BeforeUnread | RED | NoUserResend; MessageProgress; HandoffProgress |
| AfterBusy | GREEN | none; 2 inputs and 1 restart |
| AfterUnreadDraft | GREEN | none; unmeasured screen and possible draft |
| AfterStopFailure | GREEN | none; verified old fallback and processing completion |
| AfterFailureUnread | GREEN | none; fallback waits for a readable screen |
| AfterRetained | GREEN | none; 5 historical retained originals |
| Controls | GREEN | none; explicit pre-content cancel/clear |
| Duplicate | RED | InputAtMostOnce; NoSilentLoss; MessageProgress |
| SilentDrop | RED | NoSilentLoss; MessageProgress; HandoffProgress |
| DraftNotice | RED | DraftLossTold |
| WrongSession | RED | SameSession |
| NoWake | RED | MessageProgress; HandoffProgress |
| RetainedRecovery | RED | MessageProgress; HandoffProgress |
| LostReceipt | RED — residual boundary | MessageProgress |

Direct raw TLC additionally reproduced BeforeUnread exit **12** at NoUserResend and AfterBusy exit **0**, 236 generated/101 distinct states, depth 19, temporal checking completed. Expected RED configs are not being counted as GREEN behavior. `AwaitEvents` models the long-running event loop's ability to wait; liveness is checked by actual PROPERTY declarations under the applied weak fairness in `Spec`, rather than by a deadlock-free claim.

The before counterexample is queued → Begin → BeforeRefuse(retained). It abstracts the existing opaque-failure threshold reaching its retain decision. The proposed path separates provider stop, absence observation, Pending/create, exact resumed attach, destination choice, arm, backend effect and owner receipt. Crash can intervene between effects, including between the input effect and settlement. `NoKeyYet` is positive writer evidence; lack of transcript evidence is never treated as proof of no send.

Fairness assumes recoverable storage/hosts, finite restart/stop failures, eventual verified stop or readable old fallback, and obtainable native receipt evidence. Old fallback additionally waits for the exact native processing terminal before trying migration again. LostReceipt intentionally shows that permanent evidence loss cannot provide unconditional progress. The model does not verify arbitrary OS failures, actual Claude flush semantics, all adjacent command FSMs, or old-writer rollback. CancelBeforeContent represents explicit cancellation, while the detailed clear timeout, Terminate fence and late callback generation checks remain Rust test obligations.

## 과거 retained와 boot

The old opaque start-failure record is `KnownNotSent`, `Reserved`, **attempt=None** (`queued_head/v2/start_failure.rs:104`). Ordinary automatic requeue requires `!notice.enqueued` and an attempt (`retained.rs:283`), while told inputs are unowed (`:249`). Boot also selects only `holds_input` snapshots (`pending_queue_persistence/queued_recovery.rs:87`, `queued_head/compat.rs:20`), so merely adding a consumer inside owner recovery would miss a told-only file. Completed old fallback can similarly leave an input-free file that still owes migration.

The proposal therefore adds a narrow Claude-only census plus an atomic owner migration for the exact historical error/provenance, including told attempt-less records. It restores originals, source members, uploads/reply boundaries and exact receipt bindings together. An `Opaque` admission bind is not repaired by calling the existing `rebind_queued` alone (`source_admission.rs:546`); exact digest/context rebinding is part of the new design. Cancelled/cleared, delivered, opaque/unconfirmed, partial-content, different-provider and different-error records are excluded. The five operating events are expected candidates, not five inspected production rows; only the first two message IDs were supplied. No operating DB/store was read.

## Main already provides

All coordinates below use the code SHA above. These behaviors are reused rather than reimplemented.

| Existing behavior | Main evidence |
|---|---|
| Same account and transcript eligibility for resume | `src/services/claude/herdr_turn.rs:590` |
| Existing O cursor continuity rather than reading from zero | `src/services/claude/session_handoff.rs:153`, `src/services/tui_o/writer/renumbered.rs:50` |
| Own resumed hook requires native ID, source=resume and incarnation nonce | `src/services/claude/session_handoff.rs:192` |
| Actual resume argv and matching prepared context | `src/services/claude_tui/session.rs:372`, `src/services/claude/tui_session_launch.rs:138` |
| Durable first-input hold and physical owner writer binding | `src/services/claude/herdr_turn/first_input.rs:22`, `:86` |
| Same-channel drive and first-key/physical writer fences | `src/services/discord/queue_io/input_owner/drive.rs:87`, `src/services/claude_tui/busy_inject/stash.rs:52`, `:135` |

## 리뷰 집중 항목

| Focus | Guard or remaining counterexample |
|---|---|
| Turn ends immediately before/after stop | Reobserve PID/start, pane/nonce and absence. Signal dispatch is not exit proof. Real Claude partial flush/resume remains a development integration check. |
| Two messages on one channel | One owner claim, durable destination, writer lease; busy intake must also honor the handoff marker. Entry race tests are planned, not claimed passed here. |
| Restart during migration | Existing Pending nonce recovery plus explicit handoff recovery/wake; include told-only and input-free unfinished handoffs in boot census. |
| Interrupted output loss/duplication | Preserve complete persisted records, spool and existing cursor. Current-turn unpersisted output loss is accepted; no cursor-zero or EOF jumps. |
| Old format restoration | Fixtures must cover attempt=None/told/Opaque and missing optional fields. Older writer reserialization can discard a new optional journal; rollback with unfinished handoffs is outside model GREEN. |
| Guards only declared in docs | Require production callers and deleting-guard mutants for strict fallback, other pending frames, clear overtake checks, notice-before-signal and boot census. |
| False 0/PASS | Two focused baseline tests, 15 cfg/52 TLC runs are counted separately. No PG/repository cargo/clippy/prepush success is claimed. Unknown and LostReceipt remain visible. |

The proposed forced stop does not reuse main's broad tmux session cleanup (`claude_tui/host_input.rs:216`). The general unread-draft busy paste behavior (`busy_inject.rs:499`, `queue_io/input_owner/effect.rs:223`) is not widened; the fallback adapter adds a strict pre-key policy for this one capability. Codex and Herdr→tmux Return retain their existing behavior.

## 증거 목록

External lane root: `/home/adk/adk/lane-runs/i8077`.

| Evidence | Path / count |
|---|---|
| Full external design, identical to the committed design | `design-1.md` |
| Issue read, title/body/comments | `logs/issue-8077.json` |
| All handoff, retained, adjacent and boot/incarnation git-grep hits | `logs/handoff-entries.txt`, `logs/retained-entries.txt`, `logs/adjacent-entries.txt`, `logs/boot-incarnation-entries.txt` |
| Existing narrow fixture search | `logs/fixture-search.txt` |
| Final focused baseline predicate log | `logs/predicate-probe-final.log`: 2 passed, 0 failed, 0 ignored, 0 filtered; doc-tests 0, not counted as coverage |
| Test names | `services::claude_tui::busy_inject::existing_46_column_fixture_also_cannot_certify_an_exit`; `services::claude_tui::busy_inject::measured_field_and_synthetic_idle_both_fail_the_exit_stash_gate` |
| Probe executable | `logs/predicate-probe/target/debug/deps/i8077_predicate_probe-69c97a0ef2df0bd8` |
| Executable SHA256 | `ad15cdda2b4b44a682c07e2c51d21dc5e88262f1d1bc47e366327c279188508a` |
| Reusable focused crate and scope notes | `logs/predicate-probe/Cargo.toml`, `logs/predicate-probe/src/`, `logs/predicate-probe/README.md` |
| Final official checker log | `logs/statechart-ci-6.log`: selected 15, matched 15, TLC runs 52, exit 0 |
| Direct TLC raw traces/state counts | `logs/tlc-before-unread.log` exit 12; `logs/tlc-after-busy.log` exit 0 |
| Exact final checked copies | `logs/sc-root/`; byte-identical model/cfg copies of committed sources |
| Source hashes, source-extraction checks, environment and binary hashes | `logs/evidence-design-1.json` |

The focused crate uses unchanged main ANSI/ownership/footer code, a screen copy with only its child-module path adjusted, and **28 byte-identical extracted ready/busy/modal functions**. Neutral frame-history and unused modal-adapter stubs are explicit. The probe is not the production `observation` entry test; that is in the implementation test plan. Isolated-crate unused-code warnings come from the intentionally unused copied API, not production changes. No `#[allow]` or baseline increase was introduced.

Environment: SHELL=/bin/bash; TMPDIR unset; CARGO_TARGET_DIR unset and unchanged; cargo resolved to `/home/adk/skills/agentdesk-issue-pipeline/scripts/cargo-shim/cargo`. `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, `CARGO_PROFILE_DEV_STRIP=none`, `CARGO_PROFILE_TEST_STRIP=none`. Default parallel tests; no `--test-threads=1`. PG executed: **no**. Repository cargo/check/clippy/source_scan_readers/prepush: **not run (design-only, no production change)**. No background process remains from these checks.

Jar SHA256: `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`. Model SHA256: `b4d02a45b9452eb8506c027d3ee735d1e35c525084523169cc81fe53548dec73`. Earlier probe/compiler/model authoring failures remain in numbered logs; only the final logs above are acceptance evidence. The official checker's zero means exact expected violations matched, never that the historical behavior meets the proposed contract.
