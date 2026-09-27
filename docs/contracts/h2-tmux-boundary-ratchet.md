# H2 — Rust prod 경계 tmux 직접 조작 사이트 래칫

## 1. 범위와 현재 상태

이 계약은 `src/**`의 **root lib 타깃 prod 코드 중 owner 밖** tmux 직접 조작 사이트의 증가를 제한한다.
owner는 `src/services/platform/tmux.rs`, `src/services/platform/tmux/availability.rs`,
`src/services/session_host.rs`, `src/services/session_host/**`이며, 별도 owner 명부도 대조한다.
테스트·bin 전용 코드·build script·proc-macro 실행·외부 크레이트 내부·운영 스크립트는 측정 범위 밖이다.

**PR-1c는 inert 계약·진단 단계다. 활성화 완료를 뜻하지 않는다.**
기준 main `cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81`에는 `clippy.toml`,
`scripts/ci/h2_admissions.toml`, 두 baseline 파일이 없고 `LIVENESS_FLOOR = 0`이다.
CI의 측정/map 호출에는 `--inert`가 있고 admission CLI는 CI에서 호출하지 않는다.
정책 exec allowlist는 아직 `gh/git/tmux`이며 `hasLivePane`와 `gh/git` 제한은 PR-2의 작업이다.
이 문서에서 활성 후 계약과 후속 방어선을 현재 구현의 보장으로 읽지 않는다.

