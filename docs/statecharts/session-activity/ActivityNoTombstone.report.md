Code SHA: 5c3ed37cddc80036fa8a274419b0219f49926704.

# SessionActivity

TLC reached 17,502 states, more than the chart budget of 500, so the chart draws only the bug paths: 3 states and 3 arrows.

## NoLatePreReopensClosedTool breaks
The shortest path has 3 steps:
0. Provider busy: unknown, watcher absent
1. foreground tool finishes: Provider idle: unknown, watcher absent
2. tool completion hook arrives: Provider idle: unknown, watcher absent
3. tool start hook arrives: Closed tool reopened by late start

## Rules that hold
TypeOK, NoEscWithoutCurrentEvidence, NoIdleKillWhileBusy, NoDeadlock
