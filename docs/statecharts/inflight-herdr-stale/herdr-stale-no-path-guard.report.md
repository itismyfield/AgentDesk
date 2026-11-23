Code SHA: bdf9ad22279dee9812e82255a559b4edff38da06 (base) with the i8343h stale-check change for the after configs.

# InflightHerdrStale

TLC reached 252 states and 802 steps. The chart draws 210 states and 676 arrows.
The chart does not draw 42 steps that stay in one chart state.

## NoPathlessHidden breaks
The shortest path has 4 steps:
0. this process path-less row: turn running, row kept, fresh
1. row passes stale age: this process path-less row: turn running, row kept
2. pane closed or retired: this process path-less row: pane closed, row kept
3. dcserver restarts (boot reaper): prior process path-less row: pane closed, row kept
4. stale check reads pane gone: prior process path-less row: pane closed, row hidden from scans

## Rules that hold
TypeOK, NoLiveTurnRowHidden, ClosedPaneRowRetires
