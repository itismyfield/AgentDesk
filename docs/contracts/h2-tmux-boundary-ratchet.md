# H2 — Rust prod 경계 tmux 직접 조작 사이트 래칫

## 1. 범위와 현재 상태

이 계약은 `src/**`의 **root lib 타깃 prod 코드 중 owner 밖** tmux 직접 조작 사이트의 증가를 제한한다.
owner는 `src/services/platform/tmux.rs`, `src/services/platform/tmux/**`,
`src/services/session_host.rs`, `src/services/session_host/**`이며, 별도 owner 명부도 대조한다.
테스트·bin 전용 코드·build script·proc-macro 실행·외부 크레이트 내부·운영 스크립트는 측정 범위 밖이다.

**PR-1c는 inert 계약·진단 단계다. 활성화 완료를 뜻하지 않는다.**
기준 main `cd8fe090acb1c1f5f309a5ca22f7cc7c61a0ec81`에는 `clippy.toml`,
`scripts/ci/h2_admissions.toml`, 두 baseline 파일이 없고 `LIVENESS_FLOOR = 0`이다.
CI의 측정/map 호출에는 `--inert`가 있고 admission CLI는 CI에서 호출하지 않는다.
cfg snapshot B1은 inert가 아니다. 필수 Linux 잡은 baseline 없이도 driver를 빌드하고
`--inert --canary`로 실제 cfg self-test를 실행하며 실패를 rc 1로 전파한다.
정책은 `agentdesk.session.hasLivePane(name)`으로 생존 상태를 조회하며, Rust exec 경계는 `gh/git`만 허용한다.
`tmux_server_pids`와 `check_file_descriptor_headroom`은 비-macOS에서도 빈 결과의 동명 스텁으로 lint 경로를 유지한다.
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
| 레인·공통 env | [h2_env.py](../../scripts/ci/h2_env.py), `sym:h2_env::environment`, `sym:h2_env::check_host` | 모든 모드의 공통 env·wrapper 정리·host triple 및 driver 전용 bootstrap |
| map metadata 봉인 | [h2_cfg_collect.py](../../scripts/ci/h2_cfg_collect.py), `sym:h2_cfg_collect::seal`, `sym:h2_cfg_collect::read_manifest` | Cargo 성공·동일 run·schema·신선도·하한·digest 검증 후 최종 manifest 게시/재검증 |
| cfg 목록 진단 | [h2_cfg_compare.py](../../scripts/ci/h2_cfg_compare.py), `sym:h2_cfg_compare::compare_cfgs` | 구조화된 cfg 원자 집합의 교집합과 양방향 차이 |

[modmap-driver](../../tools/modmap-driver/src/main.rs)는 `RUSTC_WORKSPACE_WRAPPER`로 root lib 컴파일을 식별하고
`after_expansion`에서 [module map](../../tools/modmap-driver/src/modmap.rs)을 쓴 뒤 `Compilation::Stop`한다.
file/inline/include/wrapped 행은 선언·식별자 context, 부모 파일, 중첩, 속성 출처를 보존한다.
드라이버에는 `rustc-dev`가 필요하다. `RUSTC_BOOTSTRAP=1`은 드라이버 빌드에만 사용한다.
map 실행은 wrapper를 비우고, 기존 TSV를 지운 뒤 marker 시각과 비교한다.
canary의 오류 목록이 일치해야 하며 실제 저장소 map은 file module이 1000개 이상이어야 한다.

저장소 루트의 `clippy.toml`은 양 lane 합본이며, 측정·check·admission은 해당 lane과 `both` 항목만
임시 `CLIPPY_CONF_DIR`에 렌더해 실행한다. callee 필터에도 같은 lane 설정을 쓴다.
admission은 평가가 끝날 때까지 임시 설정을 유지하고, runner가 만든 정확한 `clippy.toml` 경로 하나만
이번 실행의 R-O 설정 입력으로 인정한다. 다른 외부 파일·동명 파일·별칭·디렉터리 전체는 면제하지 않는다.
외부 `--json`에는 이 실행 경로의 예외를 부여하지 않는다.
regen은 EXEC/SUBPROC/TYPES와 반대 lane 도출을 보존하고, 이번 lane의 W/SUBPROC_W를 비운 seed에서 시작한다.
수렴 뒤 합본과 해당 lane baseline을 갱신하므로 삭제된 경로나 seed와 무관한 도출 순환은 남지 않는다.
H2 경로는 `[A-Za-z_]\w*(::[A-Za-z_]\w*)+` 형식이며 첫 segment는 `agentdesk/std/core/alloc/tokio` 중 하나다.
lint·target 필터 전에 코드 유무와 무관하게 compiler-message의 어느 span이든 `clippy.toml`이면 실패한다.
문구나 반대 lane 여부로 경고를 무시하지 않는다. 외부 `--json`도 lane 설정에서 생산해야 하며 같은 가드를 받는다.
등록 불가 오류는 호출부 구조 변경을 요구한다. TYPES 추가를 해결책으로 안내하지 않는다.

R-O는 compiler map을 사용하지만 **item 귀속 전체를 compiler def-path로 바꾼 것은 아니다**.
`h2_measure._module_walk/_module_table`은 W 도출, R-E owner pub fn 명부, R-W 증가 검사에 남아 있다.
따라서 compiler map과 walker 대조를 제거하면 이 소비자의 경로가 무검증 상태가 된다.
`SourceFile`의 함수·const/static 귀속 및 바깥 macro call-site 사용 한계는 §7에 남긴다.

