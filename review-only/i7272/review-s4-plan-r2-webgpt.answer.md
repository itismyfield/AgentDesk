요약

P0 0건, P1 1건, P2 3건입니다. 차단 사유는 4f의 운영 연결 범위에서 TUI로 들어오는 HTTP 턴 시작·routine·headless 입력이 빠진 점입니다.

A안의 핵심 계약은 계획에 일관되게 반영돼 있습니다. 사전 판독과 paste/Enter 거래, 원문 보관과 전송 권한, 입력 성공과 AI 응답 관찰을 구분했고, 사후 UI 확인이나 자동 잠금을 다른 이름으로 되살리는 지시는 발견하지 못했습니다. 다만 #7272에 후행으로 추가된 #5983의 범위가 이번 재계획에 반영되지 않았습니다. 
A안 계약 · 
범위 추가 결정

검토 기준은 문서 a44ee61e5140…, 소스 a525e9be9886…입니다. 아래는 해당 고정 SHA의 문서·소스를 대조한 설계 리뷰이며, 빌드·시험을 재실행한 결과는 아닙니다.

검토 질문	판단
1. A안 위의 단위 일관성	대체로 일관됩니다. 원문 custody·one-shot permit·물리 writer lease는 유지하고, receipt와 UI 사후 확인을 입력 진행 조건에서 제거했습니다. 다만 local Submitted를 history에 투영할 때의 의미 분리는 P2-2로 보강해야 합니다.
2. 진입점·소비점 및 원자 연결	P1. 명명한 Gateway·catch-up·boot·worker·completion 경계는 잡았지만, 사용자 결정으로 추가된 HTTP /turn/start·routine·TUI headless inbound가 원자 연결 대상에서 빠졌습니다.
3. FreshTurn·LoadFailed 결정	FreshTurn을 실제 create/recreate 결과로 정하는 계약은 맞습니다. 확정 LoadFailed의 terminal 처리도 방향이 맞지만, 그 뒤 재시도 권한을 발급하는 lifecycle과 시험이 부족합니다(P2-1).
4. Rollback	V2를 보존하는 호환 바이너리를 하한으로 삼는 계약은 타당합니다. 실제 사용할 바이너리와 production 모드의 복원·재도착 시험은 아직 미확정입니다(P2-3).
5. 크기·순서	현재 표의 파일 수·순증 산술은 맞습니다. #7402, Codex A2, #7330의 선행 조건도 명시돼 있습니다. P1의 누락 범위를 추가한 뒤에는 예산을 다시 계산해야 합니다.

크기 표를 다시 합산한 추가 여유 3파일/+120 포함 값은 다음과 같습니다. 모두 현재 계획의 30파일/+1500 상한과 +1200 PLAN선 아래입니다. 이는 예상치 확인이며 실제 diff의 cap 통과 판정은 아닙니다. 
계획 §4

단위	여유 포함 prod 파일	여유 포함 순증
4a	19	+581
4b	17	+719
4c	23	+729
4d	25	+1084
4e1	21	+636
4e2	21	+596
4f	25	+126
4h	7	+380

#7402 뒤 공통 A API를 고정하고, Codex A2 인계 전 4d/4f를 활성화하지 않으며, #7330의 router·boot 소유 인계 뒤 편집하는 순서는 유지하면 됩니다. 
계획 §5

지적
P0

없음.

P1
P1-1. TUI HTTP 턴 시작·routine·headless 입력을 단일 owner 연결에서 제외해 후행 사용자 수용 기준을 위반한다

위반한 기준

#7272의 10-24 범위 추가 결정은 대상에 TUI 채널로 들어오는 HTTP API 턴 시작, routine 디스패치, headless inbound를 명시하고 다음을 요구합니다.

“Discord 입력과 같은 단일 경로(항상 즉시 주입, 필요할 때만 보관)를 탄다.”

같은 결정은 기존 design-r2가 이 caller들을 기존 경로에 남겼다는 사실까지 지목하면서, S2 이후 조각에 설계 델타를 두라고 했습니다. 예외는 진짜 복구용 경로와 TUI가 없는 provider입니다. 따라서 원래 설계의 headless 제외 문구를 그대로 승계할 수 없습니다. 
사용자 범위 결정

계획의 누락

