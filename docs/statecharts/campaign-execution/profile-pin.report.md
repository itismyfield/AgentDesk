# Campaign profile snapshot and execution pin

코드 기준: `ffa099d4f4a6a68c26171fb464f7b5edffca2e5f`와 M0 snapshot 결속 변경. 기존 [검증 실행 모델](verification.report.md)과 [명령 영수증 진입점 표](receipt-entrypoints.md)는 변경하지 않았다. 이 모델은 새 profile 값과 이미 저장된 pin의 관계만 검사하며 campaign 실행 FSM을 복제하지 않는다.

[모델](CampaignProfilePin.tla), [정상 설정](CampaignProfilePin.cfg), [생성된 차트](profile-pin.chart.mmd), [도구 원문 보고서](profile-pin.tool.report.md).

## 검사 결과

정상 모델은 **110 distinct states / 282 steps**, chart는 6 states / 9 arrows다. 공식 `scripts/ci/statecharts_tlc.py`로 신규 7개 cfg를 검사해 **7/7 expected 일치, 13 TLC runs, 8.3초**를 확인했다. 정상은 GREEN 1개, 의도한 RED는 6개다. 원시 결과는 레인 `i7438/m0/logs/profile-pin-check.log`와 `profile-pin-check.json`에 있다.

| cfg | 바꾼 경계 | 결과 | 가장 짧은 반례 |
|---|---|---|---|
| CampaignProfilePin | 모든 결속 유지 | GREEN | 없음 |
| Before | source·scope가 resolved hash에 없음 | RED: NoFalseMatch | 변경 → 새 snapshot → 재검증, 3단계 |
| NoSource | source 결속 제거 | RED: NoFalseMatch | source 변경 → 새 snapshot → 잘못된 matching, 3단계 |
| NoScope | 승인 scope 결속 제거 | RED: NoFalseMatch | scope 변경 → 새 snapshot → 잘못된 matching, 3단계 |
| NoRole | snapshot 지원 역할쌍 결속 제거(legacy roles 고정) | RED: NoFalseMatch | role 변경 → 새 snapshot → 잘못된 matching, 3단계 |
| ReplaceOld | drift를 확인하며 저장 pin도 교체 | RED: NoSilentPinReplacement | 새 snapshot → 재검증 중 old pin 교체, 2단계 |
| Ready | snapshot 생성이 실행 준비를 개방 | RED: SnapshotNeverOpensExecution | snapshot 생성, 1단계 |

`SameInputSameHash`는 같은 입력으로 snapshot을 다시 만들어도 같은 hash임을, `MatchingPinAccepted`는 같은 pin이 drift로 거절되지 않음을 확인한다. `NoFalseMatch`는 source/scope/role 중 하나가 달라지거나 snapshot 정보 없는 옛 pin일 때 matching을 허용하지 않는다. `NoSilentPinReplacement`는 absent pin을 명시적으로 최초 생성하는 행동 외에 저장 pin을 덮어쓰지 못하게 한다. `SnapshotNeverOpensExecution`과 명시적인 실행 요청은 공개 readiness가 계속 Unavailable임을 확인한다. 모든 정상 안전 규칙과 `NoDeadlock`이 통과했다.

Before는 기존 hash가 이미 roles를 포함한다는 사실을 유지하며 source·scope만 누락한다. NoRole은 **snapshot의 지원 역할쌍 결속 전체를 제거하는 추상 부정 대조**다. M0는 `adk_profile.rs:76~83`의 legacy roles hash도 유지한다. 따라서 NoRole은 관측 roles를 바꿔도 전체 profile hash가 그대로라는 주장이 아니다. legacy 입력을 고정한 채 snapshot 지원 역할 기여분을 독립적으로 바꾸었을 때 그 결속을 누락하는 경우를 나타낸다. 실제 `PipelineRulesSnapshot::hash`의 역할 기여분은 Rust 뮤턴트 시험으로 확인한다. 모델 hash는 세 입력 digest의 충돌 없는 tuple 추상화이며, 실제 Blake3/serde 구현의 안정성과 누락은 Rust 대상 시험이 검사한다. source나 scope가 달라진 뒤 새 profile 값을 만들어 비교해야 한다는 전제를 `CreateSnapshot`으로 드러냈다. 이전 snapshot 자체를 편집하는 행동은 없다.

## 진입점 전수표

