# Herdr restart Owner: B1 구현 연결과 고정 followers

조사 base `2d0a40d3218ade5abf454c1ae11b9a96f40f1c42` 위 B1 working tree를 읽었다. 모델 담당 생산 코드 변경0·cargo/PG/herdr 실실행0이다. 아래 TLC 결과는 모델 증거이며 실제 Rust/PG 시험 PASS는 부모 레인의 보고서를 따른다. B1은 일반 launch/clear/claim 호출자가 소비하고 `/restart` seed·Ready·idle 복구 활성화는 B2로 남는다.

TLA SHA256 `0bfe4d61abc4dc24a9fb99f93377c5efea85bb70a77546b534f3f2f9bf36b3eb`. 기존63cfg/expected 행을 유지했고 신규행0·expected 변경0·감사예외0이다. 이전 e1 모델의63cfg 증거를 새 source 전체 PASS로 재사용하지 않았다. 변경을 소비하는7cfg만 새로 검사했으며 공식 최종 head의63cfg 통합은 부모 레인이 별도 기록한다.

## 수정 전/후

| 경계 | base의 코드/원인 모델 | B1 코드/After 모델 | 새 결과 |
|---|---|---|---|
| followers 읽기 | base v2.rs:395가 live queue에서 재계산; scratch recompute mutant | claim 시 한 번 고정, 이후 같은 ordered originals만 읽음 | ClaimedMembersFixed GREEN; mutant RED(exit12), raw State1→2→3→4 |
| reserve payload | base v2.rs:466은 front/claim 번호만 확인; member 비교 제거 mutant | claim number/front와 frozen followers의 길이·원본 identity·순서 비교 | ReservationUsesClaim GREEN; mutant RED(exit12), State1→2→3→4→5 |
| clear/create 허가 | 관찰만 넘기거나 permission 조기 반납 | CheckEpoch→같은 mutex의 BeginEffect 재검사→물리 I/O 반환까지 RAII | 기존 NoLaunchCheck/NoSubmitCheck의 EffectHasLivePermission 등 기대 RED 유지 |
| 단위 결산 | head만 보고 late 입력을 기존 followers에 합칠 수 있음 | head1+follower2는 고정, late3은 별도 수락·receipt·진행 | ClaimedUnitResolves·AcceptedInputResolves GREEN; clear 시 미제출 단위는 terminal receipt와 함께 폐기 |

정상3cfg(HeadBusy/HeadBusyCrash/ClearAny)는 **22 invariant/9 liveness**를 검사한다(NoDeadlock 자동 검사 별도). 추가 규칙은 ClaimedMembersFixed, ReservationUsesClaim, ClaimedUnitResolves다. 기존4 control cfg는 원래20 invariant/8 liveness를 유지했다.

## 모델과 실제 B1 포트

코드 좌표는 B1 working tree 읽기 결과다. 최종 입력·포트 source SHA는 로그의 execution-manifest.json에 보존한다. 코드의 guard가 실제 호출자에 연결되었는지 정적으로 대조했으며 운영 실행으로 간주하지 않는다.

