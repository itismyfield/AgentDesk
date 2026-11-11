# Claude busy forward handoff — design r2 (#8077)

Production code SHA: **`1fe3a57d266e1e7fe384796d22a925076f1d99fc`**. Reviewed d1 artifact: `fae2d9ee41bf57897dcef1e56869b1a19b1265cc`. Production code is unchanged in this design leg. The [full design](herdr-busy-handoff.design.md) includes the updated production entry census, source/owner classification, remaining screen readers, 22-file/+1180 estimate and implementation test/mutant plan. External copy: `/home/adk/adk/lane-runs/i8077/design-2.md`.

## 원인과 리뷰 처리

The 46×39 busy sample and the explicitly synthetic idle sample have an empty composer and a recognized ready marker, but an unmeasured clipped footer/update area makes draft_sighting Unsettled. `src/services/claude/herdr_turn/composer.rs:50` observes the screen, `:75` adds the exit-only settled-draft requirement, and `:125`–`:130` refuses with Unknown. `src/services/claude/session_handoff.rs:115` uses this for `/exit`. The busy sample additionally fails the final busy veto; the synthetic idle sample still fails the draft gate. The actual 14:17 idle screen was not captured. This baseline finding reuses d1's two focused predicate tests; r2 does not rerun them or claim a new production fix. BeforeBusy/BeforeUnread abstract the existing generic opaque-failure threshold reaching retention (`provider_dispatch/start_failure.rs:226`, `queued_head/v2/start_failure.rs:95`), not immediate retention on every first failure.

| Review | r2 treatment and production evidence | Model evidence |
|---|---|---|
| P1-1 permanent unread after a transient close failure | A provisional old destination can be released **only with exact positive no-key proof and zero keys**, then identity is freshly verified and close is retried. `composer.rs:125` is no longer a screen-recovery dependency. C-s/any key makes the destination irreversible until exact old native processing terminal | BeforeFailureUnread RED → AfterFailureUnread GREEN. OldUnread is constant; no RecoverScreen action or fairness |
| P1-2 fresh Herdr first-input refusal becomes a string/retained | Replace the own-resume first epoch's general composer readiness with a native nonce/ID/source capability. Preserve SubmissionError::Held through `first_input.rs:48`, `:94`, `:112` → `effect.rs:367`. Populate real owner known_before/first-key proof (`claude_tui/owner_input.rs:484`) and use hold_not_sent (`effect.rs:122`) plus a native/host level wake. Enum-only Held and confirmed=0 are insufficient | BeforeFirstRefusal RED → AfterFirstRefusal GREEN; NoFirstWake/NoNativeReady RED |
| P1-3 first active message never reaches Begin | Add mapped local Claude Legacy preflight **before hand_busy** at `provider_dispatch/owner_input.rs:217`/`:238`. It admits without a marker or Bound row, claims the same front, and rechecks current actor journal at commit. Existing registration Absent at `:91`/`:359` is not used as the forward entry | BeforeActiveEntry/BeforeActiveRace RED → AfterActiveEntry/AfterActiveRace GREEN. N=2 separates intake probe from actor commit |
| P1-4 numeric PID/PGID signal race | Do not call `src/services/process.rs:527`, `:546`–`:550`. New dedicated capability keeps the **same tmux control connection** from exact pane observation through kill-pane. EOF revokes authority; old numeric pane IDs are never blindly replayed after reconnect | PidSignal/ReconnectNumber RED; AfterPidReuse/AfterPaneReplacement/AfterServerReplacement GREEN. Isolated tmux protocol probe 3/3 |
| P2 journal-only census/wake | Keep only unfinished_handoff census, not historical retained eligibility. Existing `pending_queue_persistence/queued_recovery.rs:87` and `queued_head/compat.rs:20` select holds_input only. A journal-only driver must progress without a front grant | BeforeJournalCensus and journal-only NoWake RED → AfterJournalOnly GREEN |
| P2 vacuous draft rule | Submit can execute an unparked old draft in the DraftEnter mutant; normal old fallback uses the existing draft transaction. New draft UX is not decided here | DraftExecution RED at DraftNeverExecuted |
| P2 native ID versus argv UUID | Initial argv A is lineage, current binding B may continue to C (`hook_server/adoption_retry.rs:395`). Re-read the final durable native/source binding after old execution end; resume C, with a new execution nonce | BeforeContinuation/WrongSession RED → AfterContinuation GREEN |

Short paths in this table are expanded in the full design's production entry census. The new close is limited to the owned local Claude forward pane. It does not add generic kill-session/tree cleanup, Return behavior or Codex authority. The existing numeric helper's race remains in its existing callers; this proposal does not widen them.

