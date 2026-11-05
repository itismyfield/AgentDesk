# Herdr RootReplaced recovery — S0/S1

기준 코드는 `d5088398284093a5411de84355809b4ee01b0e9a`이다. 기존 `HerdrSession.tla`와 `HerdrSession.cfg`는 바꾸지 않았다. 기존 `RowEnd`가 shell/closed를 바로 Retired로 만드는 추상화와 이번 정책 admission 누락을 섞지 않도록 `HerdrRootRecovery.tla`로 분리했다. 새 모델의 endpoint mode는 UserDefault로 고정한다.

## 검사 결과

| 설정 | 상태 수 | safety | 진행 속성 | 결과 |
|---|---:|---|---|---|
| `HerdrRootBefore.cfg` | 28 | 모두 유지 | `ReplacedInputProgress` 위반 | RED: statechart exit 1, 직접 TLC exit 13 |
| `HerdrRootFixed.cfg` | 16 | 모두 유지 | `ReplacedInputProgress` 유지 | GREEN: exit 0 |
| `HerdrSameRoot.cfg` | 7 | 모두 유지 | `SameRootInputProgress` 유지 | GREEN: 같은 실행에 입력 1회, create 0 |

검사한 safety는 `SameRootNoCreate`, `InputAtMostOnce`, `NoRetireLiveExecution`, `PreserveHeldInput`, `NoUnverifiedServerTrust`, `InputUsesCurrentExecution`, `PendingBeforeCreate`, `OldGrantUnchanged` 및 deadlock 부재다. `NoRetireLiveExecution`은 퇴역 순간의 생존/quiet를 기록해 검사하며, 퇴역 후 새 실행이 살아 있는 것을 금지하지 않는다. N1 정책 admission은 현재 witness와 같은 startup proof 또는 immutable N1 exact grant만 인정한다. N2의 새 create는 별도 NextRestartReady로 만든 exact fresh grant를 기록한다. `nextRestartReady`는 startup evidence와 다른 변수이며, config 폴더 identity와 수정 시각 조건은 startup에만 적용한다.

전체 `scripts/ci/statecharts_tlc.py`는 30개 설정을 113.3초에 검사해 exit 0이며 기존 설정의 기대 반례는 그대로다. `HerdrSession.cfg`의 기대값은 `NoDeadlock,NoOrphanPane,PromptOnlyToAgent,RowMatchesPane`이다. 새 Before의 유일한 기대 반례는 TSV의 네 번째 필드 `ReplacedInputProgress`다.

## S1 범위와 비공허성

세 설정의 `InputAfterFirstPass = TRUE`는 정상 입력 Q가 첫 reader pass의 완료(Held 또는 복구 완료) 뒤에 도착한다는 가정이다. S1 GREEN에서 입력은 복구 완료 뒤 도착하므로 별도 완료 wake가 필요 없다. 첫 pass 전 입력의 claim/inflight race 및 이미 Held인 입력의 완료 wake는 S2 범위다. 이는 무조건적인 이슈 DoD 달성 주장이 아니다.

`StableRecoverableReplacement`는 local/current, old process 종료, replacement provider 부재, peer/image·명시 false config·시작 이전 수정·같은 config/socket 폴더·정상 cache만 요구한다. startup cache나 admission 성공을 진행 속성 전제로 넣지 않았다. producer 없이도 참인 환경에서 Before가 실패한다. 같은 root 설정에는 별도 `SameRootInputProgress`를 실제 PROPERTY로 넣어, 교체용 속성이 공허하게 참인 것만으로 통과하지 않는다.

`SPECIFICATION Spec`이 실제 약한 공정성을 포함한다. Before에는 존재하지 않는 producer의 fairness를 넣지 않았고, `OwnerInstalled = TRUE`일 때 기존 server-replaced producer는 비활성이다. 회복·증거 준비·target·엄격 증명·퇴역·cleanup·입력 drive·launch·bind·send는 각 실제 guard가 허용할 때만 공정하게 진행한다. local/current, 정책 또는 quiet guard가 false인 상태를 fairness로 통과시키지 않는다.

## 수정 전 반례의 코드 대조

아래 단계 번호는 `herdr-root-before.chart.mmd`를 만든 statechart report의 0–17과 같다. 경로 좌표는 모두 위 base SHA다.

