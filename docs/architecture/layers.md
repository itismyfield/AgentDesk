# 모듈 층 측정과 의존 ratchet

층의 정본은 [`layers.toml`](../../scripts/audit_maintainability/layers.toml)이다.
디렉터리 이름 대신 역할과 소스 근거를 검토한 논리 모듈 prefix를 사용한다.
일치하는 가장 긴 namespace 경계를 선택하며, 같은 경계에서 분류가 충돌하면
미분류다. 빈 prefix는 해당 crate root 하나만 가리킨다.

| 층 | 책임 | 대표 근거 |
|---|---|---|
| vocabulary (0) | 순수 값·정적 계약 | `queue_contract`, `phase_gate`, Discord `bot_role`·`restart_mode`·`session_identity` |
| foundation (1) | 순수 계산·경로·문자열 구현 | `runtime_layout::paths`, `cli::restart_terminal_proof`, 검토된 `utils` leaf |
| adapter (2) | 저장·provider·process/platform 실행 | `db`, `services::provider`, `services::platform` |
| application (3) | 업무 정책·상태 전이 orchestration | `engine`, `kanban`, `dispatch`, `services::auto_queue` |
| discord (4) | Discord 입력·출력·세션 제어면 | `services::discord`, TUI O writer/repost |
| assembly (5) | HTTP handler·최종 조립 | `app_state`, `server`, `cli`, HTTP 성격의 `auto_queue::route` |

같거나 낮은 rank로의 의존은 이 층 검사에서 허용한다. 낮은 rank에서 높은
rank로 가는 정본 edge는 역방향 의존으로 기록한다. 미분류 endpoint의 edge는
판정 불가로 따로 센다. 미해석 참조·cfg·읽기/모듈 오류가 남아 있는 경계를
역의존 0 또는 추출 가능으로 선언하지 않는다.

`services` 전체를 한 층으로 묶지 않는다. `provider`는 실행 adapter이고
HTTP extractor·AppState·응답 변환을 소유한 `auto_queue::route`는 assembly다.
그 아래 `fsm`과 업무 command는 application으로 다시 구분한다.
이름이 `pipeline_routes`여도 HTTP extractor 없이 저장·검증·locking 업무를
수행하므로 application이다. TUI O repost 아래 `#[path]`로 연결된 DB repository는
논리 위치와 무관하게 adapter다. 설정 DTO·live loader가 혼재한 `config`,
파일 동작과 Discord 운영 hook이 혼재한 `runtime_store` 등은 미분류로 남긴다.
`services::discord::restart_mode`의 enum·정적 변환은 vocabulary이고,
`cli::restart_terminal_proof`의 nonce·경로·파일 증거 판정은 foundation이다.
두 leaf는 부모의 Discord/최종 조립 책임보다 긴 prefix 규칙으로 분류한다.
모든 규칙의 이유와 `file:line` 근거는 TOML에 있으며, 미분류 모듈의 전체 목록은
감사 JSON의 `layer_analysis`에서 확인한다. 이 표를 맞추기 위한 Rust 코드 이동은 하지 않았다.

## 실행과 출력 계약

저장소 root에서 실행한다.

```sh
python3 scripts/audit_maintainability.py --format json
python3 scripts/audit_maintainability.py --check
```

`module_layers`는 baseline에 없는 부채와 기존 부채의 증가를 `error`로 보고한다.
`--check`는 이 error가 있으면 실패한다. 다른 감사 검사의 severity는 바꾸지 않는다.
`checks.module_layers.findings[0].extra.measurement`는 0A의 node·edge·coverage 계약을
그대로 유지한다. `extra.layer_analysis`가 별도의 분류·층별 edge 집계·역방향 의존·
판정 불가·SCC·분류 coverage를 제공한다. 0A coverage의 `classification`은 소스
분석기의 기존 값이고, 층 분류 현황은 `layer_analysis`에서 확인한다.
두 결과가 모두 불완전성을 표시하며 미해석 참조를 외부 잡음이라는 이유만으로
제거하지 않는다. `--check`의 성공은 부채가 늘지 않았다는 뜻이며, 판정 불가가
남은 경계의 의존이 모두 해석됐다는 뜻은 아니다.

## 차단 규칙과 실패 해석

| 현재 측정과 baseline 비교 | 판정 | 확인할 내용 |
|---|---|---|
| 새 역방향 ID 또는 기존 ID의 발생 수 증가 | error | 보고된 source·target·kind와 참조 증거를 확인하고 증가를 제거한다. |
| 새 판정 불가 ID 또는 기존 ID의 발생 수 증가 | error | 분류·해석 실패를 확인한다. 기존 known 부채가 판정 불가로 옮겨 가도 감소로 인정하지 않는다. |
| 미분류 모듈 수·판정 불가 edge/발생 수 증가 | error | 미분류 endpoint와 변경된 층 규칙을 확인한다. |
| 새 uncertainty ID 또는 기존 ID의 발생 수 증가 | error | 미해석·cfg·읽기·모듈 오류의 증거를 확인한다. |
| baseline·이동 mapping의 누락/잘못된 형식, 측정 예외·설정 오류 | error | 입력과 측정 실패를 고친다. 현재값으로 baseline을 다시 만들어 숨기지 않는다. |
| 기존 ID의 발생 수 감소 또는 해소 | info | 감소를 검토한 뒤 아래 명령으로 정본에 반영한다. |
| baseline 안의 기존 부채 유지 | info | 기존 판정 불가와 coverage의 불완전성은 그대로 남는다. |

