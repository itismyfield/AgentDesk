# CampaignPipeline

TLC reached 118 states and 288 steps. The chart draws 12 states and 14 arrows.
The chart does not draw 84 steps that stay in one chart state.
The view hides 118 steps, so the chart does not draw them.

## Rules that hold
OneCommittedExitPerVisit, NoOrphanState, NoReplayAfterRestart, NoNewIntentAfterStop, NoLaunchAfterFence, FinishStageKeepsGrant, PriorityRespected, ApprovalNamesExactVisit, ConditionFromCurrentVisit, RetryWithinBudget, AdoptionDoesNotReenter, TerminalNeedsEvidence, NoDeadlock, InstanceTerminates
