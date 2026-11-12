Code SHA: 5c3ed37cddc80036fa8a274419b0219f49926704.

# SessionActivity

TLC reached 3,576 states, more than the chart budget of 500, so the chart draws only the bug paths: 2 states and 1 arrows.

## NoIdleKillWhileBusy breaks
The shortest path has 1 step:
0. Provider idle: unknown, watcher absent
1. attempt idle cleanup: Cleanup killed active work

## Rules that hold
TypeOK, NoEscWithoutCurrentEvidence, NoLatePreReopensClosedTool, NoDeadlock
