# Atomic write: prepare, observe, publish

코드 기준 SHA: `762ee31d48d15cab091fa8fff5c7fa4e8c5c5409` (이동 전).

모델의 bounds는 아래 생산 호출 전수표와 기존 함수의 분기에서 가져왔다. `git grep -n -E '\batomic_write\b' -- src`의 원문을 생산 cfg scanner `ci.rust_test_sources.walk(..., production=True)`와 대조했다. 주석·시험 호출·3개 import는 호출에서 제외한다. 50개 직접 호출과 generation 함수 포인터 등록 1개(실제 호출 :612)를 포함한다. 경로·payload 문자열 자체를 나열하지 않고 기존 파일/첫 쓰기 전 부재/새 bytes와 inflight 관측 적용 여부로 나눈다. 실패 단계는 기존 create_dir_all/create_tmp/write_all/sync_all/rename, errno는 ENOSPC/기타의 관측 분류, 플랫폼은 Unix/Windows뿐이다. 임의 channel 수·queue 수·재시도 상한을 만들지 않았다.

## 진입점 전수표

| 생산 좌표 (이동 전) | 처리 |
|---|---|
| `src/config/session_hosts/approval_store.rs:54` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/config/session_hosts/approval_store.rs:156` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/config/session_hosts/approval_store.rs:167` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/config/session_hosts/approval_store.rs:176` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/abandon_request_store.rs:168` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/busy_followup_retry_store.rs:233` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/catch_up/consumed_commands.rs:80` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/catch_up/scan_progress.rs:83` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/e2e_control.rs:228` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/formatting/rollback_journal.rs:202` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/formatting/rollback_journal.rs:219` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/health/reachability/ledger.rs:313` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/health/reachability/ledger.rs:347` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/health/reachability/ledger.rs:381` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/health/reachability/ledger.rs:401` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/health/reachability/ledger.rs:468` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/health/reachability/ledger.rs:490` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/health/recovery/leak_recovery_ledger.rs:267` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/health/stall_liveness/redrive_grace.rs:228` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/inject_disposition.rs:395` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/inject_disposition/dispatched.rs:94` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/model_catalog/claude.rs:175` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/outbound/completed_turn_ledger.rs:347` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/outbound/completed_turn_ledger/o_pending.rs:73` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/queued_placeholders_store.rs:108` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/queued_placeholders_store.rs:131` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/relay_recovery_circuit_evidence.rs:285` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/restart_report.rs:144` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/runtime_bootstrap/gateway_handback_breaker.rs:119` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/runtime_bootstrap/gateway_lease_recovery.rs:344` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/runtime_store.rs:542` | 함수 포인터 등록; 실행 runtime_store.rs:612 |
| `src/services/discord/status_panel_orphan_store.rs:165` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/status_panel_singleton_store.rs:69` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/status_panel_singleton_store.rs:94` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/terminal_delivery_custody.rs:258` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/terminal_delivery_custody.rs:348` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord/turn_view_reconciler/store.rs:368` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/discord_config_audit/bot_settings.rs:249` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/legacy_body_census/history.rs:217` | legacy 제외; consumer 전환 없음 |
| `src/services/mcp_config.rs:166` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/mcp_config.rs:215` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/mcp_config.rs:222` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/mcp_config.rs:260` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/mcp_config.rs:298` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/mcp_config.rs:757` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/onboarding/mod.rs:744` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/onboarding/mod.rs:823` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/tui_input/human_frame/history.rs:185` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/tui_prompt_dedupe/handoff.rs:589` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/tui_prompt_dedupe/runtime_binding/codex_cursor.rs:280` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |
| `src/services/tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs:242` | 기존 관측 함수 호출; 이번 consumer 전환 없음 |

## 규칙과 결과

직접 실행: `java -XX:+UseParallelGC -cp ~/.local/share/tla2tools/tla2tools-1.7.4.jar tlc2.TLC -workers auto -config <cfg> AtomicWrite.tla`.

| cfg | exit | 종류·이름 | 결과 |
|---|---:|---|---|
| AtomicWrite.cfg | 0 | 위반 없음 | GREEN (448 distinct states, depth 7): OldPreserved, NoPartialPublication, OutcomeMatchesPublication, DiskObservation, DirectorySemantics |
| AtomicWriteMissingObservation.cfg | 12 | Invariant: DiskObservation | RED: CreateDir에서 ENOSPC 오류 후 record 없이 Err |

반례 코드 대조: 뮤턴트는 기존 classify_io_error의 `record_enospc_now` 및 ENOSPC 관측이 빠졌다고 가정한다. 현재 runtime_store.rs:860–866에서는 errno 28이면 기록과 warn 뒤 문자열을 반환하므로 현재 코드의 결함으로 확인되지 않았다. 현재 모델은 GREEN이며 동작 변경 없이 경계를 이동한다. 구현 뒤 같은 규칙을 다시 확인한다.

## 단계·관측 경계

`prepare`가 temp 생성→write→file sync를 소유한다. 상위가 기존 inflight 진단을 실행한 뒤 lower `publish`가 rename을 수행한다. `NotPublished { stage, error: io::Error }`와 `Published`로 반환 게시 여부를 표현한다. 원본 errno는 상위 분류까지 보존된다. write 실패로 부분 temp가 남을 수 있어도 target의 옛 bytes는 보존된다. temp 정리를 새로 추가하지 않는다.

plain atomic_write는 parent sync를 요청하지 않는다. file sync는 Windows에서도 실제 `sync_all`이다. Windows Ok-but-unflushed는 S07의 **parent directory** fsync 계약이며, 모델의 DirectoryCapability로 구별하고 parentSync는 항상 NotRequested이다. durable_publish의 parent/ancestor flush 정책과 성공 후 SyncFailed/UnflushedPlatform 구분은 이번 변경 범위 밖이며 T6 회귀로 확인한다.

모델 범위 밖: 프로세스 crash·전원 손실 뒤 durability, UUID collision, 외부에서 temp/target을 바꾸는 경쟁, 여러 동시 writer 사이의 마지막 rename 선택. 기존 함수는 별도 caller lock 여부를 바꾸지 않으며 이 모델은 단일 호출의 반환/관측 계약만 검증한다. 진단은 기존 코드처럼 rename 전 시도 기록이므로 rename 실패에도 남는다. 이를 성공 확인 로그로 바꾸지 않는다.

## 구현의 관측 회귀

상위 단계별 실패 시험은 CreateDir/CreateTemp/Write/FileSync/Rename의 EIO와 ENOSPC에 대해 반환 문자열·원본 target·진단 적용 시점·ENOSPC monitor 기록을 검사한다. 준비 실패를 Ok로 바꾸는 소스 뮤턴트와 record_enospc_now를 제거한 소스 뮤턴트는 각각 선택 1/실패 1(exit 101)로 RED였다. 각 원복 SHA256은 `b224035bfb7525e2012453bad2e6b57348b6ea78cc1c4bc243ec13c419322f9c`이며 원본 바이트와 일치했다. 기존 real rename hook도 lower 실제 syscall 앞에서 실행된다.