| 모델 경계 | 실제 코드 좌표 | 확인한 계약 |
|---|---|---|
| ClearBegin | src/services/herdr_launch/clear_epoch.rs:43, src/services/discord/commands/control.rs:518 | 같은 mutex에서 즉시 epoch++/clearing 증가. 기존 effects가 있어도 시작 |
| LaunchBegin/OwnerFree | src/services/herdr_launch/clear_epoch.rs:93, :162; src/services/claude/herdr_turn.rs:241; src/services/codex/herdr_turn.rs:133; src/services/herdr_launch.rs:388 | provider/token/channel scope. clearing 중 새 startup 등록 거절, 완료 뒤 새 입력 허용 |
| CheckCreateEpoch | src/services/herdr_launch.rs:529 | Pending 이후 scope epoch 관찰. 거절은 retire_unsent |
| BeginCreateEffect/Create | src/services/herdr_launch/clear_epoch.rs:135; src/services/herdr_launch/prepared.rs:46, :71 | worker 안에서 live epoch/clearing 재검사, create_user_default/create 반환까지 EffectPermit 보유 |
| worker scope 전달 | src/services/herdr_launch/clear_epoch.rs:193; src/services/claude/herdr_turn/first_input.rs:49; src/services/codex/herdr_turn/first_input.rs:54 | CreatedSubmit 별도 thread에 Arc를 전달하고 동기 TLS scope 설치 |
| BeginSubmitEffect/SubmitEffect | src/services/claude/herdr_turn/first_input.rs:105; src/services/codex/herdr_turn/first_input.rs:172; src/services/codex/herdr_turn.rs:651 | 실제 composer mutation/run_herdr 앞에서 허가, 물리 반환까지 보유 |
| startup/effect 반환 | src/services/herdr_launch/clear_epoch.rs:119, :143; src/services/claude/herdr_turn/first_input.rs:115; src/services/codex/herdr_turn/first_input.rs:182 | 첫 submit 반환에 startup 감소, effect RAII로 effects 감소·Notify |
| clear 배수와 exact 종료 | src/services/herdr_launch/clear_epoch.rs:59; src/services/discord/commands/control/clear_teardown.rs:72, :164, :199 | transition을 놓고 startups/effects0 대기, row 재읽기, exact terminate·Retired selector CAS |
| sticky overlap | src/services/herdr_launch/clear_epoch.rs:49, :55; src/services/discord/commands/control.rs:545 | 관측한 startup/effect가 먼저 끝나도 clear guard가 살아 있는 동안 recovery Held 특수 경로를 피함 |
| claim snapshot/read | src/services/turn_orchestrator/queued_head/v2.rs:456, :398; src/services/discord/queue_io/input_owner/drive.rs:44 | claim 때 frozen followers 저장, 뒤 읽기는 같은 snapshot |
| reserve/revoke | src/services/turn_orchestrator/queued_head/v2.rs:486, :520; src/services/discord/queue_io/input_owner/pump.rs:70 | caller payload를 ordered originals와 대조하고 revoke 때 메모리 snapshot 해제 |

## 범위와 값

기존 row/seed/launch/clear 상태·effect 선택값은 이전 포트 모델 그대로다. 새 claimPhase는 코드의 claim/read/prepare/reserve/receipt/revoke 경계를 표시하는 추상 상태이며 생산 enum이나 durable 필드가 아니다. ClaimMembers=<<1,2>>는 모델 시작 전에 claim한 head와 follower다. N=3의 입력3은 같은 사람·route로 merge 가능한 late arrival을 대표한다. 실제 merge eligibility·원본 전체 필드 동등성·임의 follower 수/순열은 Rust 시험이 확인한다.

claimRead는 frozen originals를 읽고 caller는 한 번 late3을 덧붙인 잘못된 payload를 제안할 수 있다. reserve는 이를 NotSent로 거절하고 정상 read/reserve로 진행한다. 무한히 잘못된 payload를 계속 보내는 caller까지 진행성을 보장하지 않는다. fixed prefix는 한 단위이며 clear/retire는 해당 pending member를 receipt와 함께 결산한다. crash 전 reserve가 없던 claim은 재큐잉, 전송 여부 불명인 reserved 단위는 retained로 간다. 메모리 claim snapshot의 복구가 내구 저장에 추가된다는 뜻은 아니다.

한 채널·한 clear·한 crash, 한 active startup/effect로 제한한다. provider/token/channel key normalization, 다중 동시 clear, 반환하지 않는 backend, disk/PG 실패, 임의 scheduling starvation은 별도다. 실제 I/O가 결과/timeout으로 돌아오며 각 owner/receipt/clear 단계가 계속 실행된다는 fairness를 쓴다.

