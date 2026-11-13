# HerdrRestart

TLC reached 56 states and 112 steps. The chart draws 15 states and 13 arrows.
The chart does not draw 15 steps that stay in one chart state.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 13 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Old End confirmed before EOF capture
3. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Old End confirmed before EOF capture
4. Retire and settle only the old token: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Old End confirmed before EOF capture
5. Compare post-End EOF with O cursor: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
6. Persist reader offset and mode in the incarnation: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Offset durably recorded before provider launch
7. Launch once and observe own Startup or Resume: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Own startup metadata appended; seed not submitted
8. Commit new incarnation before create: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Own startup metadata appended; seed not submitted
9. Create reply is observed once: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Own startup metadata appended; seed not submitted
10. Crash after startup metadata and before seed: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Process crashed; volatile reader state lost
11. Evidence and binding admit the successor: Restart lifecycle B New Ready session has no input wake, Codex resume helper A Process crashed; volatile reader state lost
12. Recover the recorded incarnation and reader offset: Restart lifecycle B New Ready session has no input wake, Codex resume helper A Boot restored the original reader offset
13. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B New Ready session has no input wake, Codex resume helper A Visible preparation or binding failure; no tail
The run stops in "Restart lifecycle B New Ready session has no input wake, Codex resume helper A Visible preparation or binding failure; no tail" forever.

## AResumeAvailable breaks
TLC's counterexample is in tlc-AResumeAvailable.out. It has 13 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Old End confirmed before EOF capture
3. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Old End confirmed before EOF capture
4. Retire and settle only the old token: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Old End confirmed before EOF capture
5. Compare post-End EOF with O cursor: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
6. Persist reader offset and mode in the incarnation: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Offset durably recorded before provider launch
7. Launch once and observe own Startup or Resume: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Own startup metadata appended; seed not submitted
8. Commit new incarnation before create: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Own startup metadata appended; seed not submitted
9. Create reply is observed once: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Own startup metadata appended; seed not submitted
10. Crash after startup metadata and before seed: Restart lifecycle B New pane created; wait for evidence and binding, Codex resume helper A Process crashed; volatile reader state lost
11. Evidence and binding admit the successor: Restart lifecycle B New Ready session has no input wake, Codex resume helper A Process crashed; volatile reader state lost
12. Recover the recorded incarnation and reader offset: Restart lifecycle B New Ready session has no input wake, Codex resume helper A Boot restored the original reader offset
13. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B New Ready session has no input wake, Codex resume helper A Visible preparation or binding failure; no tail
The run stops in "Restart lifecycle B New Ready session has no input wake, Codex resume helper A Visible preparation or binding failure; no tail" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, AOrdinaryFresh, ADurableBeforeLaunch, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, ARequestResolves
