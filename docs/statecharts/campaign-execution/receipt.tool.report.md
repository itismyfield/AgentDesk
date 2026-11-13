# CampaignVerification

TLC reached 86 states and 264 steps. The chart draws 13 states and 29 arrows.
The chart does not draw 42 steps that stay in one chart state.
The view hides 86 steps, so the chart does not draw them.

## Rules that hold
ReceiptEffectAtMostOnce, ReceiptAtomicEffects, ReceiptRollbackHasNoEffect, ReceiptDecisionUsesLockedFacts, ReceiptSingleLock, NoDeadlock, ReceiptCallsComplete
