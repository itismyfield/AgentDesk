# HerdrRestart

TLC reached 64 states and 136 steps. The chart draws 14 states and 12 arrows.
The chart does not draw 40 steps that stay in one chart state.

## ABootOffsetStable breaks
The shortest path has 6 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
3. Persist reader offset and mode in the incarnation: Restart lifecycle B Old session and original queued input, Codex resume helper A Offset durably recorded before provider launch
4. Launch once and observe own Startup or Resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Own startup metadata appended; seed not submitted
5. Crash after startup metadata and before seed: Restart lifecycle B Old session and original queued input, Codex resume helper A Process crashed; volatile reader state lost
6. Recover the recorded incarnation and reader offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Boot replaced the recorded offset

## NoBlindCreate breaks
The shortest path has 6 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Close only the captured pane: Restart lifecycle B Exact close confirmed; settle old inflight, Codex resume helper A Verified resume intent or ordinary fresh launch
3. Retire and settle only the old token: Restart lifecycle B Old incarnation retired; originals still held, Codex resume helper A Verified resume intent or ordinary fresh launch
4. Commit new incarnation before create: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Verified resume intent or ordinary fresh launch
5. Create reply is observed once: Restart lifecycle B Fresh incarnation Pending before create, Codex resume helper A Verified resume intent or ordinary fresh launch
6. Create reply is observed once: Restart lifecycle B Unknown create acknowledgement was replayed, Codex resume helper A Verified resume intent or ordinary fresh launch

## AOffsetPreserved breaks
The shortest path has 7 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
3. Persist reader offset and mode in the incarnation: Restart lifecycle B Old session and original queued input, Codex resume helper A Offset durably recorded before provider launch
4. Launch once and observe own Startup or Resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Own startup metadata appended; seed not submitted
5. Crash after startup metadata and before seed: Restart lifecycle B Old session and original queued input, Codex resume helper A Process crashed; volatile reader state lost
6. Recover the recorded incarnation and reader offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Boot replaced the recorded offset
7. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Recovered reader skips new prefix

## ANoNewPrefixSkip breaks
The shortest path has 7 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
3. Persist reader offset and mode in the incarnation: Restart lifecycle B Old session and original queued input, Codex resume helper A Offset durably recorded before provider launch
4. Launch once and observe own Startup or Resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Own startup metadata appended; seed not submitted
5. Crash after startup metadata and before seed: Restart lifecycle B Old session and original queued input, Codex resume helper A Process crashed; volatile reader state lost
6. Recover the recorded incarnation and reader offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Boot replaced the recorded offset
7. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Recovered reader skips new prefix

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, AOrdinaryFresh, ADurableBeforeLaunch, ANoHistoricalReplay, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves, ARequestResolves, AResumeAvailable
