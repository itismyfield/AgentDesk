[코디네이터 — S4 계획 r2 WebGPT pro 리뷰 ISSUES(P1 1건, P2 3건). 같은 세션이 계획 r3을 쓴다. 코드는 쓰지 않는다. 계획 리뷰 라운드 2/2다. r1 13:5x는 회수 전에 시간 초과로 끝나 판정이 없었다. r3 뒤에 남는 지적은 구현 단위의 수용 시험으로 넘긴다]

리뷰 답변: `/home/adk/adk/lane-runs/i7272/review-s4-plan-r2-webgpt.answer.md`. 병합 델타 리뷰 이월: `/home/adk/adk/lane-runs/i7272/carry-s4m-review.md`(P2 5건). 둘 다 먼저 읽는다.

## 고칠 것 (`plan-s4-r3.md`, 옛 판은 남긴다)
1. **P1-1 — TUI 채널로 들어오는 HTTP `/turn/start`·routine·headless inbound를 단일 owner 경로에 넣는다.**
   - 근거는 사용자 10-24 범위 추가(#5983 흡수)다. 예외는 진짜 recovery와 TUI가 없는 provider뿐이다.
   - 4f 전에 준비 단위 하나(예: 4g)를 둔다. 이 단위에서 세 유입의 adapter를 만든다. 활성화는 4f에서 함께 한다.
   - §2 전수표와 FILES에 다음 경계를 실제 좌표로 더한다.
     - `server/routes/agents.rs::start_agent_turn`(busy 때 409를 조기 반환하는 지점)
     - `agents_turn_target.rs`
     - `health/headless_turn.rs`
     - `message_handler/headless_turn.rs`
     - `routines/agent_executor/launch.rs`
   - 입력 origin과 실제 실행 방식을 나눈다.
     - local TUI 대상 inbound는 공통 owner에 접수한다.
     - 실제 Process 실행과 recovery는 지금 계약을 유지한다.
   - author·placement·consent·ReplayStart·capacity를 보존한다.
   - routine run/attempt metadata는 durable custody에 넘긴 뒤에 결과를 반환한다.
   - Accepted를 Consumed로 매핑하지 않는다. routine 결과 소비 계약(`launch.rs:206-216`, `agent_executor.rs:145-185`)을 명시한다.
   - 수용 시험과 뮤턴트는 리뷰 권고대로 둔다. 대상은 axum `/turn/start`, PG routine launch, 직접 headless entry × busy·idle·cold다.
2. **P2-1** 확정 LoadFailed 뒤 다음 시도의 managed 권한 발급 계약을 넣는다.
3. **P2-2** local Submitted를 history settled로 투영할 때 늦게 온 같은 본문 row의 충돌 증거를 보존한다.
4. **P2-3** rollback 하한을 실제 production 바이너리로 검증하는 조건을 고정한다.
5. **병합 델타 리뷰 P2 5건**을 단위에 배치한다.
   - P2-1 A 경로 physical pin TOCTOU
   - P2-2 C-s 뒤 callback 실패가 초안을 stash에 둔 채 LoadFailed가 되는 문제
   - P2-3 idle owner 순서 단언
   - P2-4는 이미 수리됐다. 증거 경로만 적는다.
   - P2-5 위생
   - #7386 A안 쪽 파일(미니 #7402가 쥐고 있음)을 고쳐야 하는 항목은 "#7402 착지 뒤"로 표시한다.
- 단위별 크기를 다시 추정하고 겹침표를 갱신한다. 리뷰 단위 상한은 prod 30파일 / 순 +1500(스킬 `scripts/pr_cap_prod.py` 상수)이고, PLAN선은 +1200이다.

## 산출
- 계획 `/home/adk/adk/lane-runs/i7272/plan-s4-r3.md`와 r2 대비 변경표
- 보고서 `/home/adk/adk/lane-runs/i7272/report-s4-plan-r3.md`. 마지막 줄 `PLAN_READY <HEAD SHA>`. HEAD는 그대로 `a525e9be98`이다.
## 금지 (모든 레인·리뷰·설계 공통 — 코디네이터 규칙)
- `~/.adk/release/config/agentdesk.yaml`(백업·사본 포함)·`launchd.env`는 **읽기·grep·출력·쓰기 모두 금지**다. 운영 설정값이 필요하면 [U]로 남기고 코디네이터에 묻는다.
- 운영 서버(:8791)·운영 DB·포트 15432·`~/.adk/release/runtime` 쓰기 금지. `AgentDesk-*` tmux, user herdr, `~/.local/bin/herdr` 조작 금지.
- push·PR·GitHub 쓰기·DAG 쓰기는 코디네이터만 한다.
- 리뷰어는 코드·저장소를 수정하지 않는다. 읽기 전용이다.

## 심각도 눈금 (먼저 읽는다)
AgentDesk는 운영자 한 명이 Tailscale 안에서 쓰는 소규모 개인 운영 시스템이다. 다수 고객용 상업 서비스 기준으로 판정하지 않는다.
- P0·P1(머지 차단)은 두 경우뿐이다.
  (1) 실제 운영 경로에서 재현 가능한 유실·손상·중단. 재현 조건을 파일:줄과 함께 구체적으로 적는다.
  (2) 이 이슈의 수용 기준 위반. 위반한 기준 문장을 인용한다.
- 다음은 P2 후속이며 머지를 막지 않는다: 공격자·위장(cgroup·이름·신호 위조), 시계 역행, 장시간 정지·스톨,
  협조적 CI·운영자 전제 밖 입력, 동시에 드문 사건 둘 이상이 겹쳐야 하는 경로, 더 정밀한 대안이 있다는 지적.
- 외부 효과가 없는 코드(flag off·dormant·shadow·측정 도구)는 P0만 차단한다. 잘못된 PASS 가능성은 fail-closed 가드 제안으로 적는다.
- 이전 라운드에서 P2로 판정된 항목을 다시 P1로 올리지 않는다. 새 근거가 있으면 그 근거를 적는다.
- 지적마다 "고치지 않으면 운영에서 무슨 일이 생기는가"를 한 줄로 적는다. 답이 가정형이면 P2다.
- 지적마다 개선 방향을 함께 적는다. 지적만 하고 끝내지 않는다(P2 포함). 각 지적에 다음 세 가지를 적는다.
  (1) 어디를(파일:줄·함수) 어떻게 바꾸면 해소되는지.
  (2) 그 수정을 증명할 시험이나 뮤턴트.
  (3) 대안이 여럿이면 권고안 하나와 그 이유.
  설계 수준 변경이 필요하면 그 사실과 최소 계약을 적는다.
- 현재 방향보다 아예 새로운 방향이 더 낫다고 보면 제안해도 좋다. 제안은 지적과 따로 "새 방향 제안" 절에 적는다.
  그 절에는 요지, 현재 방향 대비 이득(단순성·안전성·비용), 전환 비용과 위험을 적는다.
  권고도 하나 고른다: 지금 갈아탈지, 아니면 현재 변경을 착지시키고 후속으로 갈지.
  새 방향 제안 자체는 머지를 막지 않는다. 현재 변경에 P0·P1이 없으면 VERDICT는 CLEAN일 수 있다.
- 마지막 `VERDICT:` 줄은 P0·P1 이 있을 때만 `ISSUES` 다. P2 만 있으면 `VERDICT: CLEAN` 으로 끝내고 P2 는 본문에 남긴다.