이 문서의 `sym:` 참조는 `scripts/check_contract_symbol_refs.py`가 실제 Python 함수 import와 1:1 대조한다.
이것은 이름·문서의 드리프트 검사다. Rust driver의 컴파일 증명이나 H2 활성화 검증을 대신하지 않는다.

## 3. 레인·재현 조건·cfg 목록 비교

측정 레인은 `linux`(`x86_64-unknown-linux-gnu`)와 `macos`(`aarch64-apple-darwin`)다.
`rust-toolchain.toml`은 현재 1.94.1을 지정한다. CI의 `components: clippy`가 Clippy를 설치하고
`h2_measure.sh`가 가용성을 검사한다. macOS hosted 레이블은 `macos-15`다.
`h2_measure.sh`는 host triple을 확인한다. shell과 직접 Python 실행은 같은 공용 환경 정리를 적용한다.
모든 모드에서 `CARGO`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `CARGO_BUILD_RUSTFLAGS`, `CARGO_BUILD_TARGET`,
`RUSTC`, `CARGO_BUILD_RUSTC`, `RUSTC_BOOTSTRAP`, `CLIPPY_ARGS`와 `CARGO_TARGET_*_{RUSTFLAGS,RUNNER,LINKER}`,
`CARGO_PROFILE_*`, `CARGO_UNSTABLE_*`, `CARGO_FEATURE_*`, `CARGO_CFG_*`, `__CARGO*`를 제거한다.
`RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER` 및 두 `CARGO_BUILD_` wrapper는 Cargo config보다 우선하도록 빈 문자열로 둔다.
`CARGO_INCREMENTAL=0`이며 driver 빌드만 `RUSTC_BOOTSTRAP=1`, map 실행만 정리 후 driver wrapper를 주입한다.
shell은 해당 키를 unset하고 wrapper를 빈 문자열로 export한다. baseline 전 inert 종료는 환경 수집보다 먼저다.
root lib/default features가 측정 기준이다. target 디렉터리·Cargo 홈·toolchain 선택은 유지한다.
env 해제만으로 Cargo config, build-script cfg, 실제 rustc argv의 일치가 증명되지는 않는다.
재현 조건을 명시하는 것이며 모든 환경에서 같은 결과를 보장하지 않는다.

cfg 비교 입력은 **원자 경계를 보존해 JSON으로 직렬화한 UTF-8 snapshot**이다.
형식은 비어 있지 않은 배열이며 원자마다 flag는 `["unix"]`, 값이 있으면 `["target_os", "linux"]`다.
이름은 식별자 문자열, 값은 Unicode 문자열이다. Python의 `str.isidentifier()`로 이름을 검사하며
Rust cfg 술어나 소스 문법을 평가하지 않는다. 아래 실행 결속 객체 이외의 객체·숫자·null·고립 surrogate는 거부한다.

rustc 1.94.1의 `--print cfg`는 값을 escape하지 않는다. 값 `a"`+개행+`b="c`인 `foo` 하나와
`foo="a"`, `b="c"` 두 원자는 같은 줄 집합을 출력할 수 있어 원문만으로 복구할 수 없다.
따라서 **원문 출력은 rc 2로 거부**하며 줄별 JSON 감싸기나 원문에서의 자동 변환도 제공하지 않는다.
수집자는 컴파일러가 가진 이름/값 경계가 사라지기 전에 JSON serializer로 snapshot을 만들어야 한다.
예: `[["foo", "a\"\nb=\"c"]]`와 `[["foo", "a"], ["b", "c"]]`는 서로 다르다.
driver의 root non-test lib `after_expansion`은 `MODMAP_CFG_OUT` 설정 시
`tcx.sess.psess.config`를 정렬·중복 제거하여 직렬화한다. flag와 빈 값도 별개 원자다.
cfg 및 `<cfg>.invocation.json`은 각각 임시 파일→rename으로 게시한다. 후자는
`MODMAP_CFG_NONCE`, 수신 argv, crate root를 기록한다. 두 파일의 게시를 하나의 원자적 완료로 보지 않는다.
쓰기가 실패하면 compiler 오류로 Cargo가 실패하며 이미 남은 map/cfg도 성공 증거가 아니다.

canary는 feature, flag/빈 값, 다중 값, escape probe를 실제 compiler에 전달한다.
Python은 독립 기대값과 정확 비교하고 target/codegen 원자의 존재 및 test/windows의 부재를 검사한다.
cfg·invocation을 사전 삭제하고 marker 이후 mtime, 이번 nonce, root/argv를 대조한다.
Cargo event/명시적 cfg argv에 없는 세션 원자를 요구하며 동일/원자 제거 비교도 실행한다.
Cargo JSONL과 stderr는 map 옆에 보존하고 compiler 진단은 CI stderr에도 출력한다.
기존 Linux `H2 module map (linux, inert)` 단계의 root-skip만 inert이며 canary는 필수다.
이 canary는 root 수집물이나 Clippy session 증거를 대신하지 않는다.

