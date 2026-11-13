# HerdrRestart

TLC reached 80 states and 184 steps. The chart draws 17 states and 16 arrows.
The chart does not draw 42 steps that stay in one chart state.

## InputAtMostOnce breaks
The shortest path has 9 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Verified resume intent or ordinary fresh launch
3. Retire and settle only the old token: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Verified resume intent or ordinary fresh launch
4. Commit new incarnation before create: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Verified resume intent or ordinary fresh launch
5. Create reply is observed once: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Verified resume intent or ordinary fresh launch
6. Evidence and binding admit the successor: Restart lifecycle B One new Ready session; originals await the owner, Codex resume helper A Verified resume intent or ordinary fresh launch
7. Submit an original to the successor once: Restart lifecycle B One new Ready session; originals settled once, Codex resume helper A Verified resume intent or ordinary fresh launch
8. Forget an original receipt: Restart lifecycle B One new Ready session; originals await the owner, Codex resume helper A Verified resume intent or ordinary fresh launch
9. Submit an original to the successor once: Restart lifecycle B Original input executed twice, Codex resume helper A Verified resume intent or ordinary fresh launch

## Rules that hold
InputCustody, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, AOrdinaryFresh, ADurableBeforeLaunch, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves, ARequestResolves, AResumeAvailable