아래 좌표는 M0 구현 worktree 기준이며, 이번에 수정하지 않은 행은 고정 base와 같다. 신규 두 child는 부모의 `#[cfg(test)]` 등록으로 휴면 경계를 유지한다(`domain/campaign/mod.rs:7~8`, `services/campaign_execution/mod.rs:18~19`). 실제 consumer 연결과 실행 활성화는 후행 조각 책임이다.

| 진입점·소비자 | 코드 좌표 | 모델·시험 경계 |
|---|---|---|
| ADK/CH 기본 profile | `src/domain/campaign/profile.rs:45,50,66` | 기본 v1 profile은 실행 Unavailable. snapshot 존재와 실행 허용을 구별 |
| 정의·요구/AC hash | `src/domain/campaign/definition.rs:122,164,199` | 요구/AC는 별도 입력이며 source/scope 편집이 이를 대신하지 않음. 이 모델은 요구/AC를 고정 |
| 실제 역할 resolver | `src/services/campaign_execution/adk_profile.rs:100,113,117,126,129,158` | 구체 모델/effort 및 capability 관측과 독립 agent를 요구. minimal resolver는 이 결과를 재사용 |
| snapshot 형식·hash | `src/domain/campaign/pipeline_rules.rs:31,47,81,88,97,105` | `CreateSnapshot`, `RepeatSnapshot`. 여섯 승인 자리·자동승인 OFF·후행 기능을 명시. serde는 snapshot 내부의 알 수 없는 필드를 거절하고 hash는 source/scope/지원 역할쌍을 모두 포함 |
| 현재 source·scope 검증 | `src/services/campaign_execution/pipeline_rules.rs:9,26,36,51,58,63,69` | 호출자가 새로 읽은 8개 필수 source/assets bytes를 요구하고 identity/digest만 남김. 승인 ref 2개·ADK/단일 이슈/고정 host/개별 PR 범위를 검사하며 source 순서를 정규화 |
| 지원 역할쌍·snapshot 생성 | `src/services/campaign_execution/pipeline_rules.rs:70,73,77,81,94,95` | 정확히 두 역할의 실제 Codex/WebGPT provider만 허용. default/missing/unknown·계정 ref·빈 관측을 거절하고 작업자리 1 및 snapshot을 기존 resolved 결과에 결속 |
| profile 복원·legacy 호환 | `src/services/campaign_execution/adk_profile.rs:62,63,64,173,174` | test-gated optional snapshot은 누락 시 None으로 복원, None 직렬화는 생략. 기존 resolver는 snapshot을 추측해 채우지 않음 |
| resolved profile hash | `src/services/campaign_execution/adk_profile.rs:75,76,79,84,85,86,88` | legacy tuple의 roles 결속을 보존. snapshot이 있으면 그 hash를 추가 결속, 없으면 기존 hash 그대로. Before는 source/scope 누락 대조, NoRole은 snapshot 기여분의 추상 누락 대조 |
| pin 최초 생성 | `src/domain/campaign/execution_pin.rs:92,101` | `PinSnapshot`. None에서만 명시적으로 pin을 만듦 |
| pin 재검증·복원 | `src/domain/campaign/execution_pin.rs:112,126` | `RevalidatePin`. old 형식의 hash 또는 source/scope/role drift는 거절하고 저장 pin은 보존 |
| Work 소비 | `src/services/campaign_execution/stage_fsm.rs:95,106,131,133` | pin hash와 definition/workflow/spec 결속 유지. 이번 모델은 해당 실행 전이를 바꾸지 않음 |
| Review 소비 | `src/services/campaign_execution/stage_fsm.rs:274,324` | seal의 기존 pin hash와 Work execution 결속을 유지. M0에서 실행 FSM 추가 없음 |
| 공개 readiness | `src/services/campaign_execution/control.rs:41` | `execution = unavailable` |
| 공개 Start/Resume/Retry | `src/services/campaign_execution/control.rs:335,339,343,348` | `RequestExecution`. snapshot 생성 후에도 ExecutionUnavailable |
| 공개 view | `src/services/campaign_execution/view.rs:190,198,268` | 같은 Unavailable과 command rejection을 표시. snapshot이 표시 상태를 바꾸지 않음 |

`rg`로 `resolve_adk_profile`, `ResolvedAdkProfile`, `resolve_minimal_adk_profile`, `ExecutionPin::pin`, `.revalidate(`를 campaign domain/service에서 확인했다. base의 직접 호출은 profile/pin 시험뿐이며 M0는 test-gated builder를 추가한다. production의 단계 소비는 이미 저장된 pin hash를 사용한다. M0는 runtime·DB·HTTP·health·운영 설정을 새로 연결하지 않는다.