`h2_modmap.py --lane linux|macos`는 driver 준비→canary→root expansion 1회를 소유한다.
`--out`은 유일한 root TSV이며 `--cfg-out`/`--meta-out`은 같은 새 run 디렉터리의 선택적 경로다.
기본값은 `target/h2/runs/<run_id>/root/modmap.{tsv,cfg.json,meta.json}`이다.
canary는 `<run_id>/canary/`에 kind=canary로 저장하며 root 증거로 읽을 수 없다.
옵션 없는 B1 경로는 유지된다. B2a는 CI 호출자가 없는 opt-in 기반이다.
B2b는 공통 env와 기존 Linux/Mac map 단계의 `--lane`을 연결하는 **동작 변경, baseline 전 root는 no-op**이다.
두 번째 map 단계는 없다. head의 필수 잡 green·소요 시간 전후 확인이 필요하며 원복 순서는 B2b→B2a→B1이다.
`h2_measure.sh`는 inert 조기 종료 뒤 helper를 사전 검사로 실행하고 고정된 env 명령만 적용한다.
마지막 `exec "${PYTHON:-python3}" scripts/ci/h2_measure.py ...`를 유지해 launcher가 같은 프로세스에서 직접 계측한다.
helper는 `h2_measure.py`를 import하거나 exec하지 않는다.

결속 cfg는 `{schema:1, run_id, nonce, atoms}` 객체다. 비교기는 기존 배열과 이 객체의 atoms를 읽는다.
원자에 가짜 cfg를 추가하지 않는다. cfg bytes 자체에 nonce/run ID를 넣어 옛 cfg의 재게시를 거부한다.
동일 callback의 invocation은 root/argv/env/kind/output과 TSV 원문을 기록해 봉인 전 다른 TSV 접합도 거부한다.
TSV 원문은 별도 hash 의존성을 늘리지 않는 driver 확인값이며 manifest에는 두 파일의 SHA256을 봉인한다.

metadata schema 1은 kind/run ID/nonce/lane/root/host/target, source SHA/tree/dirty·입력 digest,
Cargo.lock/config digest, rustc/Clippy 버전, 실제 argv/env/features/default-features와 build-script events·
생성 입력 digest, 파일 경로/개수·cfg/TSV/driver/Cargo 기록 digest를 포함한다.
helper는 compile/Cargo 호출 없이 run 기록만 검증한다. Cargo rc≠0이면 잔존 파일도 전체 무효다.
마지막 manifest만 임시 파일→원자 rename으로 게시한다. 여러 산출물 rename은 하나의 원자 작업이 아니다.
소비자는 `read_manifest`로 파일 bytes와 기록을 재검증하며 manifest 없는 부분 파일·symlink·다른 run을 거부한다.
이는 신뢰된 로컬 생산자의 일관성 검사이며 모든 기록을 함께 위조하는 쓰기 주체의 인증은 아니다.

map rc는 0=요청 작업 완료, 1=compile/canary/수집·검증·쓰기 실패, 2=CLI 입력 계약 오류,
3=lane host 불일치다. baseline 전 inert 종료는 `root=skipped`이며 root manifest를 만들지 않는다.
B2 소유 산출물은 map run의 metadata manifest다. Clippy JSON과 items의 결속은 같은 호출 세션 manifest가 맡는다.
`.d`·admission 활성과 map 세션/Clippy 세션의 R-O 동등성은 3b-A 소유다.
map session cfg와 Clippy effective cfg의 동등성·admission 활성화를 이 manifest로 주장하지 않는다.

### Canary Clippy 세션 계약

