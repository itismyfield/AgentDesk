# Codex marker-only A design r5: bounded fenced DB work

Code SHA: `289f41b1d0f48fab0dc426e652308eced4326fce`; model/document input: `858150d65d7f79b37f35f92184aecb8eb595d569`. Production code is unchanged. [Design](codex-marker-only-a.design.md) covers A-fence and A-admission under the 19:2x scope update; B/C remain deferred.

The existing AHintSpec now has an explicit DB-pending region. Requests may never reply. Input/clear/cancel can arrive while the physical fence is held, and CAS may commit after the local future is abandoned. Fixed applies a 3-second fenced DB deadline, a shared 9-second controller budget and a 1-second independent notice budget. Read timeouts may retry at most three times; an uncertain CAS is issued once and preserves its nonce/possible Pending outcome without file/create effects.

| cfg | plain safety exit | property-only exit | repository checker |
|---|---:|---:|---|
| HerdrHintADbBefore | 12 | 13 | DGuardBound, DWaitProgress RED; 3 runs |
| HerdrHintADbFixed | 0 | 0 | GREEN; 2 runs |
| HerdrHintADbNoTimeout | 12 | 13 | DGuardBound, DWaitProgress RED; 3 runs |

Fixed: 11 initial cases, 3225 generated / 1934 distinct states, depth 20. Four invariants and DWaitProgress pass with applied fairness for AHintNext and clock advancement. Seventeen positive witnesses cover all six incoming-event/stage pairs actually occurring during pending DB work, their release, three read timeouts, a late CAS, successful replies and unavailable notice not reported notified. Property-only before traces retain fence/pendingDb at clock=10 after clear/cancel. Derived property-only configs/logs are outside the repository and leave its cfgs unchanged.

Clock units are seconds. Expiry is urgent before the next clock tick under fair cooperative scheduling. This is a model of DB-induced waiting, not a hard real-time OS guarantee. The implementation must use timeout_at on the SQLx future in the guard-owning frame, never on only a spawned task/JoinHandle; no physical guard may sit in a queued blocking closure or clone carrier. Actual SQLx cancellation, RAII, post-timeout connection cleanup, file/tmux blocking behavior, bootstrap catalog failure and native Startup/Bound are implementation tests. No query-drop/timeout is treated as proof a CAS rolled back.

Existing cfgs and expected rows are unchanged; three cfgs and rows are added. The previous bounded-error-continuation models still run. Their single-pass effect counter is not proof of exactly-once wake replay or post-prompt crash handling. DNoLateEffects checks the deliberately isolated pending-DB region; it does not model a complete successful launch.

A-fence estimate: 15 prod files/+434, with active current callers and bounded CAS classification. A-admission: 30/+1135 on that dependency, including both bootstrap consumers and missing module/literal wiring. Total 43 distinct/+1569 is not one review slice.

Reproduce with Java 21 / TLC 1.7.4:

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/usr/bin/java
python3 scripts/ci/statecharts_tlc.py --base 858150d65d7f79b37f35f92184aecb8eb595d569 --head HEAD --jobs 2
```

The Mermaid file is a manually maintained summary. Raw logs/state dumps and the final head's full dependency check are in `/home/adk/adk/lane-runs/i8057/logs/h5/` and the external report. No Rust/PG/operational acceptance is claimed.
