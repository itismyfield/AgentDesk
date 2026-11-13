# Managed idle stash and transcript readiness

Code baseline: e7a769290198aa998408014ea57a01323a619709. Repair source: src/services/claude_tui/readiness/wait.rs, SHA256 0356b1277ce18ae803dc53fc318babeb2b655a70851d48d2a3662844d4e7585c.

The existing managed stash policy may park a person's draft after current Idle/SessionStart evidence. The readiness wait formerly required an empty composer and could block that existing writer path. Only the composer admission assumption changes; current-source, continuation, reservation and first-key guards stay in force.

The before mode retains a person draft without allowing a human-clear action, so ReadyPendingEventuallyExecutes fails. The after mode admits the existing managed C-s/paste/Enter/restore round trip, models the person's draft as unchanged, and counts one round trip per own input. ReadyManagedDraftPreserved and the existing no-human-draft-execution and no-Running-injection invariants hold.

| cfg | Expected | Direct TLC runs | Observed |
| --- | --- | ---: | --- |
| InputPathReadyManagedStashBefore.cfg | RED, ReadyPendingEventuallyExecutes | 2 | expected RED |
| InputPathReadyManagedStashAfter.cfg | GREEN | 2 | GREEN |

Direct execution used the repository statecharts_tlc.check_cfg implementation, plain tla2tools.jar, and all named cfg rules. Production source readiness remains a separate conjunct before any managed draft admission. Busy injection and unmanaged draft veto are unchanged.

Final readiness-only replay: 42 cfg / 109 plain TLC calls, 24 GREEN and 18 expected RED, unexpected 0. The broader checker selection included unrelated campaign models and was stopped; its partial run is not counted as a successful validation.