`h2_session.session`은 items/cfg 세션이며 `h2_modmap.py`의 canary 검증에서 실행한다.
`h2_env.environment("measure")`로 정리한 환경에서 Cargo와 버전 질의를 같은 crate cwd로 실행한다.
`ALLOWED`는 rustc release/commit, Cargo, sysroot의 Clippy 경로/버전/연결 compiler, driver compiler를 검사한다.
허용 조합과 host가 맞아야 metadata를 읽고 request를 만든다. metadata의 canonical manifest로 package를 고르고,
non-proc-macro lib 하나의 canonical 경로·package ID·crate 이름·종류를 고정하며 그 lib만 touch한다.
Cargo는 그 package ID를 명시적으로 선택한다. extra는 features/jobs/target/target-dir과 실행 제어 옵션만 받는다.
crate·상위·CARGO_HOME의 config/config.toml에서 예약된 session/toolchain [env] 키와 compiler 교체를 거부한다.
config include와 --config·package/manifest 선택 변경도 거부하며, Cargo 설정 bytes는 실행 전후 같아야 한다.
생산자는 기대 manifest/package/lib가 맞는 non-test lib 호출뿐이다. 정보 질의·bin·proc-macro와 다른 member는 위임한다.
생산자 후보의 @응답 파일 인수는 해석하지 않고 claim 전에 거부하며, 봉인에서도 같은 인수를 거부한다.
실제 Clippy canonical 경로는 환경변수가 아닌 request의 승인 경로와 claim 전에 대조한다.
request는 승인한 CLIPPY_ARGS/conf/width와 MODMAP 출력·nonce·run ID·기대 unit 환경값을 protected_env로 고정한다.
생산자는 claim·자식 전에 그 값들을 byte 대조하고 proof에 기록하며, 봉인에서도 request와 일치해야 한다.
build.rs가 준 값도 예외가 없다. target links 설정의 예약 rustc-env는 Cargo 전에 거부하는 보조 가드다.
생산자는 자식 실행 전에 `create_new` claim을 쓰고 sync한다. 실패해도 claim은 지우지 않으며 새 run 디렉터리로 재시도한다.
동일 요청 lib을 한 Cargo 호출에서 두 번 컴파일하는 구성은 정상 코드여도 fail-closed로 거부한다.
자식은 Cargo의 argv/env를 상속하고 `--cfg clippy`와 승인된 측정 정책 `--cap-lints warn`을 더한다. after_expansion에서 items와 cfg를 쓰고 중단한다.
자식 stdout/stderr는 별도 파일로 격리한다. root 전용 Clippy cfg 질의와 byte 일치 후 실제 Clippy로 exec한다.
proof `h2-session/2`는 unit/pid/nonce/run_id/argv/env_sha256/cfg/driver_rustc, 실제 Clippy 경로·버전·연결 compiler,
items_sha256/items_records를 필수 기록한다. `1-cfg`, items 없는 proof와 헤더 없는 옛 JSONL은 거부한다.
Clippy identity는 승인 경로에서 직접 질의하고 request와 대조한다. 봉인에서도 재대조하며 cfg target은 요청 lane과 같아야 한다.
봉인 전 claim의 pid/unit, request의 공통 unit 필드, 요청 lib의 non-test artifact 1개와 package ID/fresh:false를 대조한다.
proof·cfg의 결속, 파일 시각·partial·source/config 불변도 검사하며 SHA-256을 기존 JSON 원자 게시 helper로 봉인한다.
manifest/request/items 헤더 kind는 `canary-items`다. root map 소비자는 이를 거부한다. 매핑 라이브러리 `h2_items`는 이 kind만 받으며 호출자는 아직 없다.
items 경로는 session.json과 같은 run의 items.jsonl로 고정하며 추가 환경 경로를 받지 않는다.
첫 줄은 `{schema:1, run_id, nonce, kind, root, crate, cfg_clippy:true}`이며 나머지는 JSON 배열 레코드다.
순서는 `[file, lo, hi, kind, path|null, reason|null, display, line, def, parent, def_kind, macro]`다.
nested_fn만 끝에 `[fold, escape]` 두 필드를 추가한다. 옛 객체 레코드와 길이가 다른 배열은 받지 않는다.
display는 진단용 DefPath 문자열이고 등록은 지역 정의의 module/trait/Self DefId에서만 계산한다.
좌표는 `source_callsite`의 `original_relative_byte_pos`로 원문 BOM/CRLF를 보존한다.
fn 밖 AnonConst/InlineConst도 const 소유자로 기록한다. 합성 헤더만 생략하고 실행 소유자의 파일/좌표 오류는 실패한다.
header 중복은 (file, lo, hi, def_kind)로 줄이되 impl/trait은 def/parent 결속을 위해 개별로 유지한다.
nested_fn의 fold는 첫 JSONL 메타데이터 행만 제외한 0-based 번호(header kind 포함)다. escape는 접는 fn 조상의 DefId 자손 impl 존재 여부다.
탈출은 fold-escape, 외부 trait은 external:<crate>, fn 안 정의는 in-function, 비 ADT Self는 self-not-adt로 기록한다.
콜백은 헤더 포함 정확한 bytes와 레코드 수를 계산해 items.jsonl.sha256의 `{sha256, records}`에 기록한다.
각 파일은 partial→rename으로 게시한다. 부모는 자식 종료 후 파일을 재계산하여 sidecar와 비교한 뒤 proof를 게시한다.
runner도 파일=sidecar=proof의 digest/양수 레코드 수, 헤더·mtime·partial을 검사하고 두 items 파일을 manifest에 봉인한다.
봉인 전에 레코드 필드 타입(bool은 정수에서 제외), kind·path/reason 택일, 고유 def ID, 연관 item/container 및 fold 범위·대상·순환을 검사한다.
parent는 정수(root=0)이며 모듈·closure 등 미출력 부모는 허용한다. 출력된 중간 부모만 따라가며 display/이름으로 관계를 복원하지 않는다.
봉인 전 body 접합, 0개 레코드와 빈 digest는 실패한다. 모든 증거를 함께 다시 쓰는 주체의 인증은 보장하지 않는다.
map 모드의 TSV/cfg와 argv는 그대로이며 items 생산은 세션 자식만 수행한다.

**입력 source fence.** items 자식과 실제 Clippy는 별도 compile이다. 그래서 compile 사이에 원문을 A→B→A로 바꿨다 되돌리는 쓰기는 전후 bytes 비교만으로는 보이지 않는다.
fence는 이 파일 변경형 쓰기를 쓴 주체(편집기·동시 프로세스·요청 lib 전개 중 proc-macro)와 무관하게 거부한다.
- **capture 결속.** capture는 목록·lib·clippy.toml 파일마다 stat→open→fstat→read→fstat→stat을 한다. 여섯 번 모두 `(dev, ino, size, mtime_ns, ctime_ns)`가 같고 읽은 길이도 같을 때만 bytes를 채택한다. 다르면 재시도 없이 실패한다.
- **clock probe.** runner는 lib touch 뒤, capture 전에 run 디렉터리의 목록 밖 임시 파일을 되풀이해 쓴다. ctime이 두 번 전진해야 하며 마지막 값을 P로 둔다.
  - 전진하지 않거나(예산 5초), 되돌아가거나, 관측 값이 모두 초 배수이거나, 최소 간격이 1초 이상이면 fail-closed다. 관측 간격과 같은 tick 재쓰기 수는 request와 stderr에 남긴다.
