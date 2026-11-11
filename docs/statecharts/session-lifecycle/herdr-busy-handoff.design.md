# Claude busy forward handoff — implementation slice A

Implementation code SHA: `62141396080f1db13e4341ef2ac26cb7abd0bbf4`. The following documentation commit changes no production code.

Code base: `07b57b5724f6bede32f7cc0cf00c7a23652ff3c2`; implementation starts on merge `92dd105adb37ec99df1496542f7641fc092d4c15`. This replaces the r2 proposal. The final implementation SHA and Rust evidence are recorded in `/home/adk/adk/lane-runs/i8077/report-impl-3.md`; model hash and actual cfg results are in the companion report.

## Behavior and scope

A mapped local Claude Legacy execution transitions before `hand_busy`, whether active, idle, narrow, unreadable or busy. The actor commits one unfinished journal over the current front grant. The old pane receives no keys: no `/exit`, stash, C-s, or fallback submission. A failed close preserves the queued original as Held, schedules a backoff wake, and after repeated failures owes one durable deduplicated notice without a resend request.

The close helper opens one tmux control connection, pins server/session/window/pane/process-start identity, checks the execution nonce/channel context, persists the pin and rechecks authority immediately before `kill-pane -t %N`. EOF discards that connection's authority. Neither a fresh connection's reused pane number nor a numerical PID/PGID is a termination capability. PID/start observations establish identity and positive absence only. The helper never escalates to kill-session/process-tree cleanup. Linux and macOS share the tmux protocol contract; actual private-pane tests run on Linux. macOS process identity remains the existing `ps` observation and needs platform execution coverage.

After old absence, `herdr_turn/handoff.rs` reads the durable binding event log strictly, uses its existing Claude history fold for Pending/Resolved/continuation, refuses corruption, Pending and replaced transcript identity, and selects the durable native UUID. A missing legacy log retains the original captured ID; an in-memory cache cannot override a durable pin. Existing account, transcript, O cursor and own SessionStart/resume-nonce guards remain. Explicit force-fresh does not depend on the old binding log.

