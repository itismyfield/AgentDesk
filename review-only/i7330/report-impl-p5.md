BLOCKED DESIGN_DELTA P5 — PG-only 복구의 live foreign attempt 처분 권위 미정

# #7330 P5 — 활성화 준비 WIP / 설계 delta 요청

기준은 P4 `9d5220bb3f31463d7a205c50625591a40f13bb9f`다. worktree `/home/adk/adk/wt/i7330-p5`, branch `feat/i7330-stale-replay-p5`, 현재 HEAD도 기준 SHA 그대로다. **P5 소스 커밋·push 0, 미커밋 WIP 32파일**을 보존했다. `logs/p5-blocked-wip.patch`에는 untracked activation.rs까지 포함한 스냅샷이 있다. 운영 env/restart/운영 DB·서버·설정 접근·tmux/herdr·GitHub/DAG 쓰기는 하지 않았다. P4 worktree/target을 건드리지 않았다.

## 차단 근거: 첫 출력 전 stale의 재시도와 성공 응답이 사라진다

이슈 수용 기준 **“첫 출력 전 stale이면 기존처럼 재시도 1회·응답 1회(`stale_resume_retry_e2e` 유지)”**를 현재 후보의 production 연결에서 위반한다. 조합은 승인된 Claude / LegacyPrompt / local owned_direct다. 실행 중인 owner node A의 CLI가 첫 출력 전에 대기하는 동안, 같은 PG를 쓰는 off node B가 정상 부팅 복구 진입을 호출한다. 프로세스 종료·시계 역행·위장·장시간 stall을 전제로 하지 않는다.

실제 health → reserved headless → webhook/session binding → native CLI 경로를 썼다. owner는 process-start env on 및 명시 채널7330522, peer는 process-start env0/채널 목록 없음이다. peer의 local inflight는 비어 있다. owner CLI PID 생존, mailbox token, release barrier 미해제를 모두 확인하고 `worker_nodes`도 owner online/heartbeat NOW로 두었다. peer는 별도 process/새 incarnation/새 runtime root로 실제 `restore_inflight_turns`를 호출한다. CLI fixture는 release 뒤 stdout0, stale stderr, exit1로 끝나며 exact retry에서는 정상 성공한다.

| 같은 시험의 시나리오 | peer scan 전 → 후 disposition | subject native 수 | subject 성공 응답 | stale notice | 최종 상태 |
|---|---|---:|---:|---:|---|
| quiet-control: peer 없음 | started → started | **2 (원래1+retry1)** | **1** | 0 | classified_normal, settled=true |
| quiet-peer: 살아 있는 동안 off peer 복구 | started → withheld | **1 (retry0)** | **0** | 1 | withheld, settled=true |

로그의 native 합계는 앞선 ordinary 성공 대조1을 포함하므로 각각3과2다. `control_reply=1`도 별도로 센다. 원시 witness:

```text
P5_LIVE_FOREIGN scenario=quiet-control before=started_unclassified after=started_unclassified final=classified_normal owner=p5-live-owner native_alive=true native=3 control_reply=1 subject_reply=1 notice=0 settled=true
P5_LIVE_FOREIGN scenario=quiet-peer before=started_unclassified after=withheld final=withheld owner=p5-live-owner native_alive=true native=2 control_reply=1 subject_reply=0 notice=1 settled=true
```

`logs/p5-live-foreign-quiet-red.log`: 상위 선택1 FAIL/ignored0, cargo rc101. 내부 positive-control child1 PASS와 failing owner child1 FAIL을 상위 시험 수에 더하지 않는다. 같은 archived binary로 실제 PG에서 한 번 더 실행해 **동일 두 witness / rc101**을 얻었다(`p5-live-foreign-quiet-repeat.log`). 신규 시험은 `src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:903`이며 peer 실제 진입은 :933, 결과·oracle은 :1007–1022다.

