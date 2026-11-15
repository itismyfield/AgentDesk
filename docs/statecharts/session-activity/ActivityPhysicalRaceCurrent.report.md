Code SHA: 5c3ed37cddc80036fa8a274419b0219f49926704.

# SessionActivity

TLC reached 708 states, more than the chart budget of 500, so the chart draws only the bug paths: 3 states and 3 arrows.

## NoEscUnlessGenerationOrForegroundTool breaks
The shortest path has 3 steps:
0. Provider busy: unknown, watcher absent
1. check stop authority: Provider busy: unknown, watcher absent
2. provider finishes after check: Provider idle: unknown, watcher absent
3. send stop if authorized: Physical completion raced Escape

## Rules that hold
TypeOK, NoEscWithoutCurrentEvidence, NoIdleKillWhileBusy, NoLatePreReopensClosedTool, NoDeadlock