- **fence 시작 조건.** capture한 모든 파일은 probe와 같은 st_dev에 있고 `ctime < P`여야 한다. 그러면 그 뒤의 쓰기, 곧 같은 내용 복원·mtime 복원·hard link 경유 쓰기는 반드시 ctime을 바꾼다. 이는 tick 크기와 무관하다. rename 교체는 inode를 바꾼다.
- **경로 이름공간.** capture 파일마다 workspace root(포함)부터 부모까지의 각 디렉터리를 lstat로 `(dev, ino, mtime_ns, ctime_ns)` 봉인한다. root 밖 파일(clippy.toml 등)은 부모 디렉터리만 봉인한다.
  - 디렉터리도 probe와 같은 st_dev·`ctime < P`여야 한다. 그래서 조상 디렉터리를 rename으로 B 트리와 바꿨다 되돌리면 그 부모의 mtime·ctime이 남는다. leaf나 성분이 심볼릭 링크면 거부한다(링크 객체 봉인 대신 거부: 링크 대상 경로의 디렉터리는 봉인 목록 밖이다). leaf는 capture한 inode와 같은 일반 파일이어야 한다.
  - 정상 세션은 봉인 디렉터리에 항목을 만들지 않는다. run 디렉터리는 probe 전에 만들고, probe 파일·출력은 run 안에 쓴다. Cargo target 디렉터리(`--target-dir`, 없으면 metadata `target_directory`)는 probe 전에 미리 만든다. root 위 조상은 봉인하지 않는다. `/tmp`·runner 작업 디렉터리처럼 다른 작업이 항목을 바꾸는 공유 디렉터리라 정상 CI를 거부하게 된다. 남는 반례: root의 엄격한 조상을 rename으로 바꿨다 되돌리는 교체(root inode 자체는 그대로).
- **공유 매핑 가드.** 이미 dirty한 쓰기 가능 `MAP_SHARED` 매핑을 통한 저장은 ctime을 바꾸지 않을 수 있다. 그래서 capture·봉인 직후 Cargo 전에, capture `(dev, ino)`나 경로를 매핑한 프로세스를 찾으면 세션을 거부한다.
  - Linux: `/proc/<pid>/maps`의 공유(`s`) 매핑 전부. mprotect로 이미 dirty한 page를 fault 없이 다시 쓰기 가능하게 할 수 있어 현재 쓰기 권한과 무관하게 본다. maps는 `/proc/<pid>/task/<tid>/maps`로 모든 task에서 읽고, task 목록은 새 tid가 없을 때까지 다시 읽는다. `pthread_exit`로 끝난 leader는 maps가 비어도 worker가 주소 공간을 가지기 때문이다. 빈 maps는 커널 스레드(`PF_KTHREAD`)이거나 남은 task가 모두 Z/X일 때만 검사로 센다. task 목록을 읽지 못하거나 비었거나 살아 있는 task의 maps가 비어 있으면(3회 재시도) root가 아닌 프로세스는 거부한다. macOS(task VM을 프로세스가 공유하므로 pid 단위): libproc `PROC_PIDREGIONPATHINFO`에서 최대 보호가 쓰기인 영역(private COW 매핑도 보수적으로 포함).
  - maps를 읽지 못한 프로세스: uid(실제·유효·저장·파일시스템)에 0이 있으면 root 등가로 신뢰 경계 밖이라 센다. 아니면 같은 uid이거나, capture 파일의 소유자이거나, 파일에 group/other 쓰기 비트가 있으면 거부한다. 그 외 타 UID는 파일을 쓰기로 열 수 없으므로 센다. 남는 반례: 판정은 현재 소유자·mode 기준이라, 파일이 예전에 쓰기 가능했을 때 타 UID가 만든 기존 쓰기 매핑은 탐지하지 않는다. root가 아닌 타 UID 시스템 데몬도 root처럼 신뢰 경계 밖이다(협조적 CI 입력 전제). uid 네 값이 모두 스캐너 uid이고 cgroup이 `0::/user.slice/user-<uid>.slice/user@<uid>.service/init.scope`인 프로세스(정상 systemd·GitHub runner 구성에서 그 uid의 user manager와 `(sd-pam)`이 놓이는 scope이며, 위임 때문에 보안상 독점은 아니다)는 maps를 읽을 수 없어도 러너 인프라로 세고 pid·Name을 `fence.mappings.user_managers`와 stderr에 남긴다. 다른 미판독 프로세스의 거부 메시지에는 pid·Name·PPid·cgroup을 남긴다. 같은 uid가 위임된 cgroup으로 그 scope에 들어가 위장하는 경우는 협조적 CI 전제의 잔여다.
  - 목록을 다시 읽어 새 pid가 없을 때까지(최대 10회) 검사하고, 끝나지 않으면 거부한다. pid 재열거와 한 번의 task 열거 pass 안에서는 새 pid·tid만 읽고 이미 읽은 maps를 다시 읽지 않는다. 판정 불가 재시도는 그 pid의 task 전체를 처음부터 다시 읽는다. 어느 쪽도 원자적 목록이 아니다. 결과(`status=checked`, 검사·root·타 UID 수)는 request `fence.mappings`와 stderr에 남는다. 그 밖의 플랫폼은 `unavailable`이고 생산자·소비자 모두 거부한다.
  - 남는 반례: 가드 뒤 매핑(Linux는 그 pid의 마지막 성공 pass에서 쓴 `/proc/<pid>/task/<tid>/maps` 스냅샷 이후, macOS는 그 pid의 영역 조회 이후 생겨 다시 관측되지 않은 매핑, 가드 실행 중 포함)은 첫 쓰기 fault에서 ctime이 갱신된다는 전제에 기댄다. write-notify가 없는 tmpfs(shmem)와, msync 전까지 시각 갱신을 미룰 수 있는 macOS는 이 전제가 보장되지 않는다. 또 `hidepid`·다른 PID namespace처럼 보이지 않는 프로세스, ACL·capability로 쓰기 권한을 얻은 타 UID, maps 경로·장치가 실제 inode와 다른 overlay 매핑은 가드가 보지 못한다.
