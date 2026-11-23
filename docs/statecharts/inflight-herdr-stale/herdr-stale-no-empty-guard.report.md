Code SHA: bdf9ad22279dee9812e82255a559b4edff38da06 (base) with the i8343h stale-check change for the after configs.

# InflightHerdrStale

TLC reached 252 states and 812 steps. The chart draws 208 states and 681 arrows.
The chart does not draw 44 steps that stay in one chart state.

## NoLiveTurnRowHidden breaks
The shortest path has 4 steps:
0. this process row: turn running, row kept, fresh
1. row passes stale age: this process row: turn running, row kept
2. dcserver restarts (boot reaper): prior process row: turn running, row kept
3. another server answers with no panes: prior process row: turn running, replacing server lists none, row kept
4. stale check reads pane gone: Running turn's row hidden (answer lost)

## Rules that hold
TypeOK, NoPathlessHidden, ClosedPaneRowRetires
