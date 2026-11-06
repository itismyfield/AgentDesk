Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c` + park working tree; reviewed delta config `WriterReboundCurrent.cfg`. `LogCurrent=TRUE`.

구현 소스 참조: `d563afb856870980a766a77003b55a84ea60ff62` (main `4a13d1d32e0f00d4ceaff475b95c033ba7967a8b` 위 구현). 최종 검증 결과는 구현 레인 보고서를 따른다.


# WriterRebound

TLC reached 239 states and 254 steps. The chart draws 17 states and 22 arrows.
The view hides 54 steps, so the chart does not draw them.

## Rules that hold
ActiveLossVisible, CurrentNeverHistorical, UnknownNeedsAbsence, ReadyAfterFollow, ParkedReboundHalts, ParkPreservesEvidence, NoDeadlock
