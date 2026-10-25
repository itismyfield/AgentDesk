콘텐츠로 바로 가기
홈
스페이스
예약
플러그인
탐색
사이트
ChatGPT
새 채팅
점박이
1
읽지 않은 메시지 1개
고정된 스레드
1clik: Character sheet Generator
Search Mode +
AgentDesk
프로젝트
프로젝트 없음
최근
읽지 않음
설계 개정 검토
원인 분석 수리 설계
원인 분석 설계
읽지 않음
Check restart and disk usage
카프리 뜻 설명
t3code와 AgentDesk 비교
새 채팅
아이폰 초기화 방법
사진 보정
코드 리뷰 계획
코드 리뷰 계획
주행 중 유튜브 재생
2C 구현 사양 설계
론셰프 장르 분석
오부장 정보 설명
공유
오늘 오후 7:39
내가 한 말:

[사유] 설계 개정은 WebGPT pro로 한다(사용자 지시 10-23 19:4x). 입력 전달 단일 경로의 차단·교착 계약을 다시 세우는 고위험 설계다.




# AgentDesk #7272 입력 단일 경로 설계 r2




## 소스 읽기
- GitHub 저장소 `itismyfield/AgentDesk`의 고정 SHA `794e7123e1749db0dea10b638a2af84d7cc8c191`(현재 main)에서 읽는다. 예: 
https://github.com/itismyfield/AgentDesk/blob/794e7123e1749db0dea10b638a2af84d7cc8c191/src/services/turn_orchestrator/queued_head.rs
- 아래 r1 설계 좌표는 `16e52b516` 기준이므로, 인용할 때는 위 SHA에서 다시 확인한다.




## 배경
- 이슈: 
https://github.com/itismyfield/AgentDesk/issues/7272 (사용자 요청)
- 사용자 결정(10-23)
  - 6조각으로 진행한다.
  - 즉시 주입 입력도 기존 durable 기록을 남긴다(무디스크 fastpath 없음).
  - 목표는 '항상 주입 + 필요할 때만 보관'이다.
- 조각 1은 다른 세션에서 진행 중이다. 운영 Codex queued gate는 닫은 채 adapter 분리만 한다(리뷰 P2-3 권고 채택).




## 요구
1. **P1-1 해소 계약**(Unknown 하나가 채널 입력·drain·turn claim을 무기한 막음). 아래 셋을 운영 결과, 사용자에게 보이는 것, 크기로 비교하고 하나를 권고한다. 사용자 기준은 '입력이 멈추지 않는다'이다.
   - (a) 현행 live처럼 `Unconfirmed`를 전달됨으로 정산
   - (b) 상한 시간 뒤 사용자 알림과 함께 '보관 원문 유지 + 다음 입력 진행'
   - (c) 항목별 상태
2. **P1-2 해소 계약**(본문 없는 세션 부팅에서 cold 첫 입력이 교착함): provider별 첫 입력 전달 방법(Claude, Codex argv, herdr)과 그 receipt·중복 방지를 정한다.
3. P2-1~P2-8 각각의 반영 또는 기각 근거
4. 갱신된 조각 계획: 조각별 prod 파일·순증 추정, 순서, 되돌리기
   - 리뷰 단위 상한 prod 30파일 / 순증 +1500, 정지선 +1200
   - 1000줄 넘는 prod 파일 raw 증가 0
5. 진입점 전수표(입력 상태를 만들거나 소비하는 곳, file:line @ `794e7123e`), 불변식, 수용 시험, 필수 뮤턴트
6. 사용자 결정이 필요한 선택이 있으면 `DECISION_NEEDED` 절에 쉬운 말 한 문단으로 적는다. 선택지마다 사용자에게 보이는 차이를 쓴다.




## 출력
- 본문 첫 줄은 `DESIGN_READY` 또는 `DESIGN_DECISION_NEEDED`다.
- **마지막 줄은 반드시 `VERDICT: DESIGN_READY` 또는 `VERDICT: DECISION_NEEDED`다.**




---
## 첨부 1: 설계 r1 (Sol) 전문




현재 파일시스템이 읽기 전용이어서 지정한 `design-r1-sol.out.md`를 저장하지 못했습니다. 아래는 완성된 보고서입니다. 소스 수정·운영 접근·실행 재현은 하지 않았습니다.