plan-s4-r2.md:58과 §2 F1의 :126은 headless·외부 origin을 기존 경로에 남깁니다.
4f의 수정 파일과 actual ingress 시험은 Gateway·catch-up·boot·worker·completion을 중심으로 구성돼 있습니다.
4h는 /turn/deliver의 origin dedupe를 다루며, /turn/start의 접수·시작 계약이나 routine/headless의 owner 연결을 다루지 않습니다.

단순히 전수표에서 파일명 하나를 생략한 문제가 아닙니다. 범위 문구, 수정 파일, 수용 시험이 함께 이 유입들을 제외합니다. 
계획:58 · 
F1:126 · 
4f·4h · 
HTTP 델타와 시험 범위

실제 운영 경로

HTTP /turn/start는 busy TUI에서 owner에 도달하기 전에 409를 반환합니다.
src/server/routes/agents.rs::start_agent_turn()의 825–833행이 external_turn_conflict() 결과를 즉시 반환합니다. 해당 함수는 외부 TUI가 채널을 점유하거나 TUI turn 상태가 busy/unknown이면 409를 만듭니다. 이 요청은 V2 원문 접수와 A안 주입에 도달하지 않습니다.

agents.rs:825–833 · 
agents_turn_target.rs:138–154

선행 409를 지나도 기존 Immediate mailbox claim 경로가 남습니다.
HTTP는 headless 시작 API를 호출하고, headless entry는 claim_reserved_headless_turn()을 호출합니다. 기본 reservation은 TurnAdmissionOrder::Immediate이며 mailbox claim 실패는 다시 Conflict가 됩니다. HTTP는 이를 409로 반환합니다.

agents.rs:843–883 · 
headless_turn.rs:160–166 · 
turn_start.rs:171–215

routine의 headless API 호출은 실제 non-TUI 실행을 뜻하지 않습니다.
routine의 실행 모드는 특정 fresh 조건에서만 Process이고 나머지는 Tmux입니다. 이 Tmux 실행도 start_reserved_headless_agent_turn_*()을 거쳐, 최종적으로 provider_dispatch::execute()에 prompt·tmux target·host를 직접 전달합니다. 함수 이름으로 headless 전체를 제외하면 사용자 결정의 TUI inbound가 빠집니다.

execution.rs:11–17 · 
launch.rs:162–195 · 
headless_turn.rs:1035–1063

고치지 않으면 운영에서: 같은 TUI에 보내는 입력인데 Discord는 단일 owner를 타고, HTTP 턴 시작은 409로 거절되며, routine/headless는 기존 mailbox 선점·일반 provider 실행 경로를 계속 사용합니다. 현재 다섯 ingress 시험이 모두 통과해도 이 수용 기준은 충족되지 않습니다.

개선 방향 — 설계 수준 델타 필요

권고는 4f 전에 별도 준비 단위로 이 세 유입의 adapter를 추가하고, 활성화는 4f에 함께 포함하는 것입니다.

§2와 FILES에 agents.rs::start_agent_turn, agents_turn_target.rs, health/headless_turn.rs, message_handler/headless_turn.rs, routines/agent_executor/launch.rs의 실제 경계를 추가합니다.
입력 origin과 실제 실행 방식을 분리합니다. local TUI 대상의 실제 inbound는 공통 owner에 접수하고, 진짜 recovery와 실제 Process 실행은 기존 계약을 유지합니다.
기존 author·placement·consent·ReplayStart·capacity를 보존합니다. 원문과 routine run/attempt metadata를 durable custody에 인계한 뒤 접수 결과를 반환해야 합니다.
HTTP의 accepted 결과와 routine의 결과 소비 계약을 명시합니다. 특히 Accepted를 기존 Consumed로 매핑하면 안 됩니다. 현재 routine은 status == "started" 여부를 보고, 성공 반환이지만 started가 아니면 run을 완료 처리합니다. 
launch.rs:206–216 · 
agent_executor.rs:145–185

증명할 시험·뮤턴트

실제 axum /turn/start, PG routine launch, 직접 headless entry 각각에 대해 busy/idle/cold를 통과시켜 다음을 확인해야 합니다.

입력별 owner 접수 1회, content 1회, owner 밖 일반 provider effect 0회.
modal 보관 후 재시작해도 원문·동일 ticket 유지.
durable 접수 전 성공 ACK·routine 완료·checkpoint 변경 0회.
잘못되거나 만료된 replay 권한에서 content 0회.
HTTP busy→409 조기 반환 복원, 각 adapter 우회, Accepted→Consumed 변환을 개별 뮤턴트로 두고 해당 단언에서 RED.