The new first input uses the existing composer readiness and Person draft guard. A positive pre-key refusal is `SubmissionError::Held` through the real owner scope, positive NotSentProof, `hold_not_sent` and a level screen wake. Indeterminate sends remain uncertain; confirmed=0 alone never permits replay. No native-first capability or multiline adapter is added (B is deferred to #8080). C132 returns an early cancellation before binding registration or RuntimeReady, including row-pass cleanup.

## Crash contract and journal lifetime

Crash-free close failure, N=2 intake, composer refusal and cancellation preserve the existing queue original without a resend ask. The accepted exception is main's existing durable MayHaveSent/Bootstrap crash window: adopt restores Unknown and central tick retains Unconfirmed once. `retained.rs` is unchanged. This exception needs no protected Unknown custody or new submission protocol. The model represents it explicitly and applies NoRetained only while crashes=0. A retained original is terminal custody, not evidence of provider execution; InputAtMostOnce and NoSilentLoss retain separate meanings.

Reserve validates the Herdr target before discharging the handoff journal. Separately, Bound+ended recovery may finish a remaining journal only when its original is no longer owed in queue (settled, retained or canceled). A journal with no input still selects a boot actor and receives an explicit wake. Optional journal serde and None constructor defaults preserve old V2 snapshots. Historical retained recovery and Codex migration are outside this change.

## Entry and adjacent-path census

Paths below are repository-relative. Function anchors remain the reference when lines move.

| Entry or consumer | Code coordinate | Model/action and boundary |
|---|---|---|
| first/idle/active/retry owner front | `src/services/discord/queue_io/input_owner/drive.rs:87` | Probe/Commit under one channel actor |
| active Legacy preflight before busy registration | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:195`, `:232` | Begin before hand_busy; no marker prerequisite |
| active Bound busy queue | same file `:309` | existing composer path, second input remains one queue member |
| idle wrapper/late busy discovery | same file `:849`, `:939` | same owner front and existing start/target fences |
| wrapper refusal before reserve | `src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:124` | handoff_start_failed preserves Held and wakes |
| exact close dispatch and retry | `src/services/claude/session_handoff/pinned_tmux.rs:end_with`; `src/services/discord/queue_io/input_owner/claude_handoff.rs:prepare` | Revalidate/Close/CloseFails/RetryClose; no old keys |
| Legacy/Retired new launch | `src/services/claude/herdr_turn/handoff.rs:launched_source` | Launch after old absence, re-read durable ID |
| first new input/positive refusal | `src/services/claude/herdr_turn/first_input.rs:31`, `:85`, `:165` | Reserve/Refuse/WakeFirst and real proof |
| transcript observation/early cancel | same file `:176`, `:210` | C132 cancellation never announces runtime success |
| repeat-existing input | `src/services/discord/queue_io/input_owner/effect.rs:idle_claude` and `:created` | existing composer; N=2 model tags attempt2 existing |
| boot persisted actor census | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs:93` | input OR unfinished Claude journal |
| restart owner adoption and tick | `src/services/discord/queue_io/input_owner/recovery.rs:15` | existing main Unknown retention contract |
| journal-only wake and completion | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:675`; new child `claude_handoff.rs:recover` | Census/BootWake/Complete; owed check in actor |
| retained creation/consumption | `src/services/turn_orchestrator/queued_head/v2/retained.rs:46`, `:58`; owner recovery `:20` | unchanged main tick/requeue-known-unsent; no historical migration |
| Cancel exact queued input | `src/services/turn_orchestrator/queued_head/v2/control.rs:206` | invalidate its journal on committed removal |
| clear/clear_waiting | same file `:284`, `:411` | clear journal with removed originals; stale callbacks fail |
| stop/control ingress | `src/services/discord/commands/control.rs:617`, `:689`, `:715` | existing stop/cancel first-key fences; no new cleanup authority |
| Terminate/root replacement preclaim | owner_input `:849`; existing `discord/herdr_terminate/` | existing target/first-key ownership fences; no broad kill helper reuse |
| reverse Herdr return | `src/services/claude/herdr_return.rs`; `src/services/claude/session_handoff.rs:end_by_exit` | existing reverse `/exit` behavior unchanged |

Person Discord inputs acquire their existing actor receipt/front grant. Bot and AgentDesk automatic inputs retain existing sender/admission classification; this lane adds no synthetic person front. The old input pane is owned by its nonce/context and pin; the new provider input is owned by the durable attempt/writer/first-key gate. Person draft contents are neither read for close nor submitted by the handoff. Existing new-composer Person rejection remains.

## Remaining screen readers

The old close path uses tmux metadata/process identity and does not call composer readiness or draft_sighting. Existing first/new and subsequent composer checks remain in `claude/herdr_turn/composer.rs:observation` and `claude_tui` adapters. A pre-key composer refusal becomes typed Held plus screen wake, rather than a retained request. Permanent unreadable/new-person-draft screens remain a readiness boundary in slice A; the model keeps BlockedComposerBoundary expected RED for progress and never executes a Person draft. Removing that screen dependency is B, not silently included here.

The supplied adk-cc 46x39 ANSI sample established the narrow readiness failure. The private tmux close fixture embeds its UI footer; it is an excerpt, not a whole transcript or actual Claude process. Synthetic idle variants remain assumptions. Acknowledged pane close does not prove transcript flush or complete child shutdown. Isolated CLI check found 2.1.316 with no authentication; 2.1.322 partial-tail resume was not measured, as accepted by the coordinator. No operating HOME/auth/session was used.

## Verification and limits

Acceptance targets: private exact-pane/nonce-revocation/server-number-reuse tests; real owner Held/proof/wake/retry; C132 baseline RED and latest GREEN; active marker-absent PG entry with concurrent two admissions and fixed close failure; journal-only boot PG recovery; durable continuation/Pending/corrupt/source replacement; old Bootstrap crash retained exactly once with no leftover journal. A source mutant removes the final close authorization and must fail the private revocation test; restored bytes are hashed.

The formal model separates MayHaveSent, backend effect, native receipt and owner settlement. Boot census and wake are separate. BeforeEffectCrash deliberately injects the crash (weak fairness of that fault action in that cfg only); it cannot starve forever at an intentionally blocked pre-effect boundary. No close-success fairness exists in AfterPersistentClose. Controls/gateway/database/CLI behavior is established by Rust and platform acceptance, not inferred solely from model GREEN.

Size remains below PLAN +1200 and the 30-file/+1500 production cap; exact final cap and warning/check evidence belong to the implementation report. New responsibilities live in child helpers; giant parents carry connections or existing helper moves. Review focuses on close/end timing, concurrent admissions, cancellation before the final close fence, boot without input, output boundaries, old-format defaults and false 0/PASS counts.
