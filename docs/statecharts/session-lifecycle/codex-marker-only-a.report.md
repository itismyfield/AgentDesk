# Codex marker-only A design r3

Code SHA: `289f41b1d0f48fab0dc426e652308eced4326fce`; model input: `dfba87a0ae2a576507f0863e81cfb8f1ead8bfdb`. Production code is unchanged. The committed [design](codex-marker-only-a.design.md) specifies A only. Exit and HandoffWaiting B/C remain deferred; their old cfgs are untouched.

`AHintSpec` extends the existing HerdrSession module using its existing handoff variable. It preserves the original session/Pending specifications and expected violations. Hosted lookup is explicitly Missing / Legacy / Retired. A-before is r2's proposed cleanup plus main's unchanged admission, rather than an assertion that main already performs that cleanup.

| cfg | actual exit | broken rule |
|---|---:|---|
| HerdrHintALegacyBefore | 13 | AProgress |
| HerdrHintAMissingBefore | 0 | none |
| HerdrHintAFixed | 0 | none |
| HerdrHintANoFrontCheck | 12 | ANoStaleGrant |
| HerdrHintANoOutboxDiscovery | 13 | AProgress |

The focused repository checker matches all five expected sets in 11 TLC runs. Fixed checks 6 invariants plus a real liveness PROPERTY with applied `WF_vars(AHintNext)`: 15 initial cases, 297 generated states, 270 distinct states, depth 18. State-dump checks confirm 17 positive witnesses, including both successful row kinds, partial unlink recovery, both commit crash gaps, a completed restart, stale front/epoch refusal, post-cleanup file/row replacement refusal, wake redelivery with one prompt, fence contention, and outbox-only boot acknowledgement. NoFrontCheck is a safety mutant; NoOutboxDiscovery is a progress mutant. The raw Legacy trace ends in admit→wait→stutter after the hint is gone and a wake was delivered.

The model separates intent commit, release/recheck, canonical unlink, legacy unlink, cleanup receipt commit, outbox creation, wake, fresh read-only admission, exact-front grant, create/prompt effects and ack. A crash after unlink cannot erase the earlier intent. A committed receipt without an outbox remains discoverable. A physical wake may be delivered twice; logical grant/create/prompt each stay at most one. Unknown ownership/probe failure yields zero hint deletions. Changed eligibility preserves the original and is explicitly notified; it is not a generic launch failure.

Bounds: one original, two paths, one crash, one partial-unlink error, one front/epoch/file/row disturbance, one wake redelivery. Durable storage/notification/controller scheduling eventually succeeds. There is no promise under permanent storage failure, infinite preemption or uncooperative writers. Known Retired is an A refusal control, not a replacement for the existing stricter Herdr nonce cleanup path.

The physical union of per-entry locks, DB identity uniqueness, actual Source/OS lock ordering, real unlink/fsync behavior and full boot/source-dedup paths are implementation obligations, not results of this abstract model. `ACreate/APrompt` count the successful downstream effects; they do not replace the existing Pending/Startup/owned-input models or actual PG tests. No A action sends `/exit` or closes/kills a live execution. Existing B/C GREEN does not establish these A properties.

Reproduce with Java 21 and TLC 1.7.4:

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/usr/bin/java
python3 scripts/ci/statecharts_tlc.py --base dfba87a0ae2a576507f0863e81cfb8f1ead8bfdb --head HEAD --jobs 2
```

The Mermaid chart is a manually maintained action summary. Raw logs and state dumps are in `/home/adk/adk/lane-runs/i8057/logs/h3/`; `model-evidence-final.json` checks positive reachability. Full dependency selection and its actual head SHA are recorded in the external report. The design allocates A as A-fence (12 prod files/+275) then A-admission (21/+825), 32 distinct/+1100 total; neither includes B/C.