고치지 않으면: 한 노드의 정상 부팅 복구가 다른 노드의 살아 있는 quiet attempt를 withheld로 정산하여, 첫 출력 전 stale에 허용된 재시도와 성공 응답을 없앤다. 실제 운영 경로에서 재현 가능한 중단 및 위 수용 기준 위반이므로 **P5 활성화 차단**이다. 현재 미배포 WIP의 문제이며 현 운영 장애를 주장하지 않는다.

### 원인과 production 도달 근거

경로의 `D/`는 `src/services/discord/`다. 좌표는 현재 WIP 기준이다.

1. `D/runtime_bootstrap.rs:317–337`: REST worker는 gateway lease 없이 `restore_worker_queues`를 새 intake worker 시작 전에 호출한다. `D/runtime_bootstrap/queued_recovery.rs:19,205`에서 `restore_inflight_turns`까지 이어진다. fixture는 이 복구 진입을 실제 호출했으며 전체 서버 boot executable을 띄웠다고 과장하지 않는다.
2. 승인된 `D/recovery_engine/restore_inflight.rs:70–76`의 cfg(test) 제거로 PG-only 열거가 production에 연결된다. local inflight가0이어도 실행된다.
3. `restore_inflight/replay_decision.rs:75–82`는 provider 전체에서 `withheld` 또는 **owner incarnation != current process**인 started/no-effect를 고른다. target instance, live owner, orphan/lease 증명은 조회하지 않는다.
4. `src/db/replay_disposition/write/hold_notice.rs:130–134,189–193`도 같은 different-incarnation 조건으로 잠그고 withheld + notice를 commit한다. nonce CAS는 row 변경 경합을 막지만 old writer가 죽었다는 증거가 아니다.
5. owner incarnation은 `D/replay_policy.rs:20–25`의 process별 random UUID다. 살아 있는 타 노드도 필연적으로 다르다. request 등록의 target instance는 `write/start.rs:141–151`에 저장되지만 scan 술어에 쓰이지 않는다.
6. owner가 no-effect를 분류할 때 `write.rs:114–122` → `write/settle.rs:15–17`의 started 조건이 실패한다. `D/replay_policy.rs:49–55`, `replay_policy/decision.rs:48–54`는 exact permit을 주지 않고 Held로 끝낸다. 원래 요청을 legacy로 다시 등록하지는 않는다.

P4의 current-incarnation 제외 수리는 유지했다. P3/P4 리뷰 P2를 근거 없이 올리는 것이 아니다. **새 근거**는 P5의 production scan 연결, 서로 다른 실제 두 process, live foreign owner witness, peer 없는 성공 대조, quiet stale 재시도0/성공 응답0의 재현이다. 초기 ordinary-success probe에서는 live row가 withheld 및 오알림1이 됐어도 성공 응답은1이었다(`p5-live-foreign-red.log`). 그 결과를 응답 유실로 잘못 보고하지 않았다. quiet 변형에서만 위 수용 위반을 확정했다.

## 필요한 설계 delta와 권고

설계 §2.3은 **“original writer를 잃은 started row”**만 non-executable recovery로 withheld 정산하도록 한다. 지금 승인된 `incarnation != current`는 writer 소실 증명이 아니다. §4의 off node takeover와 살아 있는 owner의 exact drain도 동시에 성립해야 한다.

최소 계약:

- **이미 withheld인 row의 notice 보충**은 지금처럼 receipt lock + 영구 key + 동일 transaction으로 수행한다. producer off와 무관하며 실행 권한은 만들지 않는다.
- **started/no-effect → withheld 복구**는 해당 `(receipt_id, nonce, owner_incarnation)`의 writer 소실/소유권 박탈을 확인한 근거가 있어야 한다. 다른 노드나 다른 incarnation이라는 사실만으로 만들지 않는다. live 또는 불명인 경우 row와 기존 exact 권리를 건드리지 않고 재실행만 계속 차단한다.
- 확인한 근거는 DB 잠금/처분 경계에서도 유효해야 한다. 열거기에서만 확인하고 쓰기에서 무시하는 guard는 부족하다. local snapshot과 PG-only 진입 모두 같은 계약을 소비해야 한다.
- own-node restart, foreign live, foreign orphan, NULL old row, exact no-effect 대기, notice 없는 기존 withheld를 각각 분리해 증명한다. unknown을 legacy 허가나 “보존할 것 없음”으로 읽지 않는다.

