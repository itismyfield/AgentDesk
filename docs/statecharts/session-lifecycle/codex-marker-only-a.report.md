# Codex marker-only A design r4

Code SHA: `289f41b1d0f48fab0dc426e652308eced4326fce`; model/document input: `89a8ea63c4b86c79247f1d68118815b1e3ce7a3f`. Production code is unchanged. [Design](codex-marker-only-a.design.md) remains A only; B/C are deferred.

The existing AHintSpec now distinguishes a live retained controller from a dropped callback. A failing pre-payload step consumes one of three failures and releases physical/Source guards. ARetryContinue needs the existing callback's controller; ARecover also needs it. The existing Drive awaits that callback at drive.rs:101. There is no new wake, timer, periodic sweep or input event in the retry path. Boot observation uses the same bounded controller.

| new cfg | actual plain TLC exit | checker result |
|---|---:|---|
| HerdrHintARetryFixed | 0 | GREEN, 2 runs |
| HerdrHintARetryNoController | 13 | AProgress RED, 2 runs |
| HerdrHintARetryNoBudget | 13 | AProgress RED, 2 runs |

RetryFixed: 7 initial cases, 120 generated / 110 distinct states, depth 19. Seven invariants and AProgress with applied WF_vars(AHintNext) pass. Transient cases cover partial unlink, intent commit, receipt commit and boot reobservation. Permanent unlink/commit/boot failures exhaust three failures and terminate through preserved custody plus notice. NoController freezes after canonical unlink succeeded and legacy unlink failed, before receipt/outbox exists. NoBudget retains the future but loops forever on a permanent failure. Both REDs are semantic liveness counterexamples, not parser errors.

The A recovery source is now explicit, rather than making ARecover unconditionally available after an error. The before cfgs and all previous expected.tsv rows remain byte-identical. The shared model is corrected, so its old configs are rerun with their same expected sets; the old graph counts are historical, not this head's results. Eleven positive state witnesses confirm pre-receipt continuation without an extra wake/restart and bounded notified termination; full dependency selection is in the external report.

Proof scope is deliberately narrower than r3: this verifies bounded pre-payload continuation/termination and preserves the original on modeled failure. ANoDuplicateEffects remains a single-pass invariant. ARedeliver only increments a counter, and ACrash excludes grant/create/prompt/ack, so neither establishes end-to-end exactly-once behavior under wake replay or a crash after a prompt. Those scenarios require actual owner/front-claim/reservation/Startup tests. No stronger deduplication claim is made from the linear post-grant path.

ANotice abstracts a control outcome with preserved custody and an available independent notice sink. It is not proof that a RetainedInput commit succeeds when the queue filesystem is permanently unavailable. The design distinguishes retained commit from unchanged bytes/storage hold plus existing PG-outbox notice. If that independent sink is permanently unavailable too, the implementation must not report notified success. Actual file contents, fsync uncertainty, typed public/internal activity observations, physical lock identity, TTL pins, cross-process mutation and full Pending/Bound recovery remain implementation obligations.

Reproduce with Java 21 / TLC 1.7.4:

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/usr/bin/java
python3 scripts/ci/statecharts_tlc.py --base 89a8ea63c4b86c79247f1d68118815b1e3ce7a3f --head HEAD --jobs 2
```

The Mermaid file is a manually maintained summary. Raw logs/state dumps: `/home/adk/adk/lane-runs/i8057/logs/h4/`. A-fence estimate is 14 prod files/+299; A-admission is 26/+998. The 39 distinct/+1297 total is not one review slice.
