Code SHA: 5c3ed37cddc80036fa8a274419b0219f49926704.

# SessionActivity

TLC reached 108 states and 378 steps. The chart draws 3 states and 5 arrows.
The chart does not draw 301 steps that stay in one chart state.

## EventuallyConverges breaks
TLC's counterexample is in tlc-EventuallyConverges.out. It has 7 steps:
0. Provider busy: unknown, watcher absent
1. ignore old incarnation hook: Provider busy: unknown, watcher absent
2. check stop authority: Provider busy: unknown, watcher absent
3. submission hook arrives: Provider busy: unknown, watcher absent
4. expire hints and recheck: Provider busy: unknown after lease expiry, watcher absent
5. send stop if authorized: Provider busy: unknown after lease expiry, watcher absent
6. new source progress renews lease: Provider busy: unknown after one recovery cycle, watcher absent
7. pane busy signal unavailable: Provider busy: unknown after one recovery cycle, watcher absent
The run stops in "Provider busy: unknown after one recovery cycle, watcher absent" forever.

## EventuallyReacquires breaks
TLC's counterexample is in tlc-EventuallyReacquires.out. It has 7 steps:
0. Provider busy: unknown, watcher absent
1. ignore old incarnation hook: Provider busy: unknown, watcher absent
2. check stop authority: Provider busy: unknown, watcher absent
3. submission hook arrives: Provider busy: unknown, watcher absent
4. expire hints and recheck: Provider busy: unknown after lease expiry, watcher absent
5. send stop if authorized: Provider busy: unknown after lease expiry, watcher absent
6. new source progress renews lease: Provider busy: unknown after one recovery cycle, watcher absent
7. pane busy signal unavailable: Provider busy: unknown after one recovery cycle, watcher absent
The run stops in "Provider busy: unknown after one recovery cycle, watcher absent" forever.

## Rules that hold
TypeOK, NoEscWithoutCurrentEvidence, NoEscUnlessGenerationOrForegroundTool, NoIdleKillWhileBusy, NoLatePreReopensClosedTool, NoDeadlock