| 단계 | 모델 행동과 결과 | 코드/관측 대조 |
|---:|---|---|
| 0 | N1 Bound, W0 grant, R0 expected | `src/services/session_host/herdr/policy/permit.rs:146–154`: endpoint/nonce exact grant는 과거 witness를 보존 |
| 1 | dcserver 재시작, process cache 비움 | `src/services/session_host/herdr/launch_host.rs:204–210`: durable grant 복원과 별개로 ProofCache·witnessed 새 생성 |
| 2 | herdr 서버 W0→W1 | `src/services/session_host/herdr_gate.rs:905–915`: gate는 socket의 현재 server witness를 읽음; 관측 보고의 reboot 교체와 대응 |
| 3 | pane root R0→R1, old process 종료 | `src/services/session_host/herdr_gate/root_replaced.rs:38–56`: 다른 root의 lone shell 및 원래 프로세스 부재를 개별 확인 |
| 4 | reconcile 완료 | `src/services/discord/herdr_terminate/root_replaced.rs:345–350`: reconcile 완료 전에는 보류 |
| 5 | reader가 RootReplaced candidate 유지 | `src/services/discord/recovery_engine/herdr_reader.rs:304–324`: exact candidate 또는 앞 pass candidate를 보존 |
| 6 | installed owner 경로에 producer 없음 | `src/services/discord/queue_dispatch/kickoff.rs:120–124`: owner 설치 시 기존 server-replaced 요청 경로 전에 반환; base `recover_inner:375–378`은 관측 없이 target 생성 |
| 7 | startup 없음·old grant W0를 새 target에 복사 | `src/services/session_host/herdr/launch_host.rs:300–317`: target evidence를 값으로 획득; `src/services/discord/herdr_terminate/root_replaced.rs:378–384` target으로 첫 증명 |
| 8 | `PolicyUnverified(Identity)` Held | `src/services/session_host/herdr/policy/permit.rs:176–185`: startup/grant 어느 것도 W1이 아니면 Identity; `src/services/discord/herdr_terminate/root_replaced.rs:389`에서 Held |
| 9 | 정상 입력 Q 수락, custody 보존 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:440–447`: 기존 front를 claim 경로로 전달 |
| 10 | 옛 binding으로 시작하려고 먼저 claim | 같은 `owner_input.rs:447–448`: 기존 경로의 claim 선행 |
| 11 | pane 확인 실패 후 입력 보류·inflight 유지 | `src/services/session_host/herdr_gate.rs:973–980`: pane unconfirmed→PaneUnverified; 관측 보고 23:21:27–23:28의 inflight 생성·보존과 대응 |
| 12 | 다음 reader pass가 candidate 재시도 | `src/services/discord/recovery_engine/herdr_reader.rs:306–311,326–332`: 매 pass 기존 candidate로 recover |
| 13 | exact RootReplaced 재관측 | 같은 `herdr_reader.rs:304–324`; row/nonce가 바뀌지 않음 |
| 14 | producer 누락 재현 | 단계 6과 동일; installed owner가 계속 producer를 우회 |
| 15 | 새 target에도 W1 startup 없음 | 단계 7과 동일; 관측을 하지 않았으므로 cache를 다시 읽어도 없음 |
| 16 | strict policy가 다시 Held | 단계 8과 동일; quiet 검사 전에 Identity에서 거절 |
| 17 | candidate를 남기고 다음 pass 반복 | `herdr_reader.rs:326–332`: 성공 Retired만 결과에서 제거. 이후 13–17 lasso |

따라서 최초 장벽은 입력/inflight가 생기기 전 단계 8의 정책 admission이다. 이후 current-generation inflight는 추가 quiet 장벽이 된다(`src/services/discord/herdr_terminate/root_replaced.rs:249–253`). 기존 restarted-inflight 처리의 guard는 이전 generation을 요구하므로(`src/services/discord/recovery_engine/herdr_row_end.rs:53–73`) 이를 무조건 퇴역시키는 추상화는 두지 않았다.

## S1 전이와 잔여 반례

GREEN은 기존 권한이 없을 때 생산 경로를 호출하고 startup 조건을 충족한 증거를 만든 뒤 `CaptureTarget`에서 새 snapshot을 얻는다. 이어 첫 strict proof → local/current·quiet·같은 witness의 final proof → CAS → old hold cleanup → Pending → physical create 1 → evidence/bind → Q effect 1이다. immutable N1 grant는 그대로 W0다. 실제 guard는 `src/services/discord/herdr_terminate/root_replaced.rs:401–425`와 `src/services/session_host/herdr_gate/root_replaced.rs:23–27,54–56`에 대응한다.

추가 scratch 대조는 다음과 같다(저장소 cfg 수를 늘리지 않음).

- Fixed에서 `InputAfterFirstPass = FALSE`: 진행 속성 RED. Q가 먼저 current-generation inflight를 만들면 정책 생산 성공 뒤에도 `quiet`가 막는다. S2가 담당할 실재 경계다.
- Fixed에서 `CaptureTarget`을 기존 target 재사용으로 바꿈: 진행 속성 RED. cache가 target snapshot을 자동 갱신한다고 모델링하지 않았음을 확인한다.
- config가 서버 시작 뒤 쓰인 경우: NextRestartReady가 생겨도 startup은 없고, 별도 `LateConfigNoRetire` invariant(계속 Bound·create 0)가 유지된다.
- durable old grant 없음: old root/provider가 실제 absent이고 현재 startup proof가 정당하면 Fixed 진행 GREEN. PID 재사용과 reboot grant 규칙은 Rust 부정 시험의 영역이다.

## 모델의 경계

한 채널·Q 하나·dcserver generation 하나 증가·herdr witness W0/W1·root R0/R1·N1/N2만 다룬다. 실제 PG 실패, config token의 바이트 정체성, PID 재사용, socket/실행 이미지 공격, 두 번째 서버 교체 경합, admission fence의 구현 세부는 이 모델로 입증하지 않는다. 서버 교체는 reconcile 전 한 번이며, 관측 도중의 두 번째 교체는 실제 코드의 전후 witness 재검사 시험/감사로 검증해야 한다. production 정책 모듈을 모델로 대체하지 않는다.

고정 chart는 14개 상태·13개 화살표다. 같은 label 안에서 바뀌는 3개 단계는 그리지 않는다. Before chart는 입력 custody와 정책 lasso를 모두 보이기 위해 24개 상태·36개 화살표를 유지한다. 숨긴 외부 실패 전이는 없고, 동일 label 내부 전이만 생략한다.

## 재실행과 증거

```sh
unset TMUX TMUX_PANE
export JAVA=/opt/homebrew/opt/openjdk/bin/java
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
python3 scripts/ci/statecharts_tlc.py
~/.codex/skills/statechart/scripts/statechart docs/statecharts/session-lifecycle/HerdrRootRecovery.tla --config docs/statecharts/session-lifecycle/HerdrRootBefore.cfg --view docs/statecharts/session-lifecycle/herdr-root.view.mjs --out /tmp/herdr-root-before-new
```

jar SHA256은 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`이다. 원시 로그는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7788/logs/impl-state-*`에 있다. `impl-state-final-exits.json`은 frontend Before=1/Fixed=0, 직접 TLC Before=13/Fixed=0/SameRoot=0을 구분한다. `impl-state-environment.json`은 최종 모델·jar SHA와 TMUX 해제 환경이며, `impl-state-suite-final.log`는 전체 기대값 대조이고, `impl-state-variants.json`은 추가 scratch 대조의 exit 결과다. 각 chart-verified 디렉터리에 사용한 TLC 원문과 생성 report가 있다. 기본 PATH의 Java stub에는 runtime이 없어 Homebrew Java의 절대 경로를 사용했다. PG·cargo·운영 herdr 연결은 하지 않았다.

## S2 확장

S2의 첫 pass 전 입력과 완료 wake는 `docs/statecharts/session-lifecycle/herdr-root-s2.report.md`에 별도로 기록한다. 이 문서의 기존 S1 세 설정은 새 PreclaimEnabled/WakeEnabled를 FALSE로 두어 과거 기대 위반 집합을 유지한다. S2에서 claim 권리와 실제 start를 분리하면서 Before 상태 수는 28→33으로 바뀌었고 Fixed/SameRoot는 16/7로 유지된다.