**권고안:** 우선 PG-only 전역 열거를 **이미 withheld인 row의 보충**으로 제한하고, started/no-effect의 무파일 orphan 처분은 기존 owner가 소실됐다는 권위가 확정될 때 연결한다. local restore에서 증명할 수 있는 범위와 PG-only 범위를 나누는 최소 설계 delta를 먼저 독립 검토한다. 이는 P3의 mixed started/no-effect PG-only 복구 시험 및 알림 진행 계약도 바꾸므로, 이번 레인에서 조용히 범위를 줄이지 않았다. 현재 전역 술어 그대로 활성화하는 안은 권고하지 않는다.

대안은 receipt owner에 묶인 PG lease/incarnation 권위를 추가하는 것이다. crash 후 알림 진행까지 더 강하게 보장할 수 있지만 새로운 권위·배포·복구 계약 및 migration 검토가 필요하다. 이번 브리프는 새 migration을 허용하지 않는다. 단순 `target_instance_id = self` 제한은 foreign orphan 복구를 잃고, 같은 instance 재시작 중 overlap까지 증명하지 못하므로 완성된 해법으로 제시하지 않는다. worker_nodes의 heartbeat만 새 권위로 쓰지도 않았다. `src/services/cluster/node_registry.rs:528–542` upsert는 replay incarnation과 연결되지 않고 기존 row의 started_at도 갱신하지 않는다. self identity fallback도 :666–679에서 hostname/PID여서 항상 영구 node key라고 가정할 수 없다.

고칠 위치는 `restore_inflight/replay_decision.rs::resume/resume_pg_only`의 대상 결정과 `write/hold_notice.rs::recover_hold_notice`의 처분 허가 경계다. 새 권위가 필요하면 승인 범위를 먼저 정한다. 증명 시험은 이번 RED를 GREEN으로 만들고, owner 소실 증명 제거 mutant에서 같은 actual-entry oracle을 RED로 만들어야 한다. 기존 dead-writer/old-withheld 재개 시험도 함께 GREEN이어야 한다. **행 삭제·release/kill·legacy 재등록·재실행 권한 추가로 해결하지 않는다.**

정지 근거: [agentdesk-issue-pipeline SKILL.md](/home/adk/skills/agentdesk-issue-pipeline/SKILL.md)의 “불변식(펜스·dedup·lease·원자성)을 새로 세우는 설계는 구현 전 독립 리뷰를 필수로 한다.”를 적용한다. 현 설계의 writer-loss 전제가 성립하지 않는 재현이므로 새 복구 소유권을 임의 구현하지 않고 설계 delta를 보고한다. 크기/giant 경고 때문에 멈춘 것이 아니다.

## 구현해 둔 P5 범위

