PLAN_READY 05d89aaa6a2d047c41f6ba44a45a85991206e52d B를 관측(+약950)·적용(+약510)으로 분리; 모든 O POST·local retry·complete snapshot을 먼저 증명하고 7일 자동 퇴역으로 backlog 처리

# #7308 B 계획 r2 — 관측 계약과 자동 퇴역 권한 분리

- 기준: `05d89aaa6a2d047c41f6ba44a45a85991206e52d` = 이 worktree의 HEAD와 origin/main. 브랜치 `fix/i7308-b-observe`, worktree `/Users/itismyfield/.adk/release/worktrees/i7308-b`.
- 코드 좌표는 모두 이 기준이다. `src/services/` 아래 경로는 아래 표에서 그 접두사를 생략한다. [고정 소스](https://github.com/itismyfield/AgentDesk/tree/05d89aaa6a2d047c41f6ba44a45a85991206e52d).
- 재개 시 이전 A background PID 90723/90919는 모두 종료된 것을 확인했다. 이번 실행은 코드·git 읽기, GitHub issue 읽기, 문서를 확인한 운영 GET, 레인 산출물 작성만 수행했다. 빌드·시험·코드 수정·커밋·push·운영 변경은 없다.
- 입력: [#7308](https://github.com/itismyfield/AgentDesk/issues/7308), [#7268](https://github.com/itismyfield/AgentDesk/issues/7268), [#5071](https://github.com/itismyfield/AgentDesk/issues/5071), `review-plan-r1-webgpt.out.md:335-650`와 코디네이터의 A/B 분할 결정. 다음 단계는 코디네이터의 **WebGPT pro 독립 설계 리뷰**다. 이 문서는 구현 승인이나 시험 PASS가 아니다.

## 1. 판정과 범위

**B-관측만으로 이 채널의 기존 backlog에 의한 degraded를 해소할 수 없다. B-적용이 필요하다.** 관측 단계는 Codex의 자동 Apply와 pure judge의 퇴역 차단을 유지한다. B-적용도 7일 도달만으로 퇴역하지 않는다. complete 관측, 후보별 retention 근거, idle·미정산 output 부재, 기존 source 보존 게이트가 모두 필요하다. 운영 현물의 해당 근거는 공개 GET으로 확인할 수 없어 회복 날짜나 현재 퇴역 가능 개수를 약속하지 않는다.

권고하는 착지 순서는 두 리뷰 단위다.

1. **B-관측:** 모든 O writer의 POST lifetime counter, local retry를 포함한 owed, provider가 명시된 legacy/Herdr Codex context, complete execution/held-source 관측, 그 관측의 동일성을 production sweep→Inspect→actor 진단에 연결한다. Codex AutomaticApply는 거부한다. 새 flag·타이머·운영자 명령·복구 권한은 없다.
2. **B-적용:** B-관측이 `Complete`를 증명한 대상에만 Codex native dialogue age와 source별 retention 근거를 붙이고, 기존 Inspect→scope→AutomaticApply→atomic audit/retire 경로를 연다. 원본 transcript 삭제·프로세스 kill·배달 재시도·배달 frontier 전진 권한은 추가하지 않는다.

A는 [#7316](https://github.com/itismyfield/AgentDesk/pull/7316), squash `3302ae0c88`에 이미 착지했다. `tui_o/writer/rotation.rs:492-499`, `tui_o/writer/rotation/codex_end.rs:22-79,143` 및 `tui_o/store/codex_end.rs:221-280,394-395`가 exact End+Seal의 내구 읽기→rotation proof→기존 reader 경로를 연결한다. B는 이를 재구현하지 않는다. **과거 End-only·무증명 source에 Seal을 소급 생성하지 않는다.** B의 retention audit를 A의 provider 종료 proof로 바꾸거나 `Successor.proof`에 기록하지 않는다.

이슈의 “이미 쌓인 20개를 어떻게 정리할지 정한다”는 **#7268의 자동 7일 정책**으로 답한다. 최신/held source, young, active, pending, unread/unknown은 보존한다. 제거된 operator Inspect/Apply HTTP·CLI를 복원하지 않는다. “20”은 이슈의 역사적 backlog 수치이며 이번 GET으로 얻은 현재 수치가 아니다.

## 2. 운영 사실 — GET이 증명한 것과 증명하지 못한 것

2026-10-24 **12:03:34~12:05:17 KST / 03:03:34~03:05:17 UTC**, MacBook의 loopback `127.0.0.1:8791`에서 확인했다. 먼저 GET `/api/docs`, 이어 canonical docs `/api/docs/observability/health`, `/api/docs/integrations/discord`, `/api/docs/observability/monitoring`을 읽었다. 호출한 상태 API는 다음 다섯 GET뿐이다. 운영 DB·설정·runtime 파일을 직접 열지 않았다.

| 공개/loopback GET | 확인한 사실 | 한계 |
|---|---|---|
| `/api/health` | `status=degraded`, `server_up=true`, `fully_recovered=true`; 사유는 `tui_o:too_many_readers:1509350778043895902` 하나. deployed head는 기준 SHA와 동일 | reader 수, cursor 목록, retention 적격성 없음 |
| `/api/health/detail` | target mailbox `provider=codex`, agent status idle, queue 0, tmux present/watcher attached. Herdr configured channels에는 `1509350490461180105`만 있고 target은 없음 | 이 idle/queue는 retirement의 actor obligations·strict mailbox idle·held-set을 대체하지 않음 |
| `/api/discord/channels/1509350778043895902` | name=`adk-codex-tui-e2e`, topic=`Codex tui runtime E2E worker` | Discord metadata만 확인 |
| `/api/discord/bindings` | agentId=`adk-codex-tui-e2e`, provider=`codex`, `discord_channel_cdx`가 target, source=`config` | API가 반환한 binding이며 실제 설정 파일을 읽은 것이 아님 |
| `/api/channels/1509350778043895902/watcher-state` | `tmux_session=AgentDesk-codex-adk-codex-tui-e2e`, alive/attached=true; bound_session_id=`01a16bcc-1c30-70b1-b843-be7941250ea2`; bound_output_path는 아래 relay 경로; `unread_bytes=null` | relay binding 경로이지 O cursor의 native source 목록/EOF 증거가 아님. null은 미측정이며 0이 아님 |

GET이 반환한 relay 경로는 `/Users/itismyfield/.adk/release/runtime/sessions/agentdesk-1780a4491117-unknown-host-AgentDesk-codex-adk-codex-tui-e2e.jsonl`이다. 이 파일은 열지 않았다. 위 사실을 합하면 **현재 공개 표면상 legacy tmux Codex E2E 경로**다. hidden setting이나 provider의 raw native rollout 경로까지 확인한 것은 아니다.

**[U-운영] 현재 reader 수는 미확인이다.** 확인한 health/detail/watcher-state/docs 응답에는 reading count나 cursor census가 없다. 활성 reason은 경보 상태이지 정확한 현재 census가 아니다. main의 `MAX_READERS=3`은 `tui_o/writer/rotation.rs:38`, 경보 발생·성공 tend 뒤 reconciliation은 `tui_o/writer/rotation/readers.rs:364-369,621-626`, 해제는 `tui_o/alarm/sink.rs:12-15`다. B-관측에서 실제 actor의 reading count와 unretired-old count를 구별하여 진단 로그에 남긴다. 관측하지 못한 값은 null/Unknown이며 0으로 만들지 않는다. 별도 운영 API·수동 정리 명령은 만들지 않는다.

`health/detail`의 target reachability는 `unknown/transcript_unresolved`, durable observation은 row_absent, `governs_health_polarity=false`였다. 이것을 배달 실패·배달 완료·cleanup 허가로 읽지 않는다. health의 구조 경보와 #5071의 배달 권위는 별개다.

원시 응답·issue 원문은 `logs/b-r2/`에 저장했다. 이번에는 예전 RCA의 운영 store/transcript census나 마지막 대화 시각을 다시 읽지 않았다. r1의 과거 수치로 현재 count·퇴역 시각을 계산하지 않는다.

## 3. r1 지적의 반영과 설계 선택

| 지적 | 고치지 않으면 운영에서 생기는 일 | 권고 수정 / 증명 |
|---|---|---|
| **P1-1 모든 O writer POST counter** | legacy Codex writer는 POST count가 None이어서, 다른 조건이 맞아도 자동 정리가 `ObligationsUnread`에 머문다 | `tui_o/writer/deliver.rs:265-271`에서 모든 writer가 생성 시 기존 채널 Arc를 확보. 조회/Inspect에서 생성하지 않음. 실제 non-Herdr constructor·POST·writer drop/recreation 시험 및 Herdr-only/새 counter 교체/None→0 뮤턴트 |
| **P1-2 local retry owed** | B 적용을 열 때 queue=0이어도 아직 owed인 30초 retry를 놓쳐, 10초 quiet 뒤 미정산 source를 퇴역시킬 수 있다 | `tui_o/writer/actor/transition_snapshot.rs:228`을 기존 drain 계산(`tui_o/writer/actor.rs:682-685`)과 같은 `queue.len()+has_local_retry()`로 변경. 실제 Budget→NotFound→queue empty/retry positive→자동 Apply 보류, retry 완료 후 양성. 합산 제거 뮤턴트 |
| **P2-3 complete snapshot/scope** | cutoff 밖 pane/P5/context 변화가 관측 scope에 반영되지 않으면 옛 조사 결과가 적용에 쓰일 수 있다 | typed Complete/Unknown, canonical context/held/census/full-tail identity를 Common과 Codex scope에 포함. 최종 under-authority 재관측. >cutoff event/context/root/host 교체 및 필드 누락 뮤턴트 |
| context 수명·다중 pane 제한(이번 사실 조사) | 과거 context가 이미 지워졌거나 추가 pane의 상태가 불명확한데 이를 빈 history로 다루면 근거를 발명한다 | current Complete와 후보별 retention 근거를 분리. historical other-pane에 필요한 old context가 없으면 그 후보 Unknown. 첫 B는 추가 live/unknown pane이면 channel hold. 누락·TTL·inventory 오류·scope 변화 시험 |

r1의 P1-2는 **Codex Apply 활성화 시의 수용 기준 위반**으로 다룬다. 현재 배포에서 그 경로가 실제 메시지를 잃었다고 주장하지 않는다. P2-3을 새 운영 유실 증거 없이 P1로 재분류하지 않는다. B-관측의 false Complete는 fail-closed 거부 시험으로 봉쇄한 뒤 B-적용 리뷰를 받는다.

## 4. B-관측의 계약과 불변식

새 관측 불변식은 all-O POST lifetime, retry-inclusive owed, typed Complete/Unknown, complete held/census와 snapshot identity다. **BO는 Codex 퇴역 조건을 열지 않는다.** POST·owed 수정은 기존 Claude retention에도 동일한 보수적 관측을 제공한다.

### 4.1 typed target와 실행 관측

`Target/Context`에 provider와 host 경로를 명시한다. 개념상 target은 `(provider, channel, Legacy{logical_key, nonce} | Hosted{session_key, nonce})`다. ready port가 기록한 실제 actor provider와 일치해야 한다. channel 번호나 config binding만으로 actor provider를 추정하지 않는다.

관측 결과는 단일 `Complete(snapshot) | Unknown(reason, partial_diagnostics)` 경계로 만든다. `complete: bool`과 임의의 빈 Vec 조합으로 권한을 만들지 않는다. Claude 기존 history·scope v1의 동작/직렬화는 유지하며 Codex를 Claude SourceHistory로 위장하지 않는다.

**Complete의 필수 내용:**

- typed target/provider, actor run/generation, home epoch/admission identity;
- exact current logical key·execution nonce, immutable BindingContext의 canonical fingerprint, owner runtime root·provider root·물리 host identity·host kind;
- 실제 runtime binding의 native `SourceId`(session/path/dev/ino)와 소비 허용 상태, current pin, launch mode·resume expected;
- 현재 실행의 **전체 held set**, Pending/ownership 상태, 관련 pane census와 coverage;
- strict **전체 channel P5**의 committed tail seq와 canonical 전체 행 digest/coverage. request cutoff 뒤 행도 포함;
- component별 관측 성공 여부. runtime miss/expired/withheld, context 누락/손상, 미확정 source·pane, incomplete enumeration/coverage, budget/deadline 초과는 Unknown.

current 실행이 없거나 최신 source 하나만 확인됐다고 Complete로 만들지 않는다. 아직 runtime source가 없는 fresh launch(`shadow fresh None`), resume expected를 pin할 수 없는 경우, unresolved/conflicted Pending은 Unknown이다. strict reader의 누락 파일→empty나 TTL 목록 empty를 execution 부재의 증거로 승격하지 않는다.

### 4.2 legacy와 Herdr adapter

- **legacy:** provider-scoped SharedData·authoritative channel owner, current spawn nonce, strict immutable context, local owner marker/root/host, 실제 runtime source와 canonical native descriptor를 대조한다. legacy target에는 Hosted PG row나 PG pool을 요구하지 않는다. sweep의 기존 무조건 PG 선행 조건도 Hosted 분기로 옮긴다. `codex_tui/execution_end/capture.rs:31-69`의 local identity/runtime 대조와 native verifier를 참고하되 End 생산·source adoption API는 호출하지 않는다.
- **Herdr:** 현재의 Bound hosted row, exact provider/bot/node/root/local endpoint, launch context/nonce, hook/restore가 이미 publish한 exact runtime pin을 대조한다. Creating/Reconciling/remote/unread는 Unknown. 기존 resolver의 Claude 상수만 Codex로 바꾸는 것으로 완료하지 않는다.
- Herdr context는 `source_policy=None`, `first_prompt_digest=None`일 수 있다(`herdr_launch.rs:677-695`). `codex_verified::current_context`나 End의 fresh-shadow matcher 하나로 양쪽을 처리하지 않는다. Herdr는 Bound row+동일 nonce P5/native pin+실제 runtime의 기존 publication 증거를 사용한다. verified legacy는 기존 ownership proof/consumer permission을 추가로 요구한다. raw runtime 관측은 진단 전용이며 withheld source의 소비 권한을 열지 않는다.
- `BindingContext.host`의 instance identity와 session-host owner/execution node, `HostKind::{Tmux,Process,Herdr}`는 다른 좌표다. 같은 namespace끼리 기존 resolver로 비교한다. 문자열이 비슷하다는 이유로 서로 대체하지 않는다.

### 4.3 complete held-set과 pane coverage

held-set은 **current actual source + resume expected source + 현재 실행이 방문한 모든 new Source/Resolved source + Pending 후보**다. canonical descriptor를 dedup/sort하되 path/session/inode overlap은 기존 보수적 current 보호를 유지한다. Pending을 exact source로 결정할 수 없으면 후보의 이름·경로를 진단에 남기고 Complete는 거부한다. verified ownership의 pending/conflict/legacy-unverified를 빈 목록으로 소거하지 않는다.

두 정상 경계를 명시적으로 구별한다.

1. **same-source resume:** P5 writer가 같은 source면 append를 생략한다(`tui_prompt_dedupe/binding_events.rs:574-582,810-817`, `tui_prompt_dedupe/binding_events/codex.rs:174-179`). 따라서 current N2인데 latest P5 nonce=N1일 수 있다. immutable resume context의 expected ID와 exact actual native source가 일치하는 경로를 인정하며, N2 P5 행 ≥1을 필수로 삼지 않는다.
2. **fresh 첫 predecessor:** 첫 fresh event.old는 이전 실행의 source일 수 있다(`tui_prompt_dedupe/binding_events.rs:781`). 이를 현재 실행이 방문한 source로 무조건 넣지 않는다. new/Resolved 방문집합과 resume expected/실제 pin이 보유를 증명할 때만 held다. 그렇지 않으면 이전 source가 영구 held되어 backlog를 정리할 수 없다.

launch anchor와 현재 tip도 구별한다. fresh의 최초 binding은 current nonce/launch 근거로 확인하고, 이후 같은 실행의 new/Resolved 방문을 full history로 따라간다. resume의 P5 no-op anchor는 expected+실제 pin으로 보완한다. 현재 tip이 launch 뒤 바뀐 정상 이력을 최초 fresh prompt 하나와 다시 비교해 거부하지 않으며, 모든 방문 source의 exact native identity와 runtime tip 일치를 검증한다.

pane universe는 **전체 P5 pane 목록 ∪ authoritative live/restored watcher owner 목록 ∪ 실제 provider tmux/host inventory ∪ immutable context inventory**를 교차 확인한다. TTL mirror나 최신 pane 하나를 universe로 쓰지 않는다. 아직 P5 없는 fresh/Pending/resume도 context/inventory에 나타나야 한다. 어느 대상에도 안전하게 귀속할 수 없는 managed pane/context나 열거 오류·누락은 Unknown이다. 전수 열거를 budget에서 잘랐다면 Complete가 아니다. inventory에서 아직 current marker/P5에 연결되지 않은 새 prepared context를 찾으면 이를 과거 실행으로 임의 분류하지 않고 Unknown으로 둔다. 이미 superseded된 historical context와 현재/준비 중 context를 구별한 결과를 coverage에 담는다.

첫 B는 **검증된 단일 current execution**을 양성 범위로 한다. 추가 live/unknown pane이 있으면 channel Unknown/hold하며 multi-pane 잠금·cleanup 권한은 만들지 않는다. historical other-pane의 확인된 ended 상태와 후보별 근거는 B-적용에서 별도로 다룬다. 이 제한도 snapshot identity에 포함한다. 다른 pane가 존재하지 않는다는 결론을 TTL cache miss로 만들 수 없다.

### 4.4 POST·owed

- 모든 `ChannelWriter::with_receipts`가 writer 생성 시 기존 channel별 Arc counter를 확보한다. new/hosted/restore/recreation은 동일 생성자로 합류한다. map entry를 교체하거나 writer drop에서 요청의 count를 감소시키지 않는다.
- 기존 `Running`을 POST future 생성 전부터 request 완료/drop/abort까지 유지한다. 일반 POST와 re-post가 같은 counter를 사용한다. HTTP future가 writer보다 오래 살면 새 writer도 그 count를 읽는다.
- `posts_in_flight`는 기존 entry 조회만 한다. 미관측 `None`은 오류/Unknown이다. Inspect/Apply/test helper로 counter를 만들고 `Some(0)` 양성을 꾸미지 않는다.
- `owed = ordinary_queue.len() + usize::from(writer.has_local_retry())`. local retry의 due 전 대기도 채널 전체 owed다. source별 retry provenance를 새로 만들지 않는다. Prepared/unsealed/verified work/protected source는 각각 유지한다.

### 4.5 snapshot 동일성 및 실제 caller

- 기존 sweep를 provider별로 사용한다. ready port의 provider와 runtime을 교차 검증하고 **각 provider sweep가 자기 actor만** 선택하게 한다. 현재 60초 initial delay/완료 후 hourly 간격을 유지한다. 두 provider가 같은 ready 채널에 중복 job을 보내는 잘못된 일반화를 시험으로 막는다.
- Codex의 production 요청은 B-관측에서 **Inspect만** 만든다. actor의 provider 차단을 operation별로 나눠 Inspect의 snapshot/diagnostics를 허용하되, AutomaticApply 거부와 pure judge의 CodexDeferred는 유지한다. suspension·stopped·foreign-run·ownership 차단도 유지한다.
- 진단에 Complete/Unknown 및 component 이유, provider/run/nonce, actual held set·census/full-tail fingerprint, `owed/prepared/unsealed/posting/verified`, **실제 reading count와 unretired-old count를 구별한 값**을 남긴다. 기존 sweep 한 줄 로그에 summary/skip reasons를 붙인다. `CodexDeferred`는 관측 성공이나 퇴역 성공으로 세지 않는다.
- context fingerprint·canonical held digest·census identity·full tail을 `Common`에 넣는다. Codex용 버전 domain의 observation identity를 만든다. B-적용의 scope v2는 이 identity를 사용한다. 기존 Claude v1 digest bytes는 그대로 둔다.
- 초기/최종 관측의 identity가 다르거나 읽기가 실패하면 재평가/hold한다. 관측 결과를 durable 종료 proof로 저장하지 않는다. 이번 단계의 cursor/audit/End/Seal bytes는 변하지 않아야 한다(A의 독립적인 정상 작업은 별개).

**모듈 사이 최소 인터페이스:** 공용 Codex 관측 타입/reader 계약은 비-giant `tui_o/writer/codex_snapshot.rs`에 두고, health adapter가 읽기 전용 구현을 제공한다. 허용된 부모 `discord/health/o_transition.rs`가 이를 Context에 담는다. actor가 frozen census Vec만 받거나 health 모듈의 private 함수에 직접 의존하는 형태로 끝내지 않는다.

- `prepare(target)`는 PG/host inventory 등 async 조회와 느린 native 검증을 source authority 밖에서 수행해 `PreparedObservation` 또는 Unknown을 반환한다. 최초 Inspect와 매 후보 적용 직전에 수행한다. legacy 준비에는 PG가 필요 없다.
- `read_under_authority(authority, prepared)`는 이미 잡은 current pane authority로 nonce/context/owner marker/실제 runtime pin, authoritative owner-map/context census identity, strict 전체 P5를 현재 값과 대조하여 Complete/Unknown을 반환한다. snapshot 시작·끝에서 full-tail/coverage가 달라져도 Unknown이다. 이 함수는 async/block_on·spawn·rehydrate·publication을 하지 않는다.
- 최종 호출자는 `prepare`를 await한 뒤 `try_with_tmux_source_authority`가 준 인자를 전달한다. synchronous 비교→judge→cursor CAS 사이에는 await가 없다. cached/TTL wrapper Option을 전체 실행의 oracle로 쓰지 않는다.
- source authority 아래 STATE와 log mutex는 각각 짧게 읽고 해제한다. STATE/log mutex를 잡은 채 source authority나 서로의 wrapper getter를 재진입하지 않는다. 기존 source→STATE publication 순서(`tui_prompt_dedupe/runtime_binding.rs:97-125`)와 P5 reader 순서를 유지한다. async row/host 조회가 final local validation을 대체하지는 않는다.

이는 전체 DB·프로세스·다른 pane를 하나의 원자 snapshot으로 잠그는 새 API가 아니다. exact current publication에 대한 권위와 최종 재검증을 제공하는 관측 API다. 다른 pane/누락 coverage는 first B의 명시적 hold 범위이며, 그 사실도 관측 타입과 scope에 들어간다.

## 5. 진입점 전수표 — 관측/생산·복원·의무

아래 표의 생산 경로는 **읽기·재사용만**이며, 다른 레인 소유 생산 파일에는 hunk 0이다. 변경 표시는 이후 구현 대상이다.

| 범주 | 기준 file:line·함수 | B의 연결/보존 |
|---|---|---|
| 주기 시작 | `discord/health/recovery/stall_watchdog_task.rs:3-4`; `discord/health/o_transition/automatic.rs:118-128` spawn | caller 유지, spawn/sweep의 provider 선택만 관측 단계에서 일반화 |
| ready actor·target | `discord/health/o_transition/automatic.rs:131-168,186-204`; `tui_o/writer/host/transition.rs:24-34,43-46,73-109` | port run/provider를 명시. Claude-only runtime/PG/log 상수 분리 |
| context/home | `discord/health/o_transition.rs:29-57,62-103,107-116` | provider-aware target/context, 기존 home permit. legacy/Codex adapter 추가 |
| legacy fresh/resume context | `codex/tui_session_launch/launch_policy.rs:62-86`; `tui_prompt_dedupe/binding_context.rs:18-34,310-379` | 기존 immutable producer 그대로. 후속 관측만 추가 |
| Herdr fresh context/source | `herdr_launch.rs:397-429,669-695`; `codex/herdr_turn.rs:175-210,361-422` | row/nonce/hook pin 관측. 생산 파일 hunk 0 |
| Herdr 후속 입력 | `codex/herdr_turn.rs:501-525` | 동일 incarnation/runtime 검증의 기존 계약 읽기. hunk 0 |
| legacy restore | `discord/tui_prompt_relay/rehydration.rs:443-653` | live pane/marker/source reconcile 결과를 관측. restore를 새로 실행하지 않음 |
| Herdr restore | `discord/recovery_engine/herdr_reader.rs:234-390`; `discord/tui_prompt_relay/herdr_source.rs:274-329` | exact local endpoint/nonce baseline/publication 결과를 관측. 새 restore 권한 없음 |
| context 엄격 읽기·수명 | `tui_prompt_dedupe/binding_context.rs:186-210,232-246,491-503,522-579` | read-only inventory API 추가. 기존 prune는 변경하지 않음 |
| pane owner 목록 | `discord/tmux_watcher_registry/session_binding.rs:20-49,74-86` | authoritative live/restored map을 census 입력으로 재사용. 단독으로 completeness 선언 금지 |
| 실제 tmux inventory | `platform/tmux.rs:639-650,687-715` | 읽기 전용 enumerate 성공/오류를 구분. user Herdr 제어 없음 |
| runtime current/list | `tui_prompt_dedupe/runtime_binding/binding_access.rs:58-105`; `tui_prompt_dedupe.rs:31` | under-authority raw 상태/미관측/withheld 이유의 좁은 관측 API. TTL/permission 자체는 유지 |
| runtime publication | `tui_prompt_dedupe/runtime_binding.rs:65-125,222-280` | source authority→STATE 및 invalidate 순서 유지. observer는 publication하지 않음 |
| P5 전수 읽기 | `tui_prompt_dedupe/binding_events.rs:307-337`; `tui_o/writer/binding.rs:84-134` | full channel/provider/seq/coverage와 tail identity를 좁은 Codex reader 경계에서 함께 반환 |
| Codex ownership | `tui_prompt_dedupe/binding_events/codex.rs:374-455,513-608`; `tui_prompt_dedupe/binding_events/codex/settled.rs:11-51` | verified 전용 fold/settled latest의 한계를 구분. new read view가 whole held history를 제공 |
| same-source append 생략 | `tui_prompt_dedupe/binding_events.rs:574-582,810-817`; `tui_prompt_dedupe/binding_events/codex.rs:174-179` | 정상 resume를 context+actual source로 보완, no-op 동작 유지 |
| mailbox/actor 접수 | `tui_o/writer/transition_port.rs:286-312,326-339`; `tui_o/writer/actor/transition.rs:166-185,219-277` | run/generation/stop/suspension/ownership 유지. Codex Inspect만 개방 |
| snapshot/history | `tui_o/writer/actor/transition_snapshot.rs:26-35,137-203,259-299` | Codex typed branch, current/expected/visited/Pending 전체 held set, Common identity |
| ordinary writer 생성 | `tui_o/writer/deliver.rs:246-282,304-319`; `tui_o/writer/host.rs:693-742` | 모든 생성·복구가 기존 shared counter에 합류 |
| ordinary POST | `tui_o/writer/deliver.rs:505-577,595-649,686-710`; `discord/outbound/o_writer_io.rs:88-103` | Prepared→Running→POST future/lease lifetime 유지 |
| re-post POST | `tui_o/writer/deliver.rs:877-882`; `tui_o/repost/dispatch.rs:120-124,181-211` | 같은 counter. CountedRequest의 request 먼저 drop, counter 나중 drop 유지 |
| POST 조회·drop | `tui_o/writer/deliver.rs:103-155` | entry 조회는 생성하지 않음. 요청 생존 중 writer 교체에도 count 유지 |
| ordinary/verified owed 생산 | `tui_o/writer/actor.rs:256-274,323-327,434-470,710-720`; `tui_o/writer/rotation/owe.rs:17-70`; `tui_o/writer/owed.rs:64-108,251-325` | 기존 spool/derive/verified work의 의무·source 보호 유지 |
| queue pop·retry 생성 | `tui_o/writer/actor.rs:590-609`; `tui_o/writer/deliver.rs:547-575,745-815` | 실제 Budget→NotFound 뒤 queue empty/retry positive 가능 |
| retry 복원·송신 | `tui_o/writer/local_retry.rs:20-90`; `tui_o/store/ledger.rs:402-423,497-507`; `tui_o/writer/actor.rs:339-358` | due 30초 전에도 owed. 기존 retry 권한/횟수/기한 변경 없음 |
| 기존 drain와 cleanup owed | `tui_o/writer/actor.rs:676-705`; `tui_o/writer/actor/transition_snapshot.rs:222-234`; `tui_o/writer/transition_cleanup.rs:431-446` | drain과 같은 retry 합산. None·unread는 Unknown, 모든 양수는 보존 |
| sender surface 가드 | `scripts/check_hrot_sender_surface.py:19-25,55-93` | operation 생성/port lookup/send/receipt 소비는 automatic.rs에 유지. 새 context child는 port를 직접 import/송신하지 않음 |

## 6. B-적용의 계약과 불변식

**BA는 provider proof가 없는 Codex old source에 별도 retention 퇴역 조건을 새로 허용하는 설계 변경이다.** Claude의 기존 7일 정책과 보존 게이트는 바꾸지 않는다. 새 조건은 Complete+source별 근거+7일 age+기존 전체 게이트의 conjunction이며, A의 exact execution-end 증명과 동등하다고 주장하지 않는다.

### 6.1 정리의 근거와 후보별 retention evidence

#7268의 **비최신·마지막 대화 후 7일 이상**을 유지한다. 이것은 A의 exact execution-end proof와 다른 정책이다. 기존 자동 retention의 같은-pane 분기는 PID/PGID 사망이 아니라 **old source의 마지막 binder nonce ≠ 검증된 current execution nonce**를 사용한다(`discord/health/o_transition/automatic/termination.rs:104-130`). 이 의미를 Codex용 typed retention 근거로 기록하며 물리 종료를 관측했다고 쓰지 않는다.

후보별 근거는 `(provider/channel, exact old SourceId/key, hop seq와 original Pending seq, last binder seq/nonce, successor, current Complete identity, 종료/비보유 근거, age observation)`를 묶는다.

- **same-pane:** full strict history에서 exact old source의 마지막 binder를 찾는다. Pending→Resolved는 Resolved 행 번호와 원래 binding seq를 구별한다. 빈 nonce, 중복/모순 resolution, 후속 재방문 누락, source와 binder의 불일치는 Unknown. current nonce와 다른 binder이고, complete held-set에 없을 때만 retention 근거가 된다.
- **other-pane:** historical local helper를 부르기 전에 exact old context의 physical host/runtime root와 pane/nonce를 확인한다. 기존 helper가 지원하는 알려진 local Tmux/Process host kind가 dead/absent를 확인해야 한다. historical Herdr/remote 종료를 이 helper의 local 이름 조회로 대체하지 않는다. remote·missing context·unknown host/root/liveness는 그 후보 Unknown이다. local 동명 pane 부재를 remote 종료로 쓰지 않는다.
- **current Complete와 candidate Unknown은 별개다.** same-pane의 기존 nonce-supersession 정책에 모든 old context 보유를 새로 강제하지 않는다. 반면 other-pane에서 필요한 old context를 없다고 추정하거나 새로 생성하지 않는다.
- 기존 context sweep는 shadow/Herdr old context를 created_at 7일 이후 삭제할 수 있다(`tui_prompt_dedupe/binding_context.rs:555-576`). other-pane의 근거가 이미 사라졌다면 자동 보존이 지속될 수 있다. B에서 context GC 정책 변경·소급 attestation·새 원장·새 명령으로 우회하지 않는다. 이 한계를 관측 로그와 적용 결과에 남긴다.

### 6.2 provider별 dialogue age

기존 `tui_o/writer/actor/automatic_age.rs:17-59`의 identity/hash/EOF/newline/32MiB 검증과 7일 inclusive 경계를 공통으로 유지하고 provider별 row 해석만 분리한다. worker는 실제 actor provider를 전달한다.

- Claude `user/assistant` 해석은 그대로 유지한다.
- age 분류는 순수 읽기다. `codex_tui/rollout_tail/parser.rs:198-204`의 decoder는 prompt 관측도 호출하므로 age 계산에서 decoder를 재실행하지 않는다. 기존 parser의 문법/fixture를 참고한 순수 classifier만 사용하며 age 관측 전후 P5·runtime·delivery 원장 불변을 검사한다.
- Codex는 **O cursor가 가리키는 exact native JSONL**을 읽는다. 공개 watcher의 normalized relay 경로를 native cursor로 바꾸지 않는다. native header/session/root/descriptor가 맞지 않거나 wrapper만 확인되면 AgeUnknown이다.
- 현재 parser와 동일한 native dialogue 형태인 `response_item/message`의 user/assistant, 대응 `event_msg`의 user/agent message 및 실제 assistant 본문을 포함하는 terminal fallback을 분류한다. 지원하는 legacy message-bearing 형태도 timestamp가 있어야 한다. 각 행의 실제 RFC3339 timestamp의 최대값을 사용한다. mirror 중복은 max를 더 이르게 만들지 않는다.
- malformed JSON, 알려지지 않은 dialogue-bearing envelope/role, 판별 불가 행, message timestamp 누락/불량, partial tail, 읽기 한도/파일 교체/hash 불일치는 source 전체 AgeUnknown이다. 알려진 metadata/tool-only 행만 명시적으로 건너뛴다. mtime·파일명·session 생성·본문 없는 task_complete 시각을 dialogue 대체값으로 쓰지 않는다.
- 두 번째 probe의 exact cursor/hash/fingerprint에 age를 결합하고 최종 wall time으로 `now-last_dialogue >= 7*24h`를 다시 계산한다. future timestamp·역행은 보존한다. 새 dialogue로 hash/EOF가 변하면 새 조사부터 다시 시작한다.

### 6.3 scope 재검증과 적용 직렬화

Codex scope v2는 기존 request/hops에 **provider/typed target, context/root/host fingerprint, actual source, complete held-set digest, 전체 pane census/coverage identity, full channel tail seq+digest**를 더한다. cutoff는 후보 범위를 정할 뿐 전체 관측의 끝을 자르지 않는다. Claude v1 byte encoding은 유지한다. scope 타입/표시도 domain·버전을 구별하고, unknown 버전이나 Codex에 들어온 Claude v1 scope는 거부한다. 기존 v1 prefix로 새 bytes를 포장하지 않는다. 관측용 identity만으로 Apply를 만들지 않고, source별 retention evidence가 들어간 targeted Inspect의 scope를 받아야 한다.

각 후보마다 기존 순서를 유지한다.

1. 기존 channel session-transition lock을 시도하고, runtime mailbox idle·age를 재검사한다.
2. current pane의 source authority를 잡고 **그 authority를 인자로 전달**하여 strict current context/nonce/runtime/history/held/census/tail을 다시 관측한다. wrapper getter로 같은 mutex를 재취득하지 않는다.
3. observation Complete, request/provider/run/home, accepted scope v2, Common, candidate token(cursor/reader/FD/successor), path, 후보별 retention 근거와 obligations를 모두 다시 비교·판정한다. >cutoff 다른 pane 행 하나라도 full-tail identity가 변하면 InspectScopeChanged로 보류한다.
4. stop을 다시 읽은 뒤, 추가 await 없이 기존 store CAS에서 audit+retired를 한 번에 저장한다. Unknown write 결과는 성공으로 세지 않는다.
5. 기존 regrowth watch와 즉시 growth 검사를 실행한다. 성장하면 unretire·기존 offset부터 capture/delivery를 재개하고 경보를 유지한다.

**잠금이 보장하는 범위를 과장하지 않는다.** transition lock은 실제 provider spawn 전체를 감싸지 않는다(`discord/router/turn_start.rs:641-644`; `discord/router/message_handler/intake_turn.rs:2296-2332`). clear는 3초 timeout 뒤 lock 없이 계속할 수 있다(`discord/commands/control.rs:347-355`). 따라서 “transition lock을 얻었으므로 새 실행이 절대로 없다”를 근거로 쓰지 않는다.

current pane의 로컬 context/marker/runtime 공개·삭제가 source authority를 사용한다는 근거는 `tui_prompt_dedupe/binding_context.rs:351-370,569-577`, `discord/tmux_session_files.rs:296-312,326-352`, `tui_prompt_dedupe/runtime_binding.rs:97-125`, `tmux_common.rs:1233-1277`다. legacy spawn(`codex/absence_launch.rs:403-406`)과 Herdr spawn(`herdr_launch.rs:425-429`)은 같은 marker 함수를 쓴다. 최종 adapter는 async discovery 결과를 그대로 믿지 않고 이 로컬 증거를 borrowed authority 아래 다시 읽는다.

이 authority가 Herdr DB row/endpoint, 다른 pane의 P5 append, 실제 프로세스 생존 전체를 잠근다고 주장하지 않는다. 느린 host/row/inventory 관측은 바깥에서 수행하고 그 identity의 최종 불일치/읽기 실패는 보류한다. 추가 live/unknown pane은 첫 B의 양성 범위에서 제외한다. controlled current publication 경합과 other-pane >cutoff 변화는 별도 시험으로 고정한다. clear의 기존 timeout/kill 경로를 고치거나 cleanup 권한을 확대하지 않는다.

### 6.4 그대로 유지할 보존 게이트와 저장 효과

다음 기존 조건은 Codex 분기에서도 모두 살아 있어야 한다.

- home/ownership/admission, current pin·held source overlap, provider/source permission, actor run/restore readiness·stop·suspension;
- successor가 실제 capture된 상태, pending batch 없음, drain 완료, capture/read cursor 일치;
- channel owed(재시도 포함)/Prepared/unsealed/POST/verified work=0, candidate protected 아님;
- exact file identity/hash/EOF/newline, 최소 10초 두 probe quiet, witness의 기존 정확성·수명(해당 경로), 최종 cursor/path CAS;
- 기존 hash/time/candidate 예산, scope/token/Common 재검증, runtime idle, atomic audit+retired, regrowth watch, 성공 tend의 실제 count에 따른 health 해제.

`TransitionCleanup`에 optional typed `RetentionEvidenceV1`를 추가한다. provider/정책 버전·min_age·last_dialogue/관측 시각·정확한 cursor/hash/fingerprint·binder/successor·current snapshot/scope identity·same-pane supersession 또는 확인된 local historical basis를 기록한다. 새 Codex retention write는 이 필드가 없으면 거부한다. 과거 Claude audit는 optional 없음으로 읽을 수 있고 새 퇴역 권한을 주지 않는다. 큰 `spool.rs`의 타입들을 비-giant 자식 모듈로 옮겨 기존 `spool::TransitionCleanup` 경로를 re-export한다.

원본 native transcript를 삭제하지 않고 reader/cursor만 은퇴시킨다. audit와 retired는 기존 `tui_o/store/spool/transition.rs:22-48`의 단일 cursor write/CAS를 사용한다. 기존 A proof나 delivery receipt/frontier를 합성하지 않는다. audit 저장 실패 시 halted/Unknown을 유지한다. 실제 reading count가 3 이하가 된 뒤 성공 tend가 해당 alarm만 해제한다. legacy retired FD가 watch를 위해 남아 있어도 `Reader::reading`은 `watch_until`이 있으면 제외한다(`tui_o/writer/rotation.rs:119-120`); open FD 수로 health를 계산하지 않는다. 현재 R개라면 해당 경보 해제에는 적어도 R−3개 reading retirement가 필요할 수 있으며, R은 현재 미확인이다. r1의 R=21 가정을 쓰면 18개지만 이를 현물 수치로 보고하지 않는다.

## 7. 진입점 전수표 — 적용·age·최종 승인·health

| 범주 | 기준 file:line·함수 | B-적용 변경/재사용 |
|---|---|---|
| 자동 의사결정 | `discord/health/o_transition/automatic.rs:27-66,230-283` plan/targeted Inspect | provider별 retention evidence를 만들고 같은 sender에서만 Inspect |
| automatic Apply | `discord/health/o_transition/automatic.rs:287-313` scope/Idle/resolve/send | Codex Complete+source evidence일 때만 연결. operator 경로 없음 |
| 현재 Codex 차단 | `tui_o/writer/actor/transition.rs:245-246`; `tui_o/writer/transition_cleanup.rs:295-296` | BO는 Apply 차단 유지. BA는 typed complete/evidence guard로 대체; 무조건 삭제 금지 |
| old binder·종료 근거 | `discord/health/o_transition/automatic/termination.rs:26-29,32-88,91-145` | provider 상수 분리, original Pending seq와 last binder 구별, Codex 근거 추가 |
| historical local 확인 | `discord/health/o_transition/automatic.rs:230-245,299-304,319-324`; `discord/host_liveness.rs:92-98`; `session_host/tmux_host.rs:85-93` | exact local host/root 선행, remote/unread 보류. helper 자체 권한 확대 없음 |
| probe/예산 | `tui_o/writer/actor/transition.rs:29-36,399-434,458-468` | 64 candidates/4 batch/60초·hash budget 유지. provider별 age를 두 번째 probe와 결합 |
| age/idle | `tui_o/writer/actor/automatic_age.rs:13-59`; `tui_o/writer/actor/automatic.rs:6-35`; `discord/health/o_transition/automatic_idle.rs:10-26` | native Codex age branch, 7일·mailbox unknown 보존. 새 retry·타이머 없음 |
| scope 최초/최종 | `tui_o/writer/transition_scope.rs:64-104,109-178`; `tui_o/writer/actor/transition.rs:264-275,500-513` | Codex v2 identity 결합, Claude v1 유지 |
| 최종 source 권위 | `tui_o/writer/actor/transition.rs:477-519`; `tmux_common.rs:127-141` | authority 인자를 전달. Common·token·scope·path·evidence 재검사 |
| 보존 판정 | `tui_o/writer/transition_cleanup.rs:310-335,365-517` | 기존 current/drain/quiet/prefix/obligation/witness 게이트 유지 |
| 저장/CAS | `tui_o/store/spool/transition.rs:22-48`; `tui_o/store/spool.rs:30-74` | 기존 한 cursor write, Codex typed retention audit. giant 타입은 자식으로 이동 |
| 후속 regrowth | `tui_o/writer/actor/transition.rs:538-544`; `tui_o/writer/rotation/transition.rs:143-173` | 기존 FD/path watch, 즉시 성장 시 unretire |
| 결과·health | `tui_o/writer/actor/automatic.rs:46-60`; `discord/health/o_transition/automatic.rs:69-142`; `tui_o/writer/actor.rs:357-358,710-720`; `tui_o/writer/rotation/readers.rs:364-369,621-626`; `tui_o/alarm/sink.rs:12-15` | durable audit 결과와 실제 reading count 별도 검증. alarm 선행 삭제/MAX_READERS 변경 없음 |

## 8. #5071 배달 권위와의 대조

B의 정리 권한의 직접 출처는 #7268과 #7308이다. #5071의 “7일·500턴”은 과거 live acceptance 표본 계약이며 이 retention의 7일 근거가 아니다. #5071 전체를 다시 열거나 완료 판정을 내리지 않는다.

| #5071 계약 | B에서 지킬 경계 / 근거 |
|---|---|
| 구조 신호는 delivery/recovery 권위가 아님(본문 §4, §5.3) | reader 수·health green·watcher alive·queue0은 단독 퇴역 근거가 아니다. Complete+retention+기존 obligations 판정을 actor에서 수행 |
| exact obligation/receipt/commit가 배달 증거(§5.1-5.2) | B는 Posted/TransportConfirmed/Committed/frontier를 쓰지 않는다. O의 Prepared→Posted/Unresolved 원장(`tui_o/store/ledger.rs:41-74,147-175`)을 재해석하지 않는다 |
| Unknown이 일급 상태, 부재를 완료/실패로 붕괴 금지 | POST None, unread tail, missing context/runtime, unresolved source를 0/empty/ended로 치환하지 않는다. 공개 watcher API도 null은 unmeasured(`src/server/routes/queue_api.rs:265-269`) |
| lease/identity/영구 원장 보호 유지 | O와 Legacy가 같은 per-channel delivery lease를 사용(`discord/outbound/o_writer_io.rs:145-188`). B는 lease를 강제로 해제하거나 source permission을 열지 않음 |
| 실행명은 locator, nonce/identity가 권위 | target/provider/context/nonce/root/host/native descriptor를 대조. tmux 이름 일치만으로 old execution 종료를 주장하지 않음 |
| actual production 배선·뮤턴트 증명(§7) | 실제 constructor→delivery→settlement 및 sweep→Inspect→actor→store를 통합 시험. fake Complete/POST=0 또는 직접 seeded retry만으로 양성 완료하지 않음 |
| 관측과 파괴적 복구 분리 | BO는 관측뿐. BA의 외부 효과는 기존 cursor retirement 한정이며 kill/redrive/source 삭제를 추가하지 않음. retention audit는 배달 완료 receipt가 아님 |

기존 `tui_o/writer/actor.rs:615-645`의 spool GC는 별도 main 경로다. 그 precheck에 local retry를 명시적으로 세지 않는 점은 이번 retirement snapshot 수정과 구별한다. B는 이 함수·GC 대상/개수/권한을 바꾸지 않는다(기존 `Sources::collectable` 및 source permission 경계 재사용). reader retirement를 계기로 새 삭제 호출을 추가하지 않는다. 별도 손상/유실 재현 없이 이를 이번 P1로 확대하지 않는다.

## 9. 증명 계획 — RED → GREEN → 원인 제거 뮤턴트 RED

이번 계획 단계의 실행 수는 **시험 0, 뮤턴트 0, cargo 0, PG 0**이다. 아래는 구현 단계의 필수 증명이며, 아직 PASS라고 보고하지 않는다. compile failure·0-test·skip·fixture가 Complete/POST=0을 직접 강제한 결과는 배선 증명으로 세지 않는다.

### 9.1 B-관측

| ID | 실제 경로와 단언 | 수정 전 RED / 뮤턴트 |
|---|---|---|
| O1 | non-Herdr ordinary `new`와 production `hosted` 생성→실제 POST. 생성 직후 Some(0), request 전/생존 중 Some(1), 종료 후0. repost도 같은 counter | main non-Herdr None로 RED. Herdr-only 복원, Running 시작 제거/지연, POST 조회에서 생성하는 뮤턴트 |
| O2 | 실제 child HTTP future를 hold한 상태에서 writer drop, 동일 채널 writer 재생성. 새 writer도1, 종료/abort 뒤0. never-created 채널은 조회해도 None | counter 교체/초기화·writer drop에서 감소·None→0 뮤턴트. `repost_posts_counter()` helper는 생성 양성에 사용 금지 |
| O3 | 실제 ordinary delivery→Budget→history settlement NotFound→queue pop으로 queue0/retry1. quiet10초 후 due30초 전 Inspect 진단 owed1; actor recreation/ledger restore에서도1 | main snapshot owed0로 RED. has_local_retry 합산 제거·due 전 제외 뮤턴트. Codex BO는 Apply가 여전히 거부됨을 별도 단언 |
| O4 | legacy shadow fresh(PG pool/hosted row 없음), verified fresh, same-source resume, configured Herdr fresh/restore 각각 actual context+P5+runtime publication→ready port→기존 sweep→Inspect. Complete의 exact target/source/held/tail을 검사 | main Codex refusal/Claude-only caller로 RED. 생산 wiring 제거/provider 상수·fake zero·latest-only held-set 뮤턴트 |
| O5 | 같은 source resume는 current nonce P5행0이어도 expected+actual exact pin으로 Complete. source 재방문은 방문했던 source 모두 held. fresh predecessor는 현재 held로 넣지 않음 | current-nonce P5행 필수·latest-only held·모든 event.old 영구 held 뮤턴트. 보존/양성 두 방향 단언 |
| O6 | live pane 있지만 TTL miss, 소비 withheld, P5 없는 fresh/Pending, context missing/corrupt/root/host mismatch, incomplete census, unresolved/conflicted source 각각 Unknown. 추가 live pane도 channel hold | TTL-empty→Complete-empty, context/permission/coverage 생략 뮤턴트. 부분 결과가0/성공으로 집계되지 않음 |
| O7 | current/held는 같은 채 Inspect 전후 다른 pane의 >cutoff event만 추가하거나 context/root/host/census identity만 변경. observation id/Common이 달라짐. Claude scope v1 golden bytes 동일 | full-tail/context/held/census fingerprint 누락 뮤턴트. 이후 BA에서 실제 Apply 거부까지 연장 |
| O8 | ready에 Claude/Codex 둘 다 등록하고 기존 provider tick 각각 실행. 실제 provider 일치 actor만 Inspect. run 교체·suspension·stopped reply는 skip | provider/run 검증·sender 연결 제거 뮤턴트. BO Codex cursor/audit/End/Seal 불변; A unproven backlog fixture 보존 |

재사용 fixture: `tui_o/writer/drain_projection_tests.rs:192-227`, `tui_o/writer/local_retry_tests.rs:55-109`, `tui_o/writer/transition_execution_tests.rs:32`, `tui_o/writer/transition_scope_tests.rs:48,128`, `tui_prompt_dedupe/runtime_binding/codex_verified_executed_resume_tests.rs:104,121,195`, `discord/tui_prompt_relay/herdr_source_tests.rs:460`. 기존 `tui_o/writer/transition_actor_tests.rs:1008`의 “non-Herdr constructor=uncounted” fixture는 새 계약에 맞게 바꾸고, 진짜 unobserved/fault injection 경계를 따로 둔다. 기존 ledger seed retry 시험만으로 O3을 대체하지 않는다.

### 9.2 B-적용

| ID | 실제 경로와 단언 | 수정 전 RED / 뮤턴트 |
|---|---|---|
| APL1 | **legacy non-Herdr와 configured Herdr 모두** 실제 context/P5/runtime→sweep discovery→targeted Inspect→scope→AutomaticApply→atomic cursor/audit. 8일 old noncurrent·단일 current complete·idle·drained만 퇴역; current 유지 | BO는 Apply 차단으로 RED. consumer/producer/sender 연결 제거·Herdr-only counter 복원 뮤턴트 |
| APL2 | O3의 실제 Budget+NotFound 상태에서 quiet10초 완료, due30초 전 actual Apply가 OutputOwed(Queue), cursor/audit 불변. 실제 retry child Posted 뒤 **새 조사**에서는 퇴역 | local retry 합산 제거 뮤턴트는 이 고유 단언으로 RED. retry 영구 owed 뮤턴트는 후속 양성 RED |
| APL3 | 실제 POST/repost future 생존·writer 교체 동안0퇴역. Prepared/unsealed/verified work/protected/pending capture/drain/capture-ahead 각 상태도 보존 | 각 obligation/permission/drain 가드 제거 뮤턴트. 실제 POST 완료 뒤의 양성도 필요 |
| APL4 | Codex native response_item/event_msg/mirror/fallback/legacy dialogue, 7일 경계−1ns/정확히/＋1ns, 더 새 dialogue, 미래 시각, malformed/unknown/torn/missing timestamp, wrapper/native 혼동 | age비교·newest dialogue·형식/EOF/hash 확인 생략 뮤턴트. tool-only/mtime/파일명으로 age 합성하면 RED |
| APL5 | same-source resume expected, 현재 실행 visited source, Pending/conflict, additional live pane, TTL miss, old binder 빈 nonce/다중 resolution/후속 재방문 각각0퇴역 | latest-only held·Pending 무시·hop nonce를 old binder nonce로 대체·None→empty 뮤턴트 |
| APL6 | same-pane last binder supersession 양성. other-pane exact local ended 양성 및 remote old context+local 동명 pane absent·missing old context 음성 | old-host/root/nonce 확인 제거, host kind를physicalhost로 대체, missing→ended 뮤턴트 |
| APL7 | Inspect 뒤 >cutoff 다른 pane event, current nonce/context/root/source/held/owner/home/actor run 변화, cursor/prefix/path 교체를 각각 실제 Apply 직전에 주입. 전부 hold/unknown·audit0 | full-tail/context/census/Common/token/CAS 생략 뮤턴트. current source authority contention 및 borrowed API의 무재진입도 시험 |
| APL8 | audit+retired 저장 crash/fault 전후 재개, legacy cursor/audit 형식 restore. exact job 재실행은 중복 audit 없음. I/O unknown은 성공0 | durable audit 저장 생략·cursor-only write·write error→success 뮤턴트. 원본 파일 bytes 불변 |
| APL9 | legacy FD/configured path watch 모두 retirement 직후·restore 뒤 old source regrowth. unretire와 정확한 offset·delivery receipt/body exactly once | regrowth watch/즉시 검사 제거 뮤턴트. 0 offset 재배달 또는 body 손실도 실패 |
| APL10 | synthetic backlog20+current1. 7일 전20 보존; source별 조건 충족 때만 감소, unknown source는 남음. 실제 reading count>3→≤3에서 같은 process target health reason만 해제. 이후 clear/fresh와 actor/store restart 반복 settle 수 일정 | count만 낮춤·audit 생략·health early/always-clear 뮤턴트. current/audit/cursor/reading/Posted IDs를 함께 단언 |

A의 End+Seal 기존 회귀·actual direct replacement/managed clear 양성·무증명 backlog 보존 시험도 변경 영향 범위에서 실행한다. B 시험에 과거 End-only용 Seal을 만들어 양성을 꾸미지 않는다. 실제 운영 재시작·E2E 입력·배포 검증은 코디네이터가 수행한다.

### 9.3 구현 단계 게이트와 증거

각 단위는 수정 전 실제 assertion RED, 수정 후 GREEN, 해당 원인 제거 mutant의 assertion RED를 저장한다. mutant 전후 소스 바이트 동일 복원·실제 test-id/실행 수/실패/skip·바이너리 경로/SHA256·원시 로그·SHELL/TMPDIR/profile/PG 실제 실행 여부를 보고한다. 새 test id는 required PR job의 path/platform/run-mode 선택을 확인한다. 시험 없는 cfg(test) 지원 모듈은 만들지 않으며, 불가피한 helper-only 모듈은 허용된 test-lane 예외 형식·만료일로 명시하고 static coverage 게이트에서 확인한다. inventory는 `python3 scripts/check_test_target_integrity.py --write-lib-inventory-manifest`로만 동기화하고 test-lane 행 추가를 사용한다. 생성 문서 갱신이 필요하면 `python3 scripts/generate_inventory_docs.py`를 사용한다.

빌드 재개 때 첫 cargo 전 `wait_headroom.sh`, 새 target 필요 시 `seed_target.sh`를 수행한다. cargo-shim PATH, `unset TMUX TMUX_PANE`, `CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none`을 적용한다. 전체 lib 로컬 실행은 하지 않는다. PG가 필요한 hosted row 시험은 `pg_throwaway.sh` 임시 클러스터만 사용한다. 변경 파일 clippy 경고0·fmt·적절한 check/대상 시험·required selection·static prepush를 검증한다. 이 계획 실행에서는 이 명령들을 돌리지 않았다.

## 10. 대상 파일·예상 크기·소유권

아래는 **설계 예약치**다. prod cap 실측이 아니다. 리뷰 단위별 **30 prod 파일/순증+1500**, 구현 중 예상이 **+1200에 도달하면 코드 확대 전에 PLAN_READY 분할/설계 delta로 정지**한다. 목표는 BO 약+950, BA 약+510으로, 각각 여유200줄을 예약해도 분할선 아래다. 실제 head에서 지정 도구로 측정한다.

`python3 /Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py origin/main HEAD`

B-적용은 B-관측이 main에 착지한 뒤 새 main을 base로 한다. 각 리뷰 단위의 cap을 따로 측정하고 전체 합계도 숨기지 않는다. 계획 전체의 중복 제거 예상은 **32 prod 파일/순증+1466**이며 두 순차 리뷰 단위로 나눈 수치다(BO 23파일, BA 14파일, 중복5). 전체 B가 한 리뷰 단위의 30파일 안에 든다고 주장하지 않는다. production/source-observation API를 fixture로 대체하거나 증명을 줄여 cap을 맞추지 않는다. 필요하면 관측 안에서 writer obligations와 context snapshot을 별도 리뷰 단위로 나누는 delta 계획을 먼저 낸다.

### B-관측 — 23 prod 파일, 순증 약 +953 (예비 포함 목표 ≤+1153)

| 대상(모두 `src/services/` 아래) | 예상 순증 | 변경 |
|---|---:|---|
| `discord/health/o_transition.rs` | +25 | typed provider/target/home adapter 연결 |
| `discord/health/o_transition/automatic.rs` | +45 | provider/run 선택, Codex Inspect와 진단·Apply 차단 |
| `discord/health/o_transition/codex_context.rs` 신규 | +160 | legacy/Herdr current target 검증 |
| `discord/health/o_transition/codex_census.rs` 신규 | +110 | owner/host/context pane 전수 관측·coverage |
| `tui_o/writer/transition_port.rs` | +35 | typed provider/context·진단/관측 결과 |
| `tui_o/writer/host/transition.rs` | +12 | port에 실제 provider/run identity 연결 |
| `tui_o/writer/actor/transition_snapshot.rs` | +35 | Codex branch/Common, retry-inclusive owed |
| `tui_o/writer/actor/codex_history.rs` 신규 | +160 | held/expected/visited/Pending fold와 final local read |
| `tui_o/writer/codex_snapshot.rs` 신규 | +80 | Complete/Unknown 및 canonical identity 타입 |
| `tui_o/writer/transition_cleanup.rs` | +8 | snapshot 관측 필드; Codex judge 차단 유지 |
| `tui_o/writer/actor/transition.rs` | +20 | Inspect만 개방·authority 전달·진단 |
| `tui_o/writer/transition_scope.rs` | +15 | Codex observation identity 연결, Claude v1 유지 |
| `tui_o/writer/transition_scope/codex.rs` 신규 | +45 | canonical Codex identity encoding |
| `tui_o/writer/rotation/transition.rs` | +5 | 실제 reading count 읽기 전용 진단 |
| `tui_o/writer/deliver.rs` | +2 | 모든 O constructor에서 기존 POST Arc 확보 |
| `tui_prompt_dedupe/binding_context.rs` | −10 목표 | 작은 관련 읽기 helper를 자식으로 이동·inventory export |
| `tui_prompt_dedupe/binding_context/inventory.rs` 신규 | +80 | strict read-only context inventory(옮긴 helper 포함) |
| `tui_prompt_dedupe/runtime_binding.rs` | −15 목표 | 관련 source 비교 helper를 binding_access로 이동, 좁은 API export |
| `tui_prompt_dedupe/runtime_binding/binding_access.rs` | +50 | raw 상태/TTL/withheld 관측(옮긴 helper 포함) |
| `tui_prompt_dedupe/binding_events/codex.rs` | +2 | 기존 private strict reader를 쓰는 child 선언 |
| `tui_prompt_dedupe/binding_events/codex/observation.rs` 신규 | +85 | full channel strict history/tail·ownership 관측 경계 |
| `tui_o/writer/mod.rs`, `tui_o/writer/actor.rs` | +2, +2 | 비-giant 자식 모듈 배선 |

별도 한계 처리 API가 이 예약치를 넘으면 구현 전 분할한다. `discord/health/o_transition/codex_*` child는 `transition_port`를 import/송신하지 않는 관측 데이터만 반환하고, 허용된 부모 `o_transition.rs`에서 Context로 조립한다. 따라서 sender allowlist를 넓히지 않는다.

### B-적용 — 14 prod 파일, 순증 약 +513 (예비 포함 목표 ≤+713)

| 대상(`src/services/` 아래) | 예상 순증 | 변경 |
|---|---:|---|
| `discord/health/o_transition/automatic.rs` | +15 | 기존 sender의 targeted Inspect/AutomaticApply 연결 |
| `discord/health/o_transition/automatic/termination.rs` | +30 | provider·last binder·same/other-pane 분리 |
| `discord/health/o_transition/automatic/codex_retention.rs` 신규 | +105 | 후보별 local retention 근거 |
| `tui_o/writer/actor/automatic_age.rs` | +12 | 공통 exact reader와 provider dispatch |
| `tui_o/writer/actor/codex_age.rs` 신규 | +110 | native dialogue 분류·Unknown 이유 |
| `tui_o/writer/actor/automatic.rs` | +12 | age/idle 및 근거 결합 |
| `tui_o/writer/actor/transition.rs` | +25 | final identity/evidence/age 재검증·write 연결 |
| `tui_o/writer/transition_cleanup.rs` | +15 | Codex complete/evidence 필수 judge |
| `tui_o/writer/transition_scope.rs` | +12 | Codex v2 approval를 BA에서 허용 |
| `tui_o/store/spool.rs` | −30 목표 | audit 관련 기존 타입을 자식으로 이동/re-export |
| `tui_o/store/spool/retention.rs` 신규 | +100 | 기존 audit 타입+optional typed retention evidence |
| `tui_o/store/spool/transition.rs` | +10 | 기존 atomic CAS, Codex retention evidence 필수 |
| `tui_o/writer/actor/codex_retention.rs` 신규 | +95 | actor final 후보 근거/typed audit 조립 |
| `tui_o/writer/actor.rs` | +2 | 비-giant 자식 배선 |

**giant 원칙:** baseline raw line 수는 `tui_prompt_dedupe/binding_context.rs=1161`, `tui_prompt_dedupe/runtime_binding.rs=1049`, `tui_o/store/spool.rs=1200`이다. 각 파일은 raw 증가0 이하가 필수다. 위 관련 helper/type의 기계적 이동으로 배선 공간을 만들고, 의미 없는 줄 압축·빈 줄 삭제로 cap을 꾸미지 않는다. `tui_o/writer/deliver.rs=961`, `tui_prompt_dedupe/binding_events.rs=999`도 주의한다. deliver 변경은 좁은 생성자 교체뿐, `tui_prompt_dedupe/binding_events.rs`는 hunk0이며 Codex child에서 읽기 경계를 둔다. `tmux_common.rs=3777`과 다른 giant 생산 파일도 hunk0이다.

**FILES/겹침:** i5340_s3act completed에 따라 `tui_o/writer/**`는 이 레인 소유로 확인됐다. `brief-rca.md:39-43`의 활성 소유 목록인 `cluster/**`, `db/intake_outbox*`, `runtime_bootstrap/{queued_recovery,framework_setup,gateway_runtime}.rs`, `discord/commands/control.rs`, `discord/inflight/removal*`, `turn_orchestrator/**`, `tui_input/human_frame/**`, `discord/health/turn_deliver/inject*`, `session_host/herdr*`, `*/herdr_turn*`, `provider_dispatch*`, `discord/http.rs`, `task_notification_delivery/**`, `message_outbox*`, `session_relay_sink*`에는 **hunk 0**이다. 이 표의 관측 API/adapter만으로 구현할 수 없고 그 파일 변경이 필요해지면 FILES로 멈춘다. 이번 계획은 어느 소유 파일도 수정하지 않았다.

시험 파일은 production 수와 별도이며 actor/provider integration·age/census/scope/store child 시험을 붙인다. required test-lane 행 추가는 사전 허용 범위, 생성물은 공식 생성/동기화 명령만 사용한다. 이 단계에서는 시험 목록·생성물도 수정하지 않았다.

## 11. 리뷰 집중 항목과 남은 한계

- **경계 시각:** 7일 inclusive ±1ns, quiet10초, local retry due30초의 서로 다른 경계. old context의 created_at 기반 prune와 last-dialogue 기반 age는 다른 시계다. 누락은 AgeUnknown/후보 Unknown이며 정책을 낮추지 않는다.
- **경합:** final borrowed authority와 source publication의 실제 코드 경계를 사용한다. transition lock 전칭 보장을 주장하지 않는다. run/provider/home/context/actual source/held/census/full tail/token/path를 재비교하고 변경 시 write0을 검증한다. 추가 live/unknown pane을 first B에서 보류한다. 다른 pane/process/Herdr row 전체를 원자적으로 고정하는 신규 계약은 이번 범위 밖이다.
- **옛 형식:** 기존 Claude scope v1/cursor/audit, same-source resume의 P5 생략, seq 없는 legacy hop의 unique strict match를 유지한다. unknown/빈 nonce/사라진 historical context는 보존한다. End-only에 Seal 없음은 정상이며 소급 생산하지 않는다.
- **문서만 있는 가드 금지:** Complete는 실제 producer/context/runtime/P5/census를 통과해야 한다. BO의 production caller와 identity를 먼저 증명하고 BA는 그 타입 없이는 적용할 수 없어야 한다. fake snapshot 양성만으로 설계 완료하지 않는다.
- **false 0/PASS:** reading과 old-unsettled, queue와 retry, unobserved POST와0, null tail과0, Unknown과Eligible, Eligible과실제 Retired, Retired와health clear를 각각 구별한다. test0/skip/compile failure mutant는 성공으로 세지 않는다.
- **[U-운영 적격성]** GET으로 현재 count·각 old source age/binder/context 상태를 확인하지 못했다. BO 배포 후 production Inspect 진단을 코디네이터가 읽고 어떤 source가 Complete/Unknown인지 확인해야 한다. 모두 Unknown이면 BA 배포만으로 건강 회복을 약속하지 않고 원인별 다음 설계 delta를 낸다. Unknown을 empty로 바꾸는 우회는 없다.
- **[U-보존된 historical 증거]** same-pane nonce-supersession 양성은 old context 소실 때문에 자동으로 막지 않는다. other-pane의 exact old host/root 근거가 소실된 경우는 명시적으로 남을 수 있다. 이를 해결하려면 별도 증거 수명 설계가 필요하며 이번에 새 cleanup/attestation/GC 변경 권한을 만들지 않는다.
- **상한:** BO 목표+953/23파일, BA 목표+513/14파일은 인터페이스 확정 전 추정이다. 각 단위 +1200 예상에서 분할 계획으로 정지한다. giant raw 증가·소유 겹침·required test 선택을 희생하지 않는다.

## 12. 이번 실행의 증거와 완료 경계

- 운영 GET 원본: `logs/b-r2/{docs-health,docs-discord,docs-monitoring,health,health-detail,channel,bindings,watcher-state}.json`.
- 이슈 원문: `logs/b-r2/issue-{5071,7268,7308}.json`. 좁은 관측 요약은 `logs/b-r2/observation-summary.json`, 원시 응답 SHA256 목록은 `logs/b-r2/evidence-sha256.txt`다. r1 리뷰 원문과 현재 기준 source의 file:line을 직접 대조했다. 코드 사실 조사 3개는 병렬 읽기 전용으로 수행했으며 독립 설계 리뷰를 대신하지 않는다.
- 금지 설정·launchd.env 읽기/grep/출력/쓰기0, 운영 DB 직접 접근0, 운영 상태 파일 직접 읽기0, 운영 HTTP 쓰기0, runtime/store 쓰기0, tmux/herdr 제어0, GitHub/DAG 쓰기0, 코드 수정0, build/test0.
- 완료된 것은 **B 계획 r2와 읽기 전용 관측**이다. BO/BA 구현·배포·현재 backlog 정리·health 회복은 아직 수행하지 않았다. 다음은 이 문서의 WebGPT pro 독립 설계 리뷰다.
