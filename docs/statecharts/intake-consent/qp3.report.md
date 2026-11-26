# Consent for a confirmed child on its parent's endpoint

The card is posted on the actual Discord child and bound to its channel, message and nonce. The execution endpoint comes from the confirmed parent. Each held original retains its own provider, source, generation and ticket. See the [checked chart](qp3.chart.mmd).

`SpecQp3` starts at a durable hold. Weak fairness covers successful issue or retry, an exact approval by the current owner, and apply on the verified execution endpoint. The finite model allows one failed card send, one restart and two receipt replays. It checks custody preservation and at most one release per original as part of `EffectAtMostOnce`. The existing `Spec` checks the proof and CAS steps before the durable hold.

## Results

TLC 1.7.4 checked base `afc7377ba0` and the model for production revision `aa3cd4d3b6`. RED names the rules violated on that configuration.

| Configuration suffix after `IntakeConsent` | Before | After | Distinct states |
| --- | --- | --- | ---: |
| Bookkeeping | RED | RED, RollbackExactAndBookkeeping | 15,922 |
| Current | GREEN | GREEN | 15,482 |
| D1Public | RED | RED, NoUnauthorizedIssue and NoEffectsThenRollback | 67,364 |
| Forge | RED | RED, NoUnauthorizedIssue | 74,126 |
| OwnerOnly | RED | RED, NoStaleProofWrite | 15,998 |
| Proposed | GREEN | GREEN | 15,922 |
| Reuse | RED | RED, ProofConsumedOnce | 29,234 |
| ScopeOpen | RED | RED, NoUnauthorizedIssue and NoEffectsThenRollback | 29,096 |
| Stale | RED | RED, NoStaleProofWrite | 16,946 |
| Qp3Before | RED, EffectEventuallyOne | Baseline reproduced | 2 |
| Qp3Fixed | Qp3Before witnesses stalled approval | GREEN, all three rules | 70 |
| Qp3MutOwnEndpoint | New configuration | RED, EffectEventuallyOne | 4 |
| Qp3MutReplayEffect | New configuration | RED, EffectAtMostOnce and ReplayAddsNoEffect | 70 |

## Counterexamples

- Qp3Before reaches retry and stutters there. The base adapter looked up the held child's own endpoint when opening the peer and again at the retry command. [Base adapter](https://github.com/itismyfield/AgentDesk/blob/afc7377ba0effacf797718ef2245032e5e4ed056/src/services/discord/router/herdr_consent.rs#L58-L69), [base retry](https://github.com/itismyfield/AgentDesk/blob/afc7377ba0effacf797718ef2245032e5e4ed056/src/services/discord/router/herdr_consent.rs#L205-L221).
- Qp3MutOwnEndpoint allows issue and retry to post a card. The trace stutters in Pending because the decision looks up the child's distinct own endpoint. The component checks the inherited endpoint and matches the real card. [Decision adapter](../../../src/services/discord/router/herdr_consent.rs#L152).
- Qp3MutReplayEffect violates both safety rules on Hold → IssueCard → Decide → ApplyConsent → ReplayApplied. Replay reaches two effects while the durable receipt records one. Production recovery maps Applied to ReleaseThenWake, and custody settlement uses the stored original intents. [Recovery workflow](../../../src/services/session_host/herdr/policy/consent/workflow.rs#L220), [settlement](../../../src/services/session_host/herdr/policy/consent/settlement.rs#L268).