정본은 [r9 결정](https://github.com/itismyfield/AgentDesk/issues/5340#issuecomment-5819255658)이다.
R-O 방식에는 이후 [사용자 (a) 결정](https://github.com/itismyfield/AgentDesk/issues/5340#issuecomment-5839117239)과
[최종 compiler driver 결정](https://github.com/itismyfield/AgentDesk/issues/5340#issuecomment-5843281892)이 우선한다.
텍스트 Rust cfg 평가기였던 #6276은 닫혔으며 그 구현이나 레인별 텍스트 기대표를 재도입하지 않는다.

## 2. 현재 구현과 계약 심볼

| 책임 | 구현·참조 | 입력과 결과 |
|---|---|---|
| Clippy 측정 | [h2_measure.py](../../scripts/ci/h2_measure.py), `sym:h2_measure::measure` | lib JSON의 disallowed_methods/types 진단 → 레인별 행과 W/SUBPROC_W 도출 |
| 고정점 재생성 | `sym:h2_measure::regen` | EXEC seed에서 반복 측정, 최대 20패스; clippy 설정과 baseline 갱신 |
| admission 종합 | [h2_admission.py](../../scripts/ci/h2_admission.py), `sym:h2_admission::evaluate` | base/head, JSON, 명시적 TSV → zero-rule·R-O·R-E·증가 승인 오류 |
| R-W 증가 검사 | `sym:h2_admission::rw_problem` | EXEC/W/TYPES 증가 item이 등록된 W에 속하는지 확인 |
| R-O compiler 대조 | [h2_depinfo.py](../../scripts/ci/h2_depinfo.py), `sym:h2_depinfo::ro_problems` | root lib dep-info와 expanded module map, duplicate_mod 진단 대조 |
| 잔존 walker 대조 | `sym:h2_depinfo::walker_problems` | compiled file의 realpath→modpath와 R-W 텍스트 walker의 실제 open 경로 대조 |
| module map 수집 | [h2_modmap.py](../../scripts/ci/h2_modmap.py), `sym:h2_modmap::map_modules` | 이번 실행의 TSV 존재·신선도·형식·file module 하한 검사 |
| cfg 목록 진단 | [h2_cfg_compare.py](../../scripts/ci/h2_cfg_compare.py), `sym:h2_cfg_compare::compare_cfgs` | 컴파일러가 출력한 cfg 원자 집합의 교집합과 양방향 차이 |

[modmap-driver](../../tools/modmap-driver/src/main.rs)는 `RUSTC_WORKSPACE_WRAPPER`로 root lib 컴파일을 식별하고
`after_expansion`에서 [module map](../../tools/modmap-driver/src/modmap.rs)을 쓴 뒤 `Compilation::Stop`한다.
file/inline/include/wrapped 행은 선언·식별자 context, 부모 파일, 중첩, 속성 출처를 보존한다.
드라이버에는 `rustc-dev`가 필요하다. `RUSTC_BOOTSTRAP=1`은 드라이버 빌드에만 사용한다.
map 실행은 wrapper를 비우고, 기존 TSV를 지운 뒤 marker 시각과 비교한다.
canary의 오류 목록이 일치해야 하며 실제 저장소 map은 file module이 1000개 이상이어야 한다.

R-O는 compiler map을 사용하지만 **item 귀속 전체를 compiler def-path로 바꾼 것은 아니다**.
`h2_measure._module_walk/_module_table`은 W 도출, R-E owner pub fn 명부, R-W 증가 검사에 남아 있다.
따라서 compiler map과 walker 대조를 제거하면 이 소비자의 경로가 무검증 상태가 된다.
`SourceFile`의 함수·const/static 귀속 및 바깥 macro call-site 사용 한계는 §7에 남긴다.

이 문서의 `sym:` 참조는 `scripts/check_contract_symbol_refs.py`가 실제 Python 함수 import와 1:1 대조한다.
이것은 이름·문서의 드리프트 검사다. Rust driver의 컴파일 증명이나 H2 활성화 검증을 대신하지 않는다.

## 3. 레인·재현 조건·cfg 목록 비교

측정 레인은 `linux`(`x86_64-unknown-linux-gnu`)와 `macos`(`aarch64-apple-darwin`)다.
`rust-toolchain.toml`은 현재 1.94.1이며 Clippy를 설치한다. macOS hosted 레이블은 `macos-15`다.
`h2_measure.sh`는 host triple을 확인하고 `CARGO_BUILD_TARGET`, `RUSTFLAGS`,
`CARGO_ENCODED_RUSTFLAGS`를 해제하며 incremental을 끈다. root lib/default features가 측정 기준이다.
env 해제만으로 Cargo config, build-script cfg, 실제 rustc argv의 일치가 증명되지는 않는다.
재현 조건을 명시하는 것이며 모든 환경에서 같은 결과를 보장하지 않는다.

cfg 비교 입력은 각 레인에서 **실제로 실행한 컴파일러의 `--print cfg` 형식 UTF-8 출력**이다.
예를 들어 각 호스트에서 `rustc --print cfg > lane.cfg`를 실행하면 그 컴파일러의 기본 cfg를 얻는다.
root lib 비교에 쓰려면 실제 invocation의 target, feature/custom `--cfg`, codegen 옵션 등도 반영해야 한다.
일반 `rustc --print cfg` 출력만으로 Cargo feature나 build-script가 넣은 cfg를 수집했다고 주장하지 않는다.
호출 argv·toolchain·source/config·build-script 결과와의 provenance 연결은 PR-3b에서 검증할 잔여다.

두 호스트의 결과를 모은 뒤 실행한다:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/ci/h2_cfg_compare.py \
  --linux linux.cfg --macos macos.cfg
```

stdout은 `common`, `linux_only`, `macos_only` 세 정렬 배열을 가진 JSON이다.
각 줄은 `unix` 또는 `target_os="linux"` 같은 원자 하나다. 빈 줄·순서·중복은 무시한다.
같은 key의 여러 값(`target_feature`, `target_has_atomic`, `feature` 등)은 각각 보존한다.
인용 값·escape는 불투명 문자열로 보존하며 동치 평가를 하지 않는다.
출력 형식을 검사할 뿐 cfg 술어, `cfg_attr`, 소스 파일·모듈 도달성을 해석하지 않는다.

| rc | 뜻 | 처리 |
|---|---|---|
| 0 | 두 유효 목록의 집합이 동일 | 목록 동등성만 확인; 증거의 신선도·완전성·target triple 증명은 아님 |
| 1 | 하나 이상의 차이 | 양쪽 전용 원자를 진단; OS/architecture 차이도 숨기지 않음 |
| 2 | 필수 인자 누락, 파일 읽기/UTF-8/형식 오류, 빈 목록 | stderr에 레인·파일(형식 오류는 줄 번호); 비교 JSON을 내지 않음 |

Linux/macOS의 정상 target cfg는 다르므로 rc 1은 예상 가능한 진단이다.
이를 곧바로 admission 위반으로 취급하거나 allowlist로 지우지 않는다.
비교 도구 자체는 컴파일러를 실행하지 않고 입력 파일·baseline도 수정하지 않는다.
이번 CI 배선은 이 도구의 행동 테스트뿐이며 실제 목록 비교로 H2를 활성화하지 않는다.

[r8](https://github.com/itismyfield/AgentDesk/issues/5340#issuecomment-5818836310)의 §6.5/A12′는 hosted 측정이 15분을 넘을 때
cfg 파일/항목 목록으로 대체하는 방안을 기술했다. 이후 compiler 결정에 맞춰 PR-1c는 실제 cfg 목록의
차이 진단만 제공한다. 소스의 macOS cfg 항목 수를 세는 파서나 R-O 대체 게이트는 만들지 않는다.
비용 초과 시 측정 배치 변경은 후속 결정 사항이며 이 도구의 rc 0을 macOS 측정 성공으로 대체할 수 없다.

## 4. 래칫 규칙

행 키는 `(file, enclosing_item, callee)`이며 레인별 호출 수를 기록한다.
동명 익명 item 충돌에는 span 시작 줄 보조 정보와 admission의 `lines`를 사용한다.
owner의 진단은 행 계수에서 제외한다. 아래는 활성 시 적용할 계약이며 현재 함수 구현은 inert 상태다.

| 규칙/세트 | 계약 |
|---|---|
| EXEC | 등록된 저수준 tmux 실행 함수의 owner 밖 직접 참조 |
| W* / R-W | EXEC/W/TYPES 참조 함수의 전이 폐포; 단일 측정 패스로 등록 집합과 도출 집합 등호 검사 |
| TYPES | `TmuxTuiActionExecutor`, `TmuxSendBackend` 두 경로 참조; trait 객체 소비 경계 보조 |
| SUBPROC | std/tokio `process::Command::new` 사이트 |
| SUBPROC_W / R-D | 비리터럴 프로그램 또는 DISPATCHERS 리터럴+비리터럴 인자 사이트의 함수와 그 **1-hop 호출자**; 전이 폐포 없음 |
| R-E | owner pub fn = EXEC ∪ NONEXEC ∪ PS; SUBPROC/TYPES 고정 집합, W/SUBPROC_W 고정점, 사문·등록 불가 항목 검사 |
| R-C / R-C2 / R-F | owner 밖 Command+tmux literal multiset 고정, tmux const/static·exec/posix_spawn 새 우회 거부 |
| R-O | owner 명부·owner path 제한, Cargo.lock tmux/pty 패키지·H9 검사, 아래 compiler 입력/모듈 대조 |

R-O는 Clippy JSON에서 root non-test lib artifact를 골라 같은 hash의 `.d`를 읽는다.
`.rs` 입력과 file module 집합은 양방향으로 같아야 하고 비 `.rs` 입력은 data allowlist에 속해야 한다.
저장소 밖 입력, include! 삽입, macro가 만든 file module·파일 모듈을 감싼 부모·hand-written item 재배치,
item 내부 file module, macro 속성, owner의 `#[path]`, 비 `.rs` file module을 거부한다.
written spelling과 realpath의 확장자 의무를 모두 보존하고 `clippy::duplicate_mod` 진단도 거부한다.
allowlist 데이터나 같은 `.rs`를 읽었다는 이유만으로 임의 코드 삽입을 승인하지 않는다.
dep-info는 proc-macro의 임의 파일 I/O에 대한 감사 로그가 아니다.

## 5. Baseline과 one-shot admission

baseline은 `scripts/ci/h2_baseline_services.toml`(`src/services/**`)과
`scripts/ci/h2_baseline_others.toml`(그 외)로 나눈다. 이 경계는 PR 크기 제한을 위한 데이터 분할이다.
섹션은 `exec`, `w`, `types`, `subproc`, `subproc_w_callers`이며 `linux/macos` 열을 유지한다.
PR-3a1~3a3은 같은 source/config snapshot의 양 레인 `--regen` 산출물이어야 한다.
도중 main이 진행되면 재생성한다. CI가 baseline을 자동 상향하지 않는다.

admission은 `h2_admissions.toml`의 base 접두부를 변경하지 않고 suffix에만 추가한다.
`file/item/callee/old/new/lane/issue`가 필수이고 `lane`은 linux/macos/both다.
`old == base`, `new == head`, 증가 키의 정확한 1회 승인, 양 레인 중복·불필요 승인 거부를 검사한다.
R-W/R-E/R-O/zero-rule 위반은 admission으로 면제되지 않는다.
활성 시 base baseline·W 설정 부재는 rebase 오류이며 baseline 부재를 bootstrap green으로 처리하지 않는다.

## 6. 단계와 CI

순서는 PR-1a → 1b → **1c** → 2 → 3a1 → 3a2 → 3a3 → 3b다.
측정·admission·dep-info·driver backend는 이미 착지했다. 이 PR은 계약과 cfg 진단만 보탠다.
`scripts/ci-script-checks.sh`의 guards 샤드가 H2 Python 테스트를 실행하고,
기존 contract-symbol CI 호출이 이 문서의 Python 참조도 검사한다.
기존 Linux CI는 inert 측정+driver canary, Mac CI는 inert 측정/map을 호출한다.
canary 성공이나 baseline 없는 no-op은 root lib의 양 레인 측정 증거가 아니다.

PR-3b 설계 v1의 활성화 준비 판정은 **BLOCKED**다. 다음은 이 PR에서 해결하지 않은 착수 조건이다:

- 같은 snapshot의 Clippy JSON/.d와 별도 expansion 패스 TSV의 source·target·cfg·신선도 연결.
- 정상 doc 속성 오탐 수리, 두 baseline/섹션/레인 완전성, 실제 W 수렴과 0보다 큰 liveness floor 실측.
- 양 레인 실패·취소·skip·증거 누락을 required 결과로 전파하는 CI와 비용 증거.

현재 loader는 부분 baseline/누락 lane을 허용한다. `--inert`도 baseline이 생기면 컴파일을 시작할 수 있다.
따라서 데이터 착지·관측·활성을 각각 검증해야 한다. receipt나 새 required 배선은 아직 구현된 계약이 아니다.
원자적 활성 시 baseline 부재=red 및 양 레인 required 결과를 함께 연결한다.

## 7. 수용 한계와 구현 중 발견한 잔여

r9의 15개 ID군을 모두 유지한다(H10의 a/b는 같은 행에 구분).
방어선 중 정책 제한·typed API·required 측정은 §1/§6의 선행 작업이 끝나야 유효하다.

| ID | 한계 | 남는 방어선/후속 |
|---|---|---|
| H1′ | 함수 포인터·클로저로 실행 함수를 값으로 전달하여 W 밖에서 호출 | R-E 등록 경로 이름 변경·삭제 감지, 리뷰 |
| H2″ | 매크로 생성 호출의 enclosing_item이 호출 함수와 다를 수 있음 | 정의 함수 W 취급과 R-W, 리뷰; 완전 item 귀속 보장은 아님 |
| H2‴ | SUBPROC_W 2-hop 이상 간접 실행 | SUBPROC 사이트·1-hop 호출자 래칫, PR-2 정책 제한, 리뷰; 완전 폐포는 CLI 표면 과포함으로 불채택 |
| H3 | build.rs·빌드 스크립트·proc-macro의 실행과 임의 I/O | lib 측정 범위 밖, 리뷰 |
| H4 | 외부 크레이트 내부 실행으로 우회 | Cargo.lock tmux/pty 이름 거부, 의존성 리뷰; 모든 우회 크레이트 검출은 아님 |
| H5 | scripts/routines/E2E의 직접 tmux | 범위 밖; deploy-release.sh, session-anchor*.zsh, relay_watchdog.py, _defaults.sh 및 E2E/smoke 스크립트 |
| H6 | gh/git allowlist도 git -c core.sshCommand·gh alias 등의 셸 우회를 막지 못함 | 인자 검사 미구현, 리뷰 |
| H7 | Clippy lint 이름 변경·삭제 | 1.94.1 고정, 업그레이드 시 실제 진단·liveness 검증 |
| H8 | impl의 익명 const/static/closure enclosing_item 이름 충돌 | span 시작 줄 보조 필드, 충돌 admission에 lines 요구 |
| H9 | Windows 전용 항목은 양 레인 미측정 | src/runtime_layout/windows_links.rs::create_directory_junction 미등록, 해당 파일 tmux 문자열 금지 |
| H10a / H10b | a: hasLivePane의 서버 재시작 직후 stale socket; b: binary_resolver 캐시의 최초 호출 의존 | a: PR-2 호출측 재시도; b: 격리 HOME/PATH 가짜 tmux 또는 owner 내부 runner 주입의 있음/없음/오류 행동 테스트 필수 |
| H11 | W와 SUBPROC_W churn으로 admission 부담 | one-shot suffix, 분기별 재기준 PR; r9 규모 추정은 현재 실측값이 아님 |
| H12 | DISPATCHERS 밖 make/just/사용자 래퍼 리터럴의 동적 인자 | 상수 목록 변경+regen+admission, 리뷰 |
| H13 | 설계 어휘 census와 Clippy JSON 사이 오차 | 규모 추정은 실제 양 레인 측정값으로 교체 |
| H14 | macOS hosted 레이블 변경으로 측정 중단 | 레이블 갱신과 실제 host 단언; 현재 inert no-op은 host를 검사하지 않음 |
| H15 | 정상 owner doc 속성의 concat!/include_str!도 item macro로 오탐 | [owner_shape_problems:132][shape]; PR-3b 전 수리할 차단 결함, 영구 허용 아님 |
| H16 | macro fn의 module 귀속·`<module>` R-W 면제, raw identifier·const/static 내부 item 경로의 선재 한계 | [SourceFile:66/measure:309][items], [rw_problem:274][rw]; 파일 map은 R-W 전체 증명이 아님, 실제 baseline 영향 검증 |
| H17 | 정상 항등 macro 모듈·hand-written item 재배치도 fail-closed 거부 | [modmap_problems:124][modmap]; 출처 보존 규칙의 보수적 오탐, 컴파일 가능한 모든 Rust 문법 지원 약속 없음 |
| H18 | hardlink·대소문자 alias의 파일 동치가 realpath만으로 증명되지 않음 | [classify:92/walker_problems:174][aliases]; duplicate_mod와 함께 실측 필요, 미측정을 해결로 세지 않음 |
| H19 | 두 compiler 패스가 같은 source/target/cfg였는지 입증하는 receipt 부재 | [map_modules:57][collector], [root_lib_depinfo:45][depinfo]; PR-3b 증거 연결 필요, cfg 목록 일치만으로 대체 불가 |

H15~H19는 r9 §8-3에 따라 추가한다. 근거는 기준 main 코드와
`design-pr3b-v1.md`의 「이전 DESIGN_BLOCKED와의 대조」, 「우회 경로 전수표」(P3-E/I/F/J·V5)다.
후자는 코디네이터 레인 기록이며 위 링크는 같은 기준 SHA의 저장소 근거를 고정한다.

[shape]: https://github.com/itismyfield/AgentDesk/blob/cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81/scripts/ci/h2_admission.py#L132
[items]: https://github.com/itismyfield/AgentDesk/blob/cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81/scripts/ci/h2_measure.py#L66
[rw]: https://github.com/itismyfield/AgentDesk/blob/cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81/scripts/ci/h2_admission.py#L274
[modmap]: https://github.com/itismyfield/AgentDesk/blob/cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81/scripts/ci/h2_depinfo.py#L124
[aliases]: https://github.com/itismyfield/AgentDesk/blob/cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81/scripts/ci/h2_depinfo.py#L92
[collector]: https://github.com/itismyfield/AgentDesk/blob/cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81/scripts/ci/h2_modmap.py#L57
[depinfo]: https://github.com/itismyfield/AgentDesk/blob/cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81/scripts/ci/h2_depinfo.py#L45

## 8. 설계 동결

r9 §1~§7과 r9에서 변경하지 않은 r8 규칙이 동결 범위이며 충돌 시 r9가 우선한다.
R-O 구현 방식은 §1의 후속 compiler 결정으로 갱신했다. PR 순서·성격은 유지하며 각 PR은 20파일/+800 이하다.
파일·줄 추정 ±20%를 기준으로 하고 초과 분할은 같은 성격 안에서 한다.
수치 차이, 테스트 기법 교체, dispatcher 증감, cfg 스텁 위치, baseline 분할 조정은 구현 리뷰에서 처리한다.
새 한계는 이 §7 표에 근거와 함께 추가한다. 그 자체로 설계를 재개하지 않는다.

설계 재개는 구현으로 확인된 다음 세 P0에 한해 코디네이터가 실패한 층만 교체한다:

1. lib 타깃에서 force-warn disallowed_methods/types JSON 진단이 모두 불가능해 L1이 무력화됨.
2. 단일 패스에서 W* 고정점 검증이 불가능해 R-W 귀납이 붕괴함.
3. 활성 workflow에서 baseline 부재가 green이 되는 경로를 제거할 수 없음.

재개해도 §7 한계와 PR 순서는 유지한다. A10은 대체안과 함께 유지, A16은 폐기하며 H10b 행동 테스트는 필수다.
§6의 남은 실측·활성화 조건은 이 설계 동결을 활성 완료 선언으로 바꾸지 않는다.