모든 ID는 독립적으로 비교한다. 한 ID가 줄어든 양으로 다른 ID의 신규·증가를
상쇄하지 않는다. 해소된 ID는 갱신 시 `zero_rules`에 0으로 고정하며, 다시 나타나면
error다. 단순 합계가 같아도 판정 불가 edge의 대상이 바뀌면 새 ID로 차단한다.

## baseline 생성과 식별자

정본 경로는 [`baselines/generated/module_layers.json`](../../scripts/audit_maintainability/baselines/generated/module_layers.json)이다.
초기 측정 소스 SHA는 `0aeb8fc5996b790b3dabc1f788ea9a6982dcde1b`이고,
schema 3 확장 입력 SHA는 `6d7c1851e5a127d153b0193bab54c0a32e7c391e`다.
생성 명령과 입력 SHA를 생성 JSON에도 기록한다. 소스 입력은 `src/**`와 `Cargo.toml`이며
생성기는 지정 commit과 추적 소스가 일치하는지, 버전 관리 밖의 소스가 없는지 확인한다.
층 설정의 의미 기반 SHA256도 기록한다. 같은 입력 SHA·소스·매핑에서 재생성한
바이트는 같아야 한다. 다른 checkout 경로와 선언/실행 순서는 식별자를 바꾸지 않는다.

0A가 제공하는 source 단위는 **논리 모듈**이다. baseline은 source item을 추정하지
않고 `source_grain="module"`을 명시한다. edge 식별자는 source module → canonical
target module → reference kind의 SHA256이다.
불확실성은 source·path·reason과 category를 식별자로 삼아 같은 행을 묶고 발생 수를
보존한다. 줄 번호와 worktree 경로는 식별자에서 제외하고, 상대 `file:line`은 증거로만 둔다.

edge의 `count`는 같은 module→target→kind로 해석된 참조의 **발생 수**다.
grouped import는 leaf마다 세고 같은 줄의 호출도 각각 센다. 줄바꿈만 바꾸면
발생 수는 바뀌지 않는다. 0A의 node·edge·SCC·coverage는 그대로 두고, site 집합으로
줄이기 전의 발생 수를 별도 projection으로 제공한다. uncertainty의 `count`는
0A의 원시 행 발생 수다. 서로 다른 `file:line` site 수는 진단 증거이며 부채 count가 아니다.

schema 3 정본은 역방향 부채·판정 불가 edge·모든 불확실성의 versioned ID와
발생 수, 미분류·판정 불가 수, 해소된 ID의 zero-rule을 보존한다.
`indeterminate_dependencies`도 `id/source/target/kind/count` record로 남겨
총수가 같은 대상 변경을 비교할 수 있게 한다.
`unclassified_modules`는 미분류 모듈 수이고, `indeterminate_edges`는 미분류 endpoint로
판정할 수 없는 정본 edge 수, `indeterminate_occurrences`는 그 참조 발생 수다.
edge count의 단위는 `count_unit_version=1`과 `edge_count_unit`에 기록한다.
미분류 또는 known→unknown 전환을 단순 부채 감소로 세지 않는다.
정렬된 record 하나를 한 줄에 써서 부채 하나의 변경이 그 줄에 드러난다.
SCC 구성원·전체 sites·미분류 목록 같은 진단 snapshot은 감사 JSON이나 `target/`
산출물로 생성하며 정본에 커밋하지 않는다.

임의 손편집과 현재값으로의 전면 덮어쓰기는 허용하지 않는다. 일반 생성 명령은 기존 파일을
바이트로 비교하며, 같은 바이트의 재생성만 허용하고 다른 내용은 거부한다.
줄끝이 CRLF로 바뀐 파일도 동일 바이트로 인정하지 않는다. 새 파일은 exclusive-create로 쓴다.
소스/매핑 변경 뒤 초기 파일을 지워 다시 만드는 방식으로 부채를 지우지 않는다.

## 감소만 반영하는 갱신

소스를 commit한 뒤 해당 SHA로 실행한다. 아래 SHA는 현재 확장 기준점이며,
이후 감소를 반영할 때는 감소가 들어간 commit의 SHA를 넣는다.

