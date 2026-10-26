DESIGN_BLOCKED 알림 “1회”의 계수 계약 확정 필요 — 단일 durable 알림과 Discord 실제 도착 exactly-once를 구분해야 함

# #7330 조사·설계 r1

작성: 2026-10-25 KST. worktree `/home/adk/adk/wt/i7330`. HEAD와 조사 시점 origin/main은 모두 `f3724fb6366599a9041bcc63d267be668991c799`이다. 이하 상대 경로의 file:line은 이 SHA 기준이며, 제안 파일·설정·시험은 **계획**이다. 소스 변경·커밋·push·GitHub/DAG 쓰기·운영 접근은 하지 않았다. Cargo 시험은 실행하지 않았다. 따라서 아래 GREEN/RED는 시험 설계와 예상이며 실행 성적이 아니다.

## 0. 결론과 리뷰 진입 조건

권고 구현은 기존 `intake_outbox` replay receipt를 유일한 재실행 처분 권위로 사용한다. provider effect 전에 `started_unclassified`를 commit하고, stale 때 같은 attempt의 완전한 no-effect 증거가 있을 때만 exact continuation을 1회 허가한다. 출력·도구·불완전 관측·재시작 복구는 `withheld`로 남긴다. 새로운 입력 owner나 주입 경로는 만들지 않는다. 활성화는 non-TUI effect에만 적용하며, off는 **새 비보호 요청**을 기준 main 경로로 돌리고 기존 보호 요청은 끝까지 보호한다.

설계 확정 전 코디네이터가 정해야 할 한 가지가 있다. 수용 문장 “출력·도구 뒤 stale이면 자동 재실행 0, withheld 처분과 알림 1회”에서 알림 1회가 다음 중 무엇인지다.

1. **권고 A: 요청당 durable 알림 이벤트 1개**, 정상 전달 시 Discord 알림 1개. 전송은 기존 outbox 재시도 계약을 사용하며, POST 성공 후 sent ACK 전 crash의 전달 불명은 별도로 보고한다. 재실행 0의 불변식은 이 불명과 무관하게 유지한다.
2. **B: 모든 crash·다른 노드·장시간 재개에도 Discord 실제 도착 정확히 1개.** 현재 알림 transport만으로는 증명할 수 없다. 실제 Discord anchor를 effect 전에 확보·영속화하고 그 anchor만 멱등 편집하는 등 별도 출력 계약이 필요하다. synthetic anchor의 headless까지 이를 의무화하면 provider 시작/첫 출력 전 경로도 바뀐다. 이 출력 계약의 범위 승인이 먼저다.

이 문서는 A를 **미승인 권고안**으로 설계하고, B의 반례와 대안을 §2.5에 분리한다. “outbox 1행=Discord 1도착”으로 몰래 축소하지 않는다. 이 불명확성을 남긴 채 DESIGN_READY나 활성화 PASS를 내지 않는다. 나머지 조사·분할·시험 설계는 아래에 완료했다.

off의 권고 계약도 명시한다. 기존 withheld를 off로 해제하는 의미의 완전 과거회귀는 “재시작·다른 노드에서 withheld 요청 재실행 0”과 양립하지 않는다. **기존 행은 보호, 새 요청 producer만 off**가 수용 가능한 rollback 정의다. destructive 해제·migration down·과거 row 삭제를 설계하지 않는다.

### 원문과 확인 방법

