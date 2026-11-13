# CampaignVerification

TLC reached 548 states, more than the chart budget of 500, so the chart draws only the bug paths: 12 states and 13 arrows.

## ReceiptAtomicEffects breaks
The shortest path has 6 steps:
0. no caller holds the lock; durable effect 0, receipt absent
1. a request begins a transaction: no caller holds the lock; durable effect 0, receipt absent
2. one caller acquires the campaign and run locks: reading the saved receipt; durable effect 0, receipt absent
3. the caller checks the receipt before deciding: reading locked facts; durable effect 0, receipt absent
4. the caller reads state and stored seal inside the lock: deciding from locked facts; durable effect 0, receipt absent
5. the service decides without releasing the lock: writing state and evidence in the transaction; durable effect 0, receipt absent
6. state and evidence escape before receipt commit: rendering the saved response; durable effect 1, receipt absent

## ReceiptRollbackHasNoEffect breaks
The shortest path has 7 steps:
0. no caller holds the lock; durable effect 0, receipt absent
1. a request begins a transaction: no caller holds the lock; durable effect 0, receipt absent
2. one caller acquires the campaign and run locks: reading the saved receipt; durable effect 0, receipt absent
3. the caller checks the receipt before deciding: reading locked facts; durable effect 0, receipt absent
4. the caller reads state and stored seal inside the lock: deciding from locked facts; durable effect 0, receipt absent
5. the service decides without releasing the lock: writing state and evidence in the transaction; durable effect 0, receipt absent
6. state and evidence escape before receipt commit: rendering the saved response; durable effect 1, receipt absent
7. a failure rolls back and releases the locks: no caller holds the lock; durable effect 1, receipt absent; rollback left a durable effect

## ReceiptEffectAtMostOnce breaks
The shortest path has 13 steps:
0. no caller holds the lock; durable effect 0, receipt absent
1. a request begins a transaction: no caller holds the lock; durable effect 0, receipt absent
2. one caller acquires the campaign and run locks: reading the saved receipt; durable effect 0, receipt absent
3. the caller checks the receipt before deciding: reading locked facts; durable effect 0, receipt absent
4. the caller reads state and stored seal inside the lock: deciding from locked facts; durable effect 0, receipt absent
5. the service decides without releasing the lock: writing state and evidence in the transaction; durable effect 0, receipt absent
6. state and evidence escape before receipt commit: rendering the saved response; durable effect 1, receipt absent
7. a failure rolls back and releases the locks: no caller holds the lock; durable effect 1, receipt absent; rollback left a durable effect
8. a request begins a transaction: no caller holds the lock; durable effect 1, receipt absent; rollback left a durable effect
9. one caller acquires the campaign and run locks: reading the saved receipt; durable effect 1, receipt absent; rollback left a durable effect
10. the caller checks the receipt before deciding: reading locked facts; durable effect 1, receipt absent; rollback left a durable effect
11. the caller reads state and stored seal inside the lock: deciding from locked facts; durable effect 1, receipt absent; rollback left a durable effect
12. the service decides without releasing the lock: writing state and evidence in the transaction; durable effect 1, receipt absent; rollback left a durable effect
13. state and evidence escape before receipt commit: rendering the saved response; durable effect 2, receipt absent; rollback left a durable effect

## Rules that hold
ReceiptDecisionUsesLockedFacts, ReceiptSingleLock, NoDeadlock, ReceiptCallsComplete
