# CampaignVerification

TLC reached 255 states and 840 steps. The chart draws 19 states and 66 arrows.
The chart does not draw 143 steps that stay in one chart state.
The view hides 239 steps, so the chart does not draw them.

## ReceiptDecisionUsesLockedFacts breaks
The shortest path has 5 steps:
0. no caller holds the lock; durable effect 0, receipt absent
1. a request reads facts before acquiring the lock: no caller holds the lock; durable effect 0, receipt absent
2. one caller acquires the campaign and run locks: reading the saved receipt; durable effect 0, receipt absent
3. the caller checks the receipt before deciding: reading locked facts; durable effect 0, receipt absent
4. the service keeps the snapshot read before the lock: deciding from a snapshot read before the lock; durable effect 0, receipt absent
5. the service decides without releasing the lock: writing state and evidence in the transaction; durable effect 0, receipt absent; decision used facts from outside the lock

## Rules that hold
ReceiptEffectAtMostOnce, ReceiptAtomicEffects, ReceiptRollbackHasNoEffect, ReceiptSingleLock, NoDeadlock, ReceiptCallsComplete
