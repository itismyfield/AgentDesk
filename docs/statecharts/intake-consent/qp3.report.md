# Consent for a confirmed child on its parent's endpoint

The card is posted on the actual Discord child and bound to its channel, message and nonce. The execution endpoint comes from the confirmed parent. Each held original retains its own provider, source, generation and ticket. See the [checked chart](qp3.chart.mmd).

`SpecQp3` starts at a durable hold. Weak fairness covers issue or retry, approval, live owner validation and apply, receipt replay, and release of each eligible original. An Applied original eventually receives one release effect or is settled by an exact clear. The model admits one restart while a replay attempt remains, at most two replay attempts, and one successful clear. Clear starts from the exact durable queue removal and keeps the sibling channel, another generation, and the foreign endpoint intact. The chart merges pre-Apply clears into the current card phase. Mailbox and outbox activity is abstracted as an effect on the original custody reference. The existing `Spec` covers proof and CAS before the hold.

## Results

TLC 1.7.4 compared model revision `37105b5665` with the extended model of production revision `aa3cd4d3b6`. RED lists violated rules.

| Configuration suffix after `IntakeConsent` | Before | After | Distinct states before → after |
| --- | --- | --- | ---: |
| Bookkeeping | RED, RollbackExactAndBookkeeping | Same | 15,922 → 15,922 |
| Current | GREEN | GREEN | 15,482 → 15,482 |
| D1Public | RED, NoUnauthorizedIssue and NoEffectsThenRollback | Same | 67,364 → 67,364 |
| Forge | RED, NoUnauthorizedIssue | Same | 74,126 → 74,126 |
| OwnerOnly | RED, NoStaleProofWrite | Same | 15,998 → 15,998 |
| Proposed | GREEN | GREEN | 15,922 → 15,922 |
| Reuse | RED, ProofConsumedOnce | Same | 29,234 → 29,234 |
| ScopeOpen | RED, NoUnauthorizedIssue and NoEffectsThenRollback | Same | 29,096 → 29,096 |
| Stale | RED, NoStaleProofWrite | Same | 16,946 → 16,946 |
| Qp3Before | RED, EffectEventuallyOne | RED, EffectEventuallyOne and RetryEventuallyPosts | 2 → 4 |
| Qp3Fixed | GREEN, 3 rules | GREEN, 9 rules | 70 → 96 |
| Qp3MutClearGeneration | New | RED, ClearPreservesUnmatched | — → 96 |
| Qp3MutDuplicateRelease | New | RED, OriginalReleaseAtMostOnce | — → 156 |
| Qp3MutOwnEndpoint | RED, EffectEventuallyOne | Same | 4 → 8 |
| Qp3MutOwnRoute | New | RED, EffectEventuallyOne, OriginalEventuallySettled, RetryEventuallyPosts, LiveOwnerEventuallyApplies and AppliedEventuallyResumes | — → 98 |
| Qp3MutReplayEffect | RED, EffectAtMostOnce and ReplayAddsNoEffect | Same | 70 → 96 |

## Counterexamples and code

- Qp3Before stalls during retry. Qp3MutOwnEndpoint posts a card then stalls in Pending because the decision uses the child's own endpoint. The component checks the inherited endpoint and matches the real card. [Base retry](https://github.com/itismyfield/AgentDesk/blob/afc7377ba0effacf797718ef2245032e5e4ed056/src/services/discord/router/herdr_consent.rs#L205-L221), [decision adapter](../../../src/services/discord/router/herdr_consent.rs#L152).
- Qp3MutOwnRoute chooses one faulty endpoint lookup per execution. The retry variant stalls with no card after a failed send. The live-owner variant stalls after approval as preflight rejects the config apply. The recovery variant stalls once a process restart needs receipt replay, leaving an eligible original unsettled. Each stalled boundary has its own temporal property and TLC trace. [Retry](../../../src/services/discord/router/herdr_consent.rs#L237), [live owner](../../../src/services/discord/router/herdr_consent/transport.rs#L98), [Applied recovery](../../../src/services/discord/router/herdr_consent.rs#L416).
- Qp3MutDuplicateRelease follows Hold → IssueCard → Decide → ApplyConsent → ReleaseOriginal → ReplayApplied → ReleaseOriginal. The same custody reference receives a second effect. Qp3MutReplayEffect instead repeats config apply during replay. The runtime uses ReleaseThenWake for Applied receipts and guards each release and wake with the durable original custody marker. [Recovery workflow](../../../src/services/session_host/herdr/policy/consent/workflow.rs#L220), [settlement](../../../src/services/session_host/herdr/policy/consent/settlement.rs#L268), [custody](../../../src/services/discord/router/herdr_consent/custody.rs#L209).
- Qp3MutClearGeneration discards both generations in one clear. The runtime checks the installed endpoint and exact provider, channel, source and generation after confirming durable removal. [Clear settlement](../../../src/services/discord/commands/control/clear_report.rs#L150).