- [#7330 본문](https://github.com/itismyfield/AgentDesk/issues/7330): TUI 밖, 2B-recovery/2C, 2B-inject b 이관, 완료 조건 5개를 읽었다. `gh issue view 7330` 시도는 `gh: command not found`(127). 연결 GitHub `fetch_issue`로 원문을 읽고 `logs/github-7330.json`에 저장했다. #7330 댓글은 본문 메타데이터상 0개다.
- [#7272 본문](https://github.com/itismyfield/AgentDesk/issues/7272), [6조각·durable owner 결정](https://github.com/itismyfield/AgentDesk/issues/7272#issuecomment-6354419914), [HTTP·루틴·headless의 TUI 유입 흡수/복구 예외](https://github.com/itismyfield/AgentDesk/issues/7272#issuecomment-6374812566), [bootstrap receipt·stash 조정](https://github.com/itismyfield/AgentDesk/issues/7272#issuecomment-6375403783)을 읽었다. 원문은 `logs/github-7272.json`.
- 추가 대조: `/home/adk/adk/lane-runs/i7272/design-r2.md:205`(Submit/BootstrapAndFirstInput), `:264`(입력 writer와 완료 관찰 분리), `:311`(지원 범위), `:336`의 headless 유지 문구는 위 후속 댓글의 **TUI로 들어오는 headless는 이관** 결정으로 보정한다. 이 문서의 기준 SHA는 `794e7123e1749db0dea10b638a2af84d7cc8c191`이므로 그 좌표를 현재 소스로 오인하지 않는다.
- 적용 스킬: `/home/adk/skills/agentdesk-issue-pipeline/SKILL.md`. DAG 갱신은 사용자 브리프가 코디네이터 전용으로 제한하므로 하지 않았다. `scripts/pr_cap_prod.py:14`는 `FILES, NET = 30, 1500`이다.

### main에 이미 있는 것: 재구현하지 않을 기반

`git log origin/main -- src/db/replay_disposition`만으로는 6개가 모두 나오지 않는다. 최초 migration/consumer와 input-b는 이 하위 디렉터리를 바꾸지 않았기 때문이다. 이 명령의 원시 결과를 `logs/replay-history.txt`에 보존하고, `git log origin/main --grep='#6008'` 및 파일 대조로 다음 **기능 6 PR**을 확정했다. 디렉터리 이력의 #7004(test-only registry 제거)·#6858(의존성)은 이 6개로 세지 않는다.

| PR | main commit | 이미 있는 권위/소비자 |
|---|---|---|
| #6766 piece 1 | `c8d0f217b2a657f3a8127ab1bd9b2cafec9e4796` | `migrations/postgres/0136_request_replay_disposition.sql:3`, `src/db/replay_disposition.rs:7`, restore hold와 DB consumer fence |
| #6801 2A-authority | `0edb837733f01c5fccdcc96f67bfc8d6629cfc6e` | `src/db/replay_disposition/write.rs:73`, `write/start.rs:272`, `migrations/postgres/0137_request_replay_exact_attempt.sql:7`, move-only permit |
| #6816 2A-policy | `6e7e4096cbe7e8e27c41cced6393a7af22dd8b05` | `src/services/discord/replay_policy/live.rs:49`, `replay_policy.rs:31`, direct Claude tap와 bridge 분기 |
| #6842 2B-input-a | `0ce3294098879c16c360f87358b78c184260636f` | `router/intake_dispatch/replay_admission.rs:179`, `router/message_handler/intake_turn/replay_start.rs:27` |
| #6861 2B-input-b | `c071986da4be60375277ac5990c453538ae216f2` | deferred/consumed typed settlement, worker와 original custody 유지 |
| #6919 2B-inject a | `91d9bfa0730090217d68079e5abc22adad5413e9` | prepared projection/terminal classification 기반. production busy-pane 결합은 미활성·이번 범위 밖 |

#6174(`d6d33458902622ff1be1d91ae780e1c718aaf59f`)는 옛 #6008 wake 시험 기반이다. 그 완료를 #7330의 runtime 활성화 완료라고 세지 않는다. 현재 `replay_admission.rs:60–64`의 production supplier는 무조건 false다. 따라서 아래 보호 시험의 코드가 존재하는 사실과 production 수용 완료를 구분한다. #7330의 5개 수용 조건 전체가 main에서 완료됐다는 주장은 하지 않는다.

## 1. 진입점 전수표

별칭: D=`src/services/discord/`, S=`src/services/`, R=`D/replay_policy.rs`, L=`D/replay_policy/live.rs`. `보호`는 token의 LiveAttempt 또는 canonical receipt/hold가 있는 경우다. `첫 출력`은 Discord 청크 게시 시각보다 앞선 **provider raw 활동 관측**을 기준으로 한다. 도구는 실행 완료를 기다리지 않고 ToolUse부터 위험으로 센다.

### 1.1 stale 분류와 bridge 재실행

| ID | main file:line / 진입 함수 | 현재 첫 출력 전/후·도구 구분 | 이번 처분/배선 |
|---|---|---|---|
| S1 | `D/turn_bridge/stale_resume.rs:21`, `:67`, `:132` 문자열·result·Error 검사 | 검사기 자체는 시각·도구를 모름. assistant 본문을 검색하지 않고 error/result만 봄 | 좁은 phrase 집합 유지. 활동 판정을 문자열 검사에 섞지 않음 |
| S2 | 같은 파일 `:106` output file offset 이후 result 검사 | 해당 offset 이후 stale 유무만 앎. 파일 EOF는 child 종료 증거가 아님 | file scan만으로 no-effect 허가 금지 |
| S3 | `D/tmux_output_stream.rs:469` result parser | stale bool 누적. 다른 tool/body 상태는 별도 보유하나 stale 분류에 결합 안 됨 | non-TUI wrapper 관측을 canonical policy로 연결. TUI reader는 이 레인에서 변경 안 함 |
| S4 | `D/turn_bridge/stream_loop/content_arms.rs:426–457` Error arm | legacy는 출력·도구와 무관하게 stale→resume_failure. 보호 시 `error_is_ordinary`로 먼저 gate | stale raw flag를 ordinary terminal 분류보다 먼저 고정. body 덮어쓰기·세션 reset 전에 처분 |
| S5 | `D/turn_bridge/terminal_outcome_delivery.rs:275–307` explicit stale→retry | `full_response`가 있어도 진입. 별도 activity gate 없음, 아래 공통 policy에 의존 | 공통 typed 결정의 Held/RetryQueued/Legacy 결과를 소비 |
| S6 | `D/turn_bridge/terminal_outcome_delivery/empty_response_recovery/handler.rs:193–285` 빈 응답→file 회수→첫 stale scan | 진입 때 empty였어도 회수 후 body가 생길 수 있음. tool 이력 조건 없음 | 관측된 body/도구를 지우기 전에 hold. 복구 body 비움 금지 |
| S7 | 같은 파일 `:289–322` 두 번째 file scan | 첫 scan 직후 파일 성장 경합을 다룸. 활동 구분 없음 | S6와 같은 처분. 두 scan이 알림/재시도 2개를 만들 수 없음 |
| S8 | 같은 파일 `:207–219`, `:334–369` attempted resume + disconnected + no Init 또는 <30초 backstop | 본문 empty만 앎. thinking·tool만 있었던 경우 empty일 수 있음 | no Init·짧은 시간·empty는 no-effect 증거 아님. live complete 필요 |
| S9 | `D/turn_bridge/retry_state.rs:99–152` 공통 retry_stale_resume | legacy reset+본문 clear+history retry. 보호는 R의 raw 관측으로 Exact/Hold | 새 policy를 여기에 모음. Held면 full_response/session/offset/dispatch를 유지하고 알림 intent 보장 |
| S10 | `R:243–279`, `:324–349`, `L:132–145` | 보호에서는 output/thinking/tool/unknown activity, EOF/wait/stderr를 구분. legacy는 무조건 Legacy | 기존 판단 재사용. `retry_of`가 있는 attempt의 두 번째 startup retry를 명시 거부 |
| S11 | `D/turn_bridge/retry_state.rs:23–73` spawn retry→gateway | 자체 activity 지식 없음. move-only exact 또는 legacy text를 전달 | canonical 결과 없이 protected legacy retry를 부를 수 없도록 타입/DB gate |
| S12 | `D/gateway.rs:674–727` DiscordGateway history/exact retry | gateway는 활동 모름. exact는 router continuation에 넘김 | exact lineage 유지, 별도 재실행 허가 발급 금지 |
| S13 | `D/turn_bridge/recovery_text.rs:762–905` history 조회·KV write·enqueue | 활동 모름. `RETRY_CLAIMS`는 이 프로세스의 bridge와 watcher 경합만 막음 | I/O 전 canonical source lookup 추가. off에서도 기존 보호 source를 검사. history fetch 후 enqueue admission도 fence |
| S14 | `D/tmux_watcher/pre_emit_guard.rs:293–411` stale watcher→kill/reset/notice→history scheduler; `D/tmux_overload_retry.rs:31` scheduler | stale bool만 사용. output/tool 뒤인지 gate하지 않음. host kill guard와 재실행 처분은 별개 | non-TUI wrapper만 canonical fence를 **kill/reset/notice 앞**에서 소비. TUI arm은 #7272와 별도 소유. protected면 watcher가 재시도·복구 notice를 독자 발급하지 않음 |

### 1.2 복구·재시작·타 노드·재접수

| ID | main file:line / 진입 | 현재 출력/도구·재시도 지식 | 설계 |
|---|---|---|---|
| B1 | `D/turn_bridge/stream_loop/content_arms.rs:292–297` `__session_died_retry__` Done sentinel | synthetic Done이며 raw provider 완료가 아님. 활동과 무관하게 recovery_retry 설정 | RawTap의 provider terminal로 세지 않음 |
| B2 | `D/turn_bridge/terminal_outcome_delivery.rs:183`; `terminal_outcome_delivery/recovery_retry.rs:55–95` | projected hold면 skip, 보호 live면 `R:354` hold_recovery. legacy는 reset/history retry | canonical hydrate를 barrier로 유지. 복구 sentinel에서 새 no-effect permit을 복원하지 않음 |
| B3 | `D/recovery_engine/restore_inflight.rs:70`, `:104–123` boot restore | canonical hydrate 후 hold면 debt delivery만. byte row에 activity가 없다고 no-effect로 보지 않음 | 새 자식 restore policy에 read-only 검증/필요 시 exact recovery hold를 위임 |
| B4 | `D/recovery_engine/restore_inflight/replay_hold.rs:24–78` receipt 또는 sources lookup | receipt 투영 있으면 lookup 실패 시 중단. **투영 없는 legacy row는 lookup 오류 때 projected=false→진행**(`:19`, `:61`) | 활성 non-TUI/보호가능 row에서는 Unverified면 보존·재조회, 자동 재실행 0. off+확인된 무보호 legacy만 baseline 처리 |
| B5 | 같은 파일 `:104–164` held captured body delivery | body/UTF-8 cursor/identity·mailbox 소유권 검사. provider 실행 안 함. stale 알림은 없음 | body debt와 stale notice는 별개. held라는 이유로 already-visible prefix를 덮어쓰지 않음 |
| B6 | `D/recovery_engine/restore_inflight.rs:2209–2258` restored reader Died→sentinel | 살아 있던 writer의 과거 tool 효과를 모름. 살아난 프로세스에는 live permit 없음 | B3/B4에서 보호 요청을 차단하고 B2에서도 재검사 |
| B7 | `D/router/intake_dispatch.rs:145–172` dormant source admission | canonical PG source set이 전부 blocked면 consumed, 혼합/lookup 실패는 blocked | 이미 main에 있음. off에서도 유지. 새 terminal source ring으로 대체 안 함 |
| B8 | `D/router/intake_dispatch/replay_admission.rs:254–309`, `:355–402` active preflight/register/begin | key/전체 sources/unknown state/CAS 판단. output 자체를 알 필요 없음 | active와 off 모두 기존 receipt 소비. off는 새 register만 금지 |
| B9 | `D/router/message_handler/intake_turn/replay_start.rs:49–68`, `:106–120`; `intake_turn.rs:2110–2121` | dormant fastpath는 DB lookup 없이 시작. exact reset은 next begin commit 뒤 | non-TUI만 active supplier 적용. off에서도 **기존 보호 여부 검사 앞의 unconditional dormant fastpath**를 막음 |
| B10 | `D/router/message_handler/headless_turn.rs:889–901`, `:1062`; `D/health/headless_turn.rs:16`, `:62`, `:100` | 현재 active 채널은 protected start 부재로 **실행 거부**. 활동 tap도 이 지점에서 부여 안 됨 | non-TUI headless용 자식 admission adapter가 같은 receipt API를 호출. TUI headless는 #7272 owner로 남김 |
| B11 | `D/router/message_handler/provider_dispatch.rs:215–227` 첫 effect permit spend | permit consume가 token에 live attempt 부착. 현재 driver 범위를 구분하지 않음 | 준비한 driver/host와 prompt를 sealed projection에 포함. final effect 직전 같은 driver인지 확인 |
| B12 | `D/gateway.rs:892–953` HeadlessGateway | synthetic send/edit는 성공처럼 반환, history retry는 실제로 suppress. exact override도 없음 | 이 gateway의 성공을 외부 알림/재시도 실행 증거로 세지 않음. non-TUI exact headless continuation은 기존 headless context를 보유한 자식 adapter로 인계 |
| B13 | `src/db/intake_outbox/worker_status.rs:89–123`, `:145`, `:188`, `:381–388`; `S/cluster/intake_worker.rs:432–463`, `:644–714` | 타 노드 claim/reclaim/accept/spawn에서 PG source fence와 stamped owner 검사. 원 sources를 row에서 복원 | 기존 권위 재사용. 새 incarnation은 runtime owner일 뿐 request key를 remint하지 않음 |
| B14 | `src/db/intake_outbox.rs:285`, `:363`; `0136…sql:139–218`, `:236–376` | parent retry child, dispatch/dispatch_outbox/auto_queue/history rearm·삭제에 DB fence | 이미 main에 있음. 직접 SQL·old writer 경합 PG 시험으로 재사용 증명 |
| B15 | `S/auto_queue/runtime.rs:188`, `:542`; `0137…sql:44–90` | 세션과 dispatch projection held면 자동 재할당·세션 해제 거부 | off에도 유지. 채널의 새 source는 기존 fresh_handoff 예외로 진행 |
| B16 | `D/turn_bridge/terminal_outcome_delivery/delivery_epilogue.rs:55–61`; `completion_postlude.rs:286–290`; `completion_postlude/channel_writeback.rs:70` | ordinary classification ACK 전 dispatch 완료/실패·채널 writeback 억제 | held를 완료·실패→AutoQueue retry로 바꾸지 않음 |
| B17 | `D/inflight/model/identity.rs:107–155`; `D/inflight/model.rs:555–563`; `D/inflight/removal/scoped_restore.rs:4–15` | receipt/hold projection merge 및 옛 row 기본값. scoped generation invalidator는 **cfg(test)** | 실제 restore B3의 근거와 test-only invalidator를 혼동하지 않음. projection은 권위가 아닌 cache |

### 1.3 reader와 인접 재시도: 전수 검색에서 숨기지 않을 항목

| ID | main 좌표 | 현재 구분/범위 처리 |
|---|---|---|
| A1 | `S/claude.rs:1011`, `:1188`, `:1320`, `:1339–1487` | direct child reader만 RawTap 있음. output/thinking/tool/unknown/EOF/wait/stderr 구분. raw 관측은 parsing 후 bridge send 전. 재시작/타 노드는 이 tap을 복원할 수 없음 |
| A2 | `S/codex.rs:1225–1351`, 특히 `:1302`, `:1324` | direct Codex에는 RawTap 없음. provider JSON line/exit와 synthetic Done이 섞일 수 있음. 활성화 전에 non-TUI 전용 observer를 새 자식 모듈로 연결해야 함 |
| A3 | `S/claude/process_session_launch.rs:147`, `S/codex/process_session_launch.rs:59`, `:148`; `S/session_backend.rs:879–909`; `S/provider/output_reader.rs:38`, `:145–188` | 파일/ProcessBackend/legacy wrapper reader. 일시 EOF·ready synthetic Done이 있어 direct child의 complete 증거를 그대로 대입할 수 없음. 해당 runtime owner의 exact 종료+최종 descriptor drain 증거가 없는 경우 hold |
| A4 | `D/turn_bridge/stream_loop/provider_recovery.rs:10–99`, `:104–139` | profile fallback은 request!=0·tool 없음·partial empty 검사 있음. 그 뒤 agent_recovery checkpoint도 생성 가능. stale는 S4의 protected gate를 먼저 통과해야 함. recovery takeover에 별도 replay 허가를 추가하지 않음 |
| A5 | `S/provider.rs:1387–1416`→`S/gemini.rs:371–377`, `:706–764`; `S/qwen.rs:379–413`, `:1052–1125`; `S/qwen_tmux_wrapper.rs:277–304`, `:398–450` | provider 내부 session retry loop. 공통 loop는 output/tool을 모르고 provider final/watchdog의 RetrySession에 의존. #2090 bridge retry와 별개인 기존 동작이다. 이 레인의 activation이 이 loop까지 포괄한다고 선언하지 않음. 공통 stale 문구가 이 경로에서 bridge 전에 소비되는지 [U4]로 실제 stub 실측 필요; 적용 대상이면 이 loop도 같은 permit 소비자로 연결하는 설계 델타 후 활성화. 별도 retry 권위 신설 금지 |
| A6 | `D/turn_bridge/retry_state.rs:245–287` Gemini RetryBoundary reset | 이미 발생한 provider 내부 재시도의 local 표시 rewind. retry 허가 지점이 아님. 비움으로 과거 activity가 없어졌다고 판단하지 않음 |
| A7 | `D/recovery_engine/producer_resume.rs:139–157` | CodexTui 전용. 이번 non-TUI 권위와 교집합 없음. 파일을 수정하지 않음 |

원시 census: `logs/entry-search.txt`, `logs/consumer-search.txt`. 실제 main 호출부와 cfg 조건을 대조했다. 위 표의 A5는 미실측 경계로 남겼으며 “모든 provider의 내부 복구도 해결”이라고 주장하지 않는다.

## 2. 처분 권위와 알림 설계

### 2.1 요청·attempt identity와 상태 전이

유일한 처분 행은 `intake_outbox.id`이며 `replay_only=true`, status=`unknown`, request key, 전체 source IDs, 원문 hash, original text/attachments/reply-context, nonce, owner incarnation을 가진다. producer가 원문을 이미 보유한 intake delivery row가 있어도 새로운 **입력 owner**를 만들지 않는다. 기존 replay-only receipt와 그 delivery row의 책임 분리가 그대로다.

Discord는 기존 `replay_admission.rs:83–110`의 `(provider, channel, primary message)` key를 유지하고, headless는 **최초 reservation/dispatch/intake identity를 durable하게 기록해 재개 때 재사용**한다. body hash나 새 process PID/새 synthetic message ID로 “동일 요청”을 다시 이름 붙이지 않는다. `headless_turn`에 prompt가 이미 렌더되어 들어오므로 original과 prepared prompt를 섞어 hash하지 않으며, source/owner/agent/attachments/reply_context를 그대로 전달한다. metadata 계약이 없는 외부 호출의 새 reservation은 새로운 요청이지 과거 receipt의 retry가 아니다. 재시작/타 노드에서 옛 요청을 실행 대상으로 복원할 때는 반드시 기존 key/receipt를 사용한다.

| 상태 | 허가 | 다음 전이 |
|---|---|---|
| registered_not_started | 단일 begin CAS가 성공한 effect만 시작 가능 | started_unclassified / withheld |
| started_unclassified | 일반 재실행·재claim·cleanup 금지 | 완전 no-effect→startup_failed_no_effect, 정상 terminal→classified_normal, 나머지→withheld |
| startup_failed_no_effect | 일반 admission 금지. 살아 있는 exact continuation 1개만 next begin 가능 | next nonce started_unclassified / withheld |
| classified_normal | 해당 요청 완료. 같은 key 재시작은 prepare_effect_start에서 AlreadyClassified | 과거 요청 재등록 금지, 새 source만 허용 |
| withheld | provider 실행 0; captured body 전달/알림 재조정만 | 해제 전이 없음 |
| unknown spelling / 조회 불가 | no-effect로 간주 금지 | 원문/row를 보존하고 재조회 |

DB write ACK 뒤에만 permit을 반환한다(`write.rs:69`, `write/start.rs:335`). ACK 불명이면 start/retry permit 0. 재시도 예산은 `replay_retry_of_nonce`로 요청 생애에 남기며 `decide_stale`에서 `attempt.retry_of().is_some()`면 retry 0, exhausted hold를 선택한다. main의 `decide_stale:39`와 `begin:296–298`에는 두 번째 quiet attempt의 retry-of 검사 자체가 없으므로 “move-only permit 하나”가 “요청 전체 최대 재시도 1회”를 뜻한다고 오인하지 않는다. 추가 SQL guard로 NoEffect begin의 기존 retry-of가 NULL인 경우만 허용해 old caller도 같은 상한을 지키게 한다.

### 2.2 첫 출력 경계와 경합

- **효과 경계:** provider record를 읽어 bridge channel로 보내기 전에 attempt에 activity를 누적한다. Text/Thinking=Output, ToolUse/ToolResult=Tool, 해석불가·task event=Unclassified. ToolUse는 실제 부작용 여부를 낙관하지 않는 상한이다. Discord send 완료나 `full_response.is_empty()`로 첫 출력 전을 판단하지 않는다.
- **관측 봉합:** no-effect에는 해당 attempt의 stdout EOF + 성공 wait + stderr drain + Broken 없음 + activity 없음 + 성공 provider terminal 없음이 전부 필요하다. L:97–137 기반이다. 파일의 일시 EOF·Init 부재·no output timeout·현재 비어 있는 relay body는 충분조건이 아니다.
- **단일 결정:** 현재 L:241의 take()는 중복 결정자를 막지만, terminal/stale/reader 종료의 동시성 계약을 명시적으로 봉합한다. 자식 decision state에 Deciding/Settled와 sealed observation을 두고 stale mark/normal classification이 같은 봉합을 보도록 한다. complete 봉합 이후 raw record가 추가될 수 없게 reader 종료를 한 번만 publish한다. timeout/Broken 봉합은 hold만 허용한다.
- **우선순위:** raw stale result/Error를 ordinary terminal 분류 전에 남긴다. 특히 output file에서 뒤늦게 stale를 찾을 때 그 terminal을 이미 classified_normal로 commit한 뒤 Legacy로 되돌아가는 우회가 없어야 한다. file-only resumed reader는 live no-effect로 복원하지 않으며 recovery hold로 간다.
- **처분 뒤:** held면 본문·confirmed cursor·frozen chunks·session selectors를 유지한다. 새 kill/reset/delete 권한을 추가하지 않는다. 이미 실행된 도구를 되돌리거나 정리하지 않는다. `retry_stale_resume`가 typed outcome을 반환해 completion/empty-response/dispatch failure fallback이 held를 성공·재실행으로 재해석하지 못하게 한다.

출력 직전 stale라도 아직 writer/reader가 열려 있으면 무활동 증거가 완성되지 않았으므로 hold다. 출력 직후 send가 지연돼도 RawTap activity가 먼저 기록돼 hold다. reader drain 동안 새 record가 들어온 경우도 같은 판정이다. 이는 “첫 출력 전이면 무조건 재시도”가 아니라 **provider 시작 실패가 확정된 첫 출력 전 stale**의 기존 1회 성공 경로를 보존하는 방식이다.

### 2.3 hold commit와 보존물

현재 `R:49–56`은 hold reason/nonce만 저장한다. 이를 새 `src/db/replay_disposition/hold_notice.rs`(계획)로 위임해 하나의 PG transaction에서 다음을 수행한다.

1. receipt key·nonce·incarnation·현재 disposition을 CAS한다. 다른 attempt가 이겼으면 다시 읽고 기존 처분을 소비하며 새 실행/알림을 만들지 않는다.
2. `withheld`와 `replay_hold_reason`을 기록한다. `replay_preserved`에 schema version, 원래 요청 참조, 관측 요약, partial body, UTF-8 cursor, frozen/현재 message IDs, session/runtime identity, 기존 output descriptor/offset 참조를 **보존**한다. projection/원본 JSON은 병합 보존하며 덮어쓰지 않는다. 본문이 크면 기존 captured result 저장소의 durable 참조를 사용하되 commit 전에 참조 대상이 존재해야 한다. 임의 truncate를 보존 완료로 세지 않는다.
3. 알림 identity와 routing을 저장한다. identity는 `(receipt_id, notice_kind=stale_replay_withheld)`이며 receipt의 첫 hold nonce를 payload에 고정한다. 원문 hash·generation·rendered text·새 incarnation을 알림 key로 삼지 않는다.
4. 권고 A에서는 persistent outbox enqueue와 outbox ID 참조 저장도 같은 transaction으로 한다. `message_outbox.rs:246`의 on-tx helper를 재사용한다. enqueue 실패면 hold+notice 전체 transaction은 rollback하되 이전 `started_unclassified`가 계속 재실행을 차단한다. ACK 불명은 canonical read-back으로만 해소한다. “알림 실패→legacy retry” fallback은 없다.

예외: 이미 held인 행에 알림 참조가 없으면 row lock으로 동일 알림 intent를 한 번 보완한다. `write::withhold`를 반복 호출해 CAS 실패를 성공처럼 삼키지 않는다. read-back이 확인한 같은 receipt의 기존 held 상태는 멱등 성공으로 분류한다. original writer를 잃은 started row의 recovery→withheld는 새 non-executable recovery operation으로만 수행한다. exact tuple과 DB row lock을 검증하며 live StartPermit을 복원하거나 old incarnation을 새 incarnation으로 바꾸지 않는다.

### 2.4 재시작·다른 노드·유계 저장소

- 첫 begin commit 자체가 보호 시작점이다. local inflight 저장 직전 crash로 projection이 없어도 PG source lookup이 요청을 차단한다. 다른 노드는 같은 PG request key와 sorted source locks(`0136:95–103`)를 소비한다.
- 기존 held/started/no-effect receipt를 새 target node로 claim/retry insert/rearm하려 하면 `0136:127–218` trigger와 worker query가 거절한다. dispatch는 같은 transaction의 projection(`0136:220–234`)과 `task_dispatches_replay_fence`가 뒤따른다. local 파일이 없어도 PG 권위는 남는다.
- B4의 legacy source lookup 실패는 **활성 non-TUI 범위에서는 fail-closed**로 바꾼다. receipt 없는 row에 DB 오류를 영구 hold reason으로 기록하지 않으며 raw bytes와 custody를 보존하고 재조회한다. feature off에서 보호 row가 없다는 결론도 조회 성공 전에는 내리지 않는다.
- `0136:139–142`는 blocked receipt 삭제를 막는다. withheld→registered/null로 되돌리는 전이는 `:114–124`, `:158–177`이 막는다. 따라서 최근 N개 ring·TTL에 재실행 0을 맡기지 않는다. 원문을 가진 withheld는 자동 GC하지 않는다. 저장 증가 비용은 P2로 공개한다.
- `R:177` RETRY_CLAIMS는 유계 ring은 아니지만 프로세스 종료 때 사라진다. `recovery_text.rs:742` RETRY_PENDING은 30/120초에 해제된다. 둘은 최적화일 뿐 재시작 불변식의 근거가 아니다.
- `D/inject_disposition.rs:356–368` ring은 unreadable 복구/terminal retention으로 과거 항목을 잃을 수 있다. `turn_orchestrator/queued_head.rs:74–80` snapshot terminal identities와 #7272의 retained/TTL은 **입력 제출**의 소유다. 이 목록을 replay withheld/알림의 권위로 쓰지 않는다. ring을 넘기거나 파일을 잃어도 canonical receipt가 차단한다.
- outbox persistent sent sentinel은 `message_outbox.rs:684–690` GC 예외로 남는다. 그러나 failed/cancelled는 7일 GC(`:676–680`) 대상이고 unique index에서도 빠진다(`:275–277`). 따라서 receipt 안의 알림 ID·상태가 재-enqueue 여부의 정본이다. terminal failed를 새로운 ID로 재enqueue하지 않고 같은 ID의 기존 redrive만 허용한다. 해당 ID가 사라졌으면 전달 불명으로 보존하고 두 번째 알림을 새로 만들지 않는다. 이때 “도착 1회”가 보장된다고 주장할 수 없는 이유가 다음 절이다.

SQL의 unknown-spelling은 Rust와 별도 주의가 필요하다. `Disposition::Unknown`은 차단하지만 `0136:62`와 source predicate `:71`은 알려진 blocking 상태 열거식이다. 현 check constraint가 정상 row의 unknown을 거부하므로 현재 임의 future 상태를 운영 사실로 만들지 않는다. 향후 schema 확장/복원 시험에서는 decode unknown을 legacy/null로 바꾸지 않고 read 경계에서 hold한다. SQL 자체 변경이 필요하면 migration 조각에서 둘의 semantics를 맞추되 현재 constraint·index 사용도 함께 검증한다.

### 2.5 알림 1회: 보장 가능한 부분과 차단 사유

현재 `retry_stale_resume:151`의 Hold는 no-op이고 사용자용 stale-withheld 알림을 발행하지 않는다. `warn_replay_held`는 tracing만 한다. 새 알림은 기존 “자동으로 이어갑니다” 문구를 쓰지 않고 “출력 또는 도구 활동이 있어 자동 재실행을 보류했습니다. 원 요청과 부분 결과를 보존했습니다.”라는 의미로 고정한다. provider별 상세 stderr/원문은 채널에 다시 노출하지 않는다. `message_outbox/identity.rs:36`에 이 알림의 non-turn provenance를 등록해 허용 bot loopback도 새 입력이 되지 않게 한다.

**A에서 증명할 것:** 동일 receipt에 notify event 1개, canonical outbox ID 1개, 여러 bridge/file scan/recovery/node가 동시에 접근해도 추가 enqueue 0. `sent` 후 무기한 재조회·cache eviction·restart를 해도 새 event 0. 실제 mock HTTP drain까지 실행하여 정상 전달 message ID 1개를 확인한다. enqueue 개수만 보고 전달 PASS를 내지 않는다.

**B의 구체 반례:** `src/server/mod.rs:2500–2506`은 `deliver(row)`가 200을 반환한 뒤 별도 `mark_message_outbox_sent_pg`를 수행한다. Discord가 POST를 반영한 직후 worker가 종료되면 PG row는 sent가 아니다. 새 노드가 stale claim을 회수하여 같은 row를 전송한다. persistent dedupe는 enqueue 중복만 막고 이 재전송을 막지 않는다. `manual_delivery/headless_nonce.rs:17–25`, `:65–72`는 headless_turn source와 별도 env opt-in에만 nonce를 부여하므로 새 stale notice source에는 기본 적용되지 않는다.

nonce를 확대해도 영구 exactly-once의 근거는 되지 않는다. [Discord 공식 Message API](https://github.com/discord/discord-api-docs/blob/main/developers/resources/message.mdx#create-message)는 enforce_nonce의 uniqueness 범위를 “in the past few minutes”로 제한한다(2026-10-25 열람). **nonce 확인 구간 뒤 재시작한 항목**이 바로 유계 자료구조의 밀려난 항목에 해당한다. 영속 receipt/outbox가 남아 있어도 외부 메시지 수의 영구 상한은 이 API만으로 나오지 않는다.

반대로 durable `attempted` latch를 POST 전에 소비하고 재송신을 전혀 안 하면 latch commit 후 POST 전 종료에서 알림 0이 된다. 이것도 알림 1회가 아니다. 이 반례를 “단일 운영자니 항상 PASS”로 감추지 않는다. 지속 스톨을 막는 신규 watchdog/kill 같은 권한은 추가하지 않는다.

**해소 방향/최소 계약:** 권고 A를 채택하면 1회는 durable 사용자 알림 이벤트 수로 명시하고 외부 전달 불명은 별도 수용/관측 항목으로 둔다. B를 유지하면 모든 대상 요청에 real Discord anchor를 사전에 보장하는 출력 설계를 별도 조각으로 추가하고, 이후 동일 anchor만 edit한다. 실제 anchor가 없는 headless reservation은 `router/turn_start.rs:171–175`의 synthetic IDs이므로 그대로 edit할 수 없다. `HeadlessGateway`의 fake success도 anchor 증거가 아니다. real anchor를 provider 시작 전에 보장하면 synthetic/무출력 headless 경로의 동작 범위가 바뀐다. **코디네이터 결정 전 이 두 안을 임의 혼합하지 않는다.**

고치지 않고 A를 B라고 표시하면, 전송과 sent 사이의 단일 재시작에서 알림이 중복되거나 전송 전 latch 방식에서는 0개가 될 수 있다. 증명 시험은 mock POST 반영 직후 sent write를 차단하고 다른 worker로 회수하는 crash-cut 시험이다. 재시작 지연은 가상 시각으로 nonce 구간을 넘겨 검사한다. 이 문서의 BLOCKED는 일반 상업 서비스 기준의 신뢰성 확대가 아니라 원문 “알림 1회”의 계수를 몰래 바꾸지 않기 위한 계약 질의다.

## 3. 첫 출력 전: 재시도 1회·응답 1회

기존 e2e는 `D/tui_prompt_relay/relay_e2e/stale_resume_retry_e2e.rs:13`의 `stale_resume_retry_turns_first_placeholder_into_notice_and_answers_once`다. FullEvent→실제 intake→provider stub→bridge→REST mock을 통과한다. `:21`은 StaleResumeThenSuccess, `:58–63`은 추가 retry 대기 창, `:72–98`은 recovery notice·ok·summary 및 reply target/unhandled REST 검사를 수행한다. **현재 파일에 있는 assertion이 실제 실행된 것은 아니다.**

유지 방법:

1. off/무보호 경로는 기존 classifier·history context·placeholder notice 순서를 유지한다. 이 시험은 기존 행태의 회귀 gate다.
2. on non-TUI에서는 원 attempt의 complete no-effect만 startup_failed_no_effect로 commit한다. retry_of가 없을 때만 permit 1개를 만들고, gateway→`router::start_exact_retry`→`hand_over` task-local→동일 receipt next begin으로 전달한다. old selector DB reset은 next begin transaction 안(`write/start.rs:317–333`), in-memory reset은 `replay_start.rs:111–120` 뒤에만 한다.
3. 재시도 때 original text/source/owner/reply_context를 유지한다. history는 `original_hash`를 바꾸는 request로 만들지 않고 기존 retry prompt context로 붙인다. next prompt hash는 준비된 projection에만 고정한다. 첫 실패는 ordinary dispatch failure·success footer·답변으로 정산하지 않는다.
4. headless exact retry는 stateless HeadlessGateway default로 버리지 않는다. non-TUI headless context를 가진 adapter가 같은 reservation의 receipt/원문/metadata/session routing을 보유해 기존 headless 실행 진입으로 인계한다. 새 synthetic identity를 발급하거나 Discord 일반 input으로 바꾸지 않는다. off 상태에서 현재 suppress하던 진짜 HeadlessGateway의 동작을 main의 “retry1”이라고 허위 보고하지 않는다.
5. retry가 다시 stale면 무활동이어도 exhausted hold, 추가 provider effect 0. `startup_failed_no_effect`에서 crash로 in-memory continuation이 소실되면 재시작이 permit을 재발급하지 않는다. 이는 첫 실패 뒤 crash 없이 이어지는 e2e 성공 경로와 별개다.

on 회귀 시험은 기존 e2e를 그대로 유지한 채 non-TUI driver와 production supplier를 명시 선택한 별도 변형을 만든다. 실행 횟수는 provider spawn/input effect counter로 정확히 2(실패1+성공1), 성공 reply는 원 source에 1회, retry가 계속 실패하는 변형은 총2/추가0을 검사한다. active 테스트에서 수동으로 token에 LiveAttempt를 끼우고 활성화 증거로 대체하지 않는다.

`prepare_effect_start:390–393`은 exact retry에 session_key를 필수로 요구한다. headless에 binding된 session row가 없는 조합은 사전 준비에서 session identity를 확보해야 한다. 기존 없는 session을 임의 생성하거나 fixture만 넣어 일반화하지 않고 [U3]의 실제 진입 시험으로 확인한다. bound session 없는 경우도 retry1이 필수라면 sessionless exact begin 계약을 추가 검토해야 한다.

## 4. 활성화와 kill-switch

계획 설정은 `AGENTDESK_STALE_REPLAY_V1=1`과 `AGENTDESK_STALE_REPLAY_CHANNELS=<명시 채널 목록>`이다. unset/0/false/invalid는 off, 목록 없음/잘못된 값은 새 producer 0. 운영 설정 파일은 읽거나 바꾸지 않았다. 이름은 구현 전 충돌 검색 후 확정한다.

supplier는 새 `D/replay_policy/activation.rs`(계획)에 둔다. 기존 `ReplayAdmissionContext::for_channel`의 channel-only flag를 그대로 켜지 않는다. admission 요청에 final **provider/driver/runtime scope**를 전달해 LegacyPrompt의 non-TUI effect만 보호한다. `ProviderSessionDriver::{LegacyPrompt,TuiHosting}`(`S/provider_hosting.rs:7–18`) 및 `RuntimeHandoffKind`(`S/agent_protocol.rs:68–73`)를 함께 사용하며, driver를 나중에 TUI로 바꾸는 경우 preflight를 재판정하고 기존 permit으로 실행하지 않는다. TUI 지원 여부를 “headless ingress” 이름으로 추정하지 않는다.

환경 변수는 process 시작 설정으로 취급한다. off rollback은 해당 변수 제거/0으로 **통상 배포·restart 절차를 코디네이터가 수행**하는 방식이며 이 레인이 restart하지 않는다. 환경 파일을 직접 열어 바꾸는 runbook은 제공하지 않는다. hot-reload 지원을 존재한다고 쓰지 않는다. 각 요청은 begin 때 선택된 mode/scope/version을 receipt projection에 고정한다.

| 전환 시 요청 | 처리 |
|---|---|
| off에서 새 요청, 보호 receipt 없음 | 기존 main 경로, 새 receipt/observer/notice 0 |
| on에서 등록 전 대기 중 off | begin 전 새 snapshot으로 판단. 기존 보호 source는 먼저 소비. 완전 무보호 신규만 legacy |
| registered_not_started에서 off | effect가 시작되지 않은 canonical 행을 임의 삭제하지 않음. 기존 registered request를 canonical start/drain하거나 보존한다. 정책은 신규 legacy로 재등록하지 않음 |
| started_unclassified/no-effect/withheld에서 off | 이전 보호 계약 유지. exact continuation은 기존 1회 예산 안에서 drain, live 잃으면 hold. withheld 실행 0 |
| classified_normal에서 off | 같은 key는 완료로 소비, 새로운 message는 legacy |
| 새 노드가 off 상태로 takeover | DB fence/read가 항상 켜져 이전 held를 보호. flag off를 canonical read 우회 허가로 쓰지 않음 |
| TUI ingress 또는 TUI headless | #7272 입력 owner·회복 계약 그대로. 이 flag로 replay/injection producer를 활성화하지 않음 |

**off가 main과 같은 범위:** 새롭고 무보호인 요청의 provider 입력·retry/response·history/dispatch 효과는 같아야 한다. 이미 보호 행을 가진 요청의 비재실행은 의도적으로 유지한다. 이 때문에 DB read 횟수까지 동일한 zero-IO rollback이라고 주장하지 않는다. 보호된 과거 요청까지 모두 main처럼 실행하라는 해석이면 계약 변경이므로 승인 없이 구현하지 않는다.

켜기 전 gate는 코드/시험으로 구현한다. final driver scope가 없는 callsite, raw observer가 없는 활성 대상, session-key retry 계약 미해결, A5 내부 loop 우회가 남아 있으면 **production supplier는 계속 false**다. 단순 문서의 “준비 완료” 문자열이나 env를 test-only override에 넣는 것으로 gate를 열지 않는다. off와 on 모두 production build selector를 직접 실행하는 통합 시험을 둬 `cfg(test)` 전용 활성화의 거짓 PASS를 막는다.

## 5. #7272 대조표: 권위 중복 0

#7272는 **입력을 언제·어디에·한 번 제출했는가**를 소유한다. #7330은 **non-TUI 실행이 시작된 요청을 stale/복구 후 자동 실행해도 되는가**를 소유한다. 입력 전달 receipt와 실행 후 stale disposition을 같은 의미로 만들지 않는다. 두 곳이 동일 request를 실행 허가하는 구조를 만들지 않는다.

| 권위 | #7272 소유/소유 예정: main file:line | #7330 권위/소비: main file:line | 중복을 막는 계약 |
|---|---|---|---|
| 접수·즉시 입력·보관 | `D/router/intake_gate/busy_inject.rs:198`, `D/health/turn_deliver/inject.rs:446`; `S/turn_orchestrator/injected_inputs.rs:168`, `:362`, `:414` | `D/router/message_handler/headless_turn.rs:889`; `intake_turn/replay_start.rs:27`의 **non-TUI** start만 | injected_inputs·queued_head에 새 replay owner/hold reason 저장소를 추가하지 않음 |
| head 예약·실제 paste 권한 | `S/turn_orchestrator/queued_head.rs:53–80`, `:237`, `:650`, `:742`; #7272 r2:208–215의 Submit/BootstrapAndFirstInput | `D/replay_policy/permit.rs:8–18`, `:73`; `provider_dispatch.rs:215`의 non-TUI ProviderStart | production BusyPane variant/permit을 부활시키거나 결합하지 않음(2B-inject b 제외) |
| 원문 retained·late input receipt·30초 확인 | #7272 r2:43–84, `queued_head.rs:63–80`, `queued_head/mend.rs` | `src/db/replay_disposition/receipt.rs:37–46`, `write.rs:73` | #7272 retained를 withheld로 중복 복제하지 않음. V2 metadata를 정리·삭제할 권한 없음 |
| TUI bootstrap·stash·actual native receipt | #7272 댓글 6375403783; r2:217–270; `D/health/turn_deliver/inject.rs:557`, `S/turn_orchestrator/queued_head/mend.rs:38`의 입력 직전 identity | raw non-TUI observer `L:49–59`; A1/A2/A3 | TUI Source 없음/Enter 미확인 문제를 이 레인에서 설계·수리하지 않음 |
| TUI로 들어오는 API·루틴·headless | #7272 댓글 6374812566; `D/health/headless_turn.rs:16`, `:62`, `:100`의 **TUI 선택 이후** | 같은 ingress의 **LegacyPrompt/non-TUI 선택 이후**만 | ingress 이름이 아니라 최종 effect driver로 소유 분리. 공용 parent 수정은 #7272 착지 후 delta로 순차 적용 |
| 재시작 보관 입력 | `S/turn_orchestrator/queued_head.rs:113`, pending queue snapshot; #7272 r2:136–159 | `D/recovery_engine/restore_inflight.rs:117–123`, `restore_inflight/replay_hold.rs:24` | TUI 입력 상태 복원은 #7272, 이미 시작한 non-TUI request 처분은 canonical PG. 서로의 Unknown을 retry 허가로 해석하지 않음 |
| 처분 행·DB fence | 공용 intake가 blocked source를 **소비**: `D/router/intake_dispatch.rs:157` | `src/db/replay_disposition/write.rs:73`; `write/start.rs:272`; `0136:127`, `0137:7` | 같은 DB authority 재사용. #7272는 입력 완료 상태를 근거로 PG withheld를 해제하지 않음 |
| 알림 | #7272 r2:128의 input-unconfirmed event | `R:49`→새 stale-withheld event와 같은 outbox API `S/message_outbox.rs:246` | notice kind·domain identity 분리. 한 요청의 같은 stale notice에 writer는 canonical txn 하나 |
| 진짜 복구용 /restart·healing | #7272 댓글 6374812566의 Immediate 유지 | `D/turn_bridge/terminal_outcome_delivery/recovery_retry.rs:55`; canonical hold gate만 | 복구를 새 사용자 입력으로 single-path에 재주입하지 않음 |

직접 소유가 겹치는 `injected_inputs.rs`/`queued_head.rs`는 이 레인 **수정 0**이다. 공통 headless/intake parent의 physical file 충돌은 권위 중복과 별개이며 구현 발사 전에 최신 #7272 head에 이 표를 다시 대조한다. 미래 #7272 최종 코드까지 지금 증명했다고 쓰지 않는다([U6]).

## 6. 리뷰 조각 계획과 giant 제약

다음은 계획 수치다. 아직 diff를 만들거나 pr_cap을 변경량 실측으로 돌리지 않았다. 모든 조각은 prod≤30파일/순 +1500, **+1200 도달 전에 분할 재평가**한다. 새 helper가 giant 밖으로 나와도 전체 diff cap에 포함한다. dormant 새 코드는 기존 production call graph의 no-op branch에서 참조하거나 test-only 지원만 cfg(test)에 두며 새 allow/baseline 인상은 하지 않는다.

| 조각 | 범위·주요 파일(모두 계획) | prod 파일 / 순증 예산 | 안전한 착지 상태 |
|---|---|---|---|
| P1 비활성 보존·알림 기반 | 새 DB `hold_notice.rs`, write child, 필요 migration, replay receipt read, outbox non-turn identity | 8–12 / +650~950 | production supplier false. A/B 계약 결정 뒤 notice 부분 확정 |
| P2 비활성 관측·단일 결정 | `replay_policy/live.rs`, 새 `live/seal.rs`, `replay_policy/decision.rs`; direct Codex child observer, Claude tap 재사용, non-TUI file/ProcessBackend observer | 12–20 / +850~1150 | 새 observer는 protected token에만 동작. 미지원 runtime은 activate 불가. A5 필요 시 독립 P2b(+500~950/8–14파일) |
| P3 2B-recovery | 새 `recovery_engine/restore_inflight/replay_decision.rs`, `replay_hold.rs`; `turn_bridge/retry_state/replay.rs`; non-TUI watcher guard child; `recovery_text` child | 10–17 / +700~1000 | 기존 dormant 동작 보존. protected canonical read/hold/notice 재개·legacy retry 우회 fence 시험 |
| P4 비활성 non-TUI start 결합 | 새 `headless_turn/replay_start.rs`, 기존 intake replay_start 공통 child, headless exact gateway child; 최종 driver scope와 stable key/metadata 전달 | 10–18 / +750~1100 | TUI/2B-inject b 변경 0. synthetic ID 재발급 0. 기존 source/slot owner 재사용 |
| P5 2C 활성화·kill-switch | 새 `replay_policy/activation.rs`, admission supplier 선택·drain 소비, production/test selector 통합, runbook 및 수용 시험 | 6–11 / +300~650 | 앞 조각과 실제 on/off/restart 검증 후 마지막에 producer 활성화. 기본 off |

P1→P2→P3→P4→P5 순서다. A5가 적용 대상인 것으로 확인되면 P2b를 P3 앞에 넣고 설계 delta를 리뷰한다. B를 선택할 때 real-anchor 보장·출력 transport는 위 예산에 숨기지 않고 별도 범위 승인/설계가 필요하다. 각 조각에서 tests/fixture는 prod cap 제외지만 독립 리뷰와 required job 배선은 포함한다.

### 현재 giant와 raw 증가 0 계획

| main prod 파일 | `wc -l` | 새 배선 위치/계획 |
|---|---:|---|
| `S/claude.rs` | 4250 | 기존 RawTap 호출 재사용. 추가 observer 분류/봉합은 replay_policy child. 직접 함수 추출이 필요하면 parent에서 동일 block을 제거한 뒤 child 선언/호출을 그 예산 안에 넣음 |
| `S/codex.rs` | 3190 | `execute_streaming_direct` block을 `codex/direct_stream.rs`로 이동, 새 tap은 그 child. parent raw 순증≤0 |
| `D/recovery_engine/restore_inflight.rs` | 2751 | 기존 `replay_hold` child entry를 바꿔 새로운 결정 연결. parent raw 순증≤0 |
| `D/router/message_handler/intake_turn.rs` | 2541 | 이미 있는 replay_start call 재사용. driver/metadata 준비 일부를 child로 이동해 필요한 argument 줄 상쇄 |
| `D/router/message_handler/headless_turn.rs` | 1655 | :889–901의 dormant 거부 block을 headless replay child call로 치환. 필요한 문맥 준비도 child에 둠 |
| `D/gateway.rs` | 1562 | stateless HeadlessGateway/필요 impl 일부를 child로 추출 후 contextful exact adapter 연결. parent raw 순증≤0 |
| `S/provider.rs` | 1822 | 새 관측은 `provider/output_reader.rs`(249줄) 등 child. provider.rs에는 모듈/함수 이동분 범위 안에서만 배선 |
| `S/message_outbox.rs` | 1355 | 기존 on-tx enqueue 재사용. 새 DB receipt notice module이 호출하므로 parent 증가 0 |
| `S/turn_orchestrator.rs` | 7181 | 수정 0. 기존 actor/slot owner만 호출 |
| `src/server/mod.rs` | 3032 | 권고 A는 수정 0, 기존 outbox drain 재사용. B의 transport 변경은 별도 승인 범위 |

`logs/giant-lines.txt`가 raw 근거다. 1000줄 미만이어도 `turn_bridge/mod.rs` 992, `stream_loop.rs` 985, `recovery_text.rs` 950, `retry_state.rs` 924, `terminal_outcome_delivery.rs` 887, `queued_head.rs` 933은 상한 근처다. 새 타입/배선은 자식으로 두어 새 giant를 만들지 않는다. `injected_inputs.rs` 511 / `queued_head.rs` 933도 #7272 소유이므로 수정 0이다. 실제 giant 검사는 구현 조각의 base/head raw delta로 증명하며 추출 없이 “줄만 맞추기”로 주석/서식을 지우지 않는다.

## 7. 시험 계획: 실제 진입점 RED/GREEN/뮤턴트

모든 PG 시험은 `bash /home/adk/skills/agentdesk-issue-pipeline/scripts/pg_throwaway.sh cargo test <필터>`(AGENTDESK_REQUIRE_PG=1)로 disposable PG에서만 실행한다. 운영 DB/15432를 사용하지 않는다. cargo-shim을 우회하지 않으며 기본 병렬, `--test-threads=1` 금지. 이 설계 레인에서는 실행하지 않았고 새 시험도 작성하지 않았다.

| 수용/위험 | 시험 진입·수정 전 RED 기대 | 수정 뒤 GREEN 기준 | 원인 제거 뮤턴트 |
|---|---|---|---|
| 출력 뒤 stale | 새 `stale_resume_after_text_production_path_pg`: 실제 FullEvent/intake 또는 headless non-TUI provider stub→Error→bridge. main은 producer 없음/legacy retry로 RED | spawn/input=1, stale 후 추가0; withheld1; partial/frozen/cursor 보존; A라면 durable event1·정상 REST message1 | producer를 dormant로 고정, activity 관측 제거, Held→Legacy 각각 RED |
| 도구만 실행·본문0 뒤 stale | 같은 실제 경로의 ToolUse/ToolResult→stale, Thinking→stale, unknown record→stale 변형. main 활성/알림 assertion RED | 외부 tool marker=1, spawn=1; no-effect permit0; notice 위와 동일 | tool→inactive 또는 empty-response guard만 남기는 mutant RED |
| file 첫/둘째 scan·body 회수 | `handle_empty_response_recovery`를 실제 bridge drive. 기존 `replay_turn_tests:470` fixture 확장 및 file-growth hook | 두 scan 뒤 stale에서도 동일 receipt/notice1·retry0. recovered body 그대로 | :266/:303/:350 중 하나만 legacy bypass하면 해당 case RED |
| 첫 출력 전 retry1/answer1 | 기존 `stale_resume_retry_e2e`는 main **유지 GREEN 예상**인 회귀 시험(실행 안 함). 별도 production-on non-TUI 변형은 main producer 미연결로 RED | 실패1+fresh 성공1, 답변 ok1, original reply target, recovery notice1; unhandled REST0 | no-effect evidence 거부/permit 미소비 시 on 변형 RED; 중복 continuation·retry cap 제거시 twice-fails case RED |
| startup retry cap | 실제 exact handoff를 연속 두 번 stale로 만드는 `stale_retry_budget_survives_second_failure_pg` | 총 start2, 세 번째0, persisted retry_of 유지·최종 hold | retry_of gate/SQL guard 제거 후 RED |
| restart와 다른 노드 | node A actual start→text/tool→hold, 프로세스 종료; B fresh SharedData/새 pool/새 incarnation으로 restore·worker claim·history retry·direct SQL retry-child | 최초 marker1, B spawn0; same PG held row·original 그대로; 기존 outbox ID 재사용. local inflight/RETRY_CLAIMS를 읽지 않는 별도 child process여야 함 | canonical source fence, worker query guard, restore hydrate 각각 제거 시 해당 entry RED |
| start commit/projection 간 crash | actual begin ACK 직후 local inflight 저장 전 barrier; B가 old row(no replay fields)로 인수 | receipt source lookup으로 차단. closed pool/lookup failure 때 raw row 보존·spawn0 | projected=false read-error fail-open mutant RED |
| hold/notice txn crash | begin→withhold commit ACK 유실, notice enqueue 실패, commit 전 종료, held row 복원, no local row·PG-only debt | 시작된 PG row는 blocked; hold와 notice commit 원자; 동일 held event1; PG receipt 기반 재열거로 알림 복구 | separate commit, row-ref 저장 생략, failed terminal에 fresh enqueue mutant RED |
| 알림 A/B 차이 | actual outbox drain의 mock POST 반영→sent write 직전 crash→node B reclaim. main의 durable-event test만으로는 알림 존재 자체 RED. B를 주장하면 stock drain도 duplicate counter RED | A: event1/ID1, 정상 도착1, 불명은 UNVERIFIED로 보고. B: 별도 real-anchor 설계 후 실제 message identity1로만 GREEN | permanent nonce라고 가정하는 mutant는 nonce 구간 초과/GC 후 RED. send latch 선소비는 crash-before-send에서 delivered0로 RED |
| activation/off | compiled production selector subprocess에서 unset/0/false/invalid/on+channel, TUI/non-TUI, on→off 진행중 상태 | off new request의 main effect trace 동일; on legacy stale-after-effect는 hold; 이전 held/no-effect는 off에서도 실행0; TUI producer0 | cfg(test) override만 켜기, channel-only 활성화, off에서 DB read 생략 각 RED |
| #7272 중복0 | 동일 TUI 요청/API/루틴·headless TUI와 non-TUI 요청을 actual driver resolver로 통과시킴 | TUI는 #7272 input effect 1·#7330 producer0; non-TUI는 canonical ProviderStart1·busy/input owner 신규0 | ingress 이름으로 headless 전부 on, BusyPane 결합, TUI fallback 호출 mutant RED |

기존 재사용 대상(아래는 **존재 확인**, 실행 통과 주장 아님):

- `stale_attempt_retries_only_on_committed_no_effect_evidence_pg`, `stale_decision_failures_hold_without_a_permit_pg` (`replay_policy/policy_tests.rs:67`, `:153`).
- `direct_adapter_completes_the_observation_past_its_done_pg`, `decision_wakes_on_a_fact_recorded_after_its_check_pg` (`replay_policy/live_tests.rs:240`, `:314`).
- `stale_no_effect_attempt_retries_only_through_its_exact_permit_pg`, `quick_empty_resume_after_tool_is_withheld_pg`, `recovered_started_attempt_cannot_bypass_replay_hold_pg`, `producer_dormant_keeps_legacy_execution_paths_pg`, `legacy_and_protected_retries_claim_a_request_once_pg` (`turn_bridge/.../replay_turn_tests.rs:445`, `:471`, `:516`, `:593`, `:698`). 이 fixture의 gateway는 exact를 기록하고 Started를 반환하므로 provider 재실행 횟수의 종단 증거로는 부족하다.
- `exact_retry_starts_only_through_its_continuation_pg` (`intake_turn/replay_start_tests.rs:2241`).
- `replay_hold_restore_*_pg` (`restore_inflight/replay_hold_tests.rs:91`, `:234`, `:362`, `:427`, `:503`, `:585`, `:629`). projected=false read error의 기존 시험을 off/legacy와 on/protected가능 범위로 나눠 행태 변경을 드러낸다.
- `concurrent_source_authority_serializes_with_claim_and_duplicate_start_pg`, `held_dispatch_claim_autoqueue_history_outbox_and_atomic_rollback_pg`, `legacy_null_link_cannot_requeue_via_held_history_pg`, `dormant_schema_keeps_runtime_delivery_and_preaccept_retry_attempt_policy_pg` (`src/db/replay_disposition_tests/{concurrency_tests.rs:4,consumer_tests.rs:4,:227,compatibility_tests.rs:4}`).

추가 집중 fixture: old JSON(v7/현재 replay field 없음), known held row(no local projection), 미래 version/unknown runtime, missing receipt ID, unknown spelling, invalid UTF-8 boundary, frozen prefix, PG unavailable, healthy retry after temporary error, 새 B owner가 들어온 A debt. unknown/newer raw row는 재직렬화·지우지 않고 bytes 동일을 검사한다. 메모리 counters를 비워 같은 PG만 공유하는 child process를 사용한다.

시험 harness는 provider stub의 “실제 시작” handshake를 받은 뒤에만 spawn0/추가0을 판정한다. 별도 success control이 동일 경로에서 start/REST1을 발생시키지 못하면 no-op fixture로 실패시킨다. timeout 전에 bridge/worker가 실행되지 않은 상태, test filter0건, fixture PG skip, wrapper child 미실행은 **PASS가 아니다**. 테스트 수/실패/skip 및 child 내부 실행 확인을 raw log에 남긴다.

구현 REVIEW_READY 전에는 대상 시험/뮤턴트(원복 byte 동일), 적절한 cargo check와 clippy warning no-growth, `source_scan_readers.py <wt> --base origin/main --run <log>` failed0, `prepush_gates.sh <wt> --static-only` PREPUSH_STATIC_OK가 필요하다. 새 test IDs는 required job의 path/run-mode/platform 선택 증거를 낸다. 이번 산출은 소스 없는 **설계 보고**이므로 REVIEW_READY를 선언하거나 위 게이트를 돌렸다고 하지 않는다.

## 8. 리뷰 집중 항목 선답과 위험 판정

| 항목 | 막는 계약/남은 반례 | 개선 위치·증명 |
|---|---|---|
| 첫 출력 직전/직후 경합 | raw activity 선기록 + 완전 reader 종료 봉합. UI body empty는 증거 아님. 정상 terminal과 stale가 독립 결정되면 normal→Legacy 우회 가능 | L의 seal child와 R decision child; raw record/send·stdoutEOF/stderr drain 직전 barrier 시험 |
| restart 중 경합 | started commit이 local projection보다 먼저. receipt 없는 로컬 row도 source lookup. 보호가능 범위의 read-error는 보존 | B4 `hydrate_replay_hold_checked`, P3 recovery helper; projection 전 crash + node B 실제 claim/spawn 시험 |
| 옛 형식 row 복원 | absent fields≠무활동/미시도. nullable replay fields는 decode 기본값 유지. 알 수 없는 version/runtime은 raw 그대로 | existing replay_hold child와 actual boot restore; old raw bytes 동일/known PG hold 탐지 시험 |
| 문서로만 선언한 guard | feature supplier/driver scope/permit consume를 production selector와 실제 effect counter로 증명. 미관측 runtime은 final supplier false | P4/P5 code gate·production executable probe; cfg(test) override 제거 mutant |
| 잘못된 0/PASS | synthetic HeadlessGateway 성공·outbox enqueue1·skip PG·필터0·gateway ExactHandoff::Started만으로 통과 금지 | actual provider/REST control·child exit/log cardinality·sent-before-ACK crash 시험 |
| 알림 영구 dedup | PG receipt/outbox ID는 비유계 sentinel. 외부 nonce의 짧은 창은 영구 보장이 아님. A/B 계약은 미확정 | §2.5 BLOCKED; transport 범위를 정하고 crash-cut을 수용 gate로 선택 |

현재 main에서 dormant인 새 기반에 P1을 붙여 활성화 이전 머지를 막는 방식은 쓰지 않는다. 위의 “활성화 전 필수”는 #7330 수용을 주장할 P5의 fail-closed gate다. 알려진 adjacent provider-internal retry(A5), SQL unknown 세부 의미 등은 새 증거 없이 운영 재현 P1이라고 승격하지 않는다.

P2 후속/비용은 다음과 같다. 모두 현 변경의 비활성 기반 머지를 차단하지 않는다.

- 영구 held/알림 sentinel은 누적 저장량을 증가시킨다. 고치지 않으면 운영에서 receipt/보존물 저장량이 늘어난다. `replay_disposition/receipt`의 읽기 index·pending notice partial index를 사용하고 unresolved는 삭제하지 않는다. 시험은 대량 sentinel 후 오래된 held 요청 재접수0. 권고는 cap/ring을 넣지 않고 관측 후 별도 보존 정책을 설계하는 것이다.
- reader seal timeout/호스트 장시간 스톨은 안전한 요청까지 hold시킬 수 있다. 고치지 않으면 운영에서 자동 복구 대신 원문 보관/수동 판단이 늘어난다. `replay_policy/live`에 incomplete 사유를 명확히 남기고 시간값을 근거 없이 키우지 않는다. 시험은 stalled stderr와 정상 drain control. 권고는 hold 유지다.
- A5 내부 retry는 이번 bridge 수리와 별개인 기존 경로다. 고치지 않으면 해당 provider가 이미 실행한 일을 자신의 retry loop에서 반복할 여지가 있다(현재 조사만으로 실제 stale 재현은 미확인). stub 실측 [U4] 뒤 해당 provider child에 같은 no-effect authority를 소비시키는 최소 델타를 제안한다. 권고는 전 provider 일반화·새 loop 설계 없이 실제 해당 경로만 잇는 것이다.

## 9. [U] 목록과 실측 방법

| ID | 미확인 | 실측/확정 방법·판정 |
|---|---|---|
| U1 **계약 결정** | 알림1이 durable event인지, 외부 실제 도착까지 영구 exactly-once인지 | 코디네이터가 §0 A/B 선택. A이면 실패/불명의 상태가 완료 수에 섞이지 않는 기준도 명시. B이면 real-anchor 범위 승인 후 별도 설계. 이 항목이 DESIGN_BLOCKED의 유일한 필수 결정 |
| U2 | 운영의 실제 provider driver·headless routing·노드별 feature 배포 상태 | 설정 파일을 레인이 읽지 않음. 코디네이터가 허용된 런타임 진단의 `selected_driver`, `runtime_kind`, 대상 채널을 비밀값 없이 제공. 동일 launch 조건의 disposable stub 시험. 운영 전제라고 가정하지 않음 |
| U3 | non-TUI headless 각 source의 stable request identity/session binding·exact retry 경로 | health API→reservation→headless entry→provider 실제 stub를 통과시켜 source/dispatch/session-key/canonical receipt를 기록. API·routine·worker/restart 각각 재개 때 key 동일 확인. sessionless case면 exact begin 계약 delta를 리뷰하기 전 on 불가 |
| U4 | direct Codex/ProcessBackend/legacy wrapper 및 A5 provider 내부 loop가 첫 출력 전 complete no-effect를 어떻게 증명하는지 | 파일 EOF·process wait·stderr 종료를 독립 barrier로 제어하는 실제 subprocess fixture. output/tool→stale 및 quiet stale→fresh success 양쪽 실행. Qwen/Gemini는 내부 retry effect counter도 함께 관측. unsupported 조합을 silently 제외한 채 전체 활성화 PASS 금지 |
| U5 | 현 e2e와 dormant PG 시험의 이 SHA 실제 성적 | §7의 대상 `cargo test <이름/필터>`만 수행. 테스트 바이너리 path/SHA256·실행 수·skip0/child 실행·원시 로그 확인. 이 레인은 미실행. 이미 통과했다고 재사용할 증거 없음 |
| U6 | #7272의 최신 landing head에서 shared parent/driver scope의 final 소유 | 코디네이터 제공 head에서 `git log <설계 base>..origin/main -G '<핵심 심볼>' -- src`와 §5 좌표 갱신. 공용 parent 작성은 착지 순서 뒤 해당 child 한 owner만. 현재 작업자의 dirty worktree는 읽어 사실로 사용하지 않음 |
| U7 | outbox 정상1·ACK loss·failed GC·non-turn loopback의 실제 전달 결과 | disposable PG+REST mock actual worker drain. sent write 전 crash, lease reclaim, 가상 시각+GC, allowed bot ingest. row1과 Discord message1을 각각 계수. B라면 구간 초과에도 외부1 필요 |
| U8 | 실제 변경량·giant raw0·warning0·required test lane | 구현 각 조각 base/head로 pr_cap_prod, giant raw delta 및 clippy JSON을 측정하고 source_scan/정적 gate 실행. 위 예산 수치를 측정치로 승격하지 않음 |
| U9 | 알림 worker와 PG-only 보호 행의 재개 enumeration | notice intent가 든 receipt만 있고 local inflight가 없는 fixture에서 actual periodic/boot child drain 실행. 최초 register는 전달 routing을 저장하고 cursor pagination은 전건을 결국 열거; LIMIT batch를 전체0로 오인 금지. 영구 저장소 drain의 child hook을 P3에서 연결 |

이 조사에서는 운영 설정값, 운영 DB/서버 상태를 관측하지 않았다. 그 대신 필요한 운영 전제를 코드 좌표와 U2에 고정했다. 소스 spike·mutation은 없으며 git worktree는 clean 유지가 종료 조건이다. 증거 파일은 `/home/adk/adk/lane-runs/i7330/logs/` 아래에만 추가했다. `source-census.txt`는 핵심 소스 34개의 line-numbered 정적 원문이고 `source-evidence.json`은 각 파일 SHA256·기준 head·시험/PG 미실행·clean 상태를 기록한다. 기존 시험 실행 증거로 쓰지 않는다.

다음 행동: 코디네이터가 U1 알림 계수 계약을 답한 뒤 이 설계 문서의 해당 절과 시험·예산만 갱신하고 **독립 설계 리뷰**로 넘긴다. 구현/소스 커밋은 이번 레인에서 하지 않는다.

DESIGN_BLOCKED 알림 “1회”의 계수 계약 확정 필요; 설계 문서 /home/adk/adk/lane-runs/i7330/design-r1.md
