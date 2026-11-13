# HerdrRestart

TLC reached 24 states and 48 steps. The chart draws 11 states and 9 arrows.
The chart does not draw 11 steps that stay in one chart state.

## RequestResolves breaks
TLC's counterexample is in tlc-RequestResolves.out. It has 9 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Old End confirmed before EOF capture
3. Compare post-End EOF with O cursor: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
4. Exact close is unconfirmed: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
5. Persist reader offset and mode in the incarnation: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Offset durably recorded before provider launch
6. Launch once and observe own Startup or Resume: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Own startup metadata appended; seed not submitted
7. Crash after startup metadata and before seed: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Process crashed; volatile reader state lost
8. Recover the recorded incarnation and reader offset: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Boot restored the original reader offset
9. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Resume cause accepted; old history excluded
The run stops in "Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Resume cause accepted; old history excluded" forever.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 9 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Old End confirmed before EOF capture
3. Compare post-End EOF with O cursor: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
4. Exact close is unconfirmed: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
5. Persist reader offset and mode in the incarnation: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Offset durably recorded before provider launch
6. Launch once and observe own Startup or Resume: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Own startup metadata appended; seed not submitted
7. Crash after startup metadata and before seed: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Process crashed; volatile reader state lost
8. Recover the recorded incarnation and reader offset: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Boot restored the original reader offset
9. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Resume cause accepted; old history excluded
The run stops in "Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Resume cause accepted; old history excluded" forever.

## AcceptedInputResolves breaks
TLC's counterexample is in tlc-AcceptedInputResolves.out. It has 9 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Confirm old provider End before EOF capture: Restart lifecycle B Old session and original queued input, Codex resume helper A Old End confirmed before EOF capture
2. User /restart is accepted under transition and fence: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A Old End confirmed before EOF capture
3. Compare post-End EOF with O cursor: Restart lifecycle B Restart excludes old input and captures exact pane, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
4. Exact close is unconfirmed: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A EOF and O cursor agree; prepare immutable incarnation
5. Persist reader offset and mode in the incarnation: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Offset durably recorded before provider launch
6. Launch once and observe own Startup or Resume: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Own startup metadata appended; seed not submitted
7. Crash after startup metadata and before seed: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Process crashed; volatile reader state lost
8. Recover the recorded incarnation and reader offset: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Boot restored the original reader offset
9. Bind the scoped cause and tail from the recorded offset: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Resume cause accepted; old history excluded
The run stops in "Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Resume cause accepted; old history excluded" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, AOrdinaryFresh, ADurableBeforeLaunch, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, ARequestResolves, AResumeAvailable
