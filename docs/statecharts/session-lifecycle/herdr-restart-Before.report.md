# HerdrRestart

TLC reached 6 states and 14 steps. The chart draws 5 states and 3 arrows.
The chart does not draw 7 steps that stay in one chart state.

## RestartAvailable breaks
TLC's counterexample is in tlc-RestartAvailable.out. It has 3 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Current fresh-only entry refuses explicit resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Visible preparation or binding failure; no tail
2. User /restart is refused before cancellation: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Visible preparation or binding failure; no tail
3. Show the failure and retain input custody: Restart lifecycle B Visible failure; originals remain owned, Codex resume helper A Visible preparation or binding failure; no tail
The run stops in "Restart lifecycle B Visible failure; originals remain owned, Codex resume helper A Visible preparation or binding failure; no tail" forever.

## AResumeAvailable breaks
TLC's counterexample is in tlc-AResumeAvailable.out. It has 3 steps:
0. Restart lifecycle B Old session and original queued input, Codex resume helper A Verified resume intent or ordinary fresh launch
1. Current fresh-only entry refuses explicit resume: Restart lifecycle B Old session and original queued input, Codex resume helper A Visible preparation or binding failure; no tail
2. User /restart is refused before cancellation: Restart lifecycle B Failure awaiting a visible result, Codex resume helper A Visible preparation or binding failure; no tail
3. Show the failure and retain input custody: Restart lifecycle B Visible failure; originals remain owned, Codex resume helper A Visible preparation or binding failure; no tail
The run stops in "Restart lifecycle B Visible failure; originals remain owned, Codex resume helper A Visible preparation or binding failure; no tail" forever.

## Rules that hold
InputCustody, InputAtMostOnce, NoOldPaneInput, NoForeignClose, NoBlindCreate, SeedAtMostOnce, NoSupersededReady, FreshIncarnation, SingleReadySession, InflightSettledBeforeReady, AAfterConfirmedEnd, ACursorAgreement, AOrdinaryFresh, ADurableBeforeLaunch, ABootOffsetStable, AOffsetPreserved, ANoHistoricalReplay, ANoNewPrefixSkip, AScopedCause, NoDeadlock, RequestResolves, AcceptedInputResolves, ARequestResolves