- **fence 종료.** request에 봉인한 stat 목록을 Cargo 종료 뒤 다시 stat한다. 봉인 디렉터리도 다시 lstat한다. 한 필드라도 다르거나 파일이 사라지면 manifest 없이 거부한다. 목록 추가·삭제는 기존 source state 비교가 거부한다.
- **봉인과 schema.** 통과한 manifest만 `fence` 표지(목록 digest·관측 간격)를 가진다. schema 이름은 `h2-session/2`로 유지한다(proof 생산자 불변). 대신 fence가 없는 옛 request는 소비자가 거부한다.

신뢰 경계 밖인 것:
- ctime·시계를 조작하는 root 또는 시계 권한 주체, 시계를 뒤로 되돌리는 step
- 목록 밖 파일 읽기. 목록 밖 파일에 매핑되는 site는 매핑하지 않는다. 그러나 proc-macro의 목록 밖 읽기(OUT_DIR·include 대상 등)를 모두 탐지하지는 않는다. 그 읽기가 목록 파일 site의 token을 바꾸면 fence는 보지 못한다.
- 파일시스템 일관성. 같은 st_dev와 probe 통과는 일관성 증명이 아니다. overlay는 층을 하나의 장치로 보일 수 있고, NFS는 속성 캐시로 변경을 늦게 보일 수 있으며, FUSE는 구현에 달렸다. 지원 범위는 로컬 일관 파일시스템(ext4·xfs·btrfs·APFS)이다. 파일시스템 종류를 확인하는 가드는 없다.
- 원문을 쓰지 않고 invocation마다 다른 token을 내는 proc-macro. 이것은 H20 잔여다.
도구 버전 갱신은 rust-toolchain.toml·CI·ALLOWED를 함께 바꾸고 cargo-clippy spy, cfg 자체 점검,
cold/warm 진단 byte 일치, items 동일성, workspace 생산자/claim 시험 증거를 다시 제시한다.

baseline이 없어도 `--inert --canary`는 map canary 뒤에 Clippy 세션을 실행하고, 이어서 빌드한 driver로
`tests.test_h2_session_driver`와 실제 workspace e2e `tests.test_h2_session_e2e`를 실행한다.
CI 단계는 두 모듈을 `--suite`로 명시하며 `--canary`는 둘 중 하나라도 빠지거나 겹치면 rc 2로 거부한다.
각 모듈은 최소 시험 수 이상 실행되고 skip 없이 `OK`여야 한다.
세션 target은 매 실행 UUID run 아래 새 `session-target`이며 기존 디렉터리가 있으면 거부한다.
공유 target 삭제 없이 cold build.rs 실행을 보장하고 proof cfg에 `clippy`와 `h2_items_bs_clippy`를 요구한다.
workspace fixture는 helper lib·proc-macro·독립 member를 포함하며 root와 다른 member의 소스 앞부분을 공유한다.
두 의존성 대조군은 cold/warm `compiler-message` JSONL을 실제 `cargo clippy`와 byte 비교한다.
items 자식의 HIR 질의는 early lint(`unused_imports` 등)를 발생시키며 자식 로그로 격리한다.
fixture proc-macro가 확장 중 stderr에 쓰는 rustc 형식 진단 1줄은 자식 로그에만 있고 JSONL에는 한 번만 나와야 한다.
`--package` 고정 아래 `-j 2` member 의존성 위임, `--workspace` 거부, 다른 member 직접 진입의 위임·쓰기 0,
`cdylib+rlib --all-targets`, 선점 claim의 쓰기 0, 같은 unit 동시 진입을 검사한다.
e2e는 먼저 빌드한 release driver를 사용한다. 실패, 기대보다 적은 시험 수, skip은 canary 실패다. 증거는 `target/h2/session-e2e`에 남긴다.
CANARY_ITEMS는 (kind, path|reason, file, anchor)이며 원문 regex로 기대 [lo,hi)를 독립 계산한다.
canary는 봉인 후 items를 한 번 읽고 manifest/proof digest를 대조한 동일 bytes를 파싱한다. 해시 후 경로를 다시 열지 않는다.
모든 canary 레코드의 원문 좌표·선언/매크로 호출 범위와 fold·매크로 impl parent를 확인한다.
BOM/CRLF fixture는 -text이며 원문 byte 보존부터 검사한다. 누락·중복·0개·좌표 드리프트는 canary 실패다.
필수 Linux canary 동작이 바뀌므로 해당 head의 Linux green이 머지 조건이다. 매핑/도출 전환은 후속 단계다.