**startup 종료 시각 차이:** 모델 launcher는 receipt·Source bind까지 보유하지만 실제 B1은 물리 첫 submit 반환 때 finish_start한다. 따라서 clear 뒤 늦은 Bind CAS가 session/selector를 되살리지 못하는지는 이 모델의 증명이 아니며 실제 Rust/PG 시험이 필요하다. sticky overlap 코드 대조 역시 실제 시험 PASS를 대신하지 않는다. B2 seed/Ready 진행성을 B1 효과 허가의 증거로 혼동하지 않는다.

## 새 실행 결과

정식 subset checker `checker-claims-final7.log`: **7cfg/70 TLC, 112.9초, exit0**, 미검사0. GREEN4, 기존 expected RED3의 위반집합 일치. cfg 최대66.5초로300초 예산 이내다. wrapper7도 같은 최종 source/기대집합과 일치했다. HeadBusy/HeadBusyCrash/ClearAny는500-state 그림 상한을 넘으므로 report만 생성했고 states 관측값을 아래에 보존했다. steps가 미제공이면0으로 채우지 않았다.

| cfg | states/steps | 실제 깨진 규칙 | 결과 |
|---|---:|---|---|
| [After](herdr-restart-owner-b1-After.report.md) | 65/190 | — | GREEN |
| [HeadBusy](herdr-restart-owner-b1-HeadBusy.report.md) | 697/미제공(그림상한) | — | GREEN |
| [HeadBusyCrash](herdr-restart-owner-b1-HeadBusyCrash.report.md) | 2048/미제공(그림상한) | — | GREEN |
| [ClearAny](herdr-restart-owner-b1-ClearAny.report.md) | 8379/미제공(그림상한) | — | GREEN |
| [NoLaunchCheck](herdr-restart-owner-b1-NoLaunchCheck.report.md) | 271/857 | `EffectHasLivePermission`, `NoStaleCreate` | 기대 RED |
| [NoSubmitCheck](herdr-restart-owner-b1-NoSubmitCheck.report.md) | 271/839 | `EffectHasLivePermission`, `NoSeedAfterClear` | 기대 RED |
| [PrependOverHead](herdr-restart-owner-b1-PrependOverHead.report.md) | 33/91 | `RequestResolves`, `RestartAvailable`, `AcceptedInputResolves` | 기대 RED |

### 실제 도달한 claim·clear interleaving

proof/check_claim_witness.py는 DOT를 파싱하고 빈 graph/미도달은 실패로 처리한다.

| graph | states | late3 뒤 frozen read / exact reserve | 잘못된 payload 제안 / 거절 / reserve | 단위 done | clear의 claim 결산 |
|---|---:|---:|---:|---:|---:|
| HeadBusy | 697 | 12 / 8 | 6 / 6 / 0 | 19 | 0(clear off) |
| ClearAny | 8379 | 36 / 24 | 18 / 18 / 0 | 55 | 114 |

ClearAny에서는 clear 뒤 late 입력 수락806개 edge가 있으며 별도 큐와 receipt를 보존한다. old grant를 보유한 ClearBegin/즉시 epoch 상승88개와 그 뒤 기존 create/submit 완료가 각각 도달했다. clearing/stale 중 새 BeginEffect grant는0이다. 이는 물리 I/O 발생 시각 규칙이 아니라 **허가 시각 계약**을 검사한 것이다. 코드에서 재검사나 permit 생존이 빠지면 모델 GREEN은 적용되지 않는다.

### followers 원인 mutant

같은 최종 HeadBusy.cfg와 최종 TLA를 복사해 한 guard씩만 바꿨고, 원본 바이트는 변하지 않았다. proof/runs.json은 실제 command·base/mutant SHA·cfg SHA·exit를 기록한다. 모델 source mutation이며 Rust/운영 mutation은 아니다.