```sh
python3 scripts/audit_maintainability/checks/module_layer_baseline.py \
  --input-sha 6d7c1851e5a127d153b0193bab54c0a32e7c391e \
  --output scripts/audit_maintainability/baselines/generated/module_layers.json \
  --update-reductions
```

이 명령은 기존 정본과 비교해 감소·삭제만 반영한다. 신규·증가가 하나라도 섞이거나
불확실성이 새로 생기거나 늘어도 쓰기를 거부한다. 설정·측정 오류도 쓰기를 막는다.
삭제된 부채는 zero-rule로 남긴다.
동시 갱신은 exclusive lock 획득에 실패하면 거부하고, 허용된 변경만 같은 디렉터리의
임시 파일에서 atomic replace로 반영한다. 일반 생성기의 덮어쓰기 거부는 유지한다.

schema 2에서 판정 불가 record를 추가하는 최초 확장은 같은 명령의 마지막 옵션을
`--upgrade-records`로 바꿔 한 번 실행한다. 기존 역방향·불확실성 ID/count와
집계 수가 같을 때만 record를 추가하며, 신규 부채를 수용하는 갱신에 사용하지 않는다.

## 검토된 모듈 이동의 old→new 대응

이동 대응은 [`layer_moves.toml`](../../scripts/audit_maintainability/layer_moves.toml)에 적는다.
초기 파일은 `schema_version = 1`, `moves = []`이며 대응 항목이 없다.
전체 모듈 이동으로 ID가 달라지면 이전 baseline ID와 현재 감사에 나온 새 ID를
검토해 아래 형식의 항목을 추가한다. 각 ID는 64자리 SHA256이다.

```toml
schema_version = 1

[[moves]]
section = "reverse_dependencies"
old_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
new_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
```

`section`은 `reverse_dependencies`, `indeterminate_dependencies`, `uncertainties` 중 하나다.
대응은 section 안에서 일대일이어야 한다. `old_id`는 기존 양수 부채 또는 zero-rule에
있어야 하며, 중복 old/new ID·같은 ID로의 대응·기존 ID와의 충돌은 error다.
명시하지 않은 새 ID는 신규 부채로 차단한다. 일부 item 분리의 발생 수를 자동 배분하지 않는다.

갱신 뒤에도 baseline은 이전 ID와 이전 source/target metadata를 유지하고 현재의
발생 수만 반영한다. 따라서 이동 mapping도 계속 유지한다. 새 경로에서 다시 발생한
zero-rule 부채 역시 이 대응으로 검사한다. mapping을 지우거나 새 부채에 임의 대응시켜
기존 한도를 재사용하지 않는다.

## 원감사 418과 목표 209

[원 이슈 #7353](https://github.com/itismyfield/AgentDesk/issues/7353)은 2026-10-24
`4c026e9`를 기준으로 외부 Discord 참조 418건·173파일·62대상을 기록했다.
일부 소스 좌표는 `6922bd93`에서 재확인했다. 본문·댓글과 당시 감사 기록에서
산출 명령·프로그램·418개 행 목록을 회수하지 못했다.

같은 0A r1 도구 SHA `0aeb8fc5996b790b3dabc1f788ea9a6982dcde1b`를 역사 소스
`4c026e95673deb841248432226badfde39f5a845`와 현재 소스에 적용한 결과는 다음과 같다.

| 동일한 census 방식 | 역사 4c026e9 | 현재 0aeb8fc599 | 시점 차이 |
|---|---:|---:|---:|
| import leaf | 253 | 275 | +22 |
| 직접 path | 276 | 296 | +20 |
| 외부 재수출 | 0 | 0 | 0 |
| 합계 | 529 | 571 | +42 |
| 관련 파일 | 169 | 199 | +30 |
| 서로 다른 증거 줄 | 444 | 480 | +36 |

중첩 use는 leaf마다 세므로 현재 571건과 480개 증거 줄은 다른 단위다.
별도 후보 regex는 같은 역사 소스에서 417건·171파일·61대상으로 원문과 일치하지
않았고 상위 대상 분포도 달랐다. 이를 원산출법이나 418의 재현으로 사용하지 않는다.

확정할 수 있는 것은 **같은 방식에서의 시점 차이 +42**다. 418→571의 차이 153이나
418과 529의 차이 111을 그대로 시간 증가 또는 검증된 계수 변환이라고 부르지 않는다.
원 입력 범위와 산출법이 불명확해 **418과 현재 census는 직접 비교 불가**다.
**수용 기준 3 완료 선언을 계수 호환성 복원까지 보류하고, 목표 209를 유지한다.**

역사 측정은 금지 설정을 제외한 `Cargo.toml`과 `src/**/*.rs`만 Git archive에서
추출했고 도구/helper도 tool SHA로 고정했다. 읽기 실패와 모듈 도달 오류는 0이었다.
재현 명령·archive/source/tool SHA256·검색 원문·graph는 캠페인 보고서의 증거 목록에 있다.