### Items 매핑 라이브러리 (비활성)
`scripts/ci/h2_items.py`는 CI·measure 경로에 연결하지 않은 라이브러리다. 도출 전환과 R-W 소비는 후속 단계가 맡는다.
load는 봉인 manifest(schema 1, kind `canary-items`)와 request/proof `h2-session/2`만 받는다.
기대 crate manifest와 request unit, proof unit의 공통 필드·root·`test:false`, nonce·run ID를 대조한다.
request/proof/items/sidecar/clippy.jsonl은 한 번 읽어 봉인 digest와 대조한 같은 bytes만 파싱하고 다시 열지 않는다.
items 구조는 runner와 같은 `validate_items`/`item_records`로 검사한다. 0개 레코드·헤더 없는 옛 JSONL·객체 레코드는 실패다.
request의 fence stat 목록은 소비자 capture의 파일 집합과 정확히 같아야 한다. 각 size는 capture bytes 길이와 같아야 한다. 봉인 디렉터리 목록은 그 파일들의 조회 경로와 정확히 같아야 하고, `fence.mappings.status`는 `checked`여야 한다. 또한 probe 증거·같은 st_dev·`ctime < P`를 다시 검사한다. manifest의 `fence` 표지도 필요하다. 없거나 다르면 `unsealed`다.
원문은 source state를 계산하는 한 번의 git 목록·읽기에서만 얻는다. 그 digest가 request와 같아야 하고, 같은 bytes만 매핑에 쓴다.
모든 레코드에 `hi ≤ 길이`와 lo의 원문 행 = compiler 행을 요구한다. 진단 site는 expansion을 끝까지 따라간 호출 위치다.
lib artifact는 요청 package에서 lib kind이고 src_path가 요청 lib로 resolve되는 artifact다(session 봉인과 매핑이 같은 선택을 쓴다). 경로가 같은 build script·bin artifact는 후보가 아니다.
진단은 요청 package이고 target(kind·name·crate_types·src_path 문자열)이 유일한 lib artifact의 target과 같은 것만 쓴다.
경로 resolve는 문자열마다 한 번이고, 진단 귀속은 그 target과의 구조 비교라 파일시스템 재해석에 의존하지 않는다. resolve 실패(symlink loop 포함)는 `unsealed`다.
envelope가 없거나 lib 컴파일(artifact)이 하나가 아니거나(`--all-targets`의 test 컴파일 등), artifact target이 요청 unit과 다르거나,
같은 package의 lib kind target이 그 target과 다르면 `provenance`다. 다른 package와 lib kind가 아닌 build script·bin 진단은 경로가 같아도 `foreign` 수로만 노출한다.
레코드 file은 package root 기준이다. 진단 file_name은 절대 경로이거나 request에 기록한 Cargo workspace root 기준이다.
workspace root 기준은 proof argv의 lib 입력이 그 root 기준 상대 경로일 때만 인정하며, 아니면 상대 file_name은 `no-item`이다.
site는 byte_start 행 = line_start, byte_end ≤ 길이여야 한다. 정규화 오프셋은 CRLF/BOM에서 `coord`로 실패한다.
site를 포함하는 가장 좁은 레코드를 고르며 같은 폭 다른 범위는 `ambiguous:overlap`이다.
동률은 모든 실행 소유자(fn류·const·anon-const)를 남긴다. header는 소유자의 DefId parent로 증명된 container이거나
비실행 def_kind일 때만 버리고, 그 밖의 header는 `ambiguous:unproven-header`다.
실행 소유자가 둘 이상이면 `ambiguous`, 하나면 그 path 또는 등록 불가 사유, 없으면 module-level이다.
모든 실패는 reason을 가진 `MappingError`다. 봉인·source 불일치·git capture 실패와 옛 request는 `unsealed`, 잘못된 span·spans 목록은 `coord`다.
빈 spans·primary span 없음과 목록 안 레코드 없음은 `no-item`, 목록 밖 파일은 `unsealed`다. 통과나 0건이 되지 않는다.
`canary-items`는 현재 세션의 유일한 kind이므로 root 결속은 kind가 아닌 기대 crate unit 대조로 한다.

두 호스트의 결과를 모은 뒤 실행한다:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/ci/h2_cfg_compare.py \
  --linux linux.cfg.json --macos macos.cfg.json