| 범위 | WIP 내용 / 위치 | 현재 검증 상태 |
|---|---|---|
| startup supplier | 새 `D/replay_policy/activation.rs`: 두 env 이름 충돌 검색0 후 선택, true/1만 허용, 목록 전체 strict u64/nonzero, 실패 시 empty, process 단일 immutable snapshot | archived 후보의 actual process-start8변형 PASS. hot-reload 주장0 |
| 최종 scope gate | `router/intake_dispatch/replay_admission.rs:53–70`, 공통 begin과 headless admit: can_activate 강제. channel-only context는 read-only이며 새 producer를 주지 않음 | parser/matrix/scoped admission 후보 PASS; 필수 mutant 미실행 |
| off durable 소비 | 승인 7파일의 consumer 분기. same-key normal 완료 소비, registered custody, 기존 protected fence/notice 유지 | normal/등록 전off/exact drain/query-error 후보 PASS. prior-states actual-worker fixture는 아래 FAIL |
| 공통 abandon | `db/replay_disposition/write.rs` hold-only proof, `D/replay_policy.rs` 정산/재시도, final scope/spend 실패, `turn_bridge/retry_state.rs` Refused await | 실제 Refused withheld1/notice1, unsupported exact native0/receipt1/notice1 후보 PASS. 불명 begin 뒤 다음 nonce도 hold-only로 확인 |
| remote/scope | intake remote bool 전달 + headless 최종 scope. missing driver/runtime key는 fail-closed | common begin의 remote final-scope 거절 PASS. remote 설정은 main에서 retired(`src/config.rs:3160–3187`)라 실제 SSH/remote native 통합을 수행했다고 주장하지 않음 |
| boot scan | PG-only production 연결 및 오류 재조회 | **이번 차단 원인**. empty scan의 read-counter fixture도 FAIL. 미완료 |
| fixture/정리 | REST witness에 method/URI, GET·typing 제외 및 empty/foreign write 유지. 없는 recovery snapshot의 null 삭제 | archived 후보 해당 oracle PASS. production ProcessBackend 완료 FAIL/no-effect NOT_PROVEN은 숨기지 않음 |

활성 가능 조합은 승인대로 다음과 같다. 구현 의도 표이며 최종 활성화 준비 PASS 표가 아니다.

| provider / driver / runtime | 새 producer |
|---|---|
| Claude 또는 Codex / LegacyPrompt / local owned_direct | env on + 명시 channel에서만 가능 |
| Claude/Codex ProcessBackend, file, legacy watcher, Codex warm wrapper | 불가, 무보호 신규는 legacy |
| Qwen/Gemini(A5) | 불가, 무보호 신규는 legacy |
| remote, TuiHosting, scope 불명 | 불가 |
| 이미 begun/exact인 요청이 불가 scope로 변경 | legacy 재등록0, common abandon으로 withheld + notice1 |

## 리뷰 집중 항목 — off 운영 경로의 실제 변화

아래 조회 수는 **replay 전용 SQL**이며 기존 session/webhook/history SQL을 포함하지 않는다. candidate 관측과 아직 미증명인 항목을 구분한다.

| off 조건/진입 | 변경 전 → WIP의 차이 | 관측 또는 잔여 |
|---|---|---|
| headless 신규·PG 있음·보호 row 없음 | replay SELECT0 →2(early preflight + start). provider/retry/response 효과는 유지, 새 receipt/observer/notice0 | 후보 `p5_off_no_rows_keeps_effects_with_two_replay_reads_pg` PASS: native1/성공 sent1/receipt0/notice0/read2. 전체 DB IO 불변이라고 주장하지 않음 |
| generic intake admission·PG 있음 | 기존 source hold SELECT1을 typed key/source SELECT1로 통합(추가0, 코드 대조). registered/normal 처리의 의미는 §4대로 바뀜 | 성공한 lookup의 정상·완료 소비 후보 PASS. worker까지 모든 prior states는 fixture FAIL로 미증명 |
| worker replay precheck / 공통 non-TUI begin | dormant skip/legacy shortcut 뒤에도 canonical read. 해당 경계마다 보호 row 조회가 생김(코드상 각 +1) | 최종 실제 worker 전체 trace 미완료. 위 headless의 +2와 중복 합산하지 않음 |
| headless 조회 실패(pool 존재) | fail-closed API retry 결과. 원 reservation 보존 후 원 요청 재시도 가능 | `p5_off_sql_failure_is_retryable_with_the_original_reservation_pg` 후보 PASS: 실패 중 native0, 복구/같은 reservation 재시도 native1 |
| generic intake 조회 실패 | 기존 notify 경로로 “기존 실행 여부를 확인하지 못해 새 실행을 보류했습니다. 완료 여부를 확인한 뒤 다시 시도해 주세요.” 전달 후 원 custody 보존 | `p5_off_intake_query_error_notifies_and_preserves_retry_pg` 후보 PASS: visible notice1, session0, table 복구 뒤 재접수 가능. 보존 성공을 미리 확정하는 문구는 쓰지 않음 |
| legacy retry 소비자 조회 실패 | pool이 있으면 오류를 없음으로 읽지 않고 1초 간격 재조회. pool 없음은 기존 legacy 허용 | `p5_off_legacy_lookup_error_retries_until_authority_returns_pg` 후보 PASS. SQL 복구 전 대기, 복구 뒤 보호 처분 소비 |
| boot·PG 있음·보호 row 없음 | provider별 빈 scan SELECT +1 예상, 새 effect0이어야 함 | `p5_empty_boot_scan_has_one_read_and_no_effect_pg` **FAIL**(기록키는 scan:provider, 시험은 scan:provider:DB). 실제 effect0 증명 완료로 세지 않음 |
| off 재시작·이미 withheld/notice 없음 | 영구 event를 보충하고 실행0 유지 | 후보67행/새 process 복구 시험 PASS. 동시에 live foreign started가 있는 경우는 위 **수용 기준 RED** |

