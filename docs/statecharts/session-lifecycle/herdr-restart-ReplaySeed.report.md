# HerdrRestart

TLC reached 63 states and 133 steps. The chart draws 14 states and 12 arrows.
The chart does not draw 37 steps that stay in one chart state.

## ADurableBeforeLaunch breaks
The shortest path has 4 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
3. Persist reader offset and mode in the incarnation: Restart lifecycle B Old session and original queued input, Codex resume helper A Offset durably recorded before provider launch
4. Launch once and observe own Startup or Resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Missing durable offset at before_seed

## SeedAtMostOnce breaks
The shortest path has 7 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Verified resume intent or ordinary fresh launch
3. Retire and settle only the old token: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Verified resume intent or ordinary fresh launch
4. Commit new incarnation before create: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Verified resume intent or ordinary fresh launch
5. Create reply is observed once: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Verified resume intent or ordinary fresh launch
6. Evidence and binding admit the successor: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Verified resume intent or ordinary fresh launch
7. Evidence and binding admit the successor: Restart lifecycle B Restart seed executed twice, Codex resume helper A Verified resume intent or ordinary fresh launch

## AResumeAvailable breaks
TLC's counterexample is in tlc-AResumeAvailable.out. It has 14 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Old End confirmed before EOF capture
3. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Old End confirmed before EOF capture
4. Retire and settle only the old token: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Old End confirmed before EOF capture
5. Compare post-End EOF with O cursor: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
6. Persist reader offset and mode in the incarnation: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Offset durably recorded before provider launch
7. Commit new incarnation before create: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Offset durably recorded before provider launch
8. Launch once and observe own Startup or Resume: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Missing durable offset at before_seed
9. Crash after startup metadata and before seed: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Missing durable offset at boot
10. Recover the recorded incarnation and reader offset: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Missing durable offset at failed
11. Create reply is observed once: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Missing durable offset at failed
12. Evidence and binding admit the successor: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Missing durable offset at failed
13. Evidence and binding admit the successor: Restart lifecycle B Restart seed executed twice, Codex resume helper A Missing durable offset at failed
14. Submit an original to the successor once: Restart lifecycle B Restart seed executed twice, Codex resume helper A Missing durable offset at failed
The run stops in "Restart lifecycle B Restart seed executed twice, Codex resume helper A Missing durable offset at failed" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, AOrdinaryFresh, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves, ARequestResolves