권고 이유: 현재 4h만 착지해도 /turn/start와 routine/headless는 바뀌지 않습니다. 이 범위를 4f 활성화 전에 포함해야 사용자 결정의 단일 경로가 완성됩니다. 이전 P2의 재승격이 아니라, 후행 사용자 결정에 대한 별도 수용 기준 위반입니다.

P2
P2-1. 확정 LoadFailed를 terminal로 만든 뒤, 다음 시도의 managed 권한을 발급하는 계약이 빠져 있다

계획 67행은 실패한 attempt를 끝내고 원문을 같은 큐 자리에서 retryable하게 유지하는 방식을 권고합니다. 71행은 managed terminal의 rank를 MayHaveSubmitted보다 높게 두고 Waiting으로 되돌리지 않도록 합니다. 이 두 결정은 각각 타당하지만, 종료된 attempt에서 다음 attempt로 넘어가는 권한 경계가 명시돼 있지 않습니다. 
계획:67–71

현재 API를 그대로 재사용하면 다음 제약에 걸립니다.

managed_submission.rs::carry_forward()는 같은 birth의 강한 phase를 유지합니다.
mark_may_have_submitted()는 Waiting만 수락합니다.
boundary installer도 non-Waiting record에는 새 capability를 발급하지 않습니다.
기존 followup_requeue::requeue_if_needed() 역시 non-Waiting record나 이미 idle-stash에 들어간 capability를 재큐하지 않습니다.

managed_submission.rs:66–98 · 
351–406 · 
followup_requeue.rs:107–117

고치지 않으면 운영에서: terminal 추가만 하고 기존 재시도 lifecycle을 사용하면, 원문은 남아 있어도 장애 해제 뒤 재시도 권한을 얻지 못할 수 있습니다.

다만 같은 admission ticket을 유지한다는 것이 같은 managed birth를 재사용한다는 뜻은 아닙니다. 계획에 안전하게 구현할 여지가 있으므로 영구 중단을 확정해 P1로 올리지는 않습니다.

개선 방향·권고

4a의 managed_submission/outcome.rs와 4e1의 owned_turn_start 사이에 다음 최소 계약을 추가하는 것을 권고합니다.

old attempt의 확정 미전송 terminal을 저장하고 old live capability를 끝냅니다.
원문·admission ticket·queue position은 유지합니다.
재시도에는 구분되는 managed birth/attempt와 새 one-shot capability를 발급합니다.
old callback·old snapshot은 successor를 변경하지 못합니다.

Waiting으로 단순 강등하는 방법보다 이 방식이 권고입니다. 실패한 시도의 증거를 유지하면서 새 전송 권한을 분리할 수 있습니다.

증명할 시험·뮤턴트

현재 §8의 시험은 실패 직후 phase와 원문 보존까지입니다. 
계획:475

여기에 첫 load 실패 → content 0 → 장애 해제 → 같은 ticket·같은 위치의 새 attempt 성공 1회를 추가해야 합니다. 그 사이 old callback과 stale snapshot을 넣어도 successor 변화가 없어야 합니다. old capability 재사용, terminal→Waiting 강등, 실패 후 재시도 미발급을 각각 뮤턴트로 두면 됩니다.

P2-2. local Submitted를 history의 기존 settled로 그대로 투영하면, 늦은 동일본문 row의 충돌 증거가 빠질 수 있다

r2는 native receipt 없이 local Submitted로 입력을 정산하면서 history의 echo·row 분류를 유지합니다. 이 변경에서는 입력 거래의 종료와 native row를 식별한 상태가 서로 달라집니다. 
계획:35–59

현재 foundation의 history/v2.rs는 다음처럼 연결돼 있습니다.

Registry::project()는 queue terminal identity를 발견하면 passive 상태를 "settled"로 만듭니다.
Snapshot::entries()는 passive 중 "retained"만 미확정 후보로 표시합니다.
receipt()는 동일본문 충돌 검사에서 그 후보 표시와 native completion event를 사용합니다.

history/v2.rs:392–455 · 
173–180 · 
538–555

이 projection을 그대로 쓰면 native echo가 아직 없는 A도 local Submitted 직후 충돌 후보에서 빠집니다. 같은 Source·같은 frame의 B까지 들어온 뒤 늦은 row 하나가 나타나면, 먼저 조회한 attempt가 그 row를 가져갈 여지가 생깁니다. 원 설계는 슬롯 반환과 별개로 동일본문 충돌 증거와 row 귀속을 보존하도록 요구합니다. 
설계 r2:378–404