조사 기준은 [#7272]\([https://github.com/itismyfield/AgentDesk/issues/7272)와](https://github.com/itismyfield/AgentDesk/issues/7272\)와) GitHub 고정 SHA [\`16e52b5165f63cdccfaa2636a83ce8f8672ddc56\`]\([https://github.com/itismyfield/AgentDesk/tree/16e52b5165f63cdccfaa2636a83ce8f8672ddc56)다](https://github.com/itismyfield/AgentDesk/tree/16e52b5165f63cdccfaa2636a83ce8f8672ddc56\)다). 관련 소스는 GitHub에서 읽었다. 운영 설정값은 확인하지 않았다.




## 1. 거절·보류 사유 전수표




“기다려야 함”은 **입력 대상·payload·composer가 준비되지 않았다는 뜻**이다. AI 응답 완료를 기다려야 한다는 뜻은 아니다. 명령 처리·중복 소비·권한 거절은 보관 대기와 구분한다.




**입력 판별과 전처리**




| file:line | 사유 | 기다려야 함/없앨 수 있음 | 근거 |
|---|---|---|---|
| `src/services/discord/router/intake_gate.rs:433`, `:476`, `:485`, `:647`, `:1034` | 동일 message ID 재도착, parent/thread 중복, 이미 주입 소유·종료된 입력 | 중복 소비는 유지. busy 전용 중복 분기는 공통 소유권으로 통합 가능 | 같은 Discord 입력의 중복 실행을 막는 계약이다. |
| `src/services/discord/router/intake_gate.rs:496`, `:508`, `:545`, `:726`, `:932` | 시스템 메시지, 자기 봇, 허용되지 않은 봇, monitor provenance | 대기 불필요. 입력 판별 유지 | 출력·운영 알림을 다시 prompt로 섭취하지 않기 위한 분류다. |
| `src/services/discord/router/intake_gate.rs:575`, `:614`, `:628`, `:746` | 다른 인간 대상 mention, provider/channel 불일치, required mention 누락, 사용자 auth 실패 | 대기 불필요. 기존 admission 유지 | 다른 대상의 입력을 해당 pane에 쓰지 않기 위한 경계다. |
| `src/services/discord/router/intake_gate.rs:561`, `:948`, `:1545`, `:1570` | 등록된 slash/text command, meeting, 기존 제어 입력 | 명령 처리기로 소비. 일반 입력 대기 사유에서 제거 | 명령의 부수 효과를 보존해야 한다. 알 수 없는 slash 문자열까지 일괄 배제할 필요는 없다. #7266 정리 결과를 따른다. |
| `src/services/discord/router/intake_gate.rs:651`, `:806` | voice reply 또는 pending DM answer workflow가 소비 | 대기 불필요. 해당 workflow 유지 | 일반 prompt로도 보내면 중복 처리된다. |
| `src/services/discord/router/intake_gate.rs:666`, `:873` | channel binding 또는 usable session path 없음 | 올바른 대상 준비까지 보관 필요 | 세션 복원·부팅 후 같은 주입기로 보내야 한다. |
| `src/services/discord/router/intake_gate.rs:758` | 이미 terminal인 dispatch | 재실행 금지 유지 | 완료된 업무를 다시 시작할 이유가 없다. |
| `src/services/discord/router/intake_gate.rs:861` | 첨부 admission의 owner가 nonlocal | 올바른 owner로 전달 | 로컬 pane에 먼저 주입할 사유가 아니다. |
| `src/services/discord/router/intake_gate.rs:928` | 본문과 저장된 첨부 모두 없음 | 대기 불필요. 빈 입력 제외 | 전달할 payload가 없다. |
| `src/services/discord/router/intake_gate.rs:1106`, `:1190` | 활성 dispatch thread 또는 DISPATCH 충돌 | 업무 라우팅·metadata 준비는 유지. 일반 사용자 입력의 busy 대기 근거에서는 제거 | thread/dispatch 준비와 최종 TUI 입력 전달은 다른 책임이다. |




**즉시 주입의 상위 조건**




[현재 live 주입]\([https://github.com/itismyfield/AgentDesk/blob/16e52b5165f63cdccfaa2636a83ce8f8672ddc56/src/services/discord/router/intake_gate/busy_inject.rs)에는](https://github.com/itismyfield/AgentDesk/blob/16e52b5165f63cdccfaa2636a83ce8f8672ddc56/src/services/discord/router/intake_gate/busy_inject.rs\)에는) rollout 조건과 실제 준비 조건이 함께 들어 있다.




| file:line | 사유 | 기다려야 함/없앨 수 있음 | 근거 |
|---|---|---|---|
| `src/services/discord/router/intake_gate/busy_inject.rs:40`, `:76`, `:81` | 채널 gate 미허용, Windows, live Herdr 일괄 제외 | rollout gate·Herdr 일괄 제외는 제거 가능. Windows는 지원 범위 밖 | Herdr 실제 target 확인과 blanket exclusion은 별개다. |
| `src/services/discord/router/intake_gate/busy_inject.rs:145` | 빈 입력, text command, `DISPATCH:`, `/meeting ` | 입력 타입 분류 유지. 주입 실패로 취급하지 않음 | 명령·업무 준비 뒤 전달할 일반 입력을 구분하면 된다. |
| `src/services/discord/router/intake_gate/busy_inject.rs:150`, `:238` | reconcile 미완료, restart pending, shutting down | 복원·종료의 실제 mutation 구간만 보관 필요 | 기존 AI 턴이 끝날 때까지 새 메시지를 막는 drain 정책은 단일 경로에서 불필요하다. |
| `src/services/discord/router/intake_gate/busy_inject.rs:155` | `voice_claim_deferred` | 음성 승인·claim·실발화자 준비까지 보관 필요 | 준비 완료 후 일반 payload로 같은 주입기를 사용한다. |
| `src/services/discord/router/intake_gate/busy_inject.rs:163`, `:230` | `uploads_not_ready`, materialization 실패 | 첨부 준비까지 보관 필요 | 사용할 수 없는 경로를 전달하면 첨부가 빠진다. |
| `src/services/discord/router/intake_gate/busy_inject.rs:225` | 다른 source owner 또는 terminal 소유 | 재주입 금지 유지 | 하나의 입력에 writer 하나만 있어야 한다. |
| `src/services/discord/router/intake_gate/busy_inject.rs:226`; `src/services/discord/health/turn_deliver/inject.rs:334` | `has_inject_turn` false | idle라는 이유는 제거 가능. runtime 부재는 준비 필요 | Claude transcript tmux는 idle도 통과하지만 Codex는 busy model turn을 요구한다. |
| `src/services/discord/health/turn_deliver/inject.rs:137`, `:152` | inject mode off, Codex 채널 gate | 최종 단일 경로에서는 중복 rollout 정책 제거 가능 | 실제 운영값은 미확인이다. |
| `src/services/discord/health/turn_deliver/inject.rs:378` | `provider_unsupported` | 기다려도 해결되지 않음 | 현재 주입 구현은 Claude/Codex TUI 대상이다. nonTUI 실행까지 삭제하면 안 된다. |
| `src/services/discord/health/turn_deliver/inject.rs:391`, `:424` | session/transcript/binding 미해결 또는 변경 | 올바른 대상 확보까지 보관 필요 | 다른 pane으로 보내면 안 된다. |
| `src/services/discord/health/turn_deliver/inject.rs:312`, `:393` | `not_busy`, `not_steerable`, `turn_unknown` | busy 전제는 제거 가능. provider별 submit 계약 변경 필요 | 상위 조건만 지우면 Codex의 `target_turn` 계약이 남는다. |
| `src/services/discord/health/turn_deliver/inject.rs:401` | session transition lock busy | mutation 직렬화 필요 | AI 턴 종료 대기 대신 동일 writer가 transition 해제 뒤 재시도한다. |
| `src/services/discord/health/turn_deliver/inject.rs:181`, `:194` | `input_in_flight`: claim은 있지만 row가 아직 없음 | 앞 입력의 submit 순서만 필요 | 공통 writer의 직렬화로 통합할 수 있다. |
| `src/services/discord/health/turn_deliver/inject.rs:210`, `:407` | `queue_nonempty` | AI 턴 완료 대기는 제거 가능. 미전달 입력 순서는 유지 | 앞 입력부터 busy 중에도 같은 주입기로 전달하면 된다. |
| `src/services/discord/health/turn_deliver/inject.rs:411` | Bound Herdr execution/pane 없음 | 올바른 host 준비까지 보관 필요 | tmux fallback은 다른 실행에 입력할 수 있다. |
| `src/services/discord/health/turn_deliver/inject.rs:429` | `holder_changed` | 이전 AI 턴 holder 고정은 제거 가능. runtime target 확인은 유지 | 입력이 경계에서 다음 native 턴으로 들어가는 것은 허용해야 한다. |




`Origin::Obligated`의 iMessage episode 조건(`inject.rs:472`)은 Discord 조사 범위 밖이다.




**보관 입력의 예약·선택 조건**




| file:line | 사유 | 기다려야 함/없앨 수 있음 | 근거 |
|---|---|---|---|
| `src/services/turn_orchestrator/injected_inputs.rs:175` | consumed, terminal, 같은 source 소유 | 재주입 금지 유지 | 공통 source identity로 합친다. |
| `src/services/turn_orchestrator/injected_inputs.rs:187`, `:200` | claim 변경, 다른 injection/pending dispatch/queued-head 예약 | writer 직렬화는 유지. AI 턴 claim 고정은 제거 가능 | paste와 Enter가 서로 섞이면 안 된다. |
| `src/services/turn_orchestrator/injected_inputs.rs:214`, `:230` | disk/memory backlog 또는 읽기 불명 | 원문 순서·읽기 실패 보존 필요 | disk와 memory를 각각 판별하는 이중 경로는 통합 가능하다. |
| `src/services/discord/queue_io/queued_head_inject.rs:33`, `:87`; `src/services/discord/health/turn_deliver/inject.rs:119` | queued gate off, gate wiring 없음, Claude-only | rollout·provider 제한 제거 가능 | 먼저 Codex effect와 receipt adapter가 필요하다. |
| `src/services/turn_orchestrator/queued_head/prefix.rs:22` | Ready 없음, Parked 건너뜀, Unavailable 뒤 정지 | 읽기 불명은 보존 필요. 일반 입력에 기존 retry cap 정책을 복제할 필요 없음 | busy 여부와 관계없는 별도 retry 정책이다. |
| `src/services/turn_orchestrator/queued_head/prefix.rs:38` | 단일 사람·단일 source·단일 segment·Soft·preserve 조건 | payload 준비 뒤 shape 제한 제거 가능 | 병합·허용된 봇 입력도 source IDs와 함께 전달할 수 있어야 한다. |
| `src/services/turn_orchestrator/queued_head/prefix.rs:59` | `!`, `/`, `<@`, `DISPATCH:` 접두사 | 실제 명령 분류는 유지. 문자열 접두사 일괄 거절은 제거 가능 | 준비 완료 여부를 접두사로 대신 판정하면 정상 입력이 남는다. |
| `src/services/turn_orchestrator/queued_head/prefix.rs:60` | 첨부·음성·reply·reply boundary 존재 | 공통 전처리 뒤 제거 가능 | 현재는 이 입력들을 일반 턴 fallback이 처리한다. |
| `src/services/discord/health/turn_deliver/inject/queued_owner.rs:215` | 이름 조회 timeout/error, author mismatch | 표시 이름 조회 실패는 제거 가능. 다른 source 소유는 확인 유지 | nick→global name→username→ID fallback이면 표시 때문에 배달을 막지 않는다. |
| `src/services/turn_orchestrator/queued_head.rs:650` | Reserved 경합, MayHaveSent/Unknown 차단 | 동시 effect는 직렬화. Unknown은 자동 재주입 금지 | 기존 V1은 reservation 하나이므로 Unknown이 뒤 입력도 막는다. |
| `src/services/turn_orchestrator/queued_head.rs:671`, `:681`, `:686`, `:704` | 저장 읽기 실패, pending marker, 다른 예약, expected identity 변경 | 실제 소유·순서 확인 유지 | caller snapshot 뒤 실제 head가 달라질 수 있다. |
| `src/services/turn_orchestrator/queued_head.rs:726`; `src/services/discord/health/turn_deliver/inject/queued_owner.rs:103`, `:109` | reservation/attempt/MayHaveSent 저장 실패 | effect 실행 금지·원문 보관 | effect 전 기록 실패를 무시하면 재시작 후 실행 여부를 모른다. |
| `src/services/discord/health/turn_deliver/inject/queued_owner.rs:157`, `:179` | effect 후 target 변경, settle 불명 | 자동 재주입 금지 

…
더 보기
18m 20s 동안 처리함
ChatGPT 답변:

DESIGN_READY

AgentDesk #7272 — 입력 단일 경로 설계 r2

권고는 (b), “확인 상한 시간 뒤 원문을 보관하고 다음 입력을 진행”이다. 확인되지 않은 입력을 전달 성공으로 간주하지 않으면서, 그 입력 하나가 채널 전체를 계속 막는 문제를 없앤다.

Cold 첫 입력은 durable owner가 소유하는 BootstrapAndFirstInput 한 번으로 전달한다. Claude의 첫 paste, Codex tmux의 argv prompt, Herdr의 첫 입력을 이 effect 안으로 넣는다. 기존 fresh-turn 함수에서 실행 준비·첫 전달과 AI 응답 완료 대기를 분리해야 한다.

r1의 다음 계약은 교체한다.

V1 Unknown singleton 유지 → 확인 대기에는 상한을 두고, 미확정 원문은 실행 목록 밖에 보존.
본문 없는 bootstrap → owner가 허가한 부팅과 첫 입력의 단일 effect.
transcript 부재를 일괄 준비 대기로 처리 → runtime 부재와 Source 파일 부재를 구분.
기존 receipt를 그대로 재사용 → 현재 plain frame에 맞춰 파일 생성 전 등록·늦은 Source 결합·동일 본문 모호성을 보완.

소스 기준은 전부 
794e7123e1749db0dea10b638a2af84d7cc8c191다. 이슈의 목표와 durable 기록·6조각 결정도 확인했다. 아래 시간값·새 타입명·변경량은 r2의 설계 결정 또는 추정이며 현재 구현이라고 주장하지 않는다. 
이슈 #7272, 
사용자 결정 기록

1. P1-1: 세 대안 비교와 권고

다음 규모는 Unknown 해소 영역의 비교 추정이다. Cold launch와 전체 입력 경로 통합 비용은 별도다.

대안	운영 결과	사용자에게 보이는 차이	변경 규모 추정
(a) 현 live처럼 Unconfirmed를 정산	현재 writer가 끝나면 바로 다음 입력으로 간다. 해당 입력은 자동 재전송하지 않는다. 실행 보관소의 원문은 정리된다.	가장 빨리 다음 입력을 보낼 수 있다. 그러나 확인 실패한 입력은 실제로 전달되지 않았을 수 있다. 경고를 붙여도 ADK 안의 원문 보존은 포기한다.	prod 4–7파일 / +120~250. 가장 작다.
(b) 상한 시간 뒤 원문 유지·다음 진행 — 권고	잠깐 receipt를 더 찾고, 끝내 확인되지 않으면 원문·attempt를 비실행 보관으로 옮긴다. 뒤 입력·drain·다른 입력의 claim이 진행한다.	“이 입력은 전달 여부를 확인하지 못해 원문을 보관했습니다. 다음 입력은 계속 처리합니다. 자동으로 다시 보내지는 않습니다.”라는 알림을 받는다.	prod 10–15파일 / +700~1100. snapshot·복원·deadline·history 수명 보완이 필요하다.
(c) 모든 항목에 전체 상태 부여	각 입력의 준비·전달·미확정·정산을 독립 상태로 관리한다. selector·취소·복원을 전반적으로 바꾼다.	각 입력의 상태와 처분을 더 세밀하게 제공할 수 있다.	prod 15–24파일 / +1100~1800. 한 리뷰 단위로는 정지선과 상한에 걸릴 가능성이 높다.

(b)는 일부 항목별 기록을 필요로 한다. 다만 일반 입력 전체를 새 상태 기계로 옮기는 대신, 실행이 미확정된 예외 항목만 별도로 보관한다. 물리 writer는 계속 하나다.

현재 live 경로는 확인 불명도 InjectionSettlement::Delivered(Unconfirmed)로 정산한다. 반면 queued 경로는 Unknown head를 남기고, 이 head가 withhold_take, blocks_claims, 다음 head claim까지 막는다. r2는 live의 진행성을 유지하면서 원문 보존을 추가한다. 
inject.rs:660–681, 
queued_head.rs:480–506, 
queued_head.rs:650–668

“입력이 멈추지 않는다”의 정확한 계약

정상 저장소와 준비된 대상에서, 앞 입력의 receipt가 없다는 사실만으로 뒤 입력을 무기한 막지 않는다. AI 턴 종료도 기다리지 않는다.

실제로 pane을 쓰고 있는 writer, 보호해야 할 draft/modal, 읽거나 쓸 수 없는 저장소는 별도의 준비 조건이다. Unknown 타임아웃으로 이런 보호를 강제로 없애지는 않는다.

2. P1-1 해소 계약: 확인 대기와 원문 보관을 분리한다
2.1 기존 queue snapshot을 V2로 확장한다

V1은 head_injection 하나와 terminal_identities를 갖는다. 예약 head는 반드시 실행 큐의 첫 항목이어야 한다. 따라서 head만 지우면 원문이 다시 실행 대상이 되고, terminal로 넣으면 원문이 strip된다. 원문 보존과 후속 진행을 동시에 하려면 저장 표현을 바꿔야 한다. 
queued_head.rs:53–96, 
queued_head.rs:394–453

V2는 같은 파일 안에서 다음을 구분한다.

필드	의미
items	앞으로 자동 전달할 수 있는 원 입력들. 기존 순서·병합 정책을 유지한다.
head_injection	현재 선택한 입력의 예약·effect·확인 대기. 동시에 하나다.
retained_inputs	자동 실행 대상이 아닌 원문 보관. Unconfirmed 또는 영구 입력 오류의 Rejected를 구분한다.
terminal_identities	확인 정산된 source와 정산 시각. Discord 재도착 차단에 사용한다.

retained_inputs에는 원 PendingQueueItem, 준비된 payload의 identity와 문맥 귀속, attempt, 보관 사유, 확인 deadline, 알림 intent를 보존한다.

이 목록은 다음에서 제외한다.

다음 자동 전달 항목 선택
merge 대상
실행 대기 cap 30개
일반 drain의 전달 대상
background/dispatch claim의 “앞에 대기 입력이 있음” 판정

대신 동일 source의 재접수 차단, 늦은 receipt 정산, 알림 복구에는 계속 포함한다.

2.2 확인 상한은 30초다

30초는 r2의 기본 설계값이다.

MayHaveSent를 effect 전에 durable하게 기록하면서 확인 deadline을 한 번 정한다.
정상 receipt가 먼저 오면 즉시 정산하고 다음 입력으로 간다.
확인되지 않으면 deadline 만료를 처리하는 첫 actor 실행에서 비실행 보관으로 옮긴다.
재시작은 남은 시간을 복원한다. 매번 now + 30초로 연장하지 않는다.
V1 Unknown처럼 deadline이 없는 복원 항목은 receipt를 대조하고 비실행 보관으로 이관한다. 다시 입력하지 않는다.

만료 처리는 하나의 snapshot commit이다.

정확한 source·attempt 비교 → 원문과 attempt를 retained_inputs에 넣음 → 실행 큐에서 그 입력만 제거 → 해당 head 해제 → notice intent 기록 → critical publish.

후속 입력 진행은 이 commit이 확인된 뒤 시작한다. A의 deadline이 B·C의 원문을 제거하거나 정산해서는 안 된다.

현재 backstop은 최초 2초, 이후 60초이며, 보관 metadata만 남은 상태는 backlog로 세지 않는다. 기존 task의 다음 실행 시각을 min(기존 retry 시각, 가장 이른 확인 deadline)으로 바꾸고, 만료·알림·receipt 처리를 busy/eligibility 판정 앞에 둔다. 새 메시지나 턴 종료가 없어도 작동해야 한다. 
backstop.rs:45–48, 
backstop.rs:104–112, 
backstop.rs:372–418

2.3 논리 head와 물리 writer의 수명을 분리한다

deadline 만료는 전송 실패 증거도, writer 종료 증거도 아니다.

현재 effect는 spawn_blocking으로 실행된다. future에 timeout을 걸어도 작업이 실제로 끝났다고 볼 수 없다. 
queued_owner.rs:134–154

따라서 다음 계약을 둔다.

A를 비실행 보관으로 옮겨도, 살아 있는 A의 writer·transition 보호는 독립적으로 유지한다.
B는 접수·예약할 수 있지만 실제 쓰기는 A의 물리 작업 종료와 대상 준비 확인 뒤에 한다.
A의 늦은 effect 결과는 retained A의 source·nonce·attempt를 비교하여 정산한다. “현재 head”를 정산하는 기존 호출을 그대로 쓰지 않는다.
A의 paste 잔재가 draft로 남았다면 그 draft를 보호한다. 정확히 소유한 잔재를 기존 안전한 방법으로 처리할 수 있거나 composer가 다시 준비되면 B를 진행한다.
실제 session mutation도 같은 writer/transition 보호를 우회하지 않는다.

Herdr에는 queue head와 별도로 input hold가 있다. 이 hold를 Source 부재만으로 계속 유지하면 r2도 다시 막힌다. writer 종료·동일 execution·안전한 composer를 확인하면 receipt/Source 부재와 무관하게 입력 hold를 해제해야 한다. 
claude/herdr_turn.rs:398–419, 
codex/herdr_turn.rs:379–433

2.4 어떤 증거가 어떤 정산을 허용하는가
관찰	허용하는 처리	허용하지 않는 처리
해당 attempt의 긍정 receipt	그 입력만 확인 정산	다른 head 정산
같은 프로세스가 effect 미진입을 확실히 알고 있음	NotSent로 풀고 안전하게 다시 준비	실행 가능성을 남긴 채 재시도
deadline 만료	원문 유지·비실행 보관·다음 진행	Delivered로 간주, 자동 재전송
native 턴 종료인데 receipt 없음	receipt 재대조 또는 보관	“입력이 전달되지 않았다”는 판정
현재 binding/session 변경	과거 attempt 보존, 새 target은 별도 검증	과거 입력을 새 source에서 확인하거나 다시 전송
재시작 후 ledger 등록이 보이지 않음	기존 attempt 복구·대조	등록 부재만으로 미전송 판정

따라서 리뷰 r1의 **“턴 종료·source 변경 시 terminal로 정리하고 원문 제거”**는 채택하지 않는다. 이 사건들은 후속 진행의 계기가 될 수 있지만, A의 전달 성공이나 실패를 증명하지 않는다.

2.5 사용자가 보는 것과 알림 실패 처리
실제 준비 대기에는 기존 📬 의미를 유지한다.
확인 불명에는 ⚠️를 표시한다.
비실행 보관 전환 시 다음 의미의 알림을 남긴다.

이 입력은 전달 여부를 확인하지 못해 원문을 보관했습니다. 다음 입력은 계속 처리합니다. 이 입력은 자동으로 다시 보내지 않습니다. 같은 내용을 새 메시지로 보내면 중복 실행될 수 있습니다.

알림 intent는 보관 전환과 같은 snapshot에 저장한다. 전송은 기존 persistent-dedupe outbox를 사용한다. (provider, channel, source identity, attempt nonce, notice kind)를 안정적인 알림 identity로 삼는다.

PG enqueue나 Discord 전송 성공을 B의 진행 조건으로 삼지 않는다. 알림 enqueue 후 상태 기록 전에 재시작해도 같은 identity로 다시 투영한다. 알림 source는 operational/non-turn으로 등록하여 허용된 봇 입력으로 재섭취되지 않게 한다. 기존 custody notice가 이 outbox 사용 패턴을 갖고 있다. 
custody_notice.rs:33–66,96–115, 
message_outbox.rs:280–347

늦게 확인되면 원 메시지의 경고를 정리한다. 기존 outbox 재-enqueue가 이미 전송된 본문을 수정한다고 가정하지 않는다.

2.6 재시작·clear·저장 실패

보관 항목만 남은 채널도 복원해야 한다. 현재 queue loader는 snapshot metadata를 버리고 실행 items가 있는 채널만 복원 대상으로 넣는다. V2 reader는 metadata-only 채널도 actor adoption·알림·receipt 복구 대상으로 반환해야 한다. 
pending_queue_persistence/queued_recovery.rs:67–90, 
runtime_bootstrap/queued_recovery.rs:99–149

Reserved 복원: effect가 시작되지 않은 계약에 따라 재준비한다.
MayHaveSent 복원: 자동 입력하지 않고 확인·보관 처리를 한다.
retained_inputs 복원: 실행 큐로 되돌리지 않는다.
일반 clear/purge: 실제 대기 입력에 대한 기존 처분은 유지하되, 이미 시도된 retained 원문은 삭제하지 않는다.
기존 대기 입력 취소를 retained 입력에 적용하면서 “실행 취소됨”이라고 표시하지 않는다.
unresolved retained 원문에는 24시간 TTL을 적용하지 않는다.

저장 실패도 단순 메모리 rollback만으로 끝내면 안 된다. 현재 critical publish는 파일 게시 후 오류가 날 수 있고, 이후 metadata 비교가 계속 실패할 수 있다. 
pending_queue_persistence.rs:511–523, 
queued_head.rs:282–300,789–798

같은 owner가 알고 있는 old/candidate snapshot을 readback하여 다음처럼 수습한다.

정확한 candidate: 해당 상태를 채택하고 필요한 sync를 재시도.
정확한 old: 같은 상태 전이를 재시도.
둘 다 아니거나 읽기 불가: 실제 저장소 오류로 보존·알림.

어느 경우에도 저장 불명을 이유로 A를 다시 타이핑하지 않는다.

3. 숨은 차단점: frame history도 함께 고쳐야 한다

이 SHA에서는 주입 frame 본문에 nonce가 표시되지 않는다. plain_inject는 일반 사용자 frame을 만들고, receipt는 별도 history의 등록과 native row 소유권을 이용한다. “nonce를 transcript에서 찾으면 된다”는 r1 설명은 현재 새 frame에 그대로 적용되지 않는다. 
human_frame.rs:109–117, 
rollout.rs:170–239

현재 history에는 두 제한이 있다.

전체 entry가 4096개면 등록 거절.
같은 source 파일·같은 frame의 미확정 entry가 있으면 등록 거절.

따라서 A를 queue의 retained 목록으로 옮겨도 같은 본문의 새 메시지 B가 다시 막힐 수 있다. 
history.rs:144–171

history 수명 계약

기존 runtime/injected_frames.json 안에서 다음을 구분한다.

구분	역할
Live registration	현재 입력 effect의 등록 슬롯
Retained collision evidence	전달 여부를 확정하지 못한 과거 attempt의 후보 증거
Settled recognition evidence	이미 정산된 frame의 분류·native row 소유권 증거

4096 제한은 live 등록 슬롯에 적용한다. 끝난 시도는 슬롯을 반환하고, 과거 식별에 필요한 증거는 compact 형태로 보존한다. 숫자를 높이는 방식이 아니다.

같은 본문 A가 미확정 보관 중이어도 B의 등록·전달을 막지 않는다. 다만 A를 receipt 후보에서 지우지도 않는다. 같은 native row가 A와 B 모두에 해당할 수 있으면 먼저 조회한 쪽에 임의로 귀속하지 않고 미확정으로 남긴다. B 역시 원문 보관으로 갈 수 있지만 다음 입력은 계속 진행한다.

queue snapshot의 보관·정산 결과를 정본으로 삼고 history 상태를 재적용 가능하게 투영한다. 두 저장 사이의 crash는 복원 때 수선한다.

물리 GC의 한계는 명시한다

과거 history를 지우면 plain frame의 injected 분류나 replay의 native row 귀속이 바뀔 수 있다. 현재 O의 gc_through는 실제 spool 삭제 경계를 제공하지만 모든 relay·복구 소비자의 재독해 안전 경계를 보장하지는 않는다. 
human_frame.rs:150–170, 
history.rs:316–391, 
store/ledger.rs:315–321

따라서 r2는 다음을 보장한다.

한 장기 source가 4096회를 넘어도 과거 완료 건 때문에 새 등록이 막히지 않는다.
과거 frame 분류·row 소유권·미확정 충돌 증거를 보존한다.
모든 관련 소비자의 안전 경계가 확인되지 않은 compact evidence는 물리 삭제하지 않는다.

compact evidence의 저장량 증가는 P2 비용으로 남는다. “전체 history가 항상 고정 개수 이하”라는 보장은 하지 않는다.

4. P1-2: provider별 첫 입력 전달과 receipt
4.1 단일 owner가 두 effect 형태를 선택한다

공통 owner는 대상 상태에 따라 다음 중 하나만 선택한다.

기존 runtime: Submit
실제 부재가 증명된 runtime: BootstrapAndFirstInput

둘 다 같은 접수·원문 보존·attempt·정산 계약을 쓴다. Cold라는 이유로 owner 밖의 일반 턴 fallback이 본문을 먼저 보내지 않는다.

EffectPermit은 현재 attempt의 MayHaveSent commit 뒤에만 발급하는 한 번 소비하는 권한으로 둔다. 단순한 HeadTicket 참조를 전달하는 것만으로 두 번 launch/submit할 수 없어야 한다.

4.2 첫 입력 방법 전수표
Provider / host	첫 사용자 payload 전달	effect 전에 저장할 source 기대값	실제 receipt
Claude tmux	세션 준비·launch 뒤 ready composer에 paste/Enter 1회	Claude UUID, 예정 transcript 경로, prepared execution nonce, host/root, frame identity	같은 실행에 결합된 transcript의 exact user/enqueue 기록
Codex tmux	argv prompt 1회. 이후 첫 입력 paste 없음	prepared execution nonce, provider root, 실제 argv prompt digest, resume이면 기존 session/source 기대값	같은 실행에 결합된 rollout의 exact user item
Claude Herdr	준비된 hosted execution의 gate를 거쳐 첫 paste/Enter 1회	hosted execution nonce, owner/endpoint/pane, expected Claude UUID·예정 경로	같은 실행의 Source/Resolved에 결합된 exact frame 기록
Codex Herdr	argv에는 prompt 없음. ready composer에 첫 paste/Enter 1회	hosted execution nonce, owner/root/provider, frame identity. 아직 모르는 native UUID·경로는 비워 둠	자기 실행의 Source가 결합된 후 rollout user item

Codex tmux는 argv에 prompt를 넣지만 Herdr는 argv에서 제외한다. Herdr의 현재 first_prompt는 입력 후 Source를 기다린다. 이 순서를 “Source 확보 후 첫 입력”으로 뒤집으면 다시 교착한다. 
codex.rs:478–500, 
codex/tui_session_launch.rs:420–446, 
codex/herdr_turn.rs:363–433

4.3 파일 생성 전 attempt를 이름 붙인다

현재 HeadAttempt::before_paste와 frame history 등록은 실제 파일 metadata를 요구한다. 파일 없음 표식 하나만 추가해서는 cold receipt 계약이 완성되지 않는다. 
queued_head/mend.rs:36–52, 
history.rs:144–150

attempt source를 두 형태로 둔다.

ExistingSource: session/path/dev/ino/offset, execution identity, 정확한 frame identity.
PreSource: provider·host·owner/root·execution nonce, binding watermark와 source 기대 관계, 알 수 있는 expected session/path, 정확한 frame bytes/digest, 원 Discord source IDs와 anchor.

Codex fresh의 아직 없는 native UUID·경로에 가짜 값이나 (dev, ino) = (0, 0)을 넣지 않는다. Offset 0은 실제 신규 source에만 적용하며, 기존 transcript를 resume하는 경우에는 기존 파일의 경계를 사용한다.

순서는 다음과 같다.

원문과 Reserved + PreSource를 owner snapshot에 기록한다.
같은 attempt를 기존 frame history에 planned 상태로 등록한다.
MayHaveSent commit 후 permit을 발급한다.
launch/argv/paste를 한 번 수행한다.
같은 실행의 검증된 Source/Resolved에 한 번 결합한다.
결합된 source에서 기존 provider receipt reader를 사용한다.

SessionStart, binding 성공, pane ready는 입력 receipt가 아니다. Source 탐색 증거와 사용자 payload 수신 증거를 구분한다.

4.4 실행 준비와 실제 effect 사이의 기존 경계
경로	활용할 기존 경계
Claude tmux	claude/tui_session_launch.rs:71–77의 incarnation 준비와 :110–129의 create를 분리
Codex tmux	tui_session_launch/launch_policy.rs:28–51의 prepared incarnation과 absence_launch.rs:368–372의 create 사이
Herdr	herdr_launch.rs:334–384의 incarnation 준비·prepare callback과 create 사이

이 경계에서 owner의 attempt를 durable하게 만든다. 실제 launch 뒤에 execution nonce를 추측하여 기록하지 않는다. 
Claude 준비/create, 
Codex launch policy, 
Herdr launch

4.5 기존 fresh 함수 전체를 owner 안에서 기다리지 않는다

현재 Claude fresh helper는 입력 후 transcript turn 종료까지 기다리며, Codex launch도 tail dispatch로 이어진다. Herdr 역시 submit 뒤 read-reply 단계가 붙어 있다. 이를 통째로 사용하면 첫 입력 A가 AI 완료까지 writer를 붙잡고 B를 막는다. 
claude.rs:2058–2082, 
codex/absence_launch.rs:368–422

launch·첫 submit·source/receipt 조사·출력 완료 관찰을 분리한다. 입력 writer는 물리 전달 작업이 끝나면 반환하며, AI 출력은 기존 native relay가 관찰한다.

특히 Codex Herdr의 현재 60초 Source 대기는 writer 밖으로 옮긴다. Source가 늦다는 이유로 물리 입력 hold를 계속 잡지 않는다. 
codex/herdr_turn.rs:53,406–441

4.6 살아 있지만 Source가 없는 runtime을 처리한다

이 구분이 빠지면 A를 보관한 뒤 B가 다시 session_unresolved에 막힌다.

Resolver 결과	동작
AbsentProven	기존 absence proof에 따라 새 부팅과 첫 입력을 허용
ReadyWithSource	기존 source attempt로 같은 runtime에 submit
ReadyWithoutSource	같은 exact runtime에 PreSource attempt로 submit. 새 launch는 하지 않음
NotReady / IdentityConflict / Unreadable	원문 보관. 부재로 추정하지 않음

Herdr는 현재도 Pending | Bound execution에서 location·expected execution·nonce가 맞으면 target을 만들 수 있다. 새 PG 상태가 필요한 것이 아니다. 
herdr_gate.rs:231–259

필수 사례는 다음과 같다.

A가 cold launch와 첫 paste를 했지만 Source가 끝내 나타나지 않는다. 30초 뒤 A를 보관한다. writer가 끝났고 composer가 준비됐다면 B는 같은 execution nonce에 한 번 입력한다. 총 create 횟수는 계속 1회다.

B는 A의 launch first-prompt digest를 덮어쓰지 않는다. A/B의 개별 frame identity와 runtime의 최초 source 기대값을 분리한다. Source가 늦게 생기면 각 attempt의 execution·binding 경계·expected session을 대조한다.

4.7 Claude resume _와 Codex idle relay

Claude resume의 _는 자동으로 해소됐다고 주장하지 않는다. 현재 launch script가 CLAUDE_CODE_RESUME_PROMPT="_"를 설정한다. owner 소유 launch로 바꿔도 이 CLI 동작은 남는다. 
session.rs:381–433

r2는 기존 resume보다 자동 입력을 추가하지 않고, 실제 Discord payload의 exact frame만 receipt로 인정한다. _를 전역 필터링하거나 “native synthetic turn 0개”를 수용 기준으로 삼지 않는다.

Codex idle는 target_turn=""만 넣는 것으로 충분하지 않다. 현재 다른 native turn은 Claim::Handed로 넘어간다. 
injected_steer.rs:97–153

r2는 SubmitNewTurn과 SteerExistingTurn 의도를 구분하되, 최종 귀속은 실제 receipt의 native turn으로 정한다.

새 턴이면 원 Discord 메시지를 기존 native turn anchor에 연결한다.
이미 있는 턴이면 그 턴에 입력을 연결한다.
busy→idle 경계에서 새 턴이 열려도 원 source를 유지한다.
입력마다 별도 가짜 turn·placeholder를 만들지 않는다.
원 메시지의 📥 수명과 실제 native 턴의 ⏳→✅ 수명은 기존 계약을 따른다.
5. 공통 payload와 지원 범위
공통 준비에서 보존할 기능

역할·공유 지침·profile·memory scope·session strategy·model/options·CWD/worktree 준비는 기존 helper를 추출해 사용한다. launch 시 native instruction과 그 manifest를 기록하고, warm 입력에 전달하지 않은 system prompt의 manifest를 새로 만들지 않는다. 
prompt_builder/mod.rs:214–222,298–402,644–685

입력 준비 계약은 다음과 같다.

원 Intervention과 모든 source IDs를 보존한다.
reply·첨부·본문을 기존 render_input_chunks 규칙으로 한 번 렌더한다.
표시 이름 조회 실패는 ID fallback으로 처리한다.
recovery context·recent context·one-shot은 해당 입력에 귀속한다. NotSent면 기존 복구 규칙으로 되돌리고, 미확정이면 그 attempt에 남긴다.
첨부는 owner 반환 후에도 존재해야 한다. 기존 prepare_busy_inject_uploads/retention을 재사용한다.
voice 승인·durable claim의 accepted 상태를 NotSent·재시작에서 잃지 않는다.

첨부의 기존 materialization 객체는 임시 directory를 소유한다. 일반 실행처럼 AI 완료까지 이 객체를 잡아두지 않는 새 owner에서는 retention을 명시적으로 사용해야 한다. 또한 현재 queue payload 준비는 voice_announcement를 제거하므로, “그대로 저장하면 된다”는 설명으로는 부족하다. 
materialize.rs:1–52, 
attachments/busy_inject.rs:5–27, 
queue_payload.rs:5–8

지원 조합

대상은 Claude/Codex × local tmux/Herdr의 네 조합이다. 각 조합의 fresh·resume·warm idle·warm busy를 증명한다.

이 조합 안에서는 실패했다고 owner 밖의 일반 턴 전달로 빠지지 않는다.
Gemini/Qwen/OpenCode/stream-json/headless 및 External·Obligated caller는 기존 계약을 유지한다.
Remote profile은 명시 시험 축에 포함하되 기존 지원·거절 의미를 유지한다. 현재 Codex remote-direct는 거절 경로이므로 “legacy fallback 성공”을 요구하지 않는다. 로컬 pane 오주입은 0회여야 한다. 
codex.rs:1374–1385
Native queue 표시 자체를 busy 대기 사유로 쓰지 않는다. 실제 composer·modal·draft·target 검증을 adapter에 남긴다.
렌더된 frame이 64 KiB를 넘거나 영구적인 입력 오류면 명시적 Rejected와 알림으로 정리하고 뒤 입력을 진행한다. 같은 오류를 NotSent로 영원히 재시도하지 않는다. 원문은 예외 보관에 남긴다. 현재 크기 경계는 MAX_INPUT_BYTES = 64 * 1024다. 
busy_inject.rs:45
6. P2-1~P2-8 처분
항목	처분	r2 계약과 근거
P2-1 terminal 무한 증가	반영, TTL 기준 수정	V2 terminal에 settled_at을 넣고 정산 후 최소 24시간 유지한다. snowflake 생성시각으로 자르지 않는다. 오래 기다린 입력을 오늘 정산했을 때 즉시 만료되는 것을 막는다. 기간 안의 Discord 증거를 count cap으로 먼저 버리지 않는다. retained 원문에는 TTL을 적용하지 않는다. history의 별도 저장 증가 비용은 §3처럼 공개한다.
P2-2 generation 재도착 중복	반영	Discord 재도착 판정은 기존 provider/channel/저장 namespace 안의 message ID 기준이다. generation은 ticket·CAS·attempt 소유권에 유지한다. 병합 source 각각을 검사한다. synthetic·다른 origin에 message-ID-only를 일괄 적용하지 않는다.
P2-3 조각 1 gate	반영, 사용자 결정 유지	조각 1은 production Codex queued gate를 닫아 둔다. adapter 시험과 운영 admission을 분리한다. 현재 Claude-only 조건은 inject.rs:119–123에 있다.
P2-4 증명표 누락	반영·확대	cold/warm/resume, 네 provider/host 조합, Unknown, Source 없음, bot, DM, dispatch thread role/CWD, oversized, remote, 모든 ingress를 포함한다. 지원 조합 내부의 legacy 우회를 금지한다.
P2-5 파일 소유·giant	반영	조각별 파일 예산·raw 증가 0·다른 레인 착지 순서를 명시한다. herdr/위임 롤백 레인 소유권은 제공된 작업 정보를 전제로 조율하며, 이 조사에서 운영 중 소유권을 새로 확인했다고 주장하지 않는다.
P2-6 durable 쓰기 횟수	반영, 정확한 측정 범위 지정	즉시 준비된 입력은 접수+Reserved → MayHaveSent → 정산의 queue snapshot critical publish 3회로 합친다. 준비 대기는 먼저 접수해야 하므로 추가 쓰기를 허용한다. frame history·outbox 쓰기를 포함한 “전체 fsync 3회”라고 말하지 않는다.
P2-7 resume _	자동 해소 주장 기각, 기존 동작 유지	owner-owned launch만으로 _는 사라지지 않는다. 실제 payload receipt와 분리하고 전역 _ 필터는 넣지 않는다. CLI의 내부 턴 제거는 이 설계의 보장이 아니다.
P2-8 Codex idle 귀속	반영, 빈 target만 쓰는 수정은 기각	actual native turn과 원 Discord source를 연결한다. target_turn=""만 넣어 Handed로 보내는 것으로 완료하지 않는다. idle 및 busy→새 턴 경계에서 anchor·placeholder·완료 처리가 각각 한 번이어야 한다.

관련 현재 구현: 
queued_head.rs:418–453, 
inject_disposition.rs:20–21,361–368, 
inject.rs:119–123, 
queued_head.rs:699–735

7. 갱신된 6조각 계획

6조각은 유지한다. 조각 2의 중심을 상태·복구·history 수명 계약으로 옮기고, payload 공통화 일부를 조각 3으로 이동한다. 조각 4가 모든 새 상태와 모든 배선을 한꺼번에 구현하지 않도록 나눈다.

아래 숫자는 테스트를 제외한 설계 예산이다. 실제 diff 측정값이 아니다.

조각	범위·주요 prod 파일군	prod 파일 추정	순증 추정	순서·되돌리기
1. provider adapter 분리	inject/queued_owner, queued_reconcile, effect/receipt adapter 자식 모듈. 운영 gate 유지	4–6	+70~150	현재 다른 세션 작업 유지. 저장 형식 변경 없이 독립 revert 가능
2. owner V2·미확정 보관·복구 기반	queued_head 자식 모듈, queue persistence/readers, clear/purge/drain 소비자, source identity, metadata-only 복원, notice intent/outbox 등록, history live/passive 수명	18–26	+800~1150	새 계약의 reducer·reader를 완성한다. 새 production 유입 연결은 아직 하지 않는다. V2 생성 전에는 revert 가능
3. 공통 payload·owner 소유 첫 입력	payload/voice/attachment helper, PreSource 등록·결합, Claude prepare/create·submit 분리, Codex argv 경계, 두 Herdr first-submit/observer 분리	18–26	+700~1100	조각 2 뒤. source·history 복원과 cold effect를 함께 증명한다. 기존 caller는 추출 helper를 사용하되 새 owner 호출은 다음 조각에서 연결
4. AnyTurn·모든 ingress·wake 통합	live/복원/worker/catch-up/drain 연결, ReadyWithoutSource, deadline task, Herdr hold 해제, idle relay 귀속	16–26	+600~1000	1~3 뒤 production 연결. Codex gate는 이 조각의 증명 뒤 해제. rollback은 V2 소비자를 유지하는 유입 배선 복귀로 한다
5. 대상 일반 턴 fallback 제거·정산 정리	kickoff와 gateway completion drain, worker 우회 제거, terminal TTL, 즉시 접수+예약 publish 합치기	8–16	−350~+100	§10 수용표가 실제 ingress에서 통과한 뒤 제거. 대상 밖 shared caller 유지. 이 조각만 되돌릴 수 있도록 한다
6. 대상 .dispatch 신규 쓰기·불용 분기 정리	남은 대상 전용 marker writer, rollout 중복 분기, 사용하지 않는 wrapper	4–8	−250~−100	소비자 확인 뒤 수행. 기존 pending 데이터와 legacy reader를 보존한 revert

전체 순증 예산은 대략 +1,600~3,400줄이다. r1의 −600~+450 추정은 V2 보관·cold receipt·history 수명 비용을 반영하지 못했으므로 폐기한다.

크기와 giant 규칙

각 조각은 다음 세 기준을 모두 만족해야 한다.

prod 변경 30파일 이하
prod 순증 +1500 이하
+1200에서 정지하여 아직 활성화하지 않은 helper 범위를 다음 조각으로 재배치

pr_cap_prod.py는 prod 30/+1500을 측정하며 inline tests·주석 등을 제외한다. +1200 정지선과 giant raw 증가는 별도로 확인한다. 
pr_cap_prod.py:1–23

고정 SHA에서 주의할 raw 파일 크기는 다음과 같다.

파일	raw 줄 수
src/services/turn_orchestrator.rs	7,243
src/services/claude.rs	4,411
src/services/codex.rs	3,209
discord/router/message_handler/intake_turn.rs	2,571
discord/router/intake_gate.rs	1,763
discord/gateway.rs	1,562
discord/router/intake_gate/queue_effects.rs	1,528
codex/tui_session_launch.rs	1,280
turn_orchestrator/pending_queue_persistence.rs	1,199
discord/router/intake_queue_transaction.rs	1,162
tui_prompt_dedupe/binding_context.rs	1,161

위 파일은 동일 조각 안에서 기존 코드를 자식 모듈로 추출하여 최종 raw 줄 수가 증가하지 않게 한다. queued_head.rs 930줄, inject.rs 893줄도 새 로직을 자식 모듈에 두어 1000줄을 넘기지 않는다.

다른 레인과의 순서
조각 1의 기존 분리 작업은 유지한다.
plain frame·receipt와 📥 수명 관련 파일은 #7270·#7269 결과를 기준으로 한다.
Herdr launch/provider 파일의 추출은 해당 레인 착지 뒤 수행한다.
queue bootstrap/복원 파일은 위임 롤백 레인의 reader 변경을 받은 뒤 수정한다.
리베이스 후 좌표·파일 수·raw delta를 다시 센다. 이 보고서의 좌표를 새 head에 그대로 복사하지 않는다.
Schema와 rollback 하한

Queue V2는 기존 parser가 version != 1로 거절한다. 하지만 history는 추가 version 필드만으로 구 reader를 거절할 수 없다. 구 reader가 구조적으로 읽지 못하는 V2 envelope를 사용하고, 새 reader는 V1/V2를 모두 읽는다. 
queued_head.rs:113–127, 
history.rs:37–40,105–130

V2가 운영 데이터에 기록된 뒤에는 조각 4를 통째로 임의 revert하지 않는다. queue/history V2 reader, retained 비실행 의미, source 재도착 차단, 복원 처리를 유지하는 호환 rollback을 준비한다. Snapshot 삭제나 배열 downgrade는 rollback 수단이 아니다.

8. 진입점·소비점 전수표

모든 좌표는 794e7123e 기준이다. 경로를 다음처럼 줄인다.

D/ = src/services/discord/
T/ = src/services/turn_orchestrator/
S/ = src/services/

표는 대상 Discord/TUI 입력의 생성·소유·전달·정산·복원·삭제 경계를 묶었다. 같은 파일의 단순 wrapper는 해당 owner 행에 포함한다.

8.1 접수·입력 상태 생성
진입점과 좌표	r2 계약

D/router/intake_gate.rs:300,604,1245–1289	Gateway 분류·voice 처리 뒤 live/busy 입력을 같은 owner에 접수

D/router/intake_gate/gate.rs:84–120,145–165	bot/provenance·실제 등록 명령·병합 정책 유지

D/router/intake_gate.rs:1151–1234,1356–1446,1494–1527	thread/dispatch·복구·restart·backlog 보관도 동일 custody로 수렴

D/router/intake_queue_transaction.rs:123–147,339–427	원 Intervention/source 생성, durable 접수 뒤 reaction/checkpoint

D/router/intake_gate/busy_inject.rs:81–96,145–171,212–257	blanket busy/Herdr 배제 제거, 실제 voice/upload 준비만 유지

D/router/intake_dispatch.rs:244,583–625,679–810	admission·owner routing 유지, 대상 TUI 최종 전달은 owner

D/router/intake_dispatch/queued.rs:71–119,226–249	병합 source·reply·uploads·voice 재구성 보존

D/router/message_handler/intake_turn/worker_entry.rs:49–76,116–165	claimed worker와 exact retry의 owner 우회 금지

D/catch_up/phase1_sweep.rs:625, 
phase2_sweep.rs:419, 
catch_up/api.rs:85–102	catch-up도 같은 source identity로 접수·중복 차단

D/queue_io/turn_admission.rs:266–301	durable enqueue 성공 뒤 pump wake
8.2 payload·session 준비
진입점과 좌표	r2 계약

D/router/message_handler/pre_admission_control.rs:35–121,181	제어 명령의 부수 효과·첨부 소유권 유지

D/router/message_handler/turn_context.rs:19–56	reply/context/author frame을 한 번 렌더

D/router/message_handler/attachments.rs:185–238, 
attachments/busy_inject.rs:5–27	local admission·materialize·owner 반환 뒤 파일 수명

D/router/message_handler/voice_announcement_route.rs:22–43,77–89, 
intake_turn/voice_intake.rs:9–38	승인·실발화자·durable claim carry-forward

D/router/message_handler/intake_turn.rs:326–363,428–595,889–947,1027–1137	DM/agent·session·role/provider·CWD·resume/options 준비 추출

D/prompt_builder/mod.rs:214–685	native instruction·profile·memory·manifest 보존

D/router/turn_start.rs:257–276,326–416, 
intake_turn.rs:1466–1534	recovery·recent context·feedback/WIP one-shot을 해당 attempt에 귀속
8.3 actor·저장·복원·제어 소비자
진입점과 좌표	r2 계약

T/queued_head.rs:53–127,237–300	V2 표현·adoption·버전 거절·publish readback

T/queued_head.rs:363–453	orphan 복구·원문 불변·stable source strip

T/queued_head.rs:456–506,650–812	head 보호·claim/drain 차단·예약·정산을 active/retained로 분리

T/queued_head/mend.rs:98–181	늦은 receipt는 exact attempt의 active/retained 위치에 적용

T/injected_inputs.rs:139,175–230,372–379	대상 Discord live는 공통 owner, 다른 origin 계약 유지

S/turn_orchestrator.rs:1916–1937,2084–2115,2142–2156	TryStartTurn·enqueue·TakeNextSoft가 retained를 barrier로 세지 않음

T/inbound_order.rs:96–108, 
queue_enqueue.rs:5–111	background backlog·merge·순서 판정에서 retained 제외

T/queue_cancellation.rs:75–112, 
clear_channel.rs:108–165	primary 기준 대기 취소·명시 clear와 이미 시도된 원문 보관 구분

T/checked_purge.rs:7–43,367–395, 
overflow.rs:18–30	purge에서 retained 보존, cap 30은 실행 대기에만 적용

T/pending_queue_persistence.rs:455–562,654,906	모든 저장·삭제·읽기 경계에서 metadata 보존

T/pending_queue_persistence/queued_recovery.rs:67–90, 
D/runtime_bootstrap/queued_recovery.rs:49–149,194,257–261,314–413	active·standby 복원, metadata-only adoption, queue→inflight→marker→placeholder 순서

T/dispatch_reservation.rs:448,476,533, 
S/turn_orchestrator.rs:2571–2581	legacy marker·restart drain도 같은 metadata 의미 유지

D/recovery_known_ids.rs, 
catch_up/phase1_sweep.rs:124–130	catch-up 이전에 retained/terminal source를 known으로 반영
8.4 effect·receipt·relay·wake·legacy 실행
진입점과 좌표	r2 계약

D/health/turn_deliver/inject.rs:119–132,312–443	기존 gate 유지 단계와 AnyTurn resolver 활성화를 분리

inject/queued_owner.rs:74–199	owner 준비·attempt·effect permit·정산

inject/queued_reconcile.rs:33–123,188–238,313–391	Claude 고정 해소, original/rendered frame·Source identity에 맞춘 reconcile

S/claude_tui/busy_inject/submit.rs:95–112, 
busy_inject.rs:183–250,624–679,775–869	Claude warm effect·composer 보호·receipt·불명 결과

S/codex_tui/busy_inject.rs:132–270,331–386, 
rollout.rs:170–239	idle Submit/busy Steer와 actual receipt 귀속

S/claude.rs:1772–1774,1923,2058–2082, 
codex/absence_launch.rs:332–422	cold submit와 AI 완료 대기 분리

S/claude/herdr_turn.rs:398–419,441–484,593–707, 
codex/herdr_turn.rs:213–246,363–441	Pending runtime 재사용·첫 입력·hold·Source 대기 분리

S/session_host/herdr_gate.rs:231–259,675–722	매 입력 exact execution/pane/root/provider 재검증

S/tui_input/human_frame/history.rs:144–171,251–391	planned source·수명·동일 frame 후보·native row 소유권

S/tui_prompt_dedupe/injected_steer.rs:50–153, 
observation.rs:369–399	실제 native turn과 원 source 연결, duplicate direct relay 방지

D/tui_prompt_relay/synthetic_start/claim.rs:54,236–256, 
anchor_completion.rs:155, 
S/tui_o/writer/input_facts/reactions.rs:25	native inflight·anchor·완료·입력 reaction 수명 유지

D/queue_io/queued_head_inject.rs:80–136, 
queue_dispatch/kickoff.rs:141–183,214,267–275	대상 조합의 주입 실패→일반 턴 fallback 제거

D/gateway.rs:730–837, 
queue_dispatch.rs:218–294	별도 completion-chain drain도 owner로 수렴

D/queue_io.rs:221–271, 
backstop.rs:173–205,372–458,572–661	enqueue·준비 해제·deadline·startup·event lag를 같은 pump로 연결

D/router/message_handler/provider_dispatch.rs:213–233,274–386	기존 effect permission 유지, 대상 밖 provider/shared caller 보존

D/outbound/source_registry.rs:122–123, 
S/message_outbox/identity.rs:32–46,107–135	보관 알림의 안정 identity·non-turn provenance
9. 불변식
ID	불변식
I1 — 접수 보존	durable 접수 전에는 성공 접수·checkpoint를 표시하지 않는다. 접수된 원문은 실행 대기·미확정 보관·명시 정산 중 하나에 있다.
I2 — effect 권한	사용자 payload effect는 해당 attempt의 durable MayHaveSent 뒤 발급한 단일 permit을 소비한다.
I3 — writer 단일성	같은 실제 target에는 물리 writer 하나만 있다. deadline으로 살아 있는 writer 보호를 해제하지 않는다.
I4 — 확인 대기 상한	receipt 부재로 유지하는 논리 head는 30초 deadline 처리에서 해제한다. 새 메시지·AI 종료를 조건으로 삼지 않는다.
I5 — 미확정 재전송 금지	재시작·timeout·source 변경·negative scan은 자동 재전송 근거가 아니다.
I6 — exact receipt	확인은 exact execution/source/frame/native event 소유권에 따른다. Source 존재·pane ready·턴 종료만으로 성공 처리하지 않는다.
I7 — runtime/source 구분	Source 부재를 runtime 부재로 바꾸지 않는다. 살아 있는 같은 Pending execution에 후속 입력을 보낼 수 있다.
I8 — late 결과 격리	A의 늦은 effect/receipt는 A만 정산한다. 현재 B의 head·payload·source·anchor를 바꾸지 않는다.
I9 — source 재도착	Discord 재도착 차단은 process generation에 의존하지 않는다. 병합된 모든 source를 검사한다. retained source는 존재하는 동안 재실행하지 않는다.
I10 — payload 귀속	reply·첨부·voice accepted claim·one-shot·원 발신자·역할/CWD가 live/복원에서 같은 입력에 귀속된다.
I11 — native lifecycle	입력 reservation과 AI native turn은 별개다. 실제 관찰 턴에 anchor·inflight·완료 처리를 한 번 연결한다.
I12 — 저장 형식	모르는 snapshot을 빈 큐로 읽거나 덮어쓰지 않는다. queue/history V2를 모르는 구 reader는 명시 거절한다.
I13 — 예외 보관	retained는 실행 queue cap·merge·drain·background backlog에 들어가지 않는다. generic clear/purge로 원문이 사라지지 않는다.
I14 — history 진행성	종료된 등록 슬롯은 반환한다. 과거 미확정 동일 본문이나 lifetime 4096개가 후속 입력을 영구 차단하지 않는다.

성공 terminal의 재도착 방지는 정해진 보존 기간 안의 계약이다. 무기한 exactly-once 전달은 주장하지 않는다.

10. 수용 시험과 필수 뮤턴트

시험은 높은 층의 mock 호출 수만 세지 않는다. source ID + attempt nonce별 실제 content-bearing 경계—argv launch, paste/submit, Herdr input—를 센다.

legacy effect 0은 지원 대상 Discord/TUI 요청에 한정하며, owner가 허가한 cold launch는 정상 effect다.

ID	수용 시험·관측값	반드시 RED인 뮤턴트
A01	네 provider/host 조합 × fresh/resume/warm idle/warm busy. 실제 payload 전달 1회, owner 밖 legacy effect 0회	Codex queued 차단 복원, not_busy 복원, 대상 legacy fallback 복원
A02	A not_observed, receipt 영원히 없음, B/C 준비. deadline 뒤 A 원문 유지, B/C 각각 1회, Unknown 때문에 dispatch claim이 막히지 않음	expiry 제거, A 자동 재전송, A 제거와 함께 B/C 삭제
A03	A의 물리 작업은 계속 살아 있는데 deadline 경과. 논리 head는 분리되지만 A/B의 물리 쓰기 중첩 0회	timeout만으로 writer/transition guard 해제
A04	A retained 뒤 B가 head인 동안 A의 늦은 성공·실패·receipt 도착. A만 변경, 추가 effect 0회	현재 head settle 호출 유지
A05	Unknown 29초 뒤 restart. 기존 deadline 복원. retained-only 파일도 복원·알림·동일 source 차단	restart마다 30초 재설정, metadata-only 채널 누락
A06	원문 접수·Reserved·history planned·MayHaveSent·effect·receipt·정산 각 절단점 복원. 불명 attempt 재전송 0회	ledger 부재를 NotSent로 판정, MayHaveSent 전 effect
A07	Cold Claude 파일 없음, Codex rollout 없음에서 첫 전달·receipt 정상. Codex tmux는 argv 1회·추가 paste 0회	TranscriptUnavailable 선행 조건 복원, argv 후 첫 paste 추가
A08	Codex Herdr는 Source가 첫 입력 후에만 생성되는 fixture에서 진행	Source를 기다린 뒤 첫 입력하도록 순서 변경
A09	Cold A 이후 Source를 끝내 만들지 않음. A 보관 뒤 준비된 같은 runtime에 B 1회, 총 create 1회. 재시작 후도 동일	ReadyWithoutSource 제거, no binding→Absent, writer 안 60초 Source wait 유지
A10	A의 AI 완료를 막아둔 동안 B 전달. launch/first-submit은 끝났고 출력만 계속 중	기존 fresh-turn/read-reply helper 전체를 owner에서 기다림
A11	다른 execution·clear 이후 다른 Source·같은 path의 교체 파일로 A를 확인하지 않음	execution/watermark/dev/ino 검사 제거, Source만으로 Injected
A12	같은 본문 A retained + 새 source B. B effect 1회. 겹치는 receipt는 임의 귀속하지 않음	동일 본문 등록 veto 복원, retained 후보 삭제, first-claimer-wins
A13	같은 native source에서 5000회 이상 입력. source 종료·segment rotation·GC 없이 4096 이후에도 진행. 과거 row replay가 anchor/placeholder를 늘리지 않음	lifetime CAP 복원, recognition/collision evidence 삭제
A14	Claude enqueue 확인→슬롯 반환→같은 본문 B→늦은 A command. B를 A의 command로 확인하지 않음	enqueue/command 소유권 증거 조기 삭제
A15	reply+merged sources+첨부 2개+attachment-only+voice의 저장·복원 결과 동등. owner 반환 뒤 첨부 존재. voice claim 뒤 NotSent/restart에도 원 입력 보존	reply 누락, 이중 렌더, 임시 directory 조기 drop, accepted voice 상태 제거
A16	Gateway·catch-up·boot restore·claimed worker·exact retry·completion-chain 각각 동일 owner에 도달	worker 또는 gateway::dispatch_queued_turn만 legacy로 남김
A17	human guild·허용 bot·DM·dispatch thread에서 role/provider/CWD/worktree 보존. operational 알림 재섭취 0회	allowed bot prefix 차단, role/CWD 유실, 알림 provenance 등록 누락
A18	64 KiB 경계·초과·영구 invalid 입력은 명시 처분과 알림, 뒤 B 진행	영구 오류를 무한 NotSent 재시도로 처리
A19	modal/draft/upload/voice/host/transition 준비 해제 뒤 새 메시지·턴 종료 없이 전달	wake를 turn completion에만 연결, NoCandidate로 필요한 task 종료
A20	retain publish가 rename 뒤 오류를 반환해도 readback으로 회복. 원문 한 벌, A 추가 effect 0회	metadata mismatch를 영구 오류로만 처리
A21	settle commit과 secondary confirmed 사이 crash 뒤 같은 Discord ID를 새 generation으로 재수신. 추가 effect 0회. merged secondary source도 동일	generation 필수 비교 복원, primary만 terminal 처리
A22	오래 보관된 입력을 오늘 정산하면 새 terminal이 즉시 만료되지 않음. retained에는 TTL 미적용	snowflake 생성시각 TTL, 기간 내 count cap 조기 삭제
A23	notice enqueue 실패·Discord 실패에도 B 진행. enqueue 성공 후 상태 저장 전 crash도 동일 outbox identity	알림 성공을 입력 진행 조건으로 사용
A24	retained+실행 대기 cap30, clear/purge/restart drain, old reader와 호환 rollback에서 원문 보존·재실행 0회	retained를 items에 합침, V2를 []로 처리, history 추가 version 필드만 사용
A25	Codex idle와 busy→새 턴에서 원 source anchor·placeholder·완료 처리 각 1회. Claude resume _ 유무에도 실제 payload 1회, 사용자 literal _ 보존	빈 target+Handed만 적용, _를 receipt로 인정, _ 전역 삭제
A26	즉시 준비 입력의 queue snapshot publish 3회와 접수 직후 crash 보존. 준비 대기는 먼저 durable 접수	durable 접수를 미루어 쓰기 횟수만 맞춤
A27	Remote profile은 기존 지원/거절 결과 유지·로컬 pane effect 0. 대상 밖 provider/headless caller 정상 유지	remote→local fallthrough, shared dispatch 전체 삭제

조각 5 제거 조건: 위 대상 조합과 ingress 시험이 owner 밖 전달을 차단한 상태에서 통과해야 한다. 일반 fallback이 첫 입력을 대신 보내 시험이 PASS하는 상태는 수용하지 않는다.

11. DECISION_NEEDED

추가 사용자 결정은 필요 없다. 이미 확정한 “6조각”, “즉시 입력도 durable 기록”, “입력이 멈추지 않는다”를 기준으로 (b)를 r2 권고 계약으로 채택했다. 사용자는 확인되지 않은 입력의 원문을 잃지 않고 다음 입력을 계속 보낼 수 있다. 그 대가로 해당 입력은 자동 재전송되지 않으며, 직접 다시 보내면 중복될 가능성이 있다는 안내를 받는다. 30초는 확인 대기의 기본 상한으로 정하고 새 운영 flag는 추가하지 않는다.

이 설계는 고정 SHA의 소스와 호출 경계를 읽어 작성했다. 첨부 리뷰의 운영 로그 집계는 이번 조사에서 재검증하지 않았고, 빌드·시험·운영 재현도 실행하지 않았다. 위 시험과 뮤턴트는 각 구현 조각의 수용 조건이다.

VERDICT: DESIGN_READY

ChatGPT는 실수할 수 있습니다. 중요한 정보는 확인하세요.
최신 응답




Pro
응답이 완료되었습니다
활동
서브에이전트
3개 사용
출력
아직 결과물이 없습니다