Historical classifier/migration, attempt=None/told retained recovery, Opaque rebind and eligible_handoff_retained census are **removed**. The five old operating retained messages are not promised automatic delivery by this scope. AfterRetained and RetainedRecovery cfgs were deleted. DraftNotice was replaced by the nonvacuous DraftExecution cfg because the human-draft UX policy is deferred.

## 종료·첫 입력의 구체적 계약

On one verified live tmux control connection, capture exact owner/session/window/pane/nonce identity, recheck owner generation before the close, and issue kill-pane only for the captured pane object. tmux IDs are unique only within a running server, so socket path, numeric server PID and pane number alone are not authority. EOF invalidates the capability. A new connection requires fresh ownership/source verification. No numeric PID/PGID signal fallback is permitted. [Official tmux identifier contract](https://github.com/tmux/tmux/wiki/Advanced-Use/4aaa59af320a585fe488332775a336ad06c1ccfb#unique-identifiers), [control protocol](https://man.openbsd.org/tmux#CONTROL_MODE).

Linux WSL tmux 3.4 private-server probe: exact old pane close leaves the other pane alive; retrying a removed pane returns a typed command error without closing its replacement; restarting the private server reuses %0 but the old connection cannot replay into it. **3 passed, 0 failed**. A removed-pane display-message actually returned exit0 with empty metadata; it is not a positive witness. These are protocol experiments, not AgentDesk adapter tests. The same protocol contract applies on macOS, but the deployed macOS version still requires its own isolated test. Pane close does not guarantee provider exit, descendant cleanup or transcript flush on either OS. Provider absence is checked separately; orphan cleanup is not added.

A fresh own-resume first-input capability requires old execution ended, final current native ID, exact new nonce/physical writer, durable source=resume SessionStart record and an unused original first-payload epoch. Reuse `herdr_turn.rs:705` and `hook_server/observation_ingress/resume_start.rs:27`/`:47`; do not treat any hook or a ready-looking screen as authority. The native adapter sends bracketed paste+Enter in one PaneSendInput RPC under the existing gate/writer fence. Only a positive pre-key refusal becomes Held with real no-key proof. Subscribe to native/host/lease readiness and immediately recheck its current level so an already-arrived event cannot strand the hold. Lost replies/partial input remain uncertain, not no-key.

SessionStart is not proof of arbitrary TUI emptiness or receipt. The official hook field initialUserMessage is for noninteractive -p, so it is not used as a fictional TUI injection API. [Claude hook reference](https://code.claude.com/docs/en/hooks#sessionstart-decision-control).

Human draft classifiers are unchanged. Old fallback keeps its current guard and on pre-key unread releases the old choice instead of waiting for screen recovery. A concurrently edited, not-yet-submitted human draft in the fresh pane is **outside the modeled native-first epoch guarantee**; deferred preservation/discard UX is not claimed solved. An implementation that silently executes such a draft fails acceptance. Neither a hook boolean nor this model's GREEN replaces that test/policy boundary. Volatile old draft preservation and actual Claude 2.1.322 partial-tail resume are also not guaranteed by tmux close.

## 상태 차트

**SC_RUN=GREEN for the proposed cfgs.** Actual repository CI checker: 31 configs, **136 TLC runs**, all expected broken-rule sets matched, **84.3 seconds, exit 0**. 13 cfgs GREEN; 18 expected RED, including the explicit permanent post-content LostReceipt boundary. This zero is the expectation checker's success, not a claim that the old behavior passes. No model/cfg mutation followed this final check. The last model mtime is 06:39:58 UTC; the final checker log completed at 06:41:25 UTC, within 30 minutes of the 06:17 UTC start.

The frontend `/home/adk/.claude/skills/statechart/scripts/statechart` is absent. r2 explicitly uses Java 21/TLC 1.7.4 and the repository checker; `.chart.mmd` and `.view.mjs` are clearly hand-authored projections, not frontend-generated state-space output. The directly checked isolated-root copies are byte-identical to the committed model and all 31 cfgs, and their manifest entries match the repository manifest.

Command:

```sh
python3 scripts/ci/statecharts_tlc.py \
  --root /home/adk/adk/lane-runs/i8077/logs/d2/sc-root \
  --jar /home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar --jobs 2
```

Each suffix below is `HerdrBusyHandoff<suffix>.cfg`. Broken rules are the **actual** checker result; every row matched expected.tsv.

| cfg suffix | Actual result | Actual broken rules | TLC runs |
|---|---|---|---:|
| `AfterActiveEntry` | GREEN | — | 4 |
| `AfterActiveRace` | GREEN | — | 4 |
| `AfterBusy` | GREEN | — | 4 |
| `AfterContinuation` | GREEN | — | 4 |
| `AfterFailureUnread` | GREEN | — | 4 |
| `AfterFirstRefusal` | GREEN | — | 4 |
| `AfterJournalOnly` | GREEN | — | 4 |
| `AfterPaneReplacement` | GREEN | — | 4 |
| `AfterPidReuse` | GREEN | — | 4 |
| `AfterServerReplacement` | GREEN | — | 4 |
| `AfterStopFailure` | GREEN | — | 4 |
| `AfterUnreadDraft` | GREEN | — | 4 |
| `BeforeActiveEntry` | RED (expected) | HandoffProgress, MessageProgress | 4 |
| `BeforeActiveRace` | RED (expected) | NoIntakeBypass | 5 |
| `BeforeBusy` | RED (expected) | HandoffProgress, JournalProgress, MessageProgress, NoUserResend | 5 |
| `BeforeContinuation` | RED (expected) | SameSession | 5 |
| `BeforeFailureUnread` | RED (expected) | HandoffProgress, JournalProgress, MessageProgress | 4 |
| `BeforeFirstRefusal` | RED (expected) | JournalProgress, MessageProgress, NoUserResend | 5 |
| `BeforeJournalCensus` | RED (expected) | HandoffProgress, JournalProgress | 4 |
| `BeforeUnread` | RED (expected) | HandoffProgress, JournalProgress, MessageProgress, NoUserResend | 5 |
| `Controls` | GREEN | — | 4 |
| `DraftExecution` | RED (expected) | DraftNeverExecuted | 5 |
| `Duplicate` | RED (expected) | InputAtMostOnce, NoSilentLoss | 6 |
| `LostReceipt` | RED (expected) | JournalProgress, MessageProgress | 4 |
| `NoFirstWake` | RED (expected) | JournalProgress, MessageProgress | 4 |
| `NoNativeReady` | RED (expected) | JournalProgress, MessageProgress | 4 |
| `NoWake` | RED (expected) | HandoffProgress, JournalProgress | 4 |
| `PidSignal` | RED (expected) | NoForeignClose | 5 |
| `ReconnectNumber` | RED (expected) | NoForeignClose | 5 |
| `SilentDrop` | RED (expected) | JournalProgress, MessageProgress, NoSilentLoss | 5 |
| `WrongSession` | RED (expected) | SameSession | 5 |

The model uses real PROPERTY declarations MessageProgress, HandoffProgress and JournalProgress. Intake is incoming→probe→commit; HandoffProgress includes admitted inputs even when Begin is missing. Active N=2 permits both marker-absent probes before the first Begin, then detects a second commit that bypasses the current journal. Journal-only starts down with all inputs done, effects=1 and old processing terminal complete: progress of that journal cannot be hidden by vacuous MessageProgress. BootCensus and BootWake are separate transitions and mutants.

DraftNeverExecuted is exercised by Submit and a guard-removal mutant. OldUnread never changes. StopFailures=1 plus OldUnread=TRUE goes through provisional old/no-key/revalidate/close without a readable-screen fairness assumption. New first refusal is separately modeled on the new destination and requires native cause wake. Numeric-signal and reconnect-number mutants can close a foreign object; the correct capability cannot. Native continuation B→C occurs before close and the stale-native mutant fails SameSession.

Fairness assumes a recoverable store/host, finite daemon restarts/close and pre-key failures, eventual provider end or usable verified old execution, own native hook and obtainable receipts. Fresh identity reacquisition is abstracted, not proven to serialize arbitrary external process replacement. Permanent PTY-orphan, post-content receipt loss and arbitrary unsent human editing of a new pane are outside GREEN. Controls covers explicit positive-no-key cancellation, not a proof of every real Cancel/clear/stop/Terminate state machine. Real control-generation, adapter and native CLI tests remain required.

## 이미 main에서 제공하는 것과 구현 수용 조건

No reimplementation is planned for existing resume account/transcript eligibility (`herdr_turn.rs:590`), O cursor continuity (`session_handoff.rs:153`, `tui_o/writer/renumbered.rs:50`), exact own resume nonce/source checking (`session_handoff.rs:192`), --resume argv/context (`claude_tui/session.rs:372`, `claude/tui_session_launch.rs:138`), physical writer/first-key owner fences, or existing typed CreatedSubmit (`claude_tui/owner_input.rs:325`). All refer to production SHA above.

Required implementation evidence: fixed-unread + transient-close-failure with no resend; new first-input refusal through the real owner NotSentProof/Held/wake path; active marker-absent entry with the 46-column real busy fixture; N=2 intake race; pane/server/PID aliases without foreign mutation; input-free unfinished boot recovery; argv A/binding B/continuation C; real 2.1.322 interrupted partial-tail resume and exact native receipt/O boundary; human-draft and Cancel/clear late-callback cases. Complete records/spool/cursor must survive; close acknowledgment, hook receipt, empty metadata or process exit0 alone cannot pass.

Estimated production change **22 files / net +1180**, including +170 contingency, below 30/+1500 and PLAN +1200. Giant parents are kept at net zero or reduced by moving existing related responsibilities into small children. If +1200 is exceeded, independent A (+850) includes close/active Begin/reversible old choice/journal boot **and typed no-key custody+wake**; B (+330) switches native-first readiness and adds the framed adapter. A alone is not full issue completion.

| Review focus | Prevention or remaining counterexample |
|---|---|
| Boundary time | Separate close dispatch/end, receipt/processing terminal and hook/first payload. Re-read current native binding after end. Actual partial-tail behavior remains an acceptance condition |
| Concurrency | Probe/commit journal recheck, moved original claim, first-key fence; actual production active entry test still required |
| Restart | Journal-only census and wake mutants; capabilities cannot be restored as open connections. Old-binary downgrade with unfinished new journal is unsupported |
| Old format | Missing optional journal preserves ordinary V2 behavior; historical retained records remain untouched |
| Declared-only guards | Require actual caller tests and deletion mutants for close authority, active preflight, old release, native first proof/wake and boot driver |
| False 0/PASS | Per-cfg RED/GREEN sets, typed no-key proof, receipt1 and output/spool continuity. 31-model cfgs, three tmux cases and reused two predicate tests are separate counts |

## 증거 목록

External evidence root: `/home/adk/adk/lane-runs/i8077`. All paths below are under that root unless absolute.

| Evidence | Path / actual result |
|---|---|
| Exact external design / committed copy | `design-2.md` / `docs/statecharts/session-lifecycle/herdr-busy-handoff.design.md`, byte-identical |
| Review input | `review-design-1-pro.out.md`, four P1s and P2s read |
| Updated production grep | `logs/d2/entrypoints.txt`; d1 exhaustive `logs/handoff-entries.txt`, `logs/retained-entries.txt`, `logs/adjacent-entries.txt`, `logs/boot-incarnation-entries.txt` remain source-baseline evidence |
| Final official TLC run | `logs/d2/statechart-ci-6.log`: selected31/matched31, TLC136, exit0 |
| Parsed per-cfg results | `logs/d2/model-results.json`; 13 GREEN, 18 expected RED, 0 mismatch |
| Reproducible exact checked inputs | `logs/d2/sc-root/`; model+31 cfgs+manifest entries match worktree |
| Model build helper | `logs/d2/build_model.py`, frozen before final acceptance check |
| Protocol experiment / raw transcript | `logs/d2/tmux_control_probe.py`, `logs/d2/tmux-control-probe-4.log`, `logs/d2/tmux-control-protocol.json` |
| Protocol case assertions | `logs/d2/tmux-control-evidence.json`:3 passed,0 failed; no default socket or sleep polling |
| Environment and hashes | `logs/d2/evidence-design-2.json` |
| Final artifact audit | `logs/d2/final-checks.json`: global manifest/delta CI selection, checked-input identity, jar/probe/capture hashes and production diff0 |
| Reused d1 predicate log | `logs/predicate-probe-final.log`:2 passed,0 failed,0 ignored,0 filtered; **not rerun in r2** |
| Reused d1 predicate binary | `logs/predicate-probe/target/debug/deps/i8077_predicate_probe-69c97a0ef2df0bd8`, SHA256 `ad15cdda2b4b44a682c07e2c51d21dc5e88262f1d1bc47e366327c279188508a` |
| Reused test names | `services::claude_tui::busy_inject::existing_46_column_fixture_also_cannot_certify_an_exit`; `services::claude_tui::busy_inject::measured_field_and_synthetic_idle_both_fail_the_exit_stash_gate` |
| Reused supplied capture | `adkcc-pane-busy-1424.ansi`, SHA256 `b4ed2407b6cb970bf7d5538f3f86e9564974ce627aaf7ac19643da5d5bace893` |

Model SHA256: `049eb80b897bf733357901c2df009b601dfd88f5d9c6a289b8370747df07b4f7`. TLC jar SHA256: `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`. Earlier numbered authoring/probe failures are retained for audit and are not acceptance evidence.

R2 environment: SHELL=/bin/bash; TMPDIR unset; CARGO_TARGET_DIR unset and unchanged; Java21/OpenJDK, TLC1.7.4, WSL Linux, tmux3.4. R2 repository Rust test executable: **none built/run**; actual PG: **no**; cargo/check/clippy/source_scan_readers/prepush: **not run, design only**. D1 focused probe used cargo-shim and the requested DEBUG=0/STRIP=none profiles, default parallel tests. Production changes, runtime/config/DB/store access and operational tmux/Herdr mutation: **0**. Isolated tmux resources were closed. No background process is carried forward.
