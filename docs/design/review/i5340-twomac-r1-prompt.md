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

---

# #5340 i5340_twomac 설계 요청 — 묶음 3회차: 맥북·맥미니 두 노드에서 같은 시험 채널에 위임을 켜고 S3 경로를 실측한다

## 0. 읽는 법
- 저장소 `itismyfield/AgentDesk`. 소스는 **GitHub 고정 SHA `de739c72d7856a34dbbbde6a0b54f867b698005b`(origin/main)** 로 읽는다. 접근할 수 없을 때만 작업 디렉터리에서 `git show de739c72d:<path>`로 읽는다. 아래 file:line은 이 SHA 기준이다.
- 두 노드에 배포된 바이너리는 `ccd737074`(18:30 관측)다. `ccd737074..de739c72d`는 4커밋: #7027 scope-cut(inflight save/delete/restart·boot restore의 휴면 가드 제거, `startup_reclaim.rs`·`inflight/removal/*`), #6858 의존성 갱신(Discord·legacy TLS 제거), #7024·#6992 시험 전용. 실측 바이너리를 어느 SHA로 할지도 판단 대상이다.
- 이 요청은 **설계만** 한다. 운영 설정·DB·서버를 바꾸지 않는다. cargo·시험 실행은 없다.
- 맥 지시: 설계 전제는 착지된 s3fence(A #7013 `f1010eec7`, B #7018 `ef1c56259`, C #7023 `ccd737074`) 기준으로 고정한다. 새 원장·custody·registry·재시도 대장은 만들지 않는다(삭제 결정 유지).

## 1. 목표
DAG 노드 `i5340_twomac` = "bulk run 3 (two-Mac, delegation on for the same test channel)". 선행: s3act(#6989 adopt 배선, #7025 시험), s3fence(A·B·C 착지, 수용 `--require full` rc=0), bulk1(단일 노드 Herdr 첫 gate 하네스, #6909·#6911·#6929 등). 후행: `i5340_ui`, `i5340_s7real`.

증명하려는 것(설계가 문장을 다듬어 확정한다):
1. 실제 운영 두 노드(gateway 1 + 다른 노드 1)에서 bulk1 시험 채널을 **gateway → 다른 노드로 위임**하고, 그 노드가 adopt·lease로 holder가 된 뒤 Discord 일반 입력이 holder 노드의 Herdr pane에서 턴으로 실행되고 holder의 managed writer가 답을 정확히 한 번 POST한다.
2. **reclaim 왕복**: gateway의 `channel-home reclaim` → holder drain(turn·queue·owed·POST 재검사) → Reclaimed → gateway가 옛 writer join 뒤 row 제거 → gateway writer·readiness 복귀 → 새 입력이 gateway에서 정확히 한 번 배달된다.
3. **재시작 경계**: holder·gateway 각각의 fenced restart·SIGTERM에서 writer join과 다음 boot의 역할·writer 재개가 실제 프로세스에서 성립한다(s3fence H6-2a·H6-3b·H6-6b의 실기기판).
4. 위 전 과정에서 다른 채널(위임하지 않은 tmux·Herdr 채널)에 동작 변화가 없다.
5. 런북 [O-U-3] "Mac 두 대 실제 전달·후속 대화·단절 중 완료 후 회수"를 이 채널 하나로 닫을 수 있는 범위를 정한다.

## 2. 운영 전제 — 코드 좌표·기본값·live 관측

### 2.1 설정·플래그 (값 파일은 읽지 않았다. 아래 "live"는 health GET에서 본 것뿐이다)
| 전제 | 정의 file:line | 기본값 | 적용 시점 | live (2026-10-16 18:30 KST) |
|---|---|---|---|---|
| 위임 스위치 `runtime.channel_home_delegation_enabled` | `src/config/runtime_settings.rs:138`; CLI 거절 `src/cli/channel_home.rs:70-72`; boot `src/services/discord/runtime_bootstrap.rs:526-541,580-624`; 가용성 `src/services/cluster/home_availability.rs:94-104`; capability 광고 `src/services/cluster/session_routing.rs:44-53` | None(=off) | CLI는 실행 시 config 읽기, 위임 home 시작은 **boot 때만** | [U] 미니 detail에 `channel_homes` 키 없음(등록된 home 0). 값은 미확인 |
| intake routing mode | `src/config.rs:1017-1022`(Disabled/Observe/Enforce), `src/config/runtime_profile.rs:293-312` | enabled=false, mode=observe | boot·env override | 두 노드 모두 `enabled=true, mode=enforce, source=yaml, env_override=null, owner_authority_allowlist_size=0` |
| 위임 채널 라우팅 조건 | `src/services/cluster/intake_router_hook.rs:121-215`: home row가 있으면 다른 배치보다 우선, `Worker+holder`가 아니면 `route_to_home`(:165) — `InTransition` 차단(:172), Enforce가 아니면 차단(:175), owner_authority opt-in이면 `DualAuthority`(:178) | — | 매 입력 | 조건 충족 상태(Enforce, allowlist 0) |
| cluster 역할 | `src/config/cluster_role.rs:7-12`(Hub/Runner/Auto, 기본 Auto); `runtime_profile`(Full/Runner) `src/config/runtime_profile.rs:8-12,58-66`; `gateway_preferred_instance_id` `:207` | Auto / Full / None | boot | 맥미니(127.0.0.1, 호스트명 Mac-mini.local): `cluster_standby=true`, modules.gateway=true(Full 프로파일 Standby). 100.71.169.10: `cluster_standby=false`(gateway). 이 주소가 맥북이라는 것은 [U](detail 403) |
| provider runtime 역할 분기 | `runtime_bootstrap.rs:338-363`(RestWorker), `:400-425`(Standby), `:431-456`(Gateway, gateway writer `:455`) | — | boot | 위와 같음 |
| "turns restored" 표식 | `runtime_bootstrap.rs:511-522` `turns_restored`: Gateway=`reconcile_done`(agent 없는 utility gateway 제외), RestWorker=true(=reconcile_done), **Standby=false 고정**; Standby port는 `queued_recovery.rs:291-317` `standby_restore`(자체 flag, 채널 복구 성공 시만 true) | — | boot | — |
| Herdr endpoint·채널 매핑 | `src/config/session_hosts.rs:35-58`(`endpoints.<key>.execution_node`·socket·home·session, `channels.<id>→key`), `:268`,`:283`; 위임 후보 `runtime_bootstrap.rs:557-578` `selected_herdr_channels`(O writer 선택 ∧ Claude/Codex TUI ∧ endpoint 존재), `:626-631` `herdr_runs_here`(endpoint의 execution_node == 이 노드) | 빈 map | **boot 고정**(재시작 필요) | 미니 detail: `configured_channels=["1509350490461180105"]`, endpoint `mac-book` → node `mac-book-release`, 미니에서 `local=false`, admission `open`, `restart_required=false` |
| Herdr 턴 스위치 | `runtime_settings.rs:124,128` `herdr_turn_enabled`·`herdr_codex_turn_enabled`, 읽기 `src/services/turn_host.rs:199-226` | off | live | [U] |
| O writer 선택 | `src/services/tui_o/shadow/tap.rs:26-38` `tui_o.writer.channels`/`all_tui`; 선택 계산 `src/services/tui_o/channel_policy.rs:62-70`; gateway writer `runtime_bootstrap/o_writer_host.rs:317`, 위임 writer `:391` `spawn_delegated`, `:361` `home_writers` | 빈 집합 | boot | 미니: `tui_output_gateway_channels=["claude:standby:complete:20","codex:standby:complete:3"]`, degraded `provider:*:tui_output_requires_gateway` |
| home 행 CAS | `src/db/o_channel_homes.rs`: `delegate:253`(INSERT releasing, 행 없을 때만), `finish_release:305`, `finish_reclaim:315`, `adopt:326`(released∧target=$2∧epoch), `begin_reclaim:347`(**worker만**), `remove_reclaimed:368`, `renew:392`, `force_orphan:422`(**holder NOT NULL** `:433`∧renewed_at 경과) | — | — | — |
| 운영 CLI | `src/cli/channel_home.rs:23-39` `status`/`delegate <ch> --provider --to`/`reclaim <ch>`/`force <ch>`, `reclaim:173-190`(state=Worker 아니면 거절), `force:192-201` | — | 실행 시 | — |
| watch·drain | `src/services/cluster/channel_home_boot.rs:396` `start`, `:604-686` `watch`(Released∧target=local∧`port.restored()`→adopt `:645-658`; Reclaimed→`give_back` `:659-661`; holder=local→lease·`restore_settled`·`drain_round` `:662-676`), `RENEW_EVERY=5s`(`channel_home.rs:22`), `FORCE_AFTER=HOLD_FOR+180s`(`:27`); drain `channel_home_drain.rs:133` `drain_round`, `:235` `finish_return` | — | — | — |
| 가용성 판정 | `home_availability.rs:55-63` preflight는 `MissingPool`·`MissingInstanceId`만 생산. 실패 시 그 provider의 위임 가능 Herdr 채널 전부 보류(`turn_host.rs:119`, `intake_router_hook.rs:126-132`, `tui_o/writer/host.rs:502`, `deliver.rs:161`) | — | boot | [U] |
| health 노출 | `/api/health/detail` `herdr`·`channel_homes`(`src/server/routes/health_api/runtime_profile.rs:4-12`, `channel_home.rs:799` `health`) | — | — | 미니 detail 200(loopback), 100.71.169.10 detail 403 |
| 하네스 readiness | `scripts/e2e/tui_relay/herdr.py:86-105`: endpoint가 local이어야 하고 **모든 configured endpoint가 local**이어야 통과 | — | — | 현재 매핑(endpoint 1개 on mac-book)이면 미니에서 돌리면 실패 |

### 2.2 live health 요약 (`GET /api/health`, 18:30:26~27 KST)
- 미니(127.0.0.1): status=degraded, `cluster_standby=true`, degraded_reasons = `provider:{claude,codex}:gateway_standby`, `provider:{claude,codex}:tui_output_requires_gateway`. 배포 head `ccd737074`, startup doctor failed 1/warned 6.
- 100.71.169.10: status=degraded, `cluster_standby=false`, degraded_reasons = `provider:claude:deferred_hooks_backlog:1`, `provider:claude:pending_queue_depth:12`, `provider:codex:deferred_hooks_backlog:1`, `provider:codex:pending_queue_depth:1`, `tui_o:released:1536866287983132752`, `tui_o:released:1559670200419352696`. 배포 head `ccd737074`, doctor failed 1/warned 4.
- `tui_o:released:<ch>`는 writer alarm `Released`(`src/services/tui_o/alarm.rs:207`)이며 home 행 Released와 같은 것인지는 [U].

## 3. 이미 증명된 것과 남은 것
### 3.1 s3fence 수용(head `ccd737074`, 단일 머신·throwaway PG·프로세스 하네스)
- 등록 시험 36개, 22행 PASS(H6-1·H6-2a·H6-2b·H6-3a·H6-3b·H6-4·H6-5·H6-6a·H6-6b·H7-P*·H7-U·H7-B·H7-R·H7-G*·H7-O·H8-G·H9-D·H9-W). required 뮤턴트 29개(env 16 + 소스 패치 13) 전부 KILLED, H6-3a 4개 KILLED 후 required 승격(review ref `s3fence-d3-r1`, 미착지). `check_herdr_s3_d4b_manifest.py --require full` rc=0.
- 이 증거의 한계: 실제 두 Mac·실제 Discord gateway·실제 Herdr pane이 아니다. s3_real `th6_released_target_adopts_through_its_boot_watch_pg`는 **부모가 restored 신호를 직접 준다**(`src/services/cluster/intake_worker/s3_real_tests/holder_pg_tests.rs:976-992`). H6-2a evidence_scope "Reclaimed CAS is not reached", H6-3b "CAS-stale race is not covered", H6-6b "two process generations deciding roles through a real PG lease … not covered".
### 3.2 수용 측정 §4 이월(후속 노드 `i5340_s3fence_p2` 후보, 미착수)
a1 복구 cancel_token identity(`queued_recovery.rs`) · a2 전부 걸러진 queue·marker 처분 · b1 PG 대기 판정 · b2 s3_real adopt 시험의 sleep 대기 · c1 Standby가 `finish_reclaim` CAS로 Reclaimed 도달 · c2 inflight row 복구 단언 · c3 다른 채널 무변화·`restored_ignores_queue_or_marker` · c4 marker-only · c5 손상 3종·`restore_read_error_as_empty` · c6 Held receipt · c7 복구 중 crash · c8 relay·boot 복구 경합 · d1 반환 CAS Stale · d2 두 provider runtime 순차 boot · d3 후보 없는 반환 · e1 실제 PG lease 2세대 역할 결정 · e2 실제 OS SIGTERM · f HTTP health 경계.
### 3.3 bulk1(단일 노드 Herdr 첫 gate)
- 하네스: `scripts/e2e/run_tui_relay.py --cell claude-herdr`, E-1→E-3→E-2→E-15→E-50, judge `scripts/e2e/post_deploy_turn_smoke.py`. 런북 `docs/agent-maintenance/herdr-runbook.md` §6.1(:391-431), §6.2 재시작 관측, §2.3 다중 노드(:122-132).
- 알려진 함정(`analysis-herdr-smoke-1654`, 10-13): 기존 채널의 증명 없는 회전 이력으로 `!clear`가 `rotation_unsettled(59)`로 계속 거절될 수 있다(10-13 분석 당시 좌표 `src/services/tui_o/writer/rotation/readers.rs:539-590`, `src/services/tui_o/writer/historical_hops.rs:60-88`, 이 SHA에서 재확인 필요). 텍스트 `!clear` 무응답은 #6929로 수리. bulk1 실측 결과의 최종 판정 값은 이 요청 작성자가 확인하지 못했다 [U].

## 4. 작성자가 코드 읽기로 찾은 위험 전제 (설계가 확인·반박하라)
- **R1 Standby 대상은 adopt하지 못한다.** 위임 대상의 watch는 holder≠local 분기에서 `port.restored()`가 true일 때만 adopt한다(`channel_home_boot.rs:645-658`). Standby port의 restored는 `standby_restore`가 만든 false flag이고 `restore_settled`(holder=local 분기 `:665`, 정의 `:107-131`)에서만 true가 된다. 따라서 Full 프로파일 Standby인 현재 미니가 대상이면 행이 Released에 머문다. RestWorker(`runtime_profile: runner`)는 restored=`reconcile_done`이라 adopt한다(`a_rest_worker_adopts_its_released_home_only_once_its_turns_are_restored_pg`). s3fence 설계 [U6]이 남긴 항목과 같은 것으로 보인다.
- **R2 Released·Orphaned는 CLI로 빠져나올 길이 없다.** `reclaim`은 Worker만(`cli/channel_home.rs:176`), `force_orphan`은 holder NOT NULL만(`o_channel_homes.rs:433`) 받는다. Released(holder NULL)·Orphaned 행은 운영 CLI로 지울 수 없다. 실측 중 중단·롤백 경로를 어떻게 확보할지 정해야 한다(DB 직접 조작은 사용자 승인 사항).
- **R3 Herdr endpoint는 한 노드에 고정이다.** 위임 후보는 `herdr_runs_here`(endpoint execution_node == 이 노드)여야 한다. 지금 시험 채널 endpoint는 gateway 쪽 `mac-book`이다. "같은 시험 채널"을 쓰려면 (a) endpoint를 미니로 옮기거나(양 노드 재시작, 기존 mac-book pane·Bound row 처리, 같은 채널 legacy 복귀 기능 없음 — 런북 §3, `i5340_legacy_return`), (b) gateway를 미니로 옮기고 맥북을 holder로 쓰는 방법이 있다. (b)는 모든 tmux 세션이 gateway 노드에서 돈다는 전제(`runtime_profile.rs:200-206` 주석)와 충돌할 수 있다.
- **R4 하네스는 mixed endpoint를 거절한다**(`herdr.py:98-101`). endpoint를 두 노드에 두면 어느 노드에서도 첫 gate readiness가 통과하지 않는다. 하네스 변경 없이 판정할 관측 신호를 정하거나, 최소 하네스 변경을 설계하라.
- **R5 home 등록은 boot 때만이다.** 스위치 켜기·delegate 뒤 양쪽 재시작 순서가 필요하다(후보 watch가 있으면 delegate 뒤 재시작 없이 진행되는지 확인).
- **R6 preflight 실패는 그 provider의 Herdr 채널 전부를 보류한다**(`home_availability.rs:125-150`). instance_id·PG 준비를 켜기 전에 확인할 방법이 필요하다.

## 5. 요구 산출물
1. **전제 고정표**: 위 2.1 각 행에 대해 이번 실측에 필요한 값(두 노드 각각), 현재값을 확인할 읽기 전용 방법, 변경 필요 여부, 적용 시점(live·재시작). R1~R6 각각 사실 여부 판정과 근거 file:line.
2. **토폴로지 결정안**: 대상 노드의 역할(Standby/RestWorker)·endpoint 위치·gateway 위치·시험 채널(같은 채널 vs 새 전용 채널)의 선택지와 권고 1개. R1이 사실이면 (i) 미니를 runner로 띄우기, (ii) 코드 수정(Standby target이 Released에서 채널 복구를 먼저 돌리고 restored를 세우는 최소 조각 — 변경 file·크기·시험), (iii) 기타 중 권고와 이유. 코드 수정이면 이 노드의 선행 조각으로 분리해 적는다.
3. **실측 시나리오 목록**(최소: 위임 활성화·adopt, 위임 중 턴·후속 입력·`!clear`·stop, gateway 재시작, holder 재시작, holder SIGTERM·fenced restart, reclaim 왕복, 다른 채널 무변화 대조, [O-U-3] 단절 중 완료 후 회수). 각 시나리오마다:
   - 진입 조건(설정·row 상태·두 노드 역할·이전 시나리오 결과)
   - 조작(누가 어느 노드에서 어떤 명령, Discord 입력 방법)
   - 관측 신호(`channel-home status` 행·epoch·open_intake, `/api/health/detail`의 `channel_homes`·`herdr`·degraded, Discord 도착 대조, 로그 키 — 각각 코드 좌표)
   - 판정(PASS/FAIL 조건, 시간 상한)
   - 중단 기준(무엇이 보이면 즉시 멈추나)
   - 롤백(그 시점 row 상태에서 가능한 복귀 절차, R2를 고려)
4. **운영 설정 변경 목록과 승인 지점**: 변경 항목·노드·순서·재시작 여부, 각각 사용자 결정 필요 여부(전역 영향·다른 채널 영향·되돌림 비용 기준)와 코디네이터 재량 항목 구분.
5. **증거 형식**: 시나리오별 기록 파일 구성(관측 시각·head SHA·row 스냅샷·health 스냅샷·Discord 메시지 id), 판정 스크립트가 필요하면 최소 범위.
6. **s3fence 증거와의 대응표**: 각 시나리오가 3.1의 어느 행·한계를 실기기로 닫는지, 3.2 이월 중 어떤 것을 실측으로 대체·보완하는지(대체 불가면 그렇게 적는다).
7. **위험·[U] 목록**: 확인 방법·담당(코디네이터·맥·사용자)과 함께.

## 6. 제약
- 새 테이블·migration·원장·재시도 대장·custody를 제안하지 않는다. 필요한 코드가 있으면 기존 접점의 최소 조각(file·예상 순증, 리뷰 단위 상한 prod 30파일·순증 +1500)으로 적는다. 1,000줄 넘는 prod 파일(예: `runtime_bootstrap.rs` 1444줄)은 줄 증가 0 규칙이 있으니 새 배선은 자식 모듈에 둔다.
- 운영 실측은 소규모 개인 운영 시스템 기준이다. 공격자·시계 역행 같은 가정형 엣지는 P2 후속으로 분리한다.
- 판정은 실제 관측으로 한다. exit 0·`known_gap`·`not_applicable`을 PASS로 세지 않는다.
- 맥 소유 경로(`runtime_bootstrap*`, `channel_home*`, `tui_o/**`, `session_host/**`, `turn_finalizer/**`)를 바꾸는 제안은 "맥 승인 대상"으로 표시한다.

마지막 줄은 `VERDICT: DESIGN_READY` 또는 `VERDICT: DESIGN_BLOCKED <사유 한 줄>`로 끝낸다.
