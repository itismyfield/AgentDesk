# i5340_twomac 런북 r2 — 두 Mac 같은 시험 채널 위임 실측

- 설계 좌표(design_base): `2239d6c475517b1b219f4a2bcbe0b3bb8470c4bd`(main, #7021 포함). 이 문서의 file:line은 이 SHA 기준이다.
- 실측 바이너리(run_sha): **`<RUN_SHA>`** — 오늘 밤 두 노드 배포 SHA. 배포 완료 뒤 실제 값으로 치환한다. 미치환이면 S0 FAIL.
- 탈출 CLI(`abort-release`) 착지 SHA: **`<CLI_SHA>`** — `<RUN_SHA>`의 조상이어야 한다.
- r1 대비 변경은 문서 끝 "r2 변경 대응표"에 있다.

## 결론

권고 토폴로지는 r1과 같다. 현재 gateway(G)를 유지하고, 맥미니(H)를 실측 동안 Runner/RestWorker로 띄운 뒤, 같은 bulk1 시험 채널 `1509350490461180105`(C)의 Herdr endpoint를 단계적으로 왕복시킨다.

실측 바이너리는 특정 옛 SHA로 고정하지 않는다. 오늘 밤 배포가 두 노드에 나간 뒤 그 SHA(`<RUN_SHA>`)로 다시 고정한다. S0는 G·H의 git SHA와 실행 파일 SHA-256이 모두 같아야 통과한다. `2239d6c..<RUN_SHA>` 차이에서 §11의 전제를 다시 판정한다.

운영 PG를 손으로 고치는 복구는 쓰지 않는다. Releasing·Released에서 막히면 공식 실패 경로는 `agentdesk channel-home abort-release C --expected-epoch <E>`(CLI `<CLI_SHA>`)다. Worker·Reclaiming에서 holder가 다시 뜨지 않으면 공식 복귀 수단은 holder rescue boot(§R7)다. 이 경로가 S0b 연습에서 성립하지 않으면 S10·S12를 빼고 목표 3을 닫지 않는다.

핵심 순서:

```text
row 없음 / G-local
  → S0 신원·바이너리·rescue 자료 고정
  → S0b 위임 preflight dry boot (row 없음, 양 노드 switch=true, H=Runner, H rescue 기동 연습)
  → S1 기준선 → S2 G Herdr 실행 retire
  → S3 delegate → Releasing(release_epoch=E0) → G fenced restart → control snapshot → Released(worker_epoch=E1)
  → S4 endpoint를 H로 → G 먼저 restart(writer join) → H Runner restart → Worker(E1, holder=H)
  → S5~S12 두 Mac 실측 (S13 대조는 체크포인트마다)
  → S14 H 실행 retire → quiet barrier → reclaim → Reclaiming(E1) → Reclaimed(reclaimed_epoch=E2)
  → S15 endpoint를 G로 → H 먼저 restart(delegated writer join) → G restart → row 제거 → G writer
  → S16 row 없음 / G-local 최종 배달
```

이 순서는 생산 코드 변경 없이(탈출 CLI 착지 제외) 목표 1~5를 실측한다. 실행 전 사용자 승인 게이트는 §7에 모았다. 승인은 리뷰 CLEAN·CLI 착지 뒤 맥이 한 번에 올린다.

## 1. R1~R7 판정

### R1 — Full Standby인 맥미니는 Released를 adopt하지 못한다

판정: 사실. adopt 조건은 `releases_here && port.restored()`다(`src/services/cluster/channel_home_boot.rs:646-651`). Standby의 restored는 false로 시작한다(`src/services/discord/runtime_bootstrap.rs:509-522`). Standby 채널 복구 `restore_settled`는 holder 분기(`channel_home_boot.rs:663-666`)에서만 돈다. adopt하려면 restored가 필요하고, restored가 되려면 이미 holder여야 하는 순환이다.

해소: H를 `runtime_profile=runner`로 띄운다. RestWorker는 restored로 `reconcile_done`을 쓴다(`runtime_bootstrap.rs:513-519`). RestWorker 분기는 `runtime_bootstrap.rs:338-363`이다. 토큰을 먼저 심고(`:331`) delegated home(`:347`)·delegated writer(`:348`)를 시작한 뒤 queue를 복구한다(`:351`). Discord REST 자격증명이 없으면 queue 복구가 끝나지 않아 `reconcile_done`이 서지 않는다. 그러면 adopt가 fail-closed로 계속 기다리므로 S0b에서 먼저 확인한다.

Standby target 지원 코드 수정은 이번 노드에서 하지 않는다(후속 `i5340_standby_target_restore`, 맥 승인 대상, r1 §11 후속 1과 같다).

### R2 — Releasing·Released·Orphaned의 탈출

판정: 착지 전 base에는 운영 CLI 탈출이 없다(`src/cli/channel_home.rs:23-39`. `reclaim`은 Worker만 받는다. `force`는 holder NOT NULL만 받는다, `src/db/o_channel_homes.rs:422-455`). 이번 실측은 탈출 CLI 착지(`<CLI_SHA>`)를 진입 선행조건으로 둔다.

공식 계약(`docs/design/review/i5340-home-escape-cli-r1.md` §2를 본문으로 편입):

- 명령: `agentdesk channel-home abort-release C --expected-epoch <E>`. 실행 노드는 항상 원래 gateway G다. delegation switch가 켜져 있어야 하고 `cluster.instance_id`가 비어 있지 않아야 한다.
- CAS: DELETE 1문이다. `epoch=$E` ∧ (`state='released'` ∧ holder NULL ∧ target≠G) 또는 (`state='releasing'` ∧ holder=G)이고 open intake(current/other_epoch/unrouted)가 모두 0일 때만 지운다. 0행이면 거절 사유를 출력하고 비0 종료한다. 자동 재시도는 없다.
- **quiet 조건(escape CLI 코드 리뷰 r1 P2, 2026-10-16)**: DELETE의 open intake 검사는 그 문장의 snapshot만 본다. 그래서 이미 Worker row를 읽은 지연 라우터 INSERT나 leader의 `failed_pre_accept` 재시도가 abort 뒤에 옛 home_epoch 행을 만들 수 있다. 재시도 쪽은 CLI 수리 r1b가 코드로 막는다(exact Worker일 때만 holder에게 재시도). 그래도 abort 전에는 **양 노드의 AgentDesk 서버(dcserver)를 모두 정지**해 HTTP intake router와 leader retry maintenance를 없앤다. 정지는 `launchctl` 기준이고 프로세스 부재를 PID로 확인한다. 그다음 G에서 **별도 CLI 프로세스**로 `abort-release`를 실행한다. 성공하면 G를 먼저 띄우고, row 없음·G writer·단일 응답을 확인한 뒤 H를 띄운다. 서버가 떠 있는 hot abort는 이번 실측에서 쓰지 않는다(일반 지원은 공통 lock 계약을 담은 별도 설계 후속).
- 지원 상태와 epoch:

| row 상태 | 사용 epoch | 실행 전 확인 | 사용 시점 |
|---|---|---|---|
| Releasing | E0(release_epoch) | endpoint G-local, H health `endpoint local=false`. G fenced restart 뒤에도 정해진 blocker(`turn_running`·`owed`·`posts`·`source_reset`·`not_held`·`turn_unreadable`)가 남음 | S3 |
| Released | E1(worker_epoch) | endpoint를 G로 되돌림. H 정지 또는 재기동 뒤 H health `endpoint local=false`·`channel_homes`에 C 없음(H writer 부재). G `channel_homes`에 C 없음 | S3·S4 |
| Worker·Reclaiming | — | CLI가 거절한다(`state worker`/`reclaiming`). holder가 살아 있으면 정상 reclaim, 죽어 있으면 §R7 rescue | S4 이후 |
| Reclaimed | — | CLI가 거절한다. 기존 `remove_reclaimed` 경로(S15) | S15 |
| Orphaned | — | CLI가 거절한다(미지원). twomac 중 `force` 금지로 도달하지 않는다. 생기면 실측 중단, 증거 보존, 사용자·맥 결정 | — |

- 성공 뒤 공통 순서: ① `channel-home status`에서 C row 없음 확인 → ② G fenced restart(`agentdesk restart-dcserver`) → ③ G health에서 C writer·intake 확인, 짧은 nonce 입력 1회가 G에서 정확히 한 번 응답하는지 확인. delegation switch off는 이 뒤 row 없음을 다시 확인한 다음에만 한다.
- open_intake blocker로 막힌 경우: CLI가 거절한다. 실측을 중단하고 사용자·맥이 결정한다(CLI 설계 [U-1]).
- CLI 착지 증거: 구현 커밋 `<CLI_SHA>`, T1~T6 결과와 **T4b(finish_release ↔ abort 경합, 두 순서 모두 Applied 합계 1)**를 포함한다. T4b가 빠진 착지는 실측 진입 조건을 채우지 못한다.

### R3 — endpoint는 한 노드에 고정이며, 이동 순서가 중요하다

판정: 사실(r1과 같다). endpoint를 H로 먼저 옮기고 delegate하면 G의 Releasing drain이 `legacy_reset`을 부른다(`runtime_bootstrap.rs:631-667`). Herdr-configured 채널이라 reset이 거절되고(`admin_host_guard.rs`의 configured refusal) `source_reset` blocker에 고정된다.

- forward: endpoint G-local에서 G 실행 retire → delegate → G restart → Released → endpoint를 H로 → G 먼저 restart → H restart → adopt.
- return: endpoint H-local에서 H 실행 retire → quiet barrier → reclaim → Reclaimed → endpoint를 G로 → H 먼저 restart → G restart → row 제거.
- Retired 실행은 다음 Herdr cold launch의 유효한 선행 상태다(`src/services/herdr_launch.rs`의 `HostedState::Retired` → 새 Pending 설치 허용).

### R4 — 하네스는 mixed endpoint를 거절한다

판정: 사실(`scripts/e2e/tui_relay/herdr.py:86-105`). configured endpoint가 하나뿐이면 수정 없이 쓴다. 위임 단계는 H loopback, 복귀 단계는 G loopback에서 실행한다. 하네스 결과는 보조 증거이고 row·writer·위임의 최종 PASS가 아니다. 추가 endpoint가 발견될 때만 시험 전용 `--expected-local-channel C`(예상 +20~40줄)를 둔다.

### R5 — home 등록과 delegated writer 시작은 boot 고정이다

판정: 사실. `channel_home_boot::start`(`channel_home_boot.rs:396`)는 boot 때 row와 candidate를 읽는다. delegated writer는 boot에서 한 번 시작한다(`runtime_bootstrap.rs:348` `o_writer_host::spawn_delegated`). 규칙은 이렇다. delegate 뒤 G restart. Released·endpoint 변경 뒤 G 먼저, H 다음. Reclaimed·endpoint 원복 뒤 H 먼저, G 다음. 재시작마다 새 PID·process start time·row mount·writer readiness·로컬 O ledger를 남긴다.

### R6 — preflight 실패는 provider의 선택된 Herdr 채널 전체를 보류한다

판정: 사실. `preflight`는 `MissingPool`·`MissingInstanceId`만 낸다(`src/services/cluster/home_availability.rs:56-64`). 실패하면 그 provider의 `selected_herdr_channels` 전체를 held로 기록한다(`home_availability.rs:125-149`, 입력 `runtime_bootstrap.rs:541-555`, 집합 `:557-578`). 이 집합은 turn(`turn_host.rs:119`)·intake routing(`intake_router_hook.rs:130`, `tui_o/cutover/intake_route.rs:39,58`)·O writer(`tui_o/writer/host.rs:502`)·delivery(`tui_o/writer/deliver.rs:161`)·home stop(`session_forwarding/home_stop.rs:165,329`)에서 모두 거절된다. 다른 provider와 선택되지 않은 tmux 채널은 영향을 받지 않는다.

관측 한계: `Availability`(Off/PreflightPassed/Unavailable)는 health에 노출되지 않는다(소비자는 `home_stop.rs:161,326`뿐). 판정은 boot 로그의 `channel home delegation unavailable; its channels are held`(`home_availability.rs:133-137`, `provider`·`reason` 필드) 부재와, 선택된 채널마다 실제 nonce 응답으로 한다. 이 확인을 row가 없는 S0b에서 끝낸다.

### R7 — Worker·Reclaiming에서 holder가 다시 뜨지 않으면 공식 복귀 수단이 없다 (r2 신규)

판정: 사실. Worker·Reclaiming은 `abort-release`가 거절한다. `reclaim`은 Worker를 Reclaiming으로 바꿀 뿐이다(`o_channel_homes.rs:347-365`). `finish_reclaim`은 holder H의 watch·drain만 실행한다(`channel_home_boot.rs:662-676` → `channel_home_drain.rs:133-208`). `force`는 Orphaned를 만들고 그 상태는 지원하지 않는다. 따라서 S10·S12·S14에서 H가 다시 뜨지 않으면 C가 무기한 멈춘다.

추가 사실: `restart-dcserver`는 launchd 재기동 뒤 30초 안에 ready 로그가 없으면 `releases/previous`로 current 링크를 되돌리고 다시 띄운다(`src/cli/dcserver.rs:578`(READY_TIMEOUT 30s), `:693-719`, `:442-461`). 즉 S10의 재시작 실패는 "다른 SHA의 H가 같은 Worker row를 mount"하는 상태로 이어질 수 있다.

계약 — holder rescue boot. PG row는 건드리지 않는다. 이미 검증된 `<RUN_SHA>` 실행 파일을 launchd 밖에서 같은 config·runtime root·환경으로 다시 띄워 같은 epoch를 mount하게 한 뒤 정상 reclaim으로 간다.

1. 보존(S0, H와 G 모두. 실행은 H 기준):
   - 실행 파일: 실행 중 dcserver PID의 실행 파일 경로(`lsof -a -p <PID> -d txt -Fn`)와 SHA-256. 그 파일을 증거 디렉터리 밖의 노드 로컬 고정 경로 `~/.adk/lane-runs/i5340/twomac/rescue/<run-id>/agentdesk-<RUN_SHA>`로 `cp -p`하고 hash를 대조한다(=RESCUE_BIN).
   - launch 정의: `~/Library/LaunchAgents/com.agentdesk.release.plist`(label 근거 `scripts/refresh_release_launchd_plist.py:14,40-41`) 사본(0600, 비밀값 포함 가능하므로 공유 증거에는 hash만). 정의 형태는 `src/cli/init.rs:498-535`다. ProgramArguments=`<bin> dcserver`, WorkingDirectory=`<root>`, EnvironmentVariables=PATH·HOME·`AGENTDESK_ROOT_DIR`·`AGENTDESK_DCSERVER_LABEL`과 `<root>/config/launchd.env` 항목(`init.rs:494-497,553-570`), stdout/stderr=`<root>/logs/dcserver.launchd.{stdout,stderr}.log`.
   - Runner config: `instance_agentdesk_config_path()`(`dcserver.rs:334`)가 가리키는 파일 사본(0600)과 hash. runtime root: `AGENTDESK_ROOT_DIR` 값과 realpath.
   - release 링크: `<root>/releases/current`·`previous`의 대상과 각 `agentdesk` hash(`dcserver.rs:350-360`). previous의 SHA와 `previous..<RUN_SHA>` critical-path diff를 기록한다.
2. 독립 수동 기동 명령(코드 좌표로 구성. 이 문서 작성 중 실행하지 않았다):

   ```sh
   # launchd KeepAlive 재기동을 멈춘다. 떠 있는 dcserver가 있으면 SIGTERM으로 graceful 종료된다.
   launchctl bootout gui/$(id -u)/com.agentdesk.release
   # 같은 root의 dcserver가 0개인지 확인(dcserver.rs:308-327 판정과 같은 pgrep 패턴)
   pgrep -fl 'agentdesk.*dcserver'
   # plist와 같은 cwd·환경·인자로 RESCUE_BIN을 띄운다(launchd.env 항목은 S0 사본에서 그대로 옮긴다)
   tmux new-session -d -s adk-rescue-<run-id> \
     "cd '<root>' && /usr/bin/env -i PATH='<plist PATH>' HOME='<HOME>' \
        AGENTDESK_ROOT_DIR='<root>' AGENTDESK_DCSERVER_LABEL=com.agentdesk.release <launchd.env 항목> \
        '<RESCUE_BIN>' dcserver >> '<root>/logs/dcserver.launchd.stdout.log' 2>> '<root>/logs/dcserver.launchd.stderr.log'"
   ```

   - 단일 인스턴스 flock(`<root>/runtime/dcserver.lock`, `dcserver.rs:881-909`)과 고아 정리(`kill_existing_dcserver_processes`, `dcserver.rs:463,913`) 때문에 launchd를 먼저 내려야 한다. 그러지 않으면 KeepAlive 재기동과 lock을 다툰다.
   - 기존 tmux fallback 런처(`dcserver.rs:752-800`, `<root>/scripts/_launch_dcserver.sh`)는 `<root>/bin/agentdesk`와 ROOT·LABEL만 export하고 launchd.env 항목을 싣지 않는다. 그래서 rescue에는 쓰지 않는다.
   - 복귀: rescue H가 reclaim·S15를 마친 뒤 rescue 프로세스에 SIGTERM을 보낸다(graceful, writer join). 그다음 `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.agentdesk.release.plist`. plist가 가리키는 실행 파일 hash가 `<RUN_SHA>`가 아니면(예: rollback으로 current 링크가 바뀜) launchd 복귀를 하지 않고 맥이 결정한다.
3. 발동(S10·S12·S14 실패 시):
   - endpoint는 H-local로 유지한다. G·H 설정을 되돌리지 않는다. C 신규 입력을 금지한다(운영자 공지).
   - `restart-dcserver` 출력에 `rolling back current release`가 보이거나, H 실행 파일 hash가 `<RUN_SHA>`가 아니면 바로 발동한다. 1회 재시도 없이 rescue로 간다.
   - rescue H가 같은 row를 mount하는지 본다. `channel-home status`에서 state(Worker 또는 Reclaiming)·holder=H·epoch=worker_epoch(E1) 불변. `renewed_at`이 5초 주기(`RENEW_EVERY`, `src/services/cluster/channel_home.rs:22`)로 **두 번 이상** 갱신된다. H health `channel_homes[C].home=intake_open`(Worker) 또는 draining(Reclaiming). delegated writer hosted(필드 이름 [U-3]). `writer end is unconfirmed` 없음. 실행 파일 hash=`<RUN_SHA>`.
   - writer 증명이 끝나면 정상 경로로 간다. Worker면 S14(quiet barrier → reclaim), Reclaiming이면 drain 완료를 기다린 뒤 S15.
   - S12 중이었으면 위 확인에 더해 진행 중 turn의 terminal 한 번·Discord 응답 한 개·duplicate 없음을 판정한 뒤 S14로 간다.
4. 시험 가능성: S0b-3에서 row 없음 상태로 rescue 기동을 연습한다. 연습이 실패하거나 실행할 수 없으면 S10·S12를 이번 실측에서 빼고, 보고서에 "목표 3(holder 재시작 경계) 미닫힘"을 적는다. S14는 return에 필요하므로 남긴다. 그 경우 S14 중 H 부팅 실패는 실측 중단, 증거 보존, 사용자·맥 결정으로 간다.

## 2. 전제 고정표

표기: G=현재 gateway 100.71.169.10(MacBook 여부 [U1]), H=Mac-mini.local, C=1509350490461180105.

| 전제 | G 필요값 | H 필요값 | 확인(읽기 전용) | 변경·적용 |
|---|---|---|---|---|
| 실행 바이너리 | `<RUN_SHA>` | `<RUN_SHA>` | `/api/health` 배포 head, 실행 파일 경로·SHA-256 | 변경 없음. git SHA 또는 hash가 다르면 S0 FAIL |
| 설계 좌표 | `2239d6c47…` | 같음 | `git diff 2239d6c..<RUN_SHA>`, §11 재판정 | manifest `design_base`에 분리 기록 |
| 탈출 CLI | `<CLI_SHA>` ⊂ `<RUN_SHA>` | 같음 | `git merge-base --is-ancestor <CLI_SHA> <RUN_SHA>`, `agentdesk channel-home abort-release --help` | 미포함이면 S0 FAIL |
| cluster.instance_id | G ID | H ID(서로 다름, 비어 있지 않음) | config 사본, cluster registry, health | boot 적용 |
| delegation switch | Some(true) | Some(true) | config 사본 | S0b에서 켬(재시작) |
| intake routing | enabled, enforce | 같음 | health(현재 두 노드 이 값) | 유지 |
| owner authority allowlist | 0 | 0 | health detail | 유지 |
| cluster 역할·profile | Gateway/Full | Runner/RestWorker | health `runtime_profile`·`modules`·`cluster_standby` | H만 S0b에서 변경(재시작) |
| gateway preference | G | G | config 사본 | 변경 없음 |
| endpoint | Phase A·C: G-local | Phase B: H-local | health `herdr.endpoints.<key>.{node,local}`(`src/server/routes/health_api/runtime_profile.rs:28-33`) | S4·S15에서 파일 변경, 재시작 순서 R5 |
| endpoint key | 기존 `mac-book` 유지 | 같음 | 같음 | rename 금지 |
| Herdr turn switch | provider true | 같음 | config(live) [U] | live |
| Herdr terminate switch | retire 직전만 true | 같음 | config(live) [U] | 사용 직후 false |
| O writer selection | C 포함, 양 노드 같은 명시 목록 | 같음 | config 사본, health | boot. all_tui 확대 금지 |
| selected_herdr_channels | 기록 | 기록 | writer selection ∩ provider TUI kind ∩ health `herdr.configured_channels`(`runtime_bootstrap.rs:557-578`의 계산을 운영 자료로 재현) | 직접 노출 없음 [U21] |
| H Discord REST 자격증명 | — | 있음 | S0b-2 | 없으면 S0b FAIL |
| O store | C ledger readable | 같음 | `<root>/o_store/C/ledger.jsonl` 사본 | 손상·unresolved면 중단 |
| PG pool | available | available | health, startup log | 변경 없음 |
| C row | 없음 | — | `channel-home status`(health의 `channel_homes` 부재로 판정하지 않음) | — |
| rescue 자료 | 보존 | 보존 + S0b 연습 PASS | §R7-1 | 없으면 S10·S12 제외 |
| health 접근 | loopback detail 200 | loopback detail 200 | 각 노드 loopback | — |
| 대조 채널 | 변경 없음 | 변경 없음 | selected_herdr_channels 중 C 외 전부 + 비위임 tmux 하나 | — |
| 현재 degraded | backlog·released alarm 원인 특정 | standby degraded는 Runner 전환 후 사라짐 | health 스냅샷 | 항목별 소명 없으면 중단 |

## 3. 토폴로지 선택

r1과 같다. A안(G 유지, H=Runner, 같은 C, endpoint 왕복)을 권고한다. B안(H Standby + 코드 수정)은 후속이다. C안(gateway를 H로 이동)은 기각한다. D안(새 전용 채널)은 보조 시험만 가능하다. Provider는 Claude 하나로 고정한다. 다른 채널의 endpoint·writer selection은 바꾸지 않는다.

## 4. epoch 계약과 관측 신호

### 4.1 epoch 전이 (코드 계약, `src/db/o_channel_homes.rs` design_base)

epoch는 `o_channel_home_epochs` sequence의 `nextval`에서만 새로 나온다. sequence는 채널 사이에 공유되므로 연속값을 기대하지 않는다. 판정은 "이전과 다름(그리고 증가)"으로만 한다.

| 전이 | DB 함수(line) | nextval | 결과 epoch | manifest 이름표 |
|---|---|---|---|---|
| 없음 → Releasing(holder=G,target=H) | `delegate` 253-277(264행 nextval) | 예 | E0 | `release_epoch` |
| Releasing → Released(holder NULL,target=H) | `finish_release` 305-313 → `leave` 280-303(290행 nextval) | 예 | E1 | `worker_epoch` |
| Released → Worker(holder=H,target NULL) | `adopt` 326-344 | **아니오** | E1 유지 | `worker_epoch` |
| lease 갱신(renewed_at만) | `renew` 392-418 | 아니오 | 유지 | — |
| Worker → Reclaiming(holder=H,target=G) | `begin_reclaim` 347-365 | **아니오** | E1 유지 | `worker_epoch` |
| Reclaiming → Reclaimed(holder NULL,target=G) | `finish_reclaim` 315-323 → `leave`(290행) | 예 | E2 | `reclaimed_epoch` |
| Reclaimed → row 없음 | `remove_reclaimed` 368-389(DELETE, epoch=E2 조건) | — | 없음 | — |
| holder 상태 → Orphaned | `force_orphan` 422-455(431행 nextval) | 예 | 새 값 | 이번 실측 금지 |
| Releasing/Released → row 없음 | `abort-release`(`<CLI_SHA>`) DELETE, epoch 조건 | — | 없음 | 실패 경로 전용 |

정상 시퀀스: `Releasing E0 → Released E1 → Worker E1 → Reclaiming E1 → Reclaimed E2 → row 없음`.

판정기는 manifest에 이름표만 받고 E0/E1/E2 별칭을 스스로 계산한다. 규칙: `worker_epoch ≠ release_epoch`, `reclaimed_epoch ≠ worker_epoch`. Released·Worker·Reclaiming의 epoch는 모두 `worker_epoch`와 같다. 위임 중 O ledger `Prepared.epoch`도 모두 `worker_epoch`다.

### 4.2 관측 신호

- R — PG row: `<ADK> channel-home status`(`src/cli/channel_home.rs:112-151`). state·holder·target·epoch·renewed_at·updated_at·open_intake(current_epoch/other_epoch/unrouted)를 본다.
- M — 프로세스 메모리: 각 노드 loopback `/api/health/detail`의 `runtime_profile`·`modules`·`herdr.configured_channels`·`herdr.endpoints`·`herdr.admission`·`restart_required`·`channel_homes[C]`(home/epoch/drain_blocker)·degraded. `channel_homes` 부재는 gate 미등록일 뿐 row 부재가 아니다. row 존재는 항상 R로 판정한다.
- H — Herdr 실행: `<ADK> herdr status`의 session key·execution nonce·Hosted state·node·endpoint key·pane ID·input hold.
- O — holder managed writer: `<root>/o_store/C/ledger.jsonl`. H ledger의 `Prepared(epoch=worker_epoch)`와 같은 serial의 `Posted(msg_id=M)`이 있어야 한다. G ledger에는 M이 없어야 한다. Discord에서 M은 정확히 하나다. 그 시각 row는 Worker(holder=H, epoch=worker_epoch)다.
- D — Discord REST history: 입력·응답 message ID, author, timestamp, reply reference, run nonce, content SHA-256, 응답 수. 응답은 한 메시지 분량의 짧은 문자열로 제한한다.
- L — 로그: PID·start time, `SIGTERM received — graceful shutdown`, `committed restart joins every home`, `a delegated home's writer end is unconfirmed`, row unreadable·source reset·turn unreadable, `channel home delegation unavailable; its channels are held`, O writer alarm, provider/Herdr restore 결과, `rolling back current release`.
- P — pane·transcript 최소 증거: nonce 입력 1회, transcript 턴 1회, terminal record 위치·digest.

## 5. 공통 시간 상한과 중단 기준

| 작업 | 상한 |
|---|---|
| 재시작 후 health 복구·역할 판정 | 120초 |
| idle row 전이 | holder health 정상 후 60초 |
| Released → Worker adopt | target health 정상 후 30초 |
| 짧은 turn / 장시간·재시작 경계 turn | 180초 / 300초 |
| POST 후 중복 부재 관측 | 30초 |
| `!clear` 응답 | 90초 |
| row 전이 전체 중단선 | 120초 |
| quiet barrier | 5초 간격 2회 |

즉시 중단 조건:

- 같은 nonce의 bot message ID가 2개 이상이다.
- Worker가 아닌데 일반 입력이 pane에 도달한다.
- `Prepared.epoch` ≠ `worker_epoch`(위임 중).
- 전이 완료 때 open intake 세 값 중 하나라도 0이 아니다.
- `writer end is unconfirmed`가 찍힌다.
- endpoint local이 두 노드에서 모두 true이거나 모두 false다.
- C 외 채널에 home row가 생긴다.
- control 채널의 host·session·endpoint가 바뀐다.
- terminate 결과가 indeterminate·provider_survived_close·confirmation_required다.
- row unreadable, O store 손상, ambiguous/unresolved POST가 보인다.
- 120초 안에 전이가 끝나지 않는다.
- force가 필요해진다.
- 노드 실행 파일 hash가 `<RUN_SHA>`와 다르다(R7 발동 조건이기도 하다).

## 6. 실측 시나리오

### S0 — 신원·바이너리·rescue 자료 고정 (읽기 전용)

진입: 설정·DB 무변경. C에 active turn 없음. 두 노드가 `<RUN_SHA>`로 배포되어 있다.

조작:
1. G·H의 hostname·instance ID·PID·process start time·실행 파일 경로와 SHA-256·`/api/health` 배포 head를 수집한다.
2. 관련 config 절(비밀값 제거)과 hash, cluster registry를 저장한다.
3. R·M·H·O 스냅샷을 저장한다.
4. `git diff --name-status 2239d6c..<RUN_SHA> -- <§11 경로>`와 critical-path diff를 `01-config/2239d6c..<RUN_SHA>-critical-path.diff`로 저장한다. §11 재판정표를 작성한다.
5. `<CLI_SHA>`가 `<RUN_SHA>`의 조상인지 확인한다. CLI 착지 증거(T1~T6, T4b) 경로를 기록한다.
6. §R7-1 rescue 자료를 보존한다(H 필수, G 권장).
7. 대조 채널을 정한다. C 외 selected_herdr_channels 전부와 비위임 tmux 하나다.

PASS(120초):
- G·H git SHA가 둘 다 `<RUN_SHA>`이고 실행 파일 SHA-256이 서로 같다.
- RESCUE_BIN hash도 같은 값이다.
- manifest에 미치환 자리표시자(`<RUN_SHA>`·`<CLI_SHA>`)나 옛 SHA가 없다.
- critical-path diff와 §11 재판정표가 있고 모든 행이 "변화 없음" 또는 "재판정 완료"다.
- instance ID가 확정됐고 서로 다르다. C row가 없다. PG·Herdr·O store를 읽을 수 있다.
- provider backlog·deferred hooks가 0이거나 C와 무관하다는 항목별 증거가 있다.
- `tui_o:released:*` alarm이 C의 home row가 아님을 R로 확인했다.
- doctor failed 항목이 시험 경로와 무관하다.
- endpoint가 G에만 local이다.

즉시 중단:
- 노드 신원 불명, SHA·hash 불일치, CLI 미포함.
- C row 존재, O ledger 손상, backlog 귀속 불명.
- 추가 endpoint가 있는데 R4 대응이 없다.

롤백: 없음(읽기 전용).

### S0b — 위임 preflight dry boot (row 없음)

진입: S0 PASS. C row 없음. endpoint G-local. §7 승인(switch·H Runner 전환) 완료.

조작:
1. 양 노드 config에 `runtime.channel_home_delegation_enabled=true`를 넣고 H config를 Runner(`runtime_profile=runner`, `cluster.role=runner`)로 바꾼다. **home row는 만들지 않는다.**
2. G를 fenced restart(`agentdesk restart-dcserver`)한다.
3. H는 §R7-2 독립 수동 기동 명령으로 띄운다(rescue 연습). 이어 R7-3의 확인(hash·health·역할)을 한다. 그 뒤 rescue 프로세스에 SIGTERM을 보내고 `launchctl bootstrap`으로 launchd 기동으로 되돌린다.
4. 양 노드에서 다음을 기록한다.
   - boot 로그에 `channel home delegation unavailable; its channels are held`가 없다(provider=claude). 있으면 `reason`을 기록한다.
   - 실제 selected_herdr_channels 집합(§2 계산)을 `S0b/selected_herdr_channels.json`에 저장한다.
   - H: `runtime_profile=runner`, `cluster_standby=false`, gateway 모듈 off, `herdr.endpoints.mac-book.local=false`, `channel_homes` 키 없음.
   - H Discord REST 자격증명: dcserver가 onboarding mode로 뜨지 않았다(`dcserver.rs:589-596` 경고 부재). RestWorker queue 복구가 끝났다(`runtime_bootstrap.rs:351` 이후 reconcile 완료 신호, 정확한 로그 키 [U22]).
5. selected_herdr_channels의 각 채널(C 포함)과 비위임 tmux 대조 채널에 nonce를 1회씩 보낸다.

PASS(재시작 각 120초, 응답 각 180초):
- availability가 PreflightPassed로 판정된다(위 로그 부재와 5의 응답으로 판정).
- 선택된 각 채널이 G에서 정확히 한 번 응답한다.
- H rescue 연습: RESCUE_BIN hash=`<RUN_SHA>`, Runner 역할, health 정상, SIGTERM 종료가 graceful이다. launchd 복귀 뒤에도 같은 hash·역할이다.
- H REST 자격증명을 확인했다.

FAIL 처리:
- availability·응답·REST 실패: row가 없으므로 양 노드 switch를 원복하고 H 역할을 Full/Standby로 원복한 뒤 재시작하고 종료한다. 원인을 분석한 뒤 다시 계획한다.
- rescue 연습만 실패: S1 이후는 진행할 수 있다. 다만 S10·S12를 제외하고 목표 3 미닫힘을 확정한다(§R7-4).

### S1 — gateway-local 기준선과 대조 채널 기준선

진입: S0b PASS. row 없음. endpoint G-local.

조작: C와 각 control 채널에 nonce 입력 1회씩.

PASS(180초 + 30초 quiet):
- C는 G pane에서 1회 실행되고 G O ledger에 Prepared/Posted 하나, Discord 응답 하나다.
- control 채널도 각자 응답 하나다.
- host·session·endpoint·writer가 S0b 뒤와 같다.
- queue·POST가 0으로 돌아온다.

중단: 기준선부터 duplicate·missing, control 상태 변화, `rotation_unsettled`·O alarm·inflight 미정리.

롤백: row 없음. switch 원복과 재시작.

### S2 — G Herdr 실행 retire

진입: S1 완료. active turn·queue·owed·POST 0. endpoint G-local. row 없음.

조작: G에서 `runtime.herdr_terminate_enabled=true`(live). G의 exact session key·execution nonce로 operator terminate. 완료 직후 false로 되돌린다.

PASS(90초): 결과가 정확히 retired다. G 실행 row가 Retired다. pane에서 provider가 사라졌다. hold가 없다. Discord 추가 응답이 없다.

중단·롤백: nonce 불일치·indeterminate·provider survived·confirmation required·active turn·hold 잔존이면 재시도하지 않고 수동 판정한다. row·endpoint는 바꾸지 않는다.

### S3 — delegate와 gateway release

진입: S2 PASS. endpoint G-local. C row 없음. 양 노드 switch=true(S0b). H는 Runner로 떠 있다(endpoint가 G-local이므로 H candidate 아님).

조작:
1. G에서 `<ADK> channel-home delegate C --provider claude --to <H_INSTANCE_ID>`. 즉시 R을 저장한다.
2. G에서 `<ADK> restart-dcserver`.
3. **G 재시작 직후 control snapshot**: selected_herdr_channels 중 C 외 채널과 tmux 대조 채널의 host·session·endpoint를 기록하고 nonce 1회씩 보낸다. availability 거절 로그가 없는지 확인한다.

관측·PASS:
- delegate 직후: Releasing, holder=G, target=H, epoch=release_epoch(E0).
- G 재기동 후 120초 안에 health 정상. 추가 60초 안에 Released, holder=NULL, target=H, epoch=worker_epoch(E1), E1≠E0.
- open intake 세 값이 모두 0이다.
- G 로그에 writer 종료 실패가 없다. drain blocker는 최종적으로 없다.
- control snapshot이 S1과 같고 각 채널이 응답 하나씩이다.

중단: source_reset blocker. Retired 실행이 Bound/Pending으로 바뀐다. endpoint가 G-local이 아니다. Releasing이 120초를 넘긴다. open intake가 남는다. control 채널에 거절이나 무응답이 생긴다.

롤백:
- **Releasing에서 정지**: G와 endpoint(G-local)를 그대로 두고 G를 한 번 더 fenced restart해 drain을 이어 간다. 그 뒤에도 120초 안에 Released가 되지 않고 blocker가 `turn_running`·`owed`·`posts`·`source_reset`·`not_held`·`turn_unreadable` 중 하나로 남으면 다음 순서로 간다. H health `endpoint local=false` 확인 → G에서 `abort-release C --expected-epoch <release_epoch>` → status에서 row 없음 → G fenced restart → G writer와 C nonce 단일 응답 확인. blocker가 `open_intake`면 CLI가 거절하므로 실측을 중단하고 사용자·맥이 결정한다.
- **Released에서 정지**(S4로 갈 수 없을 때): endpoint는 아직 G-local이다. H를 정지하거나 재기동해 H health `channel_homes`에 C가 없음을 확인하고, G `channel_homes`에 C가 없음을 확인한다. 그다음 G에서 `abort-release C --expected-epoch <worker_epoch>` → row 없음 → G fenced restart → G writer·단일 응답 확인.
- control 채널 이상: C 진행을 멈추고 위 상태별 롤백으로 row를 없앤 뒤 switch를 원복한다.

### S4 — endpoint 이동, G writer join, H adopt

진입: row가 정확히 Released(worker_epoch=E1, target=H). open intake 0. G 실행 Retired. G gateway writer가 POST 중이 아니다.

조작:
1. 양 노드 canonical endpoint 설정을 H로 바꾼다(key 유지).
2. G를 먼저 fenced restart한다. G health에서 endpoint local=false이고 C writer가 hosted가 아님을 확인한다.
3. H를 fenced restart한다(Runner 설정은 S0b에서 이미 적용됨). H loopback health와 R을 반복 관측한다.

PASS:
- G 이전 writer가 join됐다. 새 G는 row를 foreign으로 읽고 C writer를 시작하지 않는다.
- H는 Runner, endpoint local=true, delegated writer hosted다.
- **Released(E1) → Worker(epoch=E1 그대로, holder=H, target=NULL)**. epoch 증가를 기대하지 않는다.
- `channel_homes[C].home=intake_open`. `renewed_at`이 두 번 이상 진전한다.
- adopt는 30초 안, 전체는 120초 안에 끝난다. local=true인 노드는 정확히 하나다.

중단: H가 Standby로 뜬다. G writer가 계속 hosted다. H가 adopt했는데 writer가 없다. Worker인데 gate가 lost·draining이다. local 충돌이 있다. hash가 `<RUN_SHA>`가 아니다.

롤백:
- **row가 아직 Released**(H adopt 실패): ① 양 노드 endpoint 설정을 G로 되돌린다. ② H를 정지하거나 재기동해 H health `endpoint local=false`·`channel_homes`에 C 없음(H writer 부재)을 확인한다. ③ G `channel_homes`에 C 없음을 확인한다. ④ G에서 `abort-release C --expected-epoch <worker_epoch>`. ⑤ status에서 row 없음. ⑥ G fenced restart(endpoint G-local 적용, row 없음 → gateway writer). ⑦ G writer와 C nonce 단일 응답을 확인한다. CLI가 `state worker`로 거절하면(경합으로 adopt가 먼저 이김) 아래 Worker 경로로 간다.
- **Worker가 됐으면**: 설정을 직접 원복하지 않는다. endpoint H-local을 다시 확정하고(①을 했으면 되돌린다) H를 살린다. H가 뜨지 않으면 §R7 rescue로 간 뒤 S14 quiet barrier → reclaim → S15로 돌아간다.

### S5 — holder 첫 일반 입력과 정확히 한 번 POST

진입: S4 PASS. Worker(E1, holder=H). H intake_open. G writer 없음. queue·owed·POST 0.

조작: C에 `TWOMAC-S5-<run-id>에 대해 정확히 "ACK TWOMAC-S5-<run-id>" 한 줄만 답해.`

PASS(180초 + 30초 quiet):
- 입력 message ID 하나. H pane nonce 입력 1회. 실행 node=H, Bound.
- H ledger `Prepared(epoch=worker_epoch)`와 같은 serial에 `Posted(M5)`. G ledger에 M5 없음. Discord M5 하나, 내용 정확.
- 30초 동안 추가 ID 없음. row holder·epoch 불변.

중단: G pane·ledger 관여. duplicate. `Prepared.epoch` ≠ `worker_epoch`. Worker 아님. content transform·ambiguous·unresolved alarm.

롤백: 추가 입력을 금지한다. Worker를 유지하고 판정한 뒤 S14(quiet barrier → reclaim)로 간다.

### S6 — 후속 대화

진입: S5 PASS. 같은 H execution nonce Bound. queue·POST 0.

조작: `직전 답의 run-id만 사용해 "FOLLOWUP <run-id>"로 답해.`

PASS(180초 + 30초 quiet): 같은 execution nonce·pane에서 input 1회. H ledger 다음 serial 하나(`Prepared.epoch=worker_epoch`). Discord 응답 하나, 순서 보존. G ledger 변화 없음. transcript 교체가 일어났다면 #7021 추종(§11-7)으로 같은 세션에 이어 붙었다는 증거(새 transcript 식별자와 binding)를 남긴다.

중단: 이유 없는 새 execution. 문맥 소실. duplicate·G 관여. transcript 교체 뒤 응답 누락.

롤백: 추가 입력을 멈추고 S14로 간다.

### S7 — `!clear`

진입: S6 완료. active turn 없음. H O writer hosted. Worker E1. `herdr_terminate_enabled`는 S7 직전에 켜고 S7 판정 직후 끈다(§7 승인표의 destructive 행에 S7을 추가). 끈 채로 진행하면 아래 "terminate off" 기대값을 쓴다.

조작: Discord에서 `!clear`를 1회 보낸다.

계약(§11-6 재판정, #7034 `8cfbb3696` 기준): `/clear`는 사전 거절 없이 항상 지운다. native `/clear` 입력 경로는 삭제됐다(`commands/control/native.rs`). 한 본문이 (1) 채널 AgentDesk queue·queued head·dispatch 예약·inflight를 메모리에서 버리고(디스크 정리는 best effort), (2) 실행 중 turn을 멈추고, (3) 저장된 provider 세션 ID와 세션 기억을 잊고, (4) Herdr 채널이면 `terminate_explicit_herdr`로 provider CLI를 끝낸다(`commands/control/clear_teardown.rs:50-140`, `herdr_terminate.rs:299-305`). (5) 성공 응답과 확인 못 한 단계마다 경고 한 줄(`RETRY_FAILED`·`TRANSITION_BUSY`·`STOP_UNCONFIRMED`·`END_UNCONFIRMED`, `clear_report.rs:8-36`)을 보낸다. 다음 메시지는 `--resume` 없이 새로 띄운다. 다른 노드가 소유한 채널이면 home-fence 사유 줄을 남긴다(유지된 유일한 거절성 문구).

PASS(90초):
- 성공 응답이 한 번 도착한다. H pane에 `/clear` 텍스트가 입력되지 않는다(native 경로 없음).
- terminate on: `END_UNCONFIRMED` 경고가 없다. C의 hosted execution row가 Retired로 바뀐다(S7 직전 nonce 기준). 이어서 보낸 새 짧은 입력이 H에서 `--resume` 없는 새 launch로 처리되고 H ledger·Discord에서 정확히 한 번 나온다.
- terminate off: 응답에 `END_UNCONFIRMED` 한 줄이 있다(`TerminationResult::Disabled`). execution은 Retired가 아니다. 이 경우 새 짧은 입력을 보내지 않고 S7을 "terminate off 관측"으로만 기록한다. 옛 execution이 산 채로 새 launch가 겹치는지는 이번 실측 범위 밖이다([U] → 후속).
- 공통: hold·inflight·queued head 잔존 0. row Worker E1 holder=H 불변(clear는 home 상태를 바꾸지 않는다).

FAIL: 무응답, 응답 2회 이상, `RETRY_FAILED`·`TRANSITION_BUSY`·`STOP_UNCONFIRMED` 중 하나라도(active turn 없는 진입이라 나오면 안 된다), terminate on인데 `END_UNCONFIRMED`, H pane에 `/clear` 텍스트 입력, row 상태·epoch 변화, home-fence 사유 줄(H가 소유자인데 거절).

중단·롤백: 응답 없음이나 row 변화가 있으면 파괴적 시나리오를 중단하고 S14로 간다. 경고만 있고 row·holder가 불변이면 S7만 FAIL로 두고 계속한다. terminate는 S7 판정 직후 반드시 끈다.

### S8 — `!stop`

진입: Worker E1. 30초 이상 걸리는 제어 가능한 turn. terminate switch off.

조작: 장시간 turn을 시작한다. pane과 active turn을 확인한 뒤 `!stop`을 1회 보낸다.

예상(§11-8 재판정 전제): design_base의 `herdr_stop_settlement_available()`는 생산에서 false다(`src/services/provider/cancel_token_claude_interrupt.rs:581-591`). `herdr_cancel_enabled`도 Escape delivery를 켜지 못한다(`:599-612`). `<RUN_SHA>`에서 이 함수가 바뀌었으면 S8 기대값을 다시 쓴 뒤 진행한다.

판정(false 유지 시): stop 기능 PASS는 불가하다. 다음을 모두 만족하면 fail-closed 계약 PASS로 따로 기록한다. 명시적 refusal 또는 host-refused. Escape 미입력. mailbox token 미취소. 원래 turn이 정상 terminal에 도달. 답 정확히 한 번. 보고에 `STOP_CAPABILITY: FAIL_UNAVAILABLE`.

### S9 — gateway fenced restart

진입: Worker E1 holder=H. idle. S5·S6 PASS.

조작: G에서 `<ADK> restart-dcserver`.

PASS(120초): G PID·start time 변경. `committed restart joins every home`. row Worker·holder=H·epoch=E1 유지. H `renewed_at` 진전. H writer·endpoint local 유지. G 복귀 뒤 C 짧은 입력이 H에서 1회 처리. G ledger에 없음. S13 체크포인트를 수행한다.

중단: 재시작이 row를 G로 되돌린다. H intake가 닫힌다. duplicate. writer 종료 불확정. G 로그에 `rolling back current release`(G hash가 `<RUN_SHA>`가 아니게 됨).

롤백: G만 S0 보존 바이너리·config로 다시 띄운다(rollback이 일어났으면 §R7-2와 같은 방식으로 G RESCUE_BIN 기동, 맥 판단). row는 H에 유지하고 추가 파괴적 조작을 하지 않는다.

### S10 — holder fenced restart (rescue 연습 PASS일 때만)

진입: Worker E1. idle·queue·POST 0. H endpoint local. S0b-3 rescue 연습 PASS.

조작: H에서 `<ADK> restart-dcserver`.

PASS(120초): `committed restart joins every home`. 기존 delegated writer join. 새 H process(hash=`<RUN_SHA>`)가 같은 row를 mount. holder=H, epoch=E1 불변(renew만). `renewed_at` 2회 이상. intake open. restart 뒤 짧은 입력 1회가 H에서 처리되고 새 O serial의 `Prepared.epoch=E1`. S13 체크포인트를 수행한다.

중단: row가 Released/Orphaned로 이동. writer join 불확정. 새 process가 Standby. health 정상인데 writer 없음. `rolling back current release` 또는 hash 불일치.

롤백(실패 시 §R7 rescue 발동): endpoint H-local 유지, C 입력 금지 → `launchctl bootout` → RESCUE_BIN 수동 기동 → 같은 Worker E1 mount, `renewed_at` 2회, writer 증명 → S14로 간다. force 금지.

### S11 — gateway OS SIGTERM과 [O-U-3] 단절 중 완료

진입: Worker E1. 45~60초 걸리는 제어 가능한 turn. G supervisor stop/start 명령을 S0에서 확정(launchd label `com.agentdesk.release`, `launchctl bootout`/`bootstrap`). 승인된 유지보수 창(다른 채널 입력 일시 중단).

조작: C에서 장시간 turn을 시작하고 H pane 시작을 확인한다. G를 `launchctl bootout`으로 내린다(SIGTERM, KeepAlive 재기동 차단). G process와 websocket 소멸을 확인한다. G가 없는 동안 H turn이 끝나고 H writer가 REST로 POST한다. 답을 확인한 뒤 `launchctl bootstrap`으로 G를 올린다. 이어 후속 입력을 보낸다.

PASS(전체 300초, G 정지 최대 120초): G 로그에 SIGTERM과 graceful shutdown. G down interval 기록. 그 안에 H terminal·H ledger Posted·Discord timestamp. message ID 하나. 복구 뒤 row Worker E1 holder=H. 후속 입력도 H에서 1회. 다른 채널 기존 process가 살아 있고 같은 session으로 이어진다. G hash=`<RUN_SHA>`.

중단: G가 실제로 내려가지 않음. H POST가 G 복구 뒤에만 발생. H lease·gate 닫힘. 다른 채널 세션 종료. duplicate.

롤백: G를 즉시 같은 SHA·config로 올린다. H는 건드리지 않는다. Worker를 유지한 채 중단하거나 S14로 간다.

닫는 범위: 실제 Mac 두 대, gateway·holder 분리, holder 실행, gateway 단절 중 완료와 POST, 복귀 뒤 후속 대화, 이후 정상 reclaim. holder 자체의 네트워크 단절은 포함하지 않는다.

### S12 — holder OS SIGTERM 중 실행 (rescue 연습 PASS일 때만)

진입: G 정상. Worker E1. 45초 이상 장시간 turn. H launchd KeepAlive가 같은 SHA·config로 자동 복귀시킨다.

조작: 장시간 turn을 시작한다. pane input·active turn을 확인한다. H dcserver PID에 SIGTERM을 보낸다. Herdr pane·process는 건드리지 않는다.

PASS(300초):
- H 로그에 `SIGTERM received — graceful shutdown`. queue/inflight 저장. delegated writer join. unconfirmed 없음.
- 새 H PID(hash=`<RUN_SHA>`)가 Runner로 부팅한다. holder=H·epoch=E1 유지. 새 renew 뒤 intake_open.
- 기존 turn을 재부착·복구해 terminal 1회, Discord 응답 하나. transcript가 교체됐으면 #7021 추종으로 재부착한 증거를 남긴다(§11-7).
- inflight row가 최종 정리된다. 다음 짧은 입력도 1회 처리된다. S13 체크포인트를 수행한다.

중단: SIGKILL. 다른 SHA로 기동(hash 불일치). pane 소멸. duplicate terminal. Orphaned. writer 없이 intake만 open.

롤백(실패 시 §R7 rescue 발동): endpoint H-local 유지, C 입력 금지 → bootout → RESCUE_BIN 기동 → 같은 Worker E1 mount, `renewed_at` 2회, writer 증명 → 진행 중 turn의 terminal 1회·응답 1개·duplicate 없음 판정 → S14. force 금지.

### S13 — 다른 채널 무변화 대조

체크포인트: S0b, S1, **S3(G 재시작 직후)**, S4, S9, S10, S11, S12, S16.

대상: S0b에 기록한 selected_herdr_channels 중 C 외 전부와 비위임 tmux 대조 채널. home row가 없어야 한다.

조작(체크포인트마다): host·session·endpoint·pane ID snapshot, nonce 입력 1회, 응답 확인, C 외 row 목록 비교, availability 거절 로그 확인.

PASS: control 채널에 row 없음. endpoint·session 불변(자체 rotation이면 인과 증명). 입력 하나에 응답 하나. C 전환으로 생긴 새 degraded 없음. G 정지 구간에는 입력을 요구하지 않되 복구 뒤 같은 session이 이어진다.

중단: control writer 중지. provider-wide availability 거절. endpoint·host 이동. duplicate·missing.

롤백: C 다음 시나리오를 멈추고 현재 row 상태의 정상 복귀 절차(S3 롤백·S4 롤백·S14)를 시작한다.

### S14 — H 실행 retire, quiet barrier, reclaim drain

진입: S5·S6·재시작 시나리오 완료. active turn·queue·owed·POST 0. endpoint H-local. Worker E1.

조작:
1. H에서 terminate switch를 잠시 켜고, exact session key·nonce로 H 실행을 terminate한다. retired를 확인한 뒤 off한다.
2. G CLI config에서 delegation switch가 켜져 있는지 확인한다.
3. **quiet barrier**: C 신규 입력 금지를 공지한다(사용자·자동 producer 정지, U4). `channel-home status`의 C open_intake(current_epoch/other_epoch/unrouted)가 **5초 간격 두 번 연속 모두 0**인지 확인한다. 0이 아니면 0이 될 때까지 기다린다. 120초를 넘기면 중단한다.
4. barrier 직후 G에서 `<ADK> channel-home reclaim C`.

PASS:
- terminate: Retired, hold 없음.
- reclaim 직후: Reclaiming, holder=H, target=G, **epoch=worker_epoch(E1) 그대로**. H health draining과 실제 blocker 표시.
- blocker 해소 뒤 60초 안에: Reclaimed, holder=NULL, target=G, epoch=reclaimed_epoch(E2), E2≠E1. open intake 0. H ledger에 새 POST 없음.

중단: terminate indeterminate. blocker가 남았는데 CAS 진행. Reclaiming 120초 초과. Reclaiming 중 open intake가 생겨 drain이 멈춤. force 필요.

롤백:
- Reclaiming에서 H가 살아 있으면 H를 유지하고 drain 완료만 허용한다.
- Reclaiming에서 H가 죽거나 다시 뜨지 않으면 §R7 rescue(같은 Reclaiming E1 mount, `renewed_at` 2회, writer 증명 → drain 완료). rescue 연습이 FAIL이었으면 실측을 중단하고 증거를 보존한 뒤 사용자·맥이 결정한다.
- quiet barrier를 어긴 늦은 입력으로 Reclaiming과 claim 불가 pending row가 함께 남으면(P2-2 경합) 실측을 중단하고 증거를 보존한 뒤 맥이 결정한다. force와 abort는 쓸 수 없다.
- Reclaimed이면 row를 직접 지우지 않는다. S15로 간다.

코드 쪽 후속(P2, 이번 범위 밖): router의 delegated INSERT와 `begin_reclaim`이 home row로 직렬화되지 않는다(`intake_router_hook.rs:121-215` 읽은 뒤 await를 거쳐 INSERT, `o_channel_homes.rs:347-365`). 기존 row 잠금(`FOR SHARE`/`FOR UPDATE`)으로 직렬화하는 수리를 후속 조각으로 둔다. 새 테이블·migration은 없다. quiet barrier는 경합 확률을 줄일 뿐 코드 증명이 아니다.

### S15 — delegated writer join, row 제거, G writer 복귀

진입: Reclaimed(reclaimed_epoch=E2, target=G). H 실행 Retired. open intake 0.

조작: 양 노드 endpoint를 G로 원복한다. H를 먼저 fenced restart한다(rescue 중이면 rescue 프로세스에 SIGTERM을 보내고 launchd 복귀). H health에서 endpoint local=false·C delegated writer 없음·unconfirmed 없음을 확인한다. 그다음 G를 fenced restart한다. R·M·O를 반복 관측한다.

PASS(재시작 각 120초, row 제거 60초): H restart가 delegated writer를 join한다. 새 H는 C writer를 시작하지 않는다. G boot가 Reclaimed row를 mount하고, home-bound writer가 stop·join한 뒤 `remove_reclaimed`(epoch=E2 조건)가 row를 지운다. 그 뒤 gateway writer가 시작된다. R에서 C row 없음. G local=true, H local=false. G writer readiness 정상. H ledger 변화 없음.

중단: row 제거 전에 G gateway writer가 POST 가능해진다. H writer가 hosted로 남는다. row가 없는데 G writer가 없다. unconfirmed. local 충돌.

롤백: Reclaimed가 남으면 H를 writer 없는 상태로 두고 G를 같은 config로 다시 띄운다. row가 이미 없으면 G-local을 유지하고 G writer 복구만 한다. 새 delegate는 만들지 않는다.

### S16 — gateway 복귀 뒤 정확히 한 번 배달과 정리

진입: S15 PASS. row 없음. G-local. H writer 없음. 양 노드 안정.

조작: C와 control 채널에 마지막 nonce를 보낸다. 결과를 수집한 뒤 시험 설정을 원복한다.

PASS(180초 + 30초 quiet): C는 G pane에서 실행되고 G ledger에 새 Prepared/Posted가 생긴다. H ledger 변화 없음. Discord 응답 하나. control 채널은 기준선과 같은 host·session에서 응답 하나. row 없음.

정리 순서:
1. terminate switch off를 확인한다.
2. delegation switch off는 C row 없음을 R로 다시 확인한 뒤에만 한다.
3. H 역할 원복(Full/Standby) 여부는 사용자가 정한다. switch off와 역할 원복은 양 노드 재시작이 필요하다.
4. endpoint는 G-local을 유지한다.
5. config hash·최종 health·RESCUE_BIN 삭제 여부를 기록하고, 마지막 30초 quiet를 확인한다.

## 7. 운영 설정 변경과 승인 지점

| 변경 | 노드·시점 | 재시작 | 승인 |
|---|---|---|---|
| 실측 바이너리 `<RUN_SHA>` 확정(두 노드 동일 hash) | 배포 뒤 S0 | — | 맥 확인 |
| **CLI 착지 SHA `<CLI_SHA>`(T1~T6·T4b 증거)와 `abort-release` 사용 승인** | S3·S4 실패 시 | 이후 G restart | **사용자 승인(실측 진입 선행조건)** |
| `runtime.channel_home_delegation_enabled=true` | G·H, S0b | 필요 | 사용자 승인(provider-wide admission 영향) |
| H `runtime_profile=runner`·`cluster.role=runner` | H, S0b | 필요 | 사용자 승인(역할 변경) |
| H rescue boot 연습과 발동(launchd bootout/bootstrap, RESCUE_BIN 수동 기동) | H, S0b·실패 시 | 프로세스 재기동 | 사용자 승인 |
| gateway_preferred_instance_id=G 확인 | G·H | 변경 시 | 변경하지 않는 것이 원칙 |
| endpoint G→H / H→G | G·H, Released 뒤 / Reclaimed 뒤 | 필요 | 사용자 승인(같은 채널 실행 위치 변경) |
| `tui_o.writer.channels`에 C | G·H | 필요 | 기존 값이면 변경 없음. 추가 시 사용자 승인 |
| `herdr_turn_enabled` | provider | live | 기존 off면 사용자 승인 |
| `herdr_terminate_enabled=true` | retire 직전 | live | 사용자 승인(파괴적). 사용 직후 off |
| `herdr_terminate_enabled=true`(S7 `!clear`) | S7 직전, 판정 직후 off | live | 사용자 승인(파괴적: #7034 `/clear`가 Herdr provider CLI를 terminate). 승인하지 않으면 S7은 terminate off 관측만 한다 |
| gateway supervisor stop(S11) | G | 프로세스 정지 | 사용자 승인(다른 채널 일시 intake 중단) |
| C 신규 입력 금지(S14 quiet barrier) | 운영자 공지 | 없음 | 코디네이터 재량(자동 producer 정지는 U4) |
| evidence nonce·파일명·관측 주기, loopback health·read-only 조회·ledger 복사 | 전 구간 | 없음 | 코디네이터 재량 |
| Standby adopt 코드 수정, router↔reclaim 직렬화 | 후속 | 배포 | 맥 승인 대상 |
| `runtime_bootstrap*`, `channel_home*`, `tui_o/**`, `session_host/**`, `turn_finalizer/**` 변경 | 모든 코드안 | 배포 | 맥 승인 대상 |

금지되는 변경과 절차:
- 운영 PG 손 수리(`o_channel_homes` 직접 DELETE·UPDATE 포함).
- gateway를 H로 이동.
- 새 off 플래그·원장·방어 장치·PG 테이블·migration.
- all_tui 범위 확대.
- row가 남은 상태에서 delegation switch off.
- 정상 진행 수단으로 force.
- endpoint를 H로 먼저 옮긴 뒤 delegate.
- Reclaimed row 제거 전에 H delegated writer를 살려 둔 채 G 부팅.
- `<RUN_SHA>`와 다른 hash의 프로세스로 위임 row를 계속 운영.

## 8. 증거 형식

```text
evidence/i5340_twomac/<run-id>/
  00-manifest.json
  01-config/
    gateway.redacted.yaml
    holder.redacted.yaml
    config-sha256.json
    2239d6c..<RUN_SHA>-critical-path.diff
    premise-rejudge.md            # §11 재판정표
    node-identities.json
    binary-sha256.json            # G·H 실행 파일·RESCUE_BIN·releases/{current,previous}
    rescue-H.json                 # §R7-1 보존 항목의 경로·hash(비밀값 제외)
    cli-landing.json              # <CLI_SHA>, T1~T6·T4b 결과 경로
    approvals.json
  S00-preflight/
  S00b-dry-boot/
    selected_herdr_channels.json
    availability.json             # 로그 판정과 채널별 nonce 응답
    rescue-drill.json
  S01-baseline/ … S16-final/
  controls/
  SHA256SUMS
  final-verdict.json
```

`00-manifest.json` 필수 필드:

```json
{
  "run_id": "...",
  "started_at_kst": "...",
  "started_at_utc": "...",
  "design_base": "2239d6c475517b1b219f4a2bcbe0b3bb8470c4bd",
  "run_sha": "<RUN_SHA>",
  "cli_sha": "<CLI_SHA>",
  "binary_sha256": {
    "gateway": "...",
    "holder": "...",
    "holder_rescue": "..."
  },
  "channel_id": "1509350490461180105",
  "provider": "claude",
  "epochs": {
    "release_epoch": null,
    "worker_epoch": null,
    "reclaimed_epoch": null
  },
  "gateway": {"hostname": "...", "instance_id": "...", "pid": 0},
  "holder": {"hostname": "Mac-mini.local", "instance_id": "...", "pid": 0},
  "rescue": {"drill": "PASS|FAIL|NOT_RUN", "invoked": []}
}
```

각 Sxx 디렉터리: `start.json`, `end.json`, `channel-home.{before,after}.json`, `{gateway,holder}.health.{before,after}.json`, `{gateway,holder}.herdr-status.json`, `{gateway,holder}.o-ledger.{before,after}.jsonl`, `discord.jsonl`, `{gateway,holder}.log.jsonl`, `pane-proof.json`, `verdict.json`.

offline 판정 스크립트(시험 전용 `scripts/e2e/tui_relay/twomac_home_judge.py`, 예상 150~250줄, 생산 변경 없음) 필수 검사:
- `run_sha`·`cli_sha`가 미치환이거나 옛 SHA면 FAIL. G·H git SHA 또는 실행 파일 hash가 다르면 FAIL. critical-path diff·`premise-rejudge.md`가 없으면 FAIL.
- epoch 이름표를 받아 §4.1 시퀀스(Releasing=release_epoch, Released·Worker·Reclaiming=worker_epoch, Reclaimed=reclaimed_epoch)를 검사한다. adopt나 begin_reclaim에서 epoch가 바뀌면 FAIL.
- 전이 완료 때 open intake 0.
- 위임 중 `Prepared.epoch=worker_epoch`.
- holder ledger `Posted(msg_id)`와 Discord ID 일치, 비holder ledger에 같은 ID 없음, nonce당 bot ID 정확히 하나.
- restart마다 PID·start time 변경, `writer end is unconfirmed` 부재.
- control 채널 row 부재·host/session 불변. S0b·S3 체크포인트 누락은 FAIL.
- rescue 연습 FAIL·NOT_RUN이면 S10·S12 결과를 받지 않고 목표 3을 `NOT_CLOSED`로 낸다.
- `known_gap`·`not_applicable`·빈 증거는 PASS로 바꾸지 않는다.

스크립트 exit 0 자체는 PASS가 아니다. `verdict.json`에 기대값과 실제값이 모두 있어야 한다.

## 9. s3fence 증거 대응

| 실측 | 기존 증거 | 실제로 닫는 한계 |
|---|---|---|
| S0b dry boot | R6 단위 판정 | row 없이 실제 두 프로세스에서 preflight·selected 채널 영향 확인 |
| S3 release | H6-1, H6-2a | 실제 gateway가 Releasing을 drain하고 Released CAS |
| S4 adopt | H6-2a Released target boot watch | 부모가 restored를 주던 시험 한계를 RestWorker 실제 boot로 대체 |
| S5 first turn | H7-P*, H7-G*, H8-G, H9-D/W 보완 | 실제 Discord → 다른 Mac pane → holder O POST |
| S6 follow-up | H7-U·H7-O 보완 | 같은 실제 pane·execution에서 후속 문맥 |
| S9 gateway fenced restart | H6-6a·H6-6b 보완 | 실제 두 프로세스 세대와 PG holder 유지 |
| S10 holder fenced restart | H6-6b | 실제 delegated writer join·다음 boot 재개(rescue 연습 PASS 시) |
| S11 gateway SIGTERM/O-U-3 | H6-6b, 런북 O-U-3 | 실제 OS signal, gateway 부재 중 holder 완료·POST·복귀 |
| S12 holder SIGTERM | H6-6b | 실제 OS SIGTERM, pane 생존, inflight 복구, writer 재개(rescue 연습 PASS 시) |
| S14 reclaim drain | H6-2a·H6-3a | 기존 범위에서 닿지 못한 Reclaimed CAS 실측 |
| S15 handback | H6-3b·H6-6b | H writer 실제 join 뒤 gateway return·row 제거·writer 복귀 |
| S13 controls | H7 isolation 보완 | 비위임 채널 실제 무변화 |
| S16 final | H6-3b 후속 | row 제거 뒤 gateway 단일 배달 |

§3.2 이월 대응은 r1과 같다. 대체하지 않는 항목: a2, c1, c4, c5, c6, c7, d1, d2, d3. 보완 항목: a1, b1, c2, c3, c8, f. 대체 항목: b2(S4), e1(S9·S10·S12·S15), e2(S11·S12). 단 S10·S12가 빠지면 e1·e2의 holder 쪽은 미대체다.

과대 주장 금지: S4는 Standby 경로를 증명하지 않는다. S15는 CAS-stale race를 증명하지 않는다. SIGTERM은 crash-recovery가 아니다. quiet barrier는 router↔reclaim 직렬화 증명이 아니다. exit 0·health 200은 writer join·exact-once 증명이 아니다.

## 10. 위험과 [U]

| ID | 미확인·위험 | 확인 방법 | 담당 |
|---|---|---|---|
| U1 | 100.71.169.10이 MacBook인지 | SSH hostname·instance ID·registry | 코디네이터 |
| U2 | 양 노드 실제 config 값 | redacted 사본·hash | 코디네이터 |
| U3 | bulk1 최종 판정 | 기존 artifact | 코디네이터 |
| U4 | C 자동 producer 유무(quiet barrier 전제) | bot·scheduler·webhook 목록 | 사용자 |
| U5 | 실제 control 채널 ID | S0b selected 집합 + tmux 하나 | 사용자 |
| U6 | H cwd·인증·Claude 실행 환경 | H local Herdr smoke | 맥 |
| U7 | H O store rotation·alarm | ledger 사본, `!clear` 전 상태 | 맥·코디네이터 |
| U8 | G 실행 terminate 뒤 cold launch | S2·S4 | 맥 |
| U9 | launchd label·plist 실제 내용(`com.agentdesk.release` 가정) | S0 plist 사본 | 맥·코디네이터 |
| U10 | G detail 403 | SSH loopback GET | 코디네이터 |
| U11 | `<CLI_SHA>` 착지와 T4b 포함 | CLI PR·증거 | 맥 |
| U12 | `tui_o:released:<ch>` alarm과 home row 관계 | R과 O alarm 대조 | 코디네이터 |
| U13 | endpoint가 하나뿐인지 | config·health | 코디네이터 |
| U14 | provider backlog 12/1 귀속 | queue 조회 | 코디네이터 |
| U15 | 실제 stop 지원 | §11-8 재판정 | 맥 |
| U16 | `rotation_unsettled(59)` 잔존 | `!clear` 전 rotation 조회 | 맥·코디네이터 |
| U17 | `<RUN_SHA>` 미정 — §11 재판정 결과 | 배포 뒤 critical-path diff | 맥 |
| U18 | O ledger runtime root 분리 | realpath·device/inode | 코디네이터 |
| U19 | startup doctor failed 항목 | doctor report | 맥 |
| U20 | G outage 동안 다른 채널 영향 허용 | 유지보수 창 | 사용자 |
| U21 | selected_herdr_channels 직접 관측 수단 없음(health 미노출) | §2 계산을 config·health로 재현, S0b nonce로 대조 | 코디네이터 |
| U22 | RestWorker queue 복구 완료(reconcile_done)를 보여 주는 로그 키, H REST 자격증명 직접 확인 방법 | H boot 로그 조사 | 맥 |
| U23 | rescue 수동 기동이 launchd 기동과 같은 환경을 재현하는지(launchd.env 비밀 항목·`ps` 기반 인스턴스 판정 `dcserver.rs:295-305`) | S0b-3 연습 | 맥 |
| U24 | `releases/previous`가 `<RUN_SHA>`와 다를 때 rollback된 옛 바이너리가 Worker row를 잠시 mount하는 동안의 호환성 | S0 `previous..<RUN_SHA>` critical-path diff | 맥 |
| U25 | delegated writer hosted 상태의 health 필드 이름(CLI 설계 [U-3]) | health detail 조사 | 코디네이터 |
| U26 | H를 S0b부터 Runner로 띄울 때 RestWorker intake worker(`runtime_bootstrap.rs:352`)가 C 외 채널의 intake 배치에 영향을 주지 않는지 | S0b·S1 control nonce 응답 노드 대조 | 맥·코디네이터 |

## 11. 배포 뒤 다시 판정할 전제 (`2239d6c..<RUN_SHA>`)

S0에서 `git diff --name-status 2239d6c..<RUN_SHA> -- <경로>`를 돌리고, 바뀐 행마다 해당 시나리오의 기대값을 다시 쓴 뒤 `premise-rejudge.md`에 남긴다. 하나라도 미판정이면 S0 FAIL이다.

| # | 전제 | design_base 좌표 | 영향 시나리오 |
|---|---|---|---|
| 11-1 | runtime_bootstrap: RestWorker 분기·turns_restored·start_channel_homes·herdr_runs_here·legacy_reset·availability 입력 | `src/services/discord/runtime_bootstrap.rs:338-363,509-522,541-578,583-621,623-627,631-667` | S0b·S3·S4·S10·S12 |
| 11-2 | channel_home_boot: start·watch·adopt 조건·give_back | `src/services/cluster/channel_home_boot.rs:396-492,576-598,604-686` | S3·S4·S10·S14·S15 |
| 11-3 | o_channel_homes CAS와 epoch nextval(§4.1 표) | `src/db/o_channel_homes.rs:253-455` | 전 구간, 판정기 |
| 11-4 | drain: drain_round·open intake·finish_return | `src/services/cluster/channel_home_drain.rs:100-121,133-208,235-246` | S3·S14·S15 |
| 11-5 | claim fence·router | `src/db/intake_outbox/worker_status.rs:87-93`, `src/services/cluster/intake_router_hook.rs:121-215` | S5·S14 |
| 11-6 | `/clear` 실제 거절·commit 계약 | `src/services/discord/admin_host_guard.rs`, `src/services/session_host/herdr_clear_adapter.rs`, `src/services/tui_o/writer/{transition_port,transition_cleanup,host}.rs`의 `rotation_unsettled` | S7 |
| 11-7 | #7021 transcript 교체 추종(Claude 교체 발견·binding·O delivery 재개)과 S6·S12 재부착 | `src/services/discord/tui_prompt_relay/{claude_source,source_tick,claude_idle_runtime}.rs`, `src/services/tui_o/writer/rotation/claude_boundary.rs`, `src/services/tui_o/store/rotation.rs`, `src/services/tui_prompt_dedupe/binding_events.rs` | S6·S12 |
| 11-8 | `herdr_stop_settlement_available()`·`herdr_cancel_enabled()` | `src/services/provider/cancel_token_claude_interrupt.rs:581-605` | S8 |
| 11-9 | restart·rollback·launchd 정의(rescue 전제) | `src/cli/dcserver.rs:442-461,568-800,881-913`, `src/cli/init.rs:480-570` | S0b·S9·S10·S12·R7 |
| 11-10 | home availability·health 노출 | `src/services/cluster/home_availability.rs:56-180`, `src/server/routes/health_api/runtime_profile.rs:4-48` | S0b·S13 |
| 11-11 | 탈출 CLI 계약(abort-release SQL·거절 사유·T4b) | `<CLI_SHA>`의 `src/db/o_channel_homes.rs`·`src/cli/channel_home.rs` | S3·S4 |


### 11a. 중간 재판정 결과 — `2239d6c47..8cfbb3696`(#7032·#7033·#7030·#7034, 2026-10-16 21:4x 코디 판정)

최종 `<RUN_SHA>`는 탈출 CLI(`<CLI_SHA>`)가 들어간 배포다. 8cfbb3696에는 CLI가 없어 S0의 조상 조건을 못 채운다. 그래서 아래 표는 중간 기준이다. S0에서 `8cfbb3696..<RUN_SHA>`를 같은 방법으로 다시 판정한다(예상: CLI 조각만).

| # | 결과 | 근거 | 런북 조치 |
|---|---|---|---|
| 11-1 runtime_bootstrap | 변경 없음(blob 동일) | `git diff --name-status` 0행 | 없음 |
| 11-2 channel_home_boot | 변경 없음 | 같음 | 없음 |
| 11-3 o_channel_homes | 변경 없음 | 같음. §4.1 epoch 표 유지 | 없음 |
| 11-4 drain | 변경 없음 | 같음 | 없음 |
| 11-5 claim fence·router | 변경 없음 | 같음 | 없음 |
| 11-6 `/clear` | **변경**(#7034): 사전 거절 삭제, native `/clear` 입력 삭제, queue·session 폐기, Herdr provider CLI terminate, 경고 4종 | `admin_host_guard.rs`·`commands/control/{native,managed_reset,clear_teardown,clear_report}.rs`·`tui_o/writer/host.rs` | S7 전면 수정(위). §7에 S7 terminate on/off 행 추가 |
| 11-7 transcript 추종 | `tui_o/store/mod.rs`만 변경(#7030 Codex rollout 인수, Claude 경로 무관) | `git log 2239d6c..8cfbb3696 -- tui_o/store/mod.rs` = 515630f1f | S6·S12 기대값 유지 |
| 11-8 stop settlement | 변경 없음 | `cancel_token_claude_interrupt.rs` blob 동일 | S8 기대값 유지 |
| 11-9 dcserver·init | 변경 없음 | 같음 | rescue 전제 유지 |
| 11-10 availability·health | 변경 없음 | 같음 | 없음 |
| 11-11 탈출 CLI | 미착지(구현 중, 브랜치 feat/home-escape-cli) | — | S0에서 `<CLI_SHA>` 판정 |
| 추가 | `runtime_bootstrap/o_writer_host.rs` +18/-4(#7030 Codex takeover) | 시험 채널 C는 Claude provider | 영향 없음으로 판정, S0에서 재확인 |

## 12. 개선 후속과 새 방향 제안

- 후속 1 — Standby target restore(closed-Worker restore 계약). r1 §11과 같다.
- 후속 2 — router delegated INSERT ↔ `begin_reclaim` 직렬화(P2-2, 기존 row 잠금, migration 없음). barrier 시험 A·B와 뮤턴트(router 최종 재검증 제거, reclaim open-intake 확인 제거, 잠금 제거).
- 후속 3 — 성공 POST 진단 노출(`agentdesk o inspect` 읽기 전용 projection). r1과 같다.
- 후속 4 — `Availability`·selected_herdr_channels를 health에 읽기 전용으로 노출(U21 해소).
- 새 방향 제안(prepare/commit형 위임): r1과 같다. 지금 갈아타지 않는다.

## 최종 판정

실행은 다음 게이트가 모두 충족된 뒤에만 시작한다.
- `<RUN_SHA>`가 두 노드에서 같은 hash다. §11 재판정을 마쳤다.
- `<CLI_SHA>`가 착지했고 T1~T6·T4b 증거가 있으며 `<RUN_SHA>`에 포함된다.
- 노드 신원·config가 확정됐다. backlog·doctor failure를 정리했다. 실제 control 채널을 정했다.
- 사용자 승인: switch, H Runner 전환, rescue boot, endpoint 왕복, terminate, S11 정지, abort-release 사용.
- S0b PASS(rescue 연습 결과에 따라 S10·S12 포함 여부 확정). C row 없음.

성공 보고는 CORE PASS와 `!clear`·`!stop` 결과를 분리한다. rescue 연습이 FAIL이면 목표 3을 닫았다고 보고하지 않는다.

## r2 변경 대응표

리뷰 `review-twomac-design-r1.out.md`(VERDICT: ISSUES, P1 4·P2 3)와 사용자 10-14 금지 사항 대응. 줄 번호는 이 파일(`runbook-r2.md`) 기준이다.

| 지적 | 처리 | 위치(runbook-r2.md:line) |
|---|---|---|
| P1-1 수동 PG 복구 잔존 | 결론·R2·S3/S4 롤백·§7에서 손 SQL·긴급 복구를 모두 걷어 냈다. `abort-release C --expected-epoch <E>`로 바꿨다(Releasing=E0/release_epoch, Released=E1/worker_epoch). 성공 뒤 row 없음 → G fenced restart → G writer·단일 응답. §7은 "CLI 착지 SHA·abort-release 사용 승인"(진입 선행조건). 금지 목록에 "운영 PG 손 수리" 추가 | 14, 43-63, 323-325, 346-347, 523, 539 |
| P1-2 epoch 표기 | 코드 계약에 맞췄다: E0 Releasing, E1 Released→Worker→Reclaiming, E2 Reclaimed, 이후 row 제거. 전이마다 nextval 유무 표(코드 line 포함). manifest 이름표 release_epoch·worker_epoch·reclaimed_epoch, 판정기가 별칭 계산. S4·S5·S10·S14·S15 기대값 수정 | 161-179, 339, 478-479, 493, 593-597, 608 |
| P1-3 holder rescue boot | R7 신설: S0 보존(실행 파일 hash·RESCUE_BIN·plist·Runner config·runtime root·release 링크), 독립 수동 기동 명령(코드 좌표 `dcserver.rs`·`init.rs`, 미실행), 발동 조건(S10·S12·S14, rollback 감지 포함), 같은 epoch mount·renewed_at 2회·writer 증명 → 정상 reclaim. S0b-3 연습이 FAIL이면 S10·S12 제외·목표 3 미닫힘 | 87-123, 253-276, 410-420, 436-450, 485 |
| P1-4 SHA 고정 | `<RUN_SHA>` 자리표시자. S0 PASS = G·H git SHA와 실행 파일 SHA-256 동일. manifest에 design_base·run_sha·cli_sha·binary_sha256(G·H·rescue) 분리. diff 이름 `2239d6c..<RUN_SHA>-critical-path.diff`. §11 재판정 전제(runtime_bootstrap·channel_home_boot·o_channel_homes·drain·claim fence·/clear·#7021·stop settlement·restart/rollback·availability·CLI) | 1-5, 10-12, 222-251, 558, 583-591, 672-689 |
| P2-1 preflight dry boot | S0b 신설(row 없이 switch=true·H Runner 재부팅, availability 로그 판정, selected_herdr_channels 저장, 채널별 nonce, H REST 자격증명, 실패 시 원복). S3 G 재시작 직후 control snapshot. S13 체크포인트에 S0b·S3 추가 | 253-276, 311, 454 |
| P2-2 router↔reclaim 경합 | S14 quiet barrier(신규 입력 금지 → 5초 간격 open-intake 0 두 번 → reclaim). 코드 직렬화는 P2 후속으로 적었다 | 473-474, 486-489, 693 |
| P2-3 finish_release↔abort 시험 | CLI 착지 증거에 T4b 포함을 진입 조건으로 적었다 | 63, 231, 523, 563, 702 |
| 사용자 10-14 금지 | 새 off 플래그·원장·방어 장치·PG 테이블·migration을 넣지 않았다. 금지 목록에 명시 | 539-547 |
