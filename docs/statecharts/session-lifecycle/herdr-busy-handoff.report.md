# Claude forward handoff — slice A model report

Implementation code SHA: `62141396080f1db13e4341ef2ac26cb7abd0bbf4`. The following documentation commit changes no production code.

Code base SHA: `07b57b5724f6bede32f7cc0cf00c7a23652ff3c2`; merge checkpoint `92dd105adb37ec99df1496542f7641fc092d4c15`. Final implementation SHA and Rust evidence: `/home/adk/adk/lane-runs/i8077/report-impl-3.md`. Model SHA256: `84a3a98a4475365cf5c483e4b75826565138848abf496bff2c06b62d08353341`.

This replaces r2: no old destination/fallback/C-s/reversible-old transitions, no native readiness capability. Existing composer and Person draft guard remain. Actual accepted crash contract is Unknown→tick→retained; NoRetained covers crash-free executions. Terminal retained custody does not claim an execution. BeforeEffectCrash explicitly schedules its injected fault. See the [design](herdr-busy-handoff.design.md) and [chart](herdr-busy-handoff.chart.mmd).

Actual checker: Java21 + TLC1.7.4, repository `scripts/ci/statecharts_tlc.py`, 26 cfg / 130 TLC runs, 13 GREEN / 13 expected RED, checker exit0. All expected violation sets match. Raw log: `/home/adk/adk/lane-runs/i8077/logs/impl3d/statechart-2.log`; input copy: `logs/impl3d/sc-root/`. The initial updated run exposed a missing fairness condition for the intentionally injected pre-effect fault; it exited1 and is preserved as `statechart-1.log`. Model editing ended within 30 minutes of this resume.

| cfg suffix | Previous r3 | Current actual | Actual broken rules | TLC runs |
|---|---|---|---|---:|
| `AfterActiveEntry` | GREEN | GREEN | — | 5 |
| `AfterActiveRace` | GREEN | GREEN | — | 5 |
| `AfterBusy` | GREEN | GREEN | — | 5 |
| `AfterFirstRefusal` | GREEN | GREEN | — | 5 |
| `AfterHumanDraft` | GREEN | GREEN | — | 3 |
| `AfterJournalOnly` | GREEN | GREEN | — | 5 |
| `AfterPaneReplacement` | GREEN | GREEN | — | 5 |
| `AfterPersistentClose` | GREEN | GREEN | — | 3 |
| `AfterReceiptGapCrash` | GREEN | GREEN | — | 5 |
| `AfterSecondComposer` | GREEN | GREEN | — | 5 |
| `AfterServerReplacement` | GREEN | GREEN | — | 5 |
| `AfterTransientClose` | GREEN | GREEN | — | 5 |
| `BeforeActiveEntry` | RED | RED (expected) | HandoffProgress, MessageProgress | 5 |
| `BeforeEffectCrash` | RED | GREEN | — | 5 |
| `BeforeUnread` | RED | RED (expected) | HandoffProgress, JournalProgress, MessageProgress, NoRetained | 6 |
| `BlockedComposerBoundary` | RED | RED (expected) | JournalProgress, MessageProgress | 5 |
| `DraftBypass` | RED | RED (expected) | JournalProgress, MessageProgress, NoHumanDraftExecution | 6 |
| `NoBootWake` | RED | RED (expected) | JournalProgress | 5 |
| `NoFirstWake` | RED | RED (expected) | JournalProgress, MessageProgress | 5 |
| `NoJournalCensus` | RED | RED (expected) | JournalProgress | 5 |
| `NoRetry` | RED | RED (expected) | RetryProgress | 3 |
| `OldFallback` | RED | RED (expected) | NoOldKeys | 4 |
| `ReconnectNumber` | RED | RED (expected) | NoForeignClose | 6 |
| `ReplayUnknown` | RED | RED (expected) | InputAtMostOnce, JournalProgress, MessageProgress, NoSilentLoss | 7 |
| `StaleIntake` | RED | RED (expected) | NoOldKeys | 6 |
| `UntypedHeld` | RED | RED (expected) | NoRetained | 6 |

Only BeforeEffectCrash changed result: former progress RED becomes GREEN under the accepted terminal retention contract. Its NoRetained is explicitly crash-free; it is not a proof that Bootstrap crashes avoid main retention. Other cfg result/violation sets are unchanged. BeforeUnread and UntypedHeld remain RED for a crash-free retained leak. Persistent close failure checks custody/retry/old keys with no successful-close fairness. BlockedComposerBoundary remains progress RED because slice A preserves the screen gate. ReplayUnknown still detects duplicate execution; no Unknown replay is added.

Run command:

```sh
python3 scripts/ci/statecharts_tlc.py --java /usr/lib/jvm/java-21-openjdk-amd64/bin/java --jar /home/adk/.cache/tla2tools/tla2tools.jar --root /home/adk/adk/lane-runs/i8077/logs/impl3d/sc-root --jobs 4
```

The view and Mermaid chart are hand-authored projections, not TLC-generated state-space output. Actual Rust, DB, private tmux and CLI evidence has separate counts in the implementation report. macOS execution and 2.1.322 partial-tail resume are not claimed by these model results.
