구판 코드 기준: `34822f3bf14c35907b440dbe8c81f62ad167e2ba`. Config `WriterParkedAliasFixed.cfg`. 수리 구현 SHA: `1e2763de57d7932e1ae2585b9951169232c730d8`, main `0a2996a8c537234c1cdc8da8c0ce67a06f0f600c`. 신판은 이 구현의 old-reference 동작을 모델링한다.

# WriterParkedAlias

TLC reached 64 states and 64 steps. The chart draws 15 states and 14 arrows.
The chart does not draw 36 steps that stay in one chart state.
The view hides 8 steps, so the chart does not draw them.

## Rules that hold
FirstReplyAtMostOnce, FirstReplyUsesFreshSource, HaltRepeatZero, NoHaltForConfirmedOld, ParkedBytesUnchanged, NarrowOldOnly, StoredNameNeverDuplicated, ParkedNewStillHalts, MissingFreshTargetHalts, NoDeadlock, FirstReplyExactlyOnce
