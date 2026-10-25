PLAN_READY a525e9be9886d9dd8f85b5c7f1a0c03176b377c6

# S4 A안 재계획·main 병합 보고

## 결과·범위

- merge head **`a525e9be9886d9dd8f85b5c7f1a0c03176b377c6`**, 첫 부모 승인 foundation `fe084241801630af327bb7edac309358ddcf14b4`, 둘째 부모 main **`28eee2f87fa44e1a67152d3b15f1e7e84e5f99dd`**(#7410). foundation 이후 first-parent 커밋은 **병합 1개**다. rebase·별도 S4 구현 커밋·push·GitHub/DAG 쓰기0.
- A안의 사전 판독1회→선택 C-s→paste→고정 settle→Enter1을 main에서 받았다. 사후 own/receipt 확인과 자동 잠금을 A 제출 경로에 되살리지 않았다. S3 metadata callback은 content 전으로만 옮겼다. production V2 유입은 여전히 열지 않았다(`queued_head.rs`의 test-only V2 root와 `owner_input.rs`의 test-only scope factory).
- 새 계획: `plan-s4-r2.md`. old `plan-s4.md`는 수정0. 이전 U01~U31 전부 처분, 새 U32~U35 추가. S3d P2-1/4는 4a 첫 커밋, P2-2는 4a/4d, P2-3 비용은 S5 첫 단위. 이미 닫힌 S3c hook/S3b custody는 새 A 기반 회귀로 확인했다. 00:48의 FreshTurn과 확정 LoadFailed 종결은 **S4 계획이며 이 병합에서 구현하지 않았다**.
- #7402 소유 inject*/idle_stash/legacy_submission에는 S4 core를 계획하지 않았다. 공통 A API/필요 export는 **#7402 착지 뒤**. #7330 router/recovery도 착지/인계 뒤, #7317 CI는 공식 등록/생성만. Codex A2의 미착지/담당은 활성화 전 [U]다.
- #6736 DoD4는 S5, #6775는 HTTP origin dedupe 델타 수용, #6679는 queued head 위치 유지·shared race_loss helper 전체 유지로 구분했다. E-50의 PG 실제 ingress와 S5 격리 native 수용도 분리했다. 첫 WebGPT 시간 초과를 CLEAN/지적으로 세지 않았다.

## 손으로 푼 충돌

직접 충돌은 source4파일이었다. 아래는 conflict hunk와 그 해소를 위해 필요한 child 이동·시험 적응을 모두 포함한다. 파일 약어는 계획과 같다. **델타 리뷰 입력**은 `logs/s4r2-remerge.diff`(최종 head Git의 재병합 diff)와 이 표, stage 원문은 `logs/s4r2-conflict-stages.json`이다.

| 파일·merge 좌표/hunk | main 쪽 | 브랜치 쪽 | 해소 | 이유 |
|---|---|---|---|---|
| `S/claude/tui_session_launch.rs:37,57` | 일반 create, fixed-size guarded launch 제거, 준비에 session_prompt 기록 | S3c의 prepared helper 추출 | helper를 유지하고 create는 main 일반 create로 고정 | A안이 지운 launch geometry 경로 복구0, 기존 준비 중복0 |
| `S/claude/tui_session_launch/prepared.rs:58` | 준비 직후 system prompt 기록 | 공통 prepared helper에 기록 없음 | main의 record를 helper의 같은 준비 시각에 이동 | system-prompt 부수효과 유실 방지 |
| `C/busy_inject.rs:339`(guarded 조건 hunk) | old Attempt 의존/사후 조건 제거 | epoch 조건을 old guarded 판단에 합성 | main 사후 조건 제거 채택 | A Enter가 post-check로 막히지 않게 함 |
| `C/busy_inject.rs:435`(판독 경계 추가) | single before_paste capture | old Attempt 생성에서 epoch 관찰 | owner scope에만 epoch metadata 관찰 후 같은 before_paste | UI capture를 늘리지 않고 S3 target pin 유지 |
| `C/busy_inject.rs:505`(inject_locked hunk) | A stash submit으로 직접 진행 | Attempt/old guarded branch 기반 owner 관찰 | main A branch 채택 | old OWN/receipt gating 복구0 |
| `C/busy_inject.rs:572` 부근(삭제 hunk) | paste_into_empty/await_own와 Attempt 제거, 진단 await_receipt는 잔존 | old helper에서 callback·epoch 조건 | old helper/Attempt 삭제 채택, 진단 reader는 main 그대로 | callback을 A leaf로 이동하며 사후 확인 실행기 재생성0 |
| `C/busy_inject/epoch.rs:3` | child 없음 | observe/condition | observe만 유지, 이제 호출자 없는 condition 제거 | dead_code 증가 방지, post-paste epoch guard 복구0 |
| `C/busy_inject/stash.rs:36` | load→선택 C-s→paste→settle→Enter | old paste leaf의 owner callback | content 직전 callback1을 A leaf에 이동 | load/C-s/Enter를 content로 세지 않음; scope 없는 caller는 no-op |
| `C/owner_input.rs:151` | owner scope 없음 | idle guard pin에서 epoch 확립 | idle admission의 최초 pin을 owner scope에서만 관찰 | A idle이 old guarded pin을 호출하지 않아도 S3 lease 유지, UI 판독 추가0 |
| `D/router/message_handler/turn_context.rs:19` | reply가 recent/knowledge 뒤 | 추출 helper 위임, 옛 reply 순서 | helper 위임 유지 | 추출 경계 보존 |
| `D/router/message_handler/input_payload.rs:18,24` + `input_payload/tests.rs:30` | trigger→recent→knowledge→reply→author/body | trigger→reply→recent→knowledge→author/body | helper 본문·golden을 main 순서로 맞춤 | 최신 main bytes/순서 보존 |
| `S/tui_input/submission_tmux/guarded.rs:188` | stash_attempted 필드 제거 | pin 직후 owner observe_pin 추가 | obsolete 필드는 제거, metadata pin은 유지 | old 필드/사후 stash monitor 복구0. 이 legacy leaf는 #7402 퇴역 범위와 후속 대조 |
| `T/queued_head/compat_tests.rs:203,237` | main에 V2 fixture 없음 | 기존 S3d writer는 admission_sequence1을 내지만 S2b golden에는 필드가 빠짐 | fixed golden에 admission_sequence1을 추가하고 old-fields 시험에서 이를 다시 생략 | 첫 target 223/1의 고유 단언 RED 확인. test/writer 모두 foundation과 동일한 기존 drift이며 production 수정0, enum/default 완화0 |
| `C/host_input/guarded_submission/idle_stash/idle_stash_tests.rs:607` 및623/663/676/690 hunk | A Authority/Request/InputRun API, view fixture | S3 시험은 삭제된 identity/transcript/Delivery/DraftState API 사용 | A Applied·commit-before-paste·C-s/paste/Enter 순서로 시험 적응, manifest 공식 생성 | compile 실패를 GREEN으로 숨기지 않음. 시험 renderer 완료만 기다리며 **운영 코드에 복원 감시 재추가0** |

`composer_lock.rs`와 Codex `busy_inject.rs`에는 충돌/수동 수정이 없었다. A Claude `before_paste`/`stash::submit`/managed idle은 legacy composer admission/protect를 부르지 않는다. 남아 있는 legacy guard와 Codex post-check를 A 완료라고 부르지 않으며 #7402/Codex A2와 S4 계획에서 처분했다. owner scope factory는 여전히 시험 전용이라 이번 metadata 이동으로 운영 owner caller가 생기지 않는다.

## 생성물·기계 해소

1. `git fetch origin main` 후 **공식 `merge_main_regen.sh`를 먼저 실행**했다. 최초 rc3으로 source4충돌을 확인하고 위 hunk를 해소했다. 다시 실행하여 공식 docs/PG/non-PG/CI 생성물을 갱신했다.
2. SQL inventory checker는 main에서 퇴역했으나 JSON이 남아 있다. merge script는 이 조합에서 generator를 건너뛰고 충돌 JSON을 stage해 `MERGE_REGEN_OK`를 출력했다. 이 표식만으로 성공 판정하지 않았다. JSON은 **main exact blob으로 해소**하고 parse/충돌표식0을 확인했다. main/head SHA256 둘 다 `0ac43bd76813e33a0c79a731d2b2fa8e10315144d03365fc8559473f651f0b30`. 퇴역 checker 부활·baseline 상향0.
3. 시험 rename 뒤 `cargo fmt --all`, `python3 scripts/check_test_target_integrity.py --write-lib-inventory-manifest`, `python3 scripts/check_pg_test_lane_membership.py --write-snapshots --manifest-only`를 실행했다. PG baseline은 보존됐다. 수동 행 편집·새 allow·allowlist 증가0. merge 커밋만 amend했다.
4. 공식 merge script가 쓴 명령: `generate_env_reference.py`, `generate_inventory_docs.py`, `generate_cli_reference.py`, PG `--write-non-pg-filter`/`--write-snapshots --manifest-only`/`--write-pg-db-paths`. 아래 diff는 **foundation→merge**이며 main에서 들어온 변경도 포함한다(수동 신규 등록 수와 혼동하지 않음).

| 생성 파일 | foundation 대비 diff 줄수 |
|---|---:|
| `.github/workflows/ci-pr.yml` | +13/-14 |
| `ARCHITECTURE.md` | +103/-39 |
| `scripts/ci/non-pg-test-filter.sh` | +30/-21 |
| `scripts/lib_test_inventory_manifest.txt` | +281/-122 |
| `scripts/pg_test_lane_manifest.txt` | +71/-9 |
| `scripts/sql_execution_surface_inventory.json` | +92/-101 |

## 시험·static 결과

- 첫 빌드(`882417…`)는 구 S3 idle-stash 시험의 삭제된 API10개 오류로 rc101. `logs/s4r2-build.jsonl.attempt1`/`s4r2-build.stderr.attempt1`/`s4r2-targets.json.attempt1`에 보존했다. **기능 RED/뮤턴트 kill로 세지 않는다.** 위 시험 적응 뒤 최종 head에서 다시 빌드했다.
- 두 번째 head `ec9fe93…`의 target은 **223/1/0**이고 static은 PASS였다. 유일 실패는 fixed golden의 admission_sequence 누락이다. 그 시점의 binary/로그/선택 목록은 `.attempt2`로 보존했고 관련 test/writer가 foundation과 같은 바이트임을 `logs/s4r2-golden-foundation.json`에 기록했다. fixture 수리 뒤 마지막 head에서 **기존 통과 시험도 전부 다시 실행**하고 잘못 마운트한 두 필터와 provider/catch-up 보충22개를 추가했다.
- 최종 cargo build rc **0**, 대상 union selected **246**, 실제 요약 **[['ok', '1', '0', '0'], ['ok', '1', '0', '0'], ['ok', '10', '0', '0'], ['ok', '246', '0', '0']]**, test rc **0**. top-level union은 요약 마지막 행이며 자식 harness의 별도1/10개 실행을 union에 더하지 않는다. full-lib 실행0, 기본 병렬. throwaway PG wrapper와 AGENTDESK_REQUIRE_PG=1을 사용했다.
- static rc **0**, `PREPUSH_STATIC_OK` **True**. raw log `logs/s4r2-static.log`. 규칙/allowlist/baseline 완화0.
- 현재 leg는 merge+PLAN이다. 새 S4 source·새 뮤턴트0. 이전 head의 기능/뮤턴트를 새 head 검증으로 재사용하지 않았다. 전체 Clippy/native/Windows 검증은 이번 요청 대상이 아니며 실행/통과했다고 보고하지 않는다. lib compiler warning 원시는 `logs/s4r2-compile-warnings.json`에 남겼고 손 해소 production 파일 warning0(전체Clippy PASS 주장 아님).

| 대상 필터 | union 내 선택 수(겹침 포함) |
|---|---:|
| `services::turn_orchestrator::queued_head::` | 83 |
| `services::claude_tui::owner_input_tests::` | 2 |
| `services::tui_input::human_frame::history::` | 16 |
| `services::discord::router::message_handler::input_payload::` | 4 |
| `services::discord::router::message_handler::intake_turn::session_preparation::` | 2 |
| `services::discord::health::turn_deliver::inject_tests::` | 37 |
| `services::discord::health::turn_deliver::inject_codex_tests::` | 16 |
| `services::discord::health::turn_deliver::inject::first_input::` | 3 |
| `services::discord::health::turn_deliver::inject::queued_reconcile::` | 9 |
| `services::discord::health::turn_deliver::inject::herdr::` | 6 |
| `services::claude::first_input::` | 3 |
| `services::codex::tui_session_launch::first_input::` | 1 |
| `services::claude::herdr_turn::first_input::` | 2 |
| `services::codex::herdr_turn::first_input::` | 2 |
| `services::claude_tui::busy_inject::` | 15 |
| `services::claude_tui::composer_lock::` | 6 |
| `services::tui_input::submission_tmux::guarded::` | 21 |
| `owner_idle_stash_commits_payload` | 1 |
| `s3d_regression_` | 2 |
| `services::discord::health::turn_deliver::inject::queued_provider::` | 1 |
| `services::discord::health::turn_deliver::inject::catch_up_race_tests::` | 6 |
| `services::discord::commands::control::clear_report::` | 10 |

## 크기·소유

- 이번 소스는 merge conflict 해소와 그 API/bytes 적응뿐이다. S4 신규 구현0. manual-path 명세와 3-way source 지문은 `logs/s4r2-manual-paths.json`, `logs/s4r2-source-fingerprints.json`에 있다. 위 테스트 파일은 지문에 추가했다.
- 공식 cap의 **main→누적 S1~S3 전체**는 prod84파일/순+4790, rc1 CAP FAIL이다(`logs/s4r2-cumulative-cap.log`). 이미 분리 리뷰 CLEAN인 foundation 전체를 한 신규 리뷰 단위 PASS로 꾸미지 않는다. 이번 델타 리뷰는 병합 손 해소 hunk이며, S4는 아래 별도 계획 단위로 제출한다. 단일 PR 전체를 단일 조각으로 잘못 측정하지 않도록 코디네이터에게 이 누적값도 공개한다.
- 손 해소 production 파일 집합만 공식 cap 분류/코드 계산 함수로 재면 foundation 대비 **9파일/순-504**다(`logs/s4r2-manual-scope-cap.json`). 이 값은 같은 파일에 들어온 main 변경도 포함하며 PR 전체 cap PASS가 아니다.
- S4 추정은 4a16/+461, 4b14/+599, 4c20/+609, 4d22/+964, 4e1 18/+516, 4e2 18/+476, 4f22/+6, 4h4/+260이다. 각 단위 추가 production 여유3파일/+120을 포함해도30/+1500과 PLAN선+1200 미만이다. 실제 base/head로 재측정하며 초과 예상이면 코드를 쓰기 전 재분할한다.
- main 대비 **production giant raw 증가0**이다. tests 파일의 1000줄 초과는 production과 구분했다.

| production giant | main raw | merge raw | delta |
|---|---:|---:|---:|
| `src/services/claude.rs` | 4250 | 4250 | +0 |
| `src/services/claude_tui/host_input.rs` | 1631 | 1631 | +0 |
| `src/services/codex/tui_session_launch.rs` | 1233 | 1099 | -134 |
| `src/services/discord/router/message_handler/intake_turn.rs` | 2541 | 2473 | -68 |
| `src/services/turn_orchestrator/pending_queue_persistence.rs` | 1199 | 1117 | -82 |

## 증거 목록

- head `a525e9be9886d9dd8f85b5c7f1a0c03176b377c6`, 두 부모 SHA는 §결과와 같다. 최종 빌드 후 source 변경0 여부는 `s4r2-targets.json`의 head/head_after와 git clean으로 대조한다.
- 실제 compiled binary: `/home/adk/adk/wt/i7272/target/debug/deps/agentdesk-387fb06612dd7698`.
- 보존 binary: `/home/adk/adk/lane-runs/i7272/logs/s4r2-tests`, SHA256 **`f09e6a8524e56e85357226720dbb60a2f71456cba6f2d76c1b6d50552207bfe7`**.
- 선택된 실제 시험 이름 전수: `logs/s4r2-selected-tests.txt`. 실행 수/실패/ignored/필터별 수/실제 이름: `logs/s4r2-targets.json`, 원시 stdout/stderr `logs/s4r2-targets.log`. 빌드 JSON/stderr는 `logs/s4r2-build.jsonl`, `logs/s4r2-build.stderr`.
- PG 실제 런타임: `logs/s4r2-pg-runtime.json`에 시험 프로세스의 AGENTDESK_REQUIRE_PG=1·loopback 동적 port·postmaster 존재를 자격값 없이 기록했다.
- 환경: SHELL=/bin/bash, TMPDIR=None (미설정이면 시스템 기본), cargo dev/test debug0·strip=none·incremental1·build jobs2, test profile/default 병렬. PATH 첫 cargo는 skill cargo-shim, ADK_CARGO_SLOT_HELD 직접 설정0. test execution은 실제 throwaway PostgreSQL을 사용했으며 운영 DB 사용0.
- `logs/s4r2-static.json`과 `s4r2-static.log`: 실제 명령/rc/표식. `s4r2-merge-first.log`, `s4r2-merge-resume.log`, `s4r2-merge-amend.log`: merge/generator 단계 원시 로그.
- `logs/s4r2-issue-inputs.json`: #7386/#7402/#6736/#6775/#6679/#7330/#7317 read-only 원문. `s4r2-main-drift.log`·`s4r2-entry-map.json`·`s4r2-file-budget.json`: 계획 좌표/드리프트/파일별 M/L raw·예상치. `plan-s4-r2.md`는 구현 완료 증거가 아니라 새 독립 설계 리뷰 입력이다.
- 공식 source-reader 별도 실행은 이번 merge/PLAN 요청에서 하지 않았다. source guard 검증은 명시한 target/static 범위이며 source-reader 전수 PASS로 확장하지 않는다. native/postreview 작업은 이번 leg에서 새로 띄우지 않았다. 종전 S3d native PID3338045는 이미 종료·rc1·compile 미도달로 재사용0.

## 리뷰 집중 항목

| 항목 | 막은 방법·남은 반례 |
|---|---|
| A안 UI 경계 | main의 판독1회·C-s 선택·paste/Enter1 채택. 사후 monitor/자동 잠금 재도입0. Codex A2 미완은 계획 [U] |
| callback/경합 | S3 callback은 실제 A content 직전에만 이동. scope 없는 운영 caller no-op. TLS fail-open/legacy content 누락 수리는 4a 첫 커밋으로 남음 |
| 옛 형식·원문 | V2 test-only custody/readonly guard 유지. SQL artifact는 main exact, pending 원문·native draft 복구 권한 확대0 |
| FreshTurn/LoadFailed | 00:48 두 결정은 S4 전이/actual leaf 시험으로 계획. 현재 main 결함이 이미 수리됐다고 주장0 |
| 잘못된0/PASS | 최초 compile rc101을 test RED로 세지 않음. 최종 selected>0·실제 count/PG·static marker·JSON parse·raw fingerprint를 별도 대조 |
| 운영 권한 | config/운영 runtime/DB/AgentDesk tmux 조작0, push/GH/DAG 쓰기0. kill/cleanup 권한 신규 추가0 |

## 진행 중·후속

진행 중인 이 레그의 검증 프로세스는 없다. foreground 검증은 모두 종료했다. S4 구현은 이 r2의 독립 설계 리뷰/소유 인계 뒤 별도 지시를 따른다. #7402와 Codex A2 전 운영 단일 경로를 열지 않는다. Native/Windows·S5 실제 CLI 수용은 미실행이며 최종 기능 head에서 코디네이터가 마감 순서를 결정한다.

PLAN_READY a525e9be9886d9dd8f85b5c7f1a0c03176b377c6
