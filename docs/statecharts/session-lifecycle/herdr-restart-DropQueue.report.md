# HerdrRestart

TLC reached 56 states and 112 steps. The chart draws 7 states and 5 arrows.
The chart does not draw 74 steps that stay in one chart state.

## ACursorAgreement breaks
The shortest path has 2 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A Resume accepted a mismatched O cursor

## InputCustody breaks
The shortest path has 3 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Verified resume intent or ordinary fresh launch
3. Retire and settle only the old token: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Verified resume intent or ordinary fresh launch

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 13 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Confirm old provider End before EOF capture: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Old End confirmed before EOF capture
3. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Old End confirmed before EOF capture
4. Compare post-End EOF with O cursor: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Resume accepted a mismatched O cursor
5. Retire and settle only the old token: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
6. Commit new incarnation before create: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
7. Create reply is observed once: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
8. Persist reader offset and mode in the incarnation: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
9. Evidence and binding admit the successor: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
10. Launch once and observe own Startup or Resume: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
11. Crash after startup metadata and before seed: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
12. Recover the recorded incarnation and reader offset: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
13. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor
The run stops in "Restart lifecycle B Accepted input lost its owner, Codex resume helper A Resume accepted a mismatched O cursor" forever.

## Rules that hold
InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, AOrdinaryFresh, ADurableBeforeLaunch, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, ARequestResolves, AResumeAvailable
