PLAN_READY a525e9be9886d9dd8f85b5c7f1a0c03176b377c6

# S4 계획 r3 보고

## 산출·상태

- 새 계획: `plan-s4-r3.md`. r2 대비 변경표는 계획 §0, 상세 불변식은 §1.3~1.6, 실제 추가 진입점은 §2.1, rollback production gate는 §3.1, 파일별 예산은 §4, 리뷰 항목별 수용은 §6/§8.1이다.
- HEAD **`a525e9be9886d9dd8f85b5c7f1a0c03176b377c6`** 유지, worktree clean. 이번 레그는 문서와 읽기 증거만 작성했다. source/생성 등록 목록 수정·fetch/merge/checkout/commit/push·GitHub/DAG 쓰기0. 빌드·시험·뮤턴트·Clippy·prepush 실행0. 이전 foreground 작업은 이미 종료했음을 먼저 확인했고 중복 실행하지 않았다.
- `plan-s4-r2.md`, `report-s4-replan.md`, 입력 리뷰 원문은 hash가 그대로다. r2의246/0/0·static PASS는 **같은 기반의 과거 증거**이며 이번 계획의 새 기능/rollback 수용 PASS로 세지 않는다.
- WebGPT pro r2 리뷰(P1 1·P2 3)와 병합 이월(P2 5)을 먼저 읽었다. 계획 리뷰 **2/2**이며 첫13:5x 시간 초과는 판정이 없다. r3에 새 CLEAN 판정이 있다고 주장하지 않는다. 이후 지적·미실측은 명명한 구현 수용으로 넘기고, gate 미충족을 배포 허가로 취급하지 않는다.

## P1 범위 보정

사용자10-24 #5983 추가 기준인 “TUI 채널 HTTP API 턴 시작·routine·headless inbound도 Discord 입력과 같은 단일 경로”를 반영했다. origin과 **actual runtime / placement / recovery 권위**를 분리한다. local TUI의 busy·idle·cold는 owner 접수이며 진짜 recovery·실제 Process/non-TUI만 기존 계약이다. remote TUI는 기존 placement가 지정한 실행 노드 owner에 전달하고 local fallback하지 않는다.

아래 source는 고정 main `28eee2f87f`와 L에서 **바이트 동일**했다. source에 이미 single-owner 경로가 있다는 뜻은 아니며, 현재 early409/legacy claim/provider effect가 남았다는 확인이다.

| 실제 현재 경계 | 계획 수리·검증 |
|---|---|
| `src/server/routes/agents.rs:722,825,843,874` | agent/override/author 검증은 유지하고 local TUI는 external busy409 전에4g1 adapter. durable 후 status=accepted, actual start와 구분 |
| `src/server/routes/agents_turn_target.rs:15,138,168` | target/provider 검증과 voice 공통 wrapper 재사용, busy를 owner 제외 근거로 쓰지 않음 |
| `src/services/discord/health/headless_turn.rs:16,62,100,143,246,295,351` | DM·owner/turn channel·reservation·label 보존, 모든 실제 inbound 공통 classifier |
| `src/services/discord/router/message_handler/headless_turn.rs:21,160,890,1035` 및 entrypoints | LocalTui는 Immediate claim 전 원문 custody, owner 밖 일반 provider effect0. actual Process/recovery의 기존 순서·권한은 유지 |
| `src/services/routines/agent_executor/launch.rs:130,162,183,206,262` |4g2에서 run intent/ticket→owner custody→accepted projection. Accepted는 started/Consumed·fresh owned session이 아님 |
| `src/services/routines/agent_executor.rs:145,316,397,650,818,1171` | primary/retry/fallback/poll의 bool/!started 소비를 typed exhaustive 결과로 교체. Accepted/Pending이면 complete/checkpoint/next_due 진전0 |
| `src/services/scheduled_messages.rs:212,434` | 같은 !started→Consumed consumer를 추가 발견해4g2에 포함. launch-committed/Accepted 유지·재launch0 |
| `src/services/discord/health/turn_deliver.rs:441` | #7402 착지 뒤 Accepted→Started 오인을 same-ticket Queued 의미로 고침. 별도 재enqueue0 |

4g는 **4g1 adapter·origin 분류**, **4g2 routine/scheduled PG 소비**로 나눴고4f에서 기존 Gateway/catch-up/boot/worker/completion과 함께 활성화한다. 실제 Process·recovery는 대조 시험으로 고정한다. author·binding·placement·consent·ReplayStart·capacity를 owner permit으로 대체하지 않는다. cold의 pane/Source 부재도 Process 예외가 아니다.

Routine은 기존 run_id/attempt_kind/turn_id·actual execution_mode·원문·checkpoint/next_due 후보를 owner original에 보존한다. 기존 columns의 pending intent를 먼저 exact하게 기록하고 custody 뒤 Accepted projection을 하며, 절단점은 same-ticket으로 복구한다. 새 schema/ACK/journal0. Accepted/Pending을 generic pre-spawn error 또는 Consumed로 합치지 않는다. 실제 AI 완료 관찰만 succeeded를 만들며 local Submitted 자체도 routine 완료가 아니다.

