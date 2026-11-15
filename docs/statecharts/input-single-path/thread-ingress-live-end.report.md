# Current-process Herdr execution-gone — T1 P2-1

기준 main `d40c06eb47`, 설계 base `ca904f560d`. 기존 생산 코드를 바꾸지 않고 [설계 §2 N15/M07 및 §4·§5](../../api-thread-ingress.design.md)에 빠졌던 소비자를 추가했다. `row_end_pass:120-136`은 prior-process pathless row만이 아니라 current-process live row도 선택한다.

`herdr_row_end_live.rs:10-25,30-88,100-147`의 실제 전수 조건은 다음과 같다. current-generation, terminal kind/stop receipt 없음, 동일 nonce actor를 선택한다. transition을 잡고 fresh row/save generation, captured actor, pinned execution nonce를 다시 확인한다. execution E 확인 **뒤** 자기 source를 다시 읽는다. 자기 terminal이 있으면 reader의 typed admission에 양보하며, unread/unknown 또는 다른 actor/execution이면 hold한다. terminal 없이 E가 확정되면 기존 retire→body-free notice Delivered→captured settlement→completion cleanup/reader wake 순서로 끝낸다. notice가 거절되면 row/token을 보존한다. 이 경로는 이미 main에 있으므로 새 kill·clear·settlement 권한을 추가하지 않는다.

`ThreadIngressLiveEnd`는 위 포트의 작은 유한 모델이다. reader 경로의 실제 direct/exact terminal 수락·O 게시·receipt/clear는 기존 `ThreadIngressTerminal`의 11cfg가 검사한다. live-end leaf에서 Reader는 그 건강한 포트를 한 단계로 투영한다. Notice는 실제 Delivered 성공 포트이며 무한 전송 장애가 자동 성공한다는 보장은 없다. UnknownHeld는 source/actor/execution 미확정 시 mutation0을 검사한다. EligibleEnds/NextCanProgress는 알려진 끝이면 done·wake, 미확정이면 custody를 보존한 held에 도달함을 각각 실제 temporal 검사한다. 상수 false로 제거되는 vacuous PROPERTY를 사용하지 않는다.

## 수정 전/후 전수표

아래 공통 prefix는 ThreadIngressLiveEnd, suffix는 .cfg.example이다. L은 EligibleEnds와 NextCanProgress 두 PROPERTY다. before는 **이전 모델에 빠졌던 live-end 포트**를 omitLiveEnd로 닫은 대조군이다. main Rust에 이 포트가 없었다는 뜻이나 생산 수정 전 RED가 아니다. source=absent/Mutation=none일 때만 before cfg를 omitLiveEnd로 바꾸고, 모든 다른 source/mutant/actor/ended 값은 유지한다.

| cfg suffix | omitted-port before actual broken | current-main port after actual broken |
|---|---|---|
| Alive | - | - |
| Before | L | L |
| Gone | L | - |
| IgnoreUnknown | UnknownHeld + L | UnknownHeld + L |
| NoSettlement | L | L |
| NoWake | L | L |
| StealTerminal | TerminalReaderOnly | TerminalReaderOnly |
| Successor | - | - |
| Terminal | - | - |
| Unread | - | - |

before 4 GREEN/6 expected RED、after 5 GREEN/5 expected RED. terminal 양보 제거, settlement 제거, reader wake 제거, unread 무시의 원인 mutant4/4 RED다. source terminal은 reader에 남기며 별도 notice가 가로챌 수 없다. held는 token/inflight1, posted/finished0을 유지한다. done은 notice 또는 reader 게시1, finished1, token/inflight0, readerWoken이다.

main 병합 확인(`git merge d40c06eb47`: Already up to date) 뒤 기존 D3 11cfg와 기존 ingress/P2 39cfg도 공식 harness로 직접 실행했다. 전체60cfg=30 GREEN+30 expected RED, 기대값 불일치·tool error·skip·미선택0. source/manifest 바이트 대조를 남긴다. cfg.example은 여전히 CI 자동 등록이 아니다.

로그는 `/home/adk/adk/lane-runs/i8125/logs/tlc-t1-r1-verified.log`, before는 `tlc-t1-r1-live-before-verified.log`다. 각 scratch의 expected.tsv와 check-t1-r1-models.py/check-live-end-r1.py가 재현 명령을 남긴다. 초기 진단의 Init 미할당/상수 temporal tautology 오류 로그는 최종 PASS 증거에서 제외한다. 모델링 시간은 최초 census부터 최종 실행까지 30분 이내이며 lane 보고서에 시각을 남긴다.

## 구현 시험 인계

기존 `provider_dispatch_live_end_tests.rs:467,540,579,643`은 O 비활성/일반 DiscordGateway(None) 조합이다. 이 존재만으로 G external+O 조합을 PASS로 세지 않는다. 실제 HTTP 외부 own-turn의 execution-gone 시험은 notice 거절 시 custody, notice Delivered 뒤 row/token0·reader wake·next input1을 확인한다. 자기 terminal이 E 뒤 flush되거나 마지막 newline 없이 남으면 별도 실패 안내 없이 reader admission·O 본문1·cleanup1이어야 한다. successor/unread 보존과 terminal 발견 시 양보 제거 mutant도 포함한다.
