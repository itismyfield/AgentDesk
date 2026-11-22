# Codex legacy handoff (design r2)

A is superseded by [design r3](codex-marker-only-a.design.md): r2 omitted Found Legacy admission and split cleanup/outbox crash boundaries. B/C and their cfgs are retained as deferred design evidence.

Code snapshot: `289f41b1d0f48fab0dc426e652308eced4326fce`. This is a proposed contract, not an implementation claim.
The existing `HerdrSession` module now has a separate `HandoffSpec`; `HandoffPolicy=off` preserves its original session actions and known gaps.

| Config | Safety violations | PROPERTY HandoffProgress |
|---|---|---|
| HerdrHandoffTargetBefore | NoWrongExecutionExit, NoUnexpectedEnter, NoLiteralIntoDraft, NoBusyExit | RED |
| HerdrHandoffHintBefore | none | RED |
| HerdrHandoffQueueBefore | none (the original bytes remain retained) | RED |
| HerdrHandoffPersonBefore | none | GREEN |
| HerdrHandoffFixed | none (11 invariants, 10 initial scenarios) | GREEN |

The repository TLC checker checks the complete expected violation set, removing each violated rule and rerunning. The first focused check covered 6 configs (19 TLC runs). The repository dependency selection also includes the 7 inherited Pending-registration configs; all 13 configs match expected.tsv (30 TLC runs), with the new handoff state frozen in initialization and every inherited action, and policy off. All original expected violations remain unchanged. The fixed graph has 268 distinct states and depth 19. The original config still breaks exactly NoDeadlock, NoOrphanPane, PromptOnlyToAgent and RowMatchesPane; those historic findings are not repaired here.

Counterexamples against code:

* TargetBefore: observe old identity -> pin -> same-name pane/process replacement -> literal or Enter through unconditional LegacyTmuxGate. Code: `claude_tui/host_input.rs:144`, `codex_tui/host_input.rs:215`. A human draft or busy screen appearing between writes is also unguarded in this r1 contract.
* HintBefore: runtime hint with Missing tmux -> non-person admission reads Unknown -> Wait::Event forever. Code: `turn_presence/activity/unbound.rs:289-328`, `provider_dispatch/owner_input.rs:859-872`. No cleanup/wake transition exists in this r1 path.
* QueueBefore: two generic start errors -> retained/manual-retry original -> exact old execution ends -> no original resume. Code: `queued_head/v2/start_failure.rs:81-113`, `input_owner/held_watch.rs:64`. This is a progress failure, not evidence that raw text was deleted.
* PersonBefore: a Discord original bypasses start_waits, reaches fresh launch, and the exact fresh Herdr host/nonce permits runtime-hint cleanup on tmux Missing (`herdr_launch.rs:492`, `runtime_marker.rs:68-86`). This reconciles the reported adk-dash message with the non-person counterexample; it does not identify an actual previously stalled synthetic front from message IDs alone.

Bounds and assumptions:

One original/group, one replacement (the identity is the whole server/pane/provider/owner/process-birth/nonce tuple), one crash, one human draft/busy change, literal and Enter as distinct writes. Queue failure threshold 2 is from main. A logical wake token is idempotent; its delivery may be retried after restart. Runtime hint cleanup requires exact session Missing AND process registry empty, with stable owner and no conflicting active/durable/inflight witness in the marker-only scenario; a shell/dead pane is not that proof.

`HandoffSpec` applies weak fairness to the actual next action and the exact old execution's Ended event. Stable supported input progresses after a provable Ended; a crash after durable write intent but before a write can need an external Ended. The model does not promise that an immortal provider, endless probe failure or an unverified CLI will automatically exit. Unsupported/unknown/shell cases terminate as explicit notified refusals while retaining the original.

`HLiteral` and `HEnter` abstract a NEW compare-and-write port. Each physical write must prove the pinned identity and appropriate composer state atomically with respect to managed replacement; an ordinary read followed by `send-keys` is not a refinement. Existing SourceAuthority is only a process-local Mutex (`tmux_common.rs:75-128`). Interprocess fencing, tmux command scheduling, PID reuse, filesystem compare/unlink, real Codex `/exit` semantics and multi-original payload persistence remain implementation/acceptance obligations. If a port cannot satisfy them it is Unsupported and produces zero live input. Model GREEN alone cannot authorize a live exit.

The handoff calls the original StartLaunch/CreateOk/RecordPane/RecordEvidence/Bind actions only after Ended and cleanup; create-reply loss, unplaced resolution, full Startup/native UUID validation and O cursor recovery remain covered by their existing models and implementation tests. The Handoff scope uses the successful downstream path and does not erase the original model's failure cases.

Reproduce with Java 11+ and tla2tools 1.7.4:

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/usr/bin/java
python3 scripts/ci/statecharts_tlc.py --base 289f41b1d0f48fab0dc426e652308eced4326fce --head HEAD --jobs 2
```

Raw logs, state dumps and positive reachability witnesses are in `/home/adk/adk/lane-runs/i8057/logs/h2/`; `model-evidence.json` verifies real exit, waiting, cleanup/wake, restart and refusal states were reached. The compatibility-only child initialization/UNCHANGED and explicit off constants do not add handoff behavior to Pending registration. The first dependency-wide run caught the missing inheritance initialization; its raw ERROR log is retained as `tlc-head-inheritance-red.log`, not counted as a semantic counterexample. The design and source-entry/action inventory are in `/home/adk/adk/lane-runs/i8057/design-h-2.md`.

The r1 counterexamples instantiate target/hint/queue/person only; the restart scenarios assert the proposed r2 observation-only policy. They do not claim to prove r1 restart safety. Durable storage success and eventual controller/outbox scheduling are part of the fairness abstraction. Multi-original priority is checked against code and reserved for implementation tests, rather than inferred from this one-group graph. The Mermaid file is a manually drawn action summary; TLC results come from the repository checker.