## 리뷰 항목별 처분

| 항목 | 처리 위치·수용 조건 |
|---|---|
| Web P1-1 |4g1/4g2→4f. 실제 axum start·PG routine·direct headless 각각 busy/idle/cold, 최소36 runtime 셀: admission1/content1/owner 밖 effect0. modal restart same-ticket, durable 전 ACK/완료/checkpoint0, 권한 잘못되면 content0. early409·adapter 우회·Accepted→Consumed 각각 mutant RED |
| Web P2-1 |4a outcome +4e1 owned_turn_start. old no-content terminal 유지→새 managed birth/nonce·one-shot cap, 같은 ticket/자리. load실패0→fault해제→success1, old callback/stale snapshot 영향0. old cap 재사용/Waiting 강등/후속 권한 미발급 RED |
| Web P2-2 |4b collision child +4e2. live slot 회수와 native 후보 해제 독립. A/B 같은 Source/frame local완료 뒤 row1을 양방향 순서로 조회해 선착순 귀속0, B content는 이미1·추가 입력0. local=confirmed mutant RED |
| Web P2-3 |4f0 production R SHA/patchset/build/target/binary SHA256 고정. cfg(test) 없는 F→R boot/catch-up/재도착/restart/clear/purge→F pending 복구. 원문/history/source fence 보존·추가content0. recognition test-only/quarantine=empty RED. manifest/실행 증거 없으면 **4f 활성화0** |
| merge P2-1 |**#7402 착지 뒤4a**, owner-only if-shell physical pin. UI 사후 판독/geometry guard 아님. ready 전 교체 C-s/paste/Enter0, C-s 뒤 실패는 Stash, content 뒤는 WriteUncertain. 조건 제거 RED |
| merge P2-2 |**#7402 착지 뒤4a**, ready(C-s 전 검증) / start(C-s 뒤 commit) 분리. ready 실패 C-s0, post-stash fault Failed/Stash·NotSentProof0. 자동복원 감시·잠금0 |
| merge P2-3 |**#7402 착지 뒤4a 시험**, deterministic transport trace로 callback 시각과 no-stash 대조. renderer body 부재만 믿지 않음. start를 load/C-s 앞으로·ready를 C-s 뒤로 옮기면 RED |
| merge P2-4 |**이미 a525e9be98 수리**, 코드 재수리0. `queued_head/compat_tests.rs:203,237`, `logs/s4r2-targets.json.attempt2`의223/1과 최종 `s4r2-targets.json`의246/0/0·선택 ID/로그가 증거. 이후 queue 전체 필터와 serde 참조 ID⊆실제 선택 ID 대조 |
| merge P2-5(a) |4b exact abandoned에 A withdraw no-op 호출처 포함. positive no-content만 해제, post-stash/불명은 보존 |
| merge P2-5(b) |**#7402 착지 뒤4e2**, owner scope의 V1 진단 receipt thread 생략 권고. V1 read/write0, unscoped 진단 main 동등, submit/다음 입력을 진단에 종속0 |
| merge P2-5(c) |최종 PR head native rc0/PREPUSH_OK·binary/head 고정 뒤 코디네이터 push. 중간 static/target을 native 완료로 부르지 않음 |

이 표의 GREEN/RED는 **미래 구현 수용**이다. 이번 문서 작성에서 해당 시험을 돌렸거나 지적을 코드로 닫았다고 주장하지 않는다. P2를 P1로 재승격하지 않았다.

## 단위별 크기·FILES 순서

| 단위 | r2 prod파일/순증 | r3 prod파일/순증 | r3 여유 포함 |
|---|---:|---:|---:|
| 4a | 16 / +461 | 17 / +668 | 20 / +788 |
| 4b | 14 / +599 | 15 / +694 | 18 / +814 |
| 4c | 20 / +609 | 20 / +609 | 23 / +729 |
| 4d | 22 / +964 | 22 / +964 | 25 / +1084 |
| 4e1 | 18 / +516 | 18 / +566 | 21 / +686 |
| 4e2 | 18 / +476 | 19 / +482 | 22 / +602 |
| 4g1 | 신규 | 15 / +620 | 18 / +740 |
| 4g2 | 신규 | 14 / +523 | 17 / +643 |
| 4f0 | 신규 | 11 / +192 | 14 / +312 |
| 4f | 22 / +6 | 29 / +20 | 30 / +80 |
| 4h | 4 / +260 | 4 / +260 | 7 / +380 |

