Code SHA: bdf9ad22279dee9812e82255a559b4edff38da06 (base) with the i8343h stale-check change for the after configs.

# InflightHerdrStale

TLC reached 240 states and 784 steps. The chart draws 200 states and 660 arrows.
The chart does not draw 40 steps that stay in one chart state.

## ClosedPaneRowRetires breaks
TLC's counterexample is in tlc-ClosedPaneRowRetires.out. It has 5 steps:
0. this process row: turn running, row kept, fresh
1. pane closed or retired: this process row: pane closed, row kept, fresh
2. dcserver restarts (boot reaper): prior process row: pane closed, row kept, fresh
3. row passes stale age: prior process row: pane closed, row kept
4. Herdr stops answering: prior process row: pane closed, Herdr silent, row kept, after an outage
5. Herdr answers again: prior process row: pane closed, row kept, after an outage
The run stops in "prior process row: pane closed, row kept, after an outage" forever.

## Rules that hold
TypeOK, NoLiveTurnRowHidden, NoPathlessHidden
