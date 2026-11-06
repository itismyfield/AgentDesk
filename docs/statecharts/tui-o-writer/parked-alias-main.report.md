구판 코드 기준: `34822f3bf14c35907b440dbe8c81f62ad167e2ba`. Config `WriterParkedAliasMain.cfg`. 수리 구현 SHA: `1e2763de57d7932e1ae2585b9951169232c730d8`, main `0a2996a8c537234c1cdc8da8c0ce67a06f0f600c`. 신판은 이 구현의 old-reference 동작을 모델링한다.

# WriterParkedAlias

TLC reached 66 states and 69 steps. The chart draws 18 states and 18 arrows.
The chart does not draw 36 steps that stay in one chart state.
The view hides 10 steps, so the chart does not draw them.

## NoHaltForConfirmedOld breaks
The shortest path has 6 steps:
0. 저장 cursor A0 · session 빈 값
1. Bound(A0) 적용: A0 bind 적용 · cp1
2. 존재하는 파일에서 이름 확정·prefix 적용: 파일 있을 때 A0→A1 적용 · cp2 · 저장 A0 유지
3. 원본 소실: A pathname 소실 · 세션 없음
4. 부재 확인 후 named A0 park: 저장 A0 park · 입력 승인 가능
5. 입력 승인·bind 기록 [정상 경로 old=A1,new=B,Startup]: raw old A1→fresh B · Startup · cp2
6. old A1의 저장 A0 재open 요구 · Missing 실패: raw old A1이 Missing A0 재open 요구 · halt 1회

## HaltRepeatZero breaks
The shortest path has 7 steps:
0. 저장 cursor A0 · session 빈 값
1. Bound(A0) 적용: A0 bind 적용 · cp1
2. 존재하는 파일에서 이름 확정·prefix 적용: 파일 있을 때 A0→A1 적용 · cp2 · 저장 A0 유지
3. 원본 소실: A pathname 소실 · 세션 없음
4. 부재 확인 후 named A0 park: 저장 A0 park · 입력 승인 가능
5. 입력 승인·bind 기록 [정상 경로 old=A1,new=B,Startup]: raw old A1→fresh B · Startup · cp2
6. old A1의 저장 A0 재open 요구 · Missing 실패: raw old A1이 Missing A0 재open 요구 · halt 1회
7. owner cleanup 재호스트: 이전 원본 reopen 실패 뒤 재호스트 1 · halt 1

## FirstReplyExactlyOnce breaks
TLC's counterexample is in tlc-FirstReplyExactlyOnce.out. It has 15 steps:
0. 저장 cursor A0 · session 빈 값
1. Bound(A0) 적용: A0 bind 적용 · cp1
2. 존재하는 파일에서 이름 확정·prefix 적용: 파일 있을 때 A0→A1 적용 · cp2 · 저장 A0 유지
3. 원본 소실: A pathname 소실 · 세션 없음
4. 부재 확인 후 named A0 park: 저장 A0 park · 입력 승인 가능
5. 입력 승인·bind 기록 [정상 경로 old=A1,new=B,Startup]: raw old A1→fresh B · Startup · cp2
6. old A1의 저장 A0 재open 요구 · Missing 실패: raw old A1이 Missing A0 재open 요구 · halt 1회
7. owner cleanup 재호스트: 이전 원본 reopen 실패 뒤 재호스트 1 · halt 1
8. A0 다시 park 후 같은 seq3 재시도: 같은 old A1→B 재시도 · cp2 · 이전 halt 1 · 재호스트 1
9. old A1의 저장 A0 재open 요구 · Missing 실패: raw old A1이 Missing A0 재open 요구 · halt 2회
10. owner cleanup 재호스트: 이전 원본 reopen 실패 뒤 재호스트 2 · halt 2
11. A0 다시 park 후 같은 seq3 재시도: 같은 old A1→B 재시도 · cp2 · 이전 halt 2 · 재호스트 2
12. old A1의 저장 A0 재open 요구 · Missing 실패: raw old A1이 Missing A0 재open 요구 · halt 3회
13. owner cleanup 재호스트: 이전 원본 reopen 실패 뒤 재호스트 2 · halt 3
14. A0 다시 park 후 같은 seq3 재시도: 같은 old A1→B 재시도 · cp2 · 이전 halt 3 · 재호스트 2
15. old A1의 저장 A0 재open 요구 · Missing 실패: raw old A1이 Missing A0 재open 요구 · halt 3회
The run loops through "raw old A1이 Missing A0 재open 요구 · halt 3회", "이전 원본 reopen 실패 뒤 재호스트 2 · halt 3" and "같은 old A1→B 재시도 · cp2 · 이전 halt 3 · 재호스트 2" forever.

## Rules that hold
FirstReplyAtMostOnce, FirstReplyUsesFreshSource, ParkedBytesUnchanged, NarrowOldOnly, StoredNameNeverDuplicated, ParkedNewStillHalts, MissingFreshTargetHalts, NoDeadlock