```

stdout은 `common`, `linux_only`, `macos_only` 세 정렬 배열을 가진 JSON이다.
출력의 각 원자도 `[이름]` 또는 `[이름, 값]`이다. 배열 순서·중복 원자는 무시한다.
같은 key의 여러 값(`target_feature`, `target_has_atomic`, `feature` 등)은 각각 보존한다.
flag `["key"]`와 빈 값 `["key", ""]`는 다르다. JSON escape만 decode하며 값의 공백·개행·따옴표·
역슬래시를 정규화하거나 Rust escape로 재평가하지 않는다. 출력은 ASCII JSON escape를 사용한다.
cfg 술어, `cfg_attr`, 소스 파일·모듈 도달성을 해석하지 않는다.

| rc | 뜻 | 처리 |
|---|---|---|
| 0 | 두 구조화 원자 집합이 동일 | 이름/값 동등성만 확인; 증거의 신선도·완전성·target triple 증명은 아님 |
| 1 | 하나 이상의 차이 | 양쪽 전용 원자를 진단; OS/architecture 차이도 숨기지 않음 |
| 2 | 필수 인자 누락, 파일 읽기/UTF-8/JSON/schema 오류, 빈 목록 | stderr에 레인·파일(문법 오류는 줄, schema 오류는 원자 번호); 비교 JSON을 내지 않음 |

Linux/macOS의 정상 target cfg는 다르므로 rc 1은 예상 가능한 진단이다.
이를 곧바로 admission 위반으로 취급하거나 allowlist로 지우지 않는다.
비교 도구 자체는 컴파일러를 실행하지 않고 입력 파일·baseline도 수정하지 않는다.
CI는 비교기 행동 테스트와 실제 driver canary를 실행하며 H2 admission을 활성화하지 않는다.

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
| H7 | Clippy lint 이름 변경·삭제, compiler 속성 전개 규칙 변경 | 1.94.1 고정, 업그레이드 시 실제 진단·liveness 및 H15 소형 compile fixture 재검증 |
| H8 | impl의 익명 const/static/closure enclosing_item 이름 충돌 | span 시작 줄 보조 필드, 충돌 admission에 lines 요구 |
| H9 | Windows 전용 항목은 양 레인 미측정 | src/runtime_layout/windows_links.rs::create_directory_junction 미등록, 해당 파일 tmux 문자열 금지 |
| H10a / H10b | a: hasLivePane의 서버 재시작 직후 stale socket; b: binary_resolver 캐시의 최초 호출 의존 | a: PR-2 호출측 재시도; b: 격리 HOME/PATH 가짜 tmux 또는 owner 내부 runner 주입의 있음/없음/오류 행동 테스트 필수 |
| H11 | W와 SUBPROC_W churn으로 admission 부담 | one-shot suffix, 분기별 재기준 PR; r9 규모 추정은 현재 실측값이 아님 |
| H12 | DISPATCHERS 밖 make/just/사용자 래퍼 리터럴의 동적 인자 | 상수 목록 변경+regen+admission, 리뷰 |
| H13 | 설계 어휘 census와 Clippy JSON 사이 오차 | 규모 추정은 실제 양 레인 측정값으로 교체 |
| H14 | macOS hosted 레이블 변경으로 측정 중단 | 레이블 갱신과 실제 host 단언; 현재 inert no-op은 host를 검사하지 않음 |
| H15 | 지원 밖 속성·잘못 닫힌 속성의 macro는 보수적으로 거부 | [owner_shape_problems][shape]는 doc 및 중첩 cfg_attr의 속성 자리에서 doc RHS만 item macro 검사에서 제외; 술어·임의 속성 내부 doc는 제외하지 않음 |
| H16 | macro fn의 module 귀속·`<module>` R-W 면제, raw identifier·const/static 내부 item 경로의 선재 한계 | [SourceFile:66/measure:309][items], [rw_problem:274][rw]; 파일 map은 R-W 전체 증명이 아님, 실제 baseline 영향 검증 |
| H17 | 정상 항등 macro 모듈·hand-written item 재배치도 fail-closed 거부 | [modmap_problems:124][modmap]; 출처 보존 규칙의 보수적 오탐, 컴파일 가능한 모든 Rust 문법 지원 약속 없음 |
| H18 | hardlink·대소문자 alias의 파일 동치가 realpath만으로 증명되지 않음 | [classify:92/walker_problems:174][aliases]; duplicate_mod와 함께 실측 필요, 미측정을 해결로 세지 않음 |
| H19 | R-O map 패스와 Clippy 패스가 같은 source/target/cfg였는지 입증하는 결속 부재 | [map_modules:57][collector], [root_lib_depinfo:45][depinfo]; items와 Clippy JSON은 같은 호출 세션 manifest로 결속(매핑 비활성), map TSV·`.d` 결속은 3b-A 증거 필요, cfg 목록 일치만으로 대체 불가 |
| H20 | items 자식과 Clippy가 두 compile이라 원문 불변·전개 차이(비결정 proc-macro)는 items와 진단이 다른 token에서 나올 수 있음 | 파일 변경형 A→B→A는 입력 source fence로 거부; H2는 적대 방어가 아닌 코드 건강 래칫이며 보안 요구로 올리면 계측 Clippy 단일 compile(설계 후보 ③) |

H15의 RHS 제외는 compiler 수용 조건에 의존한다. 지원하는 key-value 속성의 값은 macro 전개 후 literal이어야 하며,
정상 doc 값은 문자열이다. item·block·non-literal 전개로 item 선언 권한을 얻을 수 없고, shape helper가 Rust 의미론을 검증하지는 않는다.
macro 이름으로 승인하지 않으며 macro_rules!·#[path] 검사와 R-E pub inventory는 원문 code view를 유지한다.
include_str!와 문자열 literal을 읽는 include!의 shape 면제는 데이터 승인이 아니다. 비rs 입력은 기존 R-O 데이터 allowlist,
.rs 입력은 실제 module 의무를 계속 적용한다. Rust 1.94.1 소형 compile fixture는 로컬 1회 로그로 검증하며 CI 단계로 추가하지 않는다.

H15~H19는 r9 §8-3에 따라 추가했다. 근거는 기준 main 코드와
`design-pr3b-v1.md`의 「이전 DESIGN_BLOCKED와의 대조」, 「우회 경로 전수표」(P3-E/I/F/J·V5)다.
후자는 코디네이터 레인 기록이며 H16~H19 링크는 같은 기준 SHA의 저장소 근거를 고정한다. H15는 현재 수리 구현을 가리킨다.

[shape]: ../../scripts/ci/h2_admission.py
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
