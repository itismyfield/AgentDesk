Code SHA: bdf9ad22279dee9812e82255a559b4edff38da06 (base) with the i8343h stale-check change for the after configs.

# InflightHerdrStale

TLC reached 246 states and 793 steps. The chart draws 205 states and 669 arrows.
The chart does not draw 40 steps that stay in one chart state.

## IdlePaneRowRetires breaks
TLC's counterexample is in tlc-IdlePaneRowRetires.out. It has 7 steps:
0. this process row: turn running, row kept, fresh
1. another server answers with no panes: this process row: turn running, replacing server lists none, row kept, fresh
2. dcserver restarts (boot reaper): prior process row: turn running, replacing server lists none, row kept, fresh
3. Herdr stops answering: prior process row: turn running, replacing server lists none, row kept, fresh, after an outage
4. Herdr answers again: prior process row: turn running, replacing server lists none, row kept, fresh, after an outage
5. turn finishes: prior process row: turn ended, pane idle, replacing server lists none, row kept, fresh, after an outage
6. pane closed or retired: prior process row: pane closed, replacing server lists none, row kept, fresh, after an outage
7. row passes stale age: prior process row: pane closed, replacing server lists none, row kept, after an outage
The run stops in "prior process row: pane closed, replacing server lists none, row kept, after an outage" forever.

## Rules that hold
TypeOK, NoLiveTurnRowHidden, NoPathlessHidden