## 반례의 코드 대조와 범위

세 binding 반례의 공통 경로는 기존 pin 유지 → snapshot 입력 하나 변경 → 새 resolved profile 생성 → `ExecutionPin::revalidate`다. base의 `ResolvedAdkProfile::hash`에는 roles가 이미 있지만 snapshot source/scope는 없으므로 Before의 source/scope 누락은 base의 계약 공백과 일치한다. M0에서는 snapshot을 hash에 넣어 그 경로를 막으며, source·scope·role 누락 Rust 뮤턴트가 실제 바이트 수준 결속을 별도로 확인한다. NoRole은 legacy roles와 snapshot roles가 모두 바뀌는 정상 resolver 경로의 정확한 대체 실행이 아니라 snapshot 역할 결속만의 독립 부정 대조다.

ReplaceOld 반례는 현재 `revalidate(&self, ...)`에서 불가능하다. Ready 반례도 `execution_readiness`와 Start/Resume/Retry의 무조건 Unavailable 때문에 현재 코드에서 불가능하다. 두 설정은 그 기존 경계를 약하게 만들었을 때 모델이 탐지하는 부정 대조이며, main의 새 버그로 보고하지 않는다.

한 번의 source/scope/role 변경만 두어도 각 경계 누락의 가장 짧은 반례가 나오므로 입력 조합의 무한 반복은 넣지 않았다. None/Old/Matching pin을 초기값으로 모두 탐색하며 old pin은 snapshot 결속 없는 과거 hash를 의미한다. 세 입력 값은 digest·승인 ref·지원 역할의 관측 차이를 추상화하며 계정값·토큰·운영 설정 원문을 저장하지 않는다.

외부 자원 가용성·프로세스 시작·두 stop·검증 영수증·DB transaction·재시작 복구 전이는 기존 모델/후행 조각 책임이다. 이번 변경은 순수 immutable 값 구성과 비교뿐이므로 별도 crash/retry 권한이나 외부 효과를 추가하지 않는다. 실제 자원/외부 응답 없이 결국 실행 완료한다고 주장할 수 없어 liveness PROPERTY를 추가하지 않았다. chart는 같은 label 안의 재생성·실행 요청·종료 stutter를 숨긴다.

## 재생성

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart \
  docs/statecharts/campaign-execution/CampaignProfilePin.tla \
  --config docs/statecharts/campaign-execution/CampaignProfilePin.cfg \
  --view docs/statecharts/campaign-execution/profile-pin.view.mjs \
  --out /tmp/campaign-profile-pin-new
```

각 부정 대조는 `--config`에 해당 cfg를 지정한다. 정상 exit 0 / 부정 대조 exit 1이며 expected.tsv는 그 정확한 위반 집합을 등록한다. CI의 changed-model 선택은 모델을 import하는 기존 검증 FSM cfg를 추가로 선택하지 않는다.

## M4 런타임 관측과 검증 호출

`CampaignRuntime.cfg`는 역할 관측, 한 번의 host 재시작, 검증 어댑터 호출을 모델링한다. `NoForeignInvocation`은 지원 역할쌍 밖의 호출을 막고 `NoUnobservedInvocation`은 현재 부팅에서 관측되지 않은 역할의 호출을 막는다. 다음 부정 대조는 각 가드가 빠졌을 때의 모델 결과다.

| cfg | 가드 변경 | 결과 |
|---|---|---|
| CampaignRuntime | provider와 현재 부팅의 관측 확인 | GREEN |
| CampaignRuntimeNoFence | provider 확인 생략 | RED `NoForeignInvocation` |
| CampaignRuntimeNoObservation | 관측 요구 생략 | RED `NoUnobservedInvocation` |
| CampaignRuntimeStaleObservation | 재시작 뒤 boot 확인 생략 | RED `NoUnobservedInvocation` |

두 역할의 검증 결과는 모델에서 하나의 값으로 추상화한다. Rust에서는 `RuntimeProfile::observe`가 각각의 역할을 직접 샘플링하고 provider marker, 세션, tool policy, 관측 시각을 검사한다. `VerificationDriver::step`은 현재 attempt의 세션과 pin을 재검증한 뒤 `AdkAdapter::materialize`로 입력을 준비한다. `HostWorker::start_phase`가 새로 잠근 상태의 최종 권위로 실제 phase를 개시한다. 재시작 뒤 새 phase 입력을 처리할 때 역할을 다시 관측하며, journal에 보관된 결과 전달은 이미 기록된 증거로 처리한다.