- 운영 no-op: 완전 신규의 효과 trace는 env8변형 및 headless off 후보에서 증명됐지만, replay IO와 기존 행 소비는 의도적으로 변한다. 모든 경로가 동일하다고 선언하지 않는다.
- 재시작 중 경합: 현재 owner만 제외하는 guard로 foreign live owner를 보호하지 못한다는 반례를 실제 두 process에서 남겼다. 새 intake/boot 경합의 writer-loss 권위는 미해결이다.
- 옛 row: missing/malformed를 None으로 합치는 기존 문제를 다시 만들지 않았지만, 이번 전체 old-row 회귀 suite는 NOT_RUN이다. NULL incarnation을 writer 소실의 증거로 간주하는 것도 delta에서 검토한다.
- 문서로만 선언한 guard: 실제 env child, actual native/source reply/receipt/notice를 셌다. 채널 supplier와 can_activate는 코드에 있으나 필수 mutant/clippy/static을 완료하지 않아 활성화 PASS로 세지 않는다.
- 잘못된0/PASS: ordinary control1을 확인한 다음 subject를 따로 계수했다. quiet no-peer의 retry1/성공reply1이 먼저 PASS한다. malformed/조회 오류를0으로 읽지 않는다. empty scan 계수 FAIL은 숨기지 않는다. 내부 child의 test result를 중복 합산하지 않는다.
- watcher buffer: 활성 가능 runtime owned_direct는 watcher를 쓰지 않는다. file/legacy watcher는 고정 비활성이라 신규 보호 producer의 버퍼 계약은 해당 없음이다. 과거 protected row의 watcher 소비와 보존을 안전한 전체 수명으로 검증했다고 주장하지 않는다.

## 크기·giant·등록

공식 스크립트로 기준 P4 → WIP 가상 index를 측정했다. 스크립트가 commit merge-base를 요구하므로 **ref에 연결하지 않은 측정용 commit object**만 만들었고 실제 branch source commit은0이다. 원시 `logs/p5-blocked-size.log`, 파일별 raw 수치 `logs/p5-blocked-change-files.txt`.

```text
prod 20 files net +321 code (+464/-143 code lines; raw +540/-160); not counted 20 entries +1148
CAP: PASS (remaining 10 files/+1179 net code)
```

분할선 +1200 미도달, prod30/net1500 상한 아래다. raw 전체는 32파일이며 untracked activation.rs도 측정/patch에 포함했다. 새 allow/baseline/migration0. giant `intake_turn.rs`는 승인한 remote 필드 전달 **:2113의 +1**, raw2541→2542다. 줄 수를 맞추기 위한 이동/삭제는 하지 않았다. #7415 전 static ratchet 실패 허용은 승인받았지만 static 자체는 아직 실행하지 않았으므로 이 예외로 PASS라고 쓰지 않는다.