모두 공식 상수 prod30파일/순+1500 안이고 여유 포함 +1200 미만이다. 이것은 **계획 산술**이며 actual diff cap PASS가 아니다. 파일별 M/L 현재 raw·giant 여부·예상 순증은 계획 §4와 `logs/s4r3-file-budget.json`에 있다. giant의 예상 delta는 모두0 이하로 두었다. 신규 자식이 부모 raw 증가 권한을 주지 않는다.

- #7402가 A/legacy 파일을 먼저 소유한다. callback/pin·진단·LivePorts의 실제 seam은 **착지 뒤** 고정한다. 삭제된 실행기/새 scanner 복제0.
- #7330은 실제 recovery/withheld/ReplayStart 권위 소유를 유지한다. #7272가 흡수한 TUI inbound adapter와 혼동하지 않고 router/headless/boot 부모는 착지/소유 인계 뒤 편집한다.
- #7317은 CI 정책/selector를 소유한다. 시험 등록은 해당 main 공식 생성/등록만 하며 퇴역 목록을 되살리지 않는다.
- routine/store/scheduled의 새 계획 파일은 구현 전 FILES 소유 인계 대상이며 이번 레그에서 수정하지 않았다. giant agents/headless/routine executor/store/scheduled는 관련 본문을 child로 추출·감소한다.
- 4f가29파일이라 여유1파일이다. 추가 gate 해제 파일이 생기면 코드 전에 PLAN_READY로 준비 단위를 더 나누되 ingress의 partial activation으로 피하지 않는다.4f0은 read-only compatibility만 production으로 올리며 writable V2 ingress는4f다.

## 증거 목록

- HEAD/코드 무변경: `logs/s4r3-read-facts.json`, `logs/s4r3-plan-validation.json`. source reader는 git/show·rg·파일 읽기만 사용했다. 검증한 source 좌표 244개는 **경로·행 범위 확인**이며 실행/call graph 완전성 증명이 아니다.
- 입력 리뷰 SHA256: `7a674bd914a864bf1c7713894e0af8706dd8bfa400f49e6bb1836aeec76cbd27`; 병합 이월 SHA256: `9de21eeb212690c8031f976d8af26c09fc12a4e7759ff173f63fffa804003e24`. r2와 이전 보고서 hash도 read-facts에 고정했다.
- 파일·산술 증거: `logs/s4r3-file-budget.json`(각 prod 파일 M/L raw·giant·순증·단위 여유). 공식 cap의30/+1500을 완화하지 않았다.
- 기존 golden 수리 증거: `logs/s4r2-golden-foundation.json`, `logs/s4r2-targets.json.attempt2`, `logs/s4r2-targets.json`, `logs/s4r2-targets.log`, `report-s4-replan.md`. 최종 binary SHA `f09e6a8524e56e85357226720dbb60a2f71456cba6f2d76c1b6d50552207bfe7`는 r2 merge 증거이고 r3 새 구현 증거가 아니다.
- 이번 빌드/시험/뮤턴트/PG/native/static 실행 수 **0**. 계획2/2 작업이므로 수행하지 않았다. 실제 production rollback R/F 바이너리도 아직 없으며 4f0/4f gate에 남겼다.

## 리뷰 집중 항목·남은 수용

| 집중 항목 | 막을 계약·남은 반례 |
|---|---|
| 경계 시각 | ready/start·C-s·actual content·local Submitted·AI 완료를 구분. LoadFailed successor의 실제 권한 발급·stale writer 차단은4a/4e1 수용 |
| 동시성 | same physical writer1, owner pin 서버 조건·ready preflight·same-ticket PG 절단점. Source/UI를 기다리는 metadata callback cycle0, lease 강제 해제0 |
| 옛 형식 복원 | old-reader unit만으로 하한을 선언하지 않음.4f0 R production의 source fence/metadata-only/재도착·control·forward 복구 양수 검증 |
| 문서만의 guard | 36셀의 실제 HTTP/PG/direct entry와 owner 밖 effect count, 모든 typed consumer·같은 birth와ticket 구분을 구현 수용으로 고정 |
| 잘못된0/PASS | 선택된 실제 ID/serde subset·binary SHA·고유 mutant·복원 byte hash 확인. 기존246 PASS·이번 문서 산술을 새 기능 PASS로 바꾸지 않음 |

U01~U35는 유지했고 U36~U42에 mode/recovery·routine 절단점·consumer·production rollback harness·A 최종 API·successor·native collision 실측을 추가했다. 시행 시 지원되지 않는 seam/소유 충돌은 FILES/DESIGN_DELTA로 고정한다. 새 cleanup/kill·운영 flag/ACK/journal/DB schema를 추가하는 우회는 없다.

## 진행 중

이 레그가 띄운 검증 프로세스는 없다. r3 뒤 남는 지적은 계획 §6/§8.1의 각 구현 단위 수용으로 넘긴다. 운영 적용이나 추가 설계 리뷰 완료를 주장하지 않는다.

PLAN_READY a525e9be9886d9dd8f85b5c7f1a0c03176b377c6
