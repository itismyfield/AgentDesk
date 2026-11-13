Code SHA: 5c3ed37cddc80036fa8a274419b0219f49926704.

# SessionActivity

TLC reached 342 states and 1137 steps. The chart draws 8 states and 15 arrows.
The chart does not draw 818 steps that stay in one chart state.

## NeverEscape breaks
The shortest path has 2 steps:
0. Provider busy: unknown, watcher absent
1. check stop authority: Provider busy: unknown, watcher absent
2. send stop if authorized: Escape delivered from activity evidence

## Rules that hold
TypeOK, NoEscWithoutCurrentEvidence, NoEscUnlessGenerationOrForegroundTool, NoIdleKillWhileBusy, NoDeadlock