고치지 않으면 운영에서: 기존 projection을 그대로 연결할 경우, 늦은 native row가 A/B 중 먼저 조회된 입력에 임의 귀속될 수 있습니다. 실제 응답 유실까지 입증한 것은 아니므로 P2입니다.

개선 방향·권고

4b/4e2의 Registry::project()와 receipt()에서 다음을 분리해야 합니다.

live slot 반환: local Submitted 또는 확정 미전송 때 즉시 수행.
충돌 후보 해제: 실제 native event로 구분됐거나 exact no-content proof로 abandoned된 경우에 수행.

권고는 기존 native event·abandoned 증거를 사용해 후보 여부를 판단하는 것입니다. 새 wire enum을 반드시 추가할 필요는 없습니다. receipt 대기나 자동 입력 잠금도 필요하지 않습니다.

증명할 시험·뮤턴트

같은 Source·같은 frame의 A/B를 둘 다 local Submitted로 정산한 뒤, native row 하나만 늦게 제공합니다. A→B와 B→A 순서로 조회해도 선착순 귀속이 발생하지 않아야 합니다. B의 입력은 이미 1회 끝나 있어야 하고, receipt 대기·추가 입력은 0회여야 합니다. local Submitted를 native-confirmed로 취급해 충돌 후보에서 빼는 뮤턴트가 이 시험에서 RED여야 합니다.

P2-3. rollback 하한은 타당하지만, 실제 production 바이너리로 검증하는 조건을 고정해야 한다

§3과 U25는 V2 원문·source fence·retained 비실행 의미를 보존하는 호환 바이너리만 rollback 대상으로 허용합니다. 이 계약은 맞습니다. 다만 실제 SHA·허용 patchset·바이너리와 실행 절차는 아직 미정입니다. 
계획:190–192 · 
U25:511

현재 L에는 V2 파일을 legacy writer가 덮어쓰지 못하게 하는 실제 보호가 있습니다. 그러나 파일 보존과 rollback 후 정상적인 source 재도착 차단은 같은 증명이 아닙니다.

queue legacy recovery는 V2를 quarantine한 뒤 실행 queue 복원에서 제외합니다.
history의 V2 recognition 경로 일부는 현재 #[cfg(test)] 안에 있습니다.
따라서 같은 crate의 old-reader 시험만으로 production rollback 가능성을 증명할 수는 없습니다.

queued_head/compat.rs:20–38 · 
queued_recovery.rs:33–49 · 
history.rs:254–263 · 
history/compat.rs:48–56

고치지 않으면 운영에서: 부적합한 구판을 호환 판으로 오인하면 V2 파일은 남아 있어도 boot의 source fence나 native echo 분류가 빠질 수 있습니다. 계획이 그런 구판의 배포를 허용한 것은 아니므로 P2로 둡니다.

개선 방향·권고

§3/U25에 4f 활성화 전에 rollback 기준 SHA·patchset·production 바이너리 identity를 고정한다는 게이트를 추가하는 것을 권고합니다. 현재 방향대로 호환 바이너리를 준비하면 되며, V2→V1 변환기나 snapshot 삭제 절차를 추가할 이유는 없습니다.

증명할 시험·뮤턴트

A24를 다음 순서의 실제 바이너리 시험으로 고정해야 합니다.

새 writer가 pending·retained·terminal·metadata-only를 포함한 V2 데이터를 생성합니다.
cfg(test)가 없는 지정 rollback 바이너리로 boot·catch-up·동일 source 재도착·restart/clear/purge를 통과시킵니다.
원문 및 retained/history 증거 보존, 동일 source의 추가 content 0회, 거절된 legacy 쓰기의 파일 무변경을 확인합니다.
다시 forward 바이너리로 올라가 pending 처리가 복구되는지 확인합니다.

V2 recognition을 테스트에서만 켜거나, quarantine을 빈 queue처럼 취급하는 뮤턴트는 이 production 바이너리 시험에서 RED여야 합니다.

VERDICT: ISSUES

ChatGPT는 실수할 수 있습니다. 중요한 정보는 확인하세요.
최신 응답




추론 수준
Pro
응답이 완료되었습니다
활동
서브에이전트
3개 사용
출력
아직 결과물이 없습니다