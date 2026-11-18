# HerdrRestart

TLC reached 112 states and 240 steps. The chart draws 17 states and 15 arrows.
The chart does not draw 78 steps that stay in one chart state.

## NoOldPaneInput breaks
The shortest path has 2 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Verified resume intent or ordinary fresh launch
2. Stale input permission types into old pane: Restart lifecycle B Old pane received a restart input, Codex resume helper A Verified resume intent or ordinary fresh launch

## AOffsetPreserved breaks
The shortest path has 7 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
3. Persist reader offset and mode in the incarnation: Restart lifecycle B Old session and original queued input, Codex resume helper A Offset durably recorded before provider launch
4. Launch once and observe own Startup or Resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Own startup metadata appended; seed not submitted
5. Crash after startup metadata and before seed: Restart lifecycle B Old session and original queued input, Codex resume helper A Process crashed; volatile reader state lost
6. Recover the recorded incarnation and reader offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Boot restored the original reader offset
7. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Resumed reader replays old history

## ANoHistoricalReplay breaks
The shortest path has 7 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. Compare post-End EOF with O cursor: Restart lifecycle B Old session and original queued input, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
3. Persist reader offset and mode in the incarnation: Restart lifecycle B Old session and original queued input, Codex resume helper A Offset durably recorded before provider launch
4. Launch once and observe own Startup or Resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Own startup metadata appended; seed not submitted
5. Crash after startup metadata and before seed: Restart lifecycle B Old session and original queued input, Codex resume helper A Process crashed; volatile reader state lost
6. Recover the recorded incarnation and reader offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Boot restored the original reader offset
7. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Old session and original queued input, Codex resume helper A Resumed reader replays old history

## Rules that hold
InputCustody, InputAtMostOnce, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, AOrdinaryFresh, ADurableBeforeLaunch, ABootOffsetStable, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, RestartAvailable, AcceptedInputResolves, ARequestResolves, AResumeAvailable
