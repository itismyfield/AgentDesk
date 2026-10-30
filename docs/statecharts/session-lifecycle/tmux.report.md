# TmuxSession

Model written against AgentDesk `origin/main` `e8d94425a04867279ced4ec967e9d93df0d7ec0a` (2026-10-30). Counterexamples were replayed against the code; see `../README.md` for dispositions.

TLC reached 400 states and 820 steps. The chart draws 13 states and 24 arrows.
The chart does not draw 395 steps that stay in one chart state.
The view hides 64 steps, so the chart does not draw them.

## RowOnlyWithTurn breaks
The shortest path has 7 steps:
0. Idle
1. a message arrives: Idle
2. a message starts a turn: Turn starting
3. the server crashes: Server down, turn row on disk
4. the server boots: Restarting, turn row not picked up
5. the recovery probe fails, so the row is left: Restarting, turn row not picked up
6. watcher restore has nothing to do: Restarting, turn row not picked up
7. intake opens: Open, a turn row names a missing session

## RowNamesSession breaks
The shortest path has 7 steps:
0. Idle
1. a message arrives: Idle
2. a message starts a turn: Turn starting
3. the server crashes: Server down, turn row on disk
4. the server boots: Restarting, turn row not picked up
5. the recovery probe fails, so the row is left: Restarting, turn row not picked up
6. watcher restore has nothing to do: Restarting, turn row not picked up
7. intake opens: Open, a turn row names a missing session

## NoDeadlock breaks
The shortest path has 8 steps:
0. Idle
1. a message arrives: Idle
2. a message starts a turn: Turn starting
3. the server crashes: Server down, turn row on disk
4. the server boots: Restarting, turn row not picked up
5. the recovery probe fails, so the row is left: Restarting, turn row not picked up
6. a message arrives: Restarting, turn row not picked up
7. watcher restore has nothing to do: Restarting, turn row not picked up
8. intake opens: Open, a turn row names a missing session

## Rules that hold
PromptOnlyToLiveSession
