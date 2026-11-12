Code SHA: 5c3ed37cddc80036fa8a274419b0219f49926704.

# SessionActivity

TLC reached 162 states and 513 steps. The chart draws 6 states and 10 arrows.
The chart does not draw 431 steps that stay in one chart state.

## EventuallyConverges breaks
TLC's counterexample is in tlc-EventuallyConverges.out. It has 6 steps:
0. Provider busy: unknown, watcher absent
1. submission hook arrives: Provider busy: unknown, watcher absent
2. read current source activity: Provider busy: busy, watcher absent
3. expire hints and recheck: Provider busy: unknown after lease expiry, watcher absent
4. ignore old incarnation hook: Provider busy: unknown after lease expiry, watcher absent
5. check stop authority: Provider busy: unknown after lease expiry, watcher absent
6. send stop if authorized: Provider busy: unknown after lease expiry, watcher absent
The run stops in "Provider busy: unknown after lease expiry, watcher absent" forever.

## EventuallyReacquires breaks
TLC's counterexample is in tlc-EventuallyReacquires.out. It has 6 steps:
0. Provider busy: unknown, watcher absent
1. submission hook arrives: Provider busy: unknown, watcher absent
2. read current source activity: Provider busy: busy, watcher absent
3. expire hints and recheck: Provider busy: unknown after lease expiry, watcher absent
4. ignore old incarnation hook: Provider busy: unknown after lease expiry, watcher absent
5. check stop authority: Provider busy: unknown after lease expiry, watcher absent
6. send stop if authorized: Provider busy: unknown after lease expiry, watcher absent
The run stops in "Provider busy: unknown after lease expiry, watcher absent" forever.

## Rules that hold
TypeOK, NoEscWithoutCurrentEvidence, NoIdleKillWhileBusy, NoLatePreReopensClosedTool, NoDeadlock