생성 목록·문서는 다음 공식 명령으로만 갱신했다(원시 `logs/p5-gen-wip3-*.log`, 모두 rc0):

- `python3 scripts/check_test_target_integrity.py --write-lib-inventory-manifest`: manifest +23/0, 총15649, digest `6683e3ac0bdad984b4c85673e53dd5b96ca8696d9ef3c0cb8e37031524e7d24c`.
- `python3 scripts/check_pg_test_lane_membership.py --write-snapshots --manifest-only`: PG manifest +20/0.
- 같은 script `--write-non-pg-filter`: +1/0, `--write-pg-db-paths`: diff0.
- `python3 scripts/generate_inventory_docs.py`: ARCHITECTURE +1/0.
- `python3 scripts/generate_env_reference.py`: env-reference +3/-1.

기존 voice harness ID duplicate canonicalization 경고는 생성 로그에 남았다. required lane 실제 선택·platform 전체 검증은 static/source-scan 전이므로 완료하지 않았다. registry 생성 성공을 시험 실행 성공으로 세지 않는다. quiet 변형은 기존 ID의 body만 바꾸어 새 ID 수는 그대로다.

## 증거 목록

로그 루트 `/home/adk/adk/lane-runs/i7330/logs/`. 테스트 binary는 Linux test/unoptimized, default 병렬이다. `SHELL=/bin/bash`, `TMPDIR` unset, `CARGO_INCREMENTAL=1`, `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`. 실제 throwaway PG16.15/AGENTDESK_REQUIRE_PG=1 사용. cargo는 PATH shim을 거쳤으며 절대 cargo/slot 우회0이다. 바이너리 직접 재실행은 이미 빌드된 동일 archived test를 공식 PG wrapper에서 실행한 것이다.

| 증거 | 실제 결과 / 한계 |
|---|---|
| `p5-baseline-red.log` | 수정 전 selected6: 기존3 PASS/신규3 FAIL/ignored0. env supplier, missing scope, off completed key RED |
| `p5-refused-baseline-red.log` | 수정 전 selected1 FAIL/ignored0, 실제 scheduler Refused 뒤 notice0 |
| `p5-live-foreign-red.log` | 초기 ordinary-only live foreign probe selected1 FAIL/ignored0. 오보류/notice1이나 정상 성공 reply1. 응답 유실 근거로 쓰지 않음 |
| `p5-env-variants-candidate.log` + result.json | archived 후보 selected1 PASS/ignored0/rc0. 내부 process-start env8변형 witness8개 |
| `p5-candidate-targets.log` + result.json | archived 후보 p5_ filter: selected25/23PASS/2FAIL/ignored0/rc101, 31.10s. foreign probe만 별도 로그에서 실행하므로 여기서는 명시 --skip. 최종 suite의 실패를 제외하거나 PASS를 주장한 것이 아님 |
| `p5-live-foreign-quiet-red.log` + result.json | 현재 소스 cargo selected1 FAIL/ignored0/rc101, 2.57s. 내부 quiet-control PASS, quiet-peer FAIL. 실제 수용 위반 |
| `p5-live-foreign-quiet-repeat.log` + result.json | 동일 archived binary/새 실제 PG, 같은 selected1 FAIL/ignored0/rc101, 2.54s. 같은 두 witness |
| `p5-windows-round1.log` | Windows GNU lib/tests cargo check rc0. 이후 소스가 바뀌어 최종 same-source 증거는 아님 |
| 초기 `p5-first-target.log`, `p5-target-round2.log` | compile E0603/tuple E0308, retired remote fixture E0609. 수정했으며 제품 RED로 세지 않음 |
| `p5-blocked-size.log`, `git diff --check` | cap PASS, whitespace check PASS. 정적 통합 게이트 대체 아님 |

바이너리/SHA256:

- 수정 전 `logs/bin/agentdesk-p5-pre-production`: `da9b86847f9a4cbd37cc2561e72abe9dcb50bac0cbf7ce1cd898a798cdbf6cea`, metadata `p5-baseline-binary.json`.
- historical ordinary/candidate `logs/bin/agentdesk-p5-foreign-live-ordinary`: `58bb0214ae9c98ee0912c8ca2e371b20c4d047c813a39aae331d37a5f4cbe682`, metadata `p5-live-foreign-ordinary-binary.json`, source `p5-live-foreign-probe-source.json`.
- current quiet RED `logs/bin/agentdesk-p5-foreign-live-quiet`: **`584041d7cdee91f91e359bfe40e4665eaac24a85c92cf874b3dc24044814989f`**, metadata `p5-live-foreign-quiet-binary.json`, source `p5-live-foreign-quiet-source.json`(27개 source SHA 대조 mismatch0).

ordinary 후보에서 quiet 판으로 바뀐 source는 진단 시험 함수1개다. production이 같아도 test source가 바뀌었으므로 historical 후보23PASS를 최종 same-head 통과에 재사용하지 않는다. 현재 HEAD는 변경 없는 P4이며 이 증거를 P4 검증 결과로도 부르지 않는다.

## 미해결·재개 순서

1. 위 writer-loss 권위의 설계 delta를 독립 검토해 PG-only already-held 보충과 started/no-effect orphan 처분의 최소 계약·소유 파일을 확정한다. 이번 quiet actual-entry RED를 보존한다.
2. fixture FAIL2개를 고친다. `tests/replay_hold.rs:327`에서 seed 후 immutable request_key를 UPDATE하지 않고 최초 INSERT에서 올바른 canonical key를 만드는 test helper를 쓴다. `replay_decision.rs`의 cfg(test) read-counter 기록 키를 private DB까지 포함하도록 맞추어 병렬 실행에서 다른 scan을 섞지 않는다. 이 두 실패를 제품 장애와 혼동하지 않는다.
3. 승인 후 writer-loss 수정, 수정 전 RED→수정 뒤 GREEN, owner-proof 제거 mutant RED를 증명한다. 기존 P3/P4 복구/legacy fence/old-row 시험과 `stale_resume_retry_e2e`도 포함한다.
4. P5 필수 mutant **can_activate 제거·abandon 정산 제거·env default-on**은 **NOT_RUN**이다. 대상/PG suite, clippy 자기 warning0, 최종 Windows, static-only, source-scan도 미완료다. 현재 증거로 REVIEW_READY/PREPUSH_OK/전체 수용을 주장하지 않는다.
5. 설계·구현·검증을 마친 후 지정 trailer로 커밋만 한다. push/운영 enable/restart는 코디네이터만 한다. native prepush detach는 REVIEW_READY 뒤이므로 이번에는 실행0이다.

켜기 runbook은 이 delta와 gate 완료 전에는 실행하지 않는다. 향후 대상 제안은 코디네이터가 소유를 확인할 수 있는 전용 검증 채널1개이며 local Claude LegacyPrompt/owned_direct부터 시작한다. 실제 채널 ID·운영 설정은 읽지 않아 [U]다. `AGENTDESK_STALE_REPLAY_V1=1`과 명시 목록을 process 환경에 설정하고 코디네이터의 통상 재시작 절차를 쓴다. off는0/제거 후 통상 재시작하며 기존 held를 파괴적으로 해제하지 않는다. 현재 후보에서는 off peer도 문제를 일으키므로 on node만 보고 enable을 판정하지 않는다.

## 진행 중

이번 레그가 띄운 cargo/PG 시험/fixture/generator는 모두 종료됐다. P5 detached/native prepush0, 이어받을 PID 없음. WIP와 원시 로그·두 판의 바이너리를 보존했다. 재개 시 먼저 process를 확인하고 완료된 검증을 무조건 중복 실행하지 않는다.

BLOCKED DESIGN_DELTA P5 — PG-only 복구의 live foreign attempt 처분 권위 미정