- recompute-followers: `ClaimedMembersFixed`, late3 수락 뒤 observed=<<1,2,3>>. raw State1(initial claim)→2(Request)→3(Admit3)→4(ClaimRead), exit12.
- omit-reserve-member-check: `ReservationUsesClaim`, 잘못 제안한 payload=<<1,2,3>>가 그대로 reserved. State1→2(Request)→3(Admit3)→4(ClaimBadPayload)→5(ClaimReserve), exit12.

[followers 대조 그림](herdr-restart-owner-b1-followers.chart.mmd)은 이 두 raw 반례와 정상 graph witness를 사람이 읽는 상태로 투영했다. 두 mutant는 각각 D와 F의 breaks 표시이며 서로 다른 모델 변형이다. Request를 제외한 restart 상세와 같은 상태 안 self-step은 숨겼고 claim/read/reserve/거절 경계는 남겼다. [BeginEffect 제거](herdr-restart-owner-b1-NoLaunchCheck.chart.mmd), [허가 조기반납](herdr-restart-owner-b1-NoSubmitCheck.chart.mmd)은 정식 wrapper 산출이다. 해당 보고서의 규칙별 최단 경로 번호를 따른다.

## 숨기지 않은 B2 진단

초기 탐색에서 HeadBusyCrash에 ClearOn=any까지 임의 확장하니 RequestResolves가 새로 깨졌다. 경로는 seed submit→receipt(delivered)→Bind 전 crash→boot→clear exact teardown→ready=false/visible=false/seed=delivered 영구 대기다. 18,683 states·raw22-step 경로를 diagnostic-claims-clearcrash/report.md와 tlc-RequestResolves.out에 보존했다. followers3규칙은 이 경로에서도 hold였다.

이것은 B2 seed/Ready·boot/clear 접점의 모델 공백이며 B1의 일반 효과 허가/claim 구현 PASS로 덮지 않았다. HeadBusyCrash는 원래 계약의 clear off를 유지하고 clear/claim은 기존 ClearAny(Crash=false)로 검사한다. 기대 RED를 신규 등록하거나 expected를 완화하지 않았다. B2 실제 시험에서 이 경로가 유효한지 replay하고, 실제 receipt/visible outcome 계약으로 분류해야 한다.

## 증거와 재실행

로그 root `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8128/logs/impl-b1/model/`:

- checker-claims-final7.log: 최종7cfg 전체 위반집합·실행수. checker-claims-first6.log는 새 B2 진단이 있던 초기 결과이며 최종 PASS가 아니다.
- render-claims-final7/: wrapper 보고7개/그림4개, results.json.
- proof/{HeadBusy,ClearAny}.dot 및 *-witness.json, run_claim_proofs.py, check_claim_witness.py, 두 mutant log와 runs.json.
- inputs-final/: 최종 TLA/cfg/view. diagnostic-inputs/는 실패한 확장 입력을 별도로 보존한다. execution-manifest.json은 입력·산출·코드 좌표 snapshot SHA·cfg별 실행수다.

환경 SHELL=/bin/zsh, Java/TLC/Node 경로는 아래 명령 그대로다. Cargo profile/PG 실행은 없음. 모델 source·cfg·view는 scratch/repo/archive 바이트 동일성을 확인하며 모든 foreground 검증 프로세스가 종료된 뒤 전달한다. A TLA/cfg와 이전 e1 아카이브는 바꾸지 않는다.

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
python3 scripts/ci/statecharts_tlc.py --root /tmp/i8128-owner-b1-check-final7 --jobs 2
python3 /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8128/logs/impl-b1/model/proof/run_claim_proofs.py
```

wrapper 재현은 최종 inputs-final/HerdrRestartOwner.tla에 기존 cfg와 view를 주고 새 --out 폴더를 쓴다. graph checker는 `proof/check_claim_witness.py GRAPH --source TLA --out NEW.json`으로 재현한다. 기존 raw 로그를 덮어쓰지 않는다.
