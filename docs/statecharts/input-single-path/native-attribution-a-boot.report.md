# NativeInputAttribution

Code SHA: `255891f0` (A 계약 모델).
그림 wrapper의 100,000-state 재검사 한계는 아래에 보존했다. authoritative 검증은 repository checker의 `a1-tlc-full.txt` (20 cfg 전체, rc0)이며 wrapper의 미검사 규칙을 PASS로 세지 않는다.

TLC reached more states than the chart budget of 500, so the chart draws only the bug paths: 2 states and 1 arrows.

## NoBootTicketLoss breaks
The shortest path has 1 step:
0. boot 준비 전: 옛 row 보존
1. 준비·custody 없이 reaper: 준비 없이 row 삭제: ticket 복구 불가

## Rules not checked
CompletionExactlyOnce, NoForeignEnd, NoSilentLoss, NoForeignResponse, NoSuccessorRelease, NoSourceBorrow, FollowerKeepsParent, ReviewResultOnce, FailureKeepsRow, IdleEventuallyReleased, HeadEventuallyInjected, EveryInputEventuallySettled, BootEventuallyRestores
After the first break, each TLC run stops at 100,000 distinct states. A run reached that bound before it checked these rules in every state.
