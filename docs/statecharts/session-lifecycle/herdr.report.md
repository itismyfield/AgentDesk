# HerdrSession

Model written against AgentDesk `origin/main` `e8d94425a04867279ced4ec967e9d93df0d7ec0a` (2026-10-30). Counterexamples were replayed against the code; see `../README.md` for dispositions.

TLC reached 97 states and 161 steps. The chart draws 12 states and 23 arrows.
The chart does not draw 54 steps that stay in one chart state.

## RowMatchesPane breaks
The shortest path has 2 steps:
0. No execution running
1. a turn starts a launch: Launch: create request sent
2. the create reply is lost: Pending row with no pane

## NoOrphanPane breaks
The shortest path has 2 steps:
0. No execution running
1. a turn starts a launch: Launch: create request sent
2. the create reply is lost: Pending row with no pane, an unrecorded pane runs Claude

## NoDeadlock breaks
The shortest path has 3 steps:
0. No execution running
1. a turn starts a launch: Launch: create request sent
2. the server crashes and restarts: Pending row with no pane
3. the server boots, reconcile only reads: Pending row with no pane

## PromptOnlyToAgent breaks
The shortest path has 8 steps:
0. No execution running
1. a turn starts a launch: Launch: create request sent
2. Herdr answers with the new pane: Launch: pane made, not recorded yet
3. the pane is recorded: Launch: pane recorded, Claude being checked
4. Claude is confirmed in the pane: Launched, not bound yet
5. the first prompt binds the execution: Bound, Claude running
6. the input gate checks Claude in the pane: Prompt checked, about to be sent
7. Claude exits: Prompt checked, about to be sent
8. the prompt is sent: Bound, Claude exited, shell left in the pane

## Rules that hold
NoDuplicatePane
