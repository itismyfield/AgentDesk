# 모듈 층 측정과 의존 ratchet

층의 정본은 [`layers.toml`](../../scripts/audit_maintainability/layers.toml)이다.
디렉터리 이름 대신 역할과 소스 근거를 검토한 논리 모듈 prefix를 사용한다.
일치하는 가장 긴 namespace 경계를 선택하며, 같은 경계에서 분류가 충돌하면
미분류다. 빈 prefix는 해당 crate root 하나만 가리킨다.

| 층 | 책임 | 대표 근거 |
|---|---|---|
| vocabulary (0) | 순수 값·정적 계약 | `domain::lifecycle`, `queue_contract`, `phase_gate`, Discord `bot_role`, `domain::session_identity` |
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
논리 위치와 무관하게 adapter다. 혼재 모듈도 본체의 책임을 검토하고,
책임이 다른 하위 모듈은 긴 prefix로 구분한다. 아직 검토하지 않았거나
책임을 정하지 못한 모듈은 사유와 함께 미분류로 남긴다.
`domain::lifecycle`이 재시작 mode enum·정적 변환을 소유하며 vocabulary다.
`domain::memory`(vocabulary)가 memory backend 값·기본값을, `utils::discord`(foundation)가 token hash 계산을 소유하고 `services::discord::settings`는 둘을 재노출한다.
`services::discord::restart_mode`는 같은 타입을 재노출하는 vocabulary 호환 shim이고,
`cli::restart_terminal_proof`의 nonce·경로·파일 증거 판정은 foundation이다.
두 leaf는 부모의 Discord/최종 조립 책임보다 긴 prefix 규칙으로 분류한다.
모든 규칙의 이유와 `file:line` 근거는 TOML에 있으며, 미분류 모듈의 전체 목록은
감사 JSON의 `layer_analysis`에서 확인한다.

## 실행과 출력 계약

저장소 root에서 실행한다.

```sh
python3 scripts/audit_maintainability.py --format json
python3 scripts/audit_maintainability.py --check
```

`module_layers`는 baseline에 없는 역방향 부채와 기존 역방향 부채의 증가를 `error`로 보고한다.
판정 불가·불확실성·미분류 합계의 증가는 기본 `warning`이며 역방향 부채를 숨기면 `error`다.
`--check`는 이 error가 있으면 실패한다. 다른 감사 검사의 severity는 바꾸지 않는다.
`checks.module_layers.findings[0].extra.measurement`는 0A의 node·edge·coverage 계약을
그대로 유지한다. `extra.layer_analysis`가 별도의 분류·층별 edge 집계·역방향 의존·
판정 불가·SCC·분류 coverage를 제공한다. 0A coverage의 `classification`은 소스
분석기의 기존 값이고, 층 분류 현황은 `layer_analysis`에서 확인한다.
두 결과가 모두 불완전성을 표시하며 미해석 참조를 외부 잡음이라는 이유만으로
제거하지 않는다. `--check`의 성공은 차단하는 역방향 부채가 늘지 않았다는 뜻이며, 판정 불가가
남은 경계의 의존이 모두 해석됐다는 뜻은 아니다.

## 차단 규칙과 실패 해석

| 현재 측정과 baseline 비교 | 판정 | 확인할 내용 |
|---|---|---|
| 새 역방향 ID 또는 기존 ID의 발생 수 증가 | error | 보고된 source·target·kind와 참조 증거를 확인하고 증가를 제거한다. |
| 새 판정 불가 ID 또는 기존 ID의 발생 수 증가 | warning | 미분류 endpoint를 확인한다. 기존 역방향 부채를 숨기거나 역방향 부채가 줄어든 source에서 늘면 error다. |
| 미분류 모듈 수·판정 불가 edge/발생 수 증가 | warning | 미분류 endpoint와 변경된 층 규칙을 확인한다. |
| 새 uncertainty ID 또는 기존 ID의 발생 수 증가 | warning | 미해석·cfg·읽기·모듈 오류를 확인한다. 역방향 부채를 숨기는 참조나 역방향 부채가 줄어든 source의 증가는 error다. |
| baseline·이동 mapping의 누락/잘못된 형식, 측정 예외·설정 오류 | error | 입력과 측정 실패를 고친다. 현재값으로 baseline을 다시 만들어 숨기지 않는다. |
| 기존 ID의 발생 수 감소 또는 해소 | info | 감소를 검토한 뒤 아래 명령으로 정본에 반영한다. |
| baseline 안의 기존 부채 유지 | info | 기존 판정 불가와 coverage의 불완전성은 그대로 남는다. |

모든 ID는 독립적으로 비교한다. 한 ID가 줄어든 양으로 다른 ID의 신규·증가를
상쇄하지 않는다. 해소된 ID는 갱신 시 `zero_rules`에 0으로 고정하며, 다시 나타나면
해당 section의 판정 규칙을 따른다. 단순 합계가 같아도 판정 불가 edge의 대상이 바뀌면
새 ID로 경고한다. 역방향 양수·zero-rule이 판정 불가나 미해석 참조로 숨겨지는 경로는 error다.

| baseline section | 기본 증가 판정 | 예외 |
|---|---|---|
| `reverse_dependencies` | error | 신규·증가·zero-rule 재등장을 차단한다. |
| `indeterminate_dependencies` | warning | 기존 역방향 부채를 숨기거나 역방향 부채가 줄어든 source에서 늘면 error다. |
| `uncertainties` | warning | 기존 역방향 부채를 숨기거나 역방향 부채가 줄어든 source에서 늘면 error다. |

## baseline 생성과 식별자

정본 경로는 [`baselines/generated/module_layers.json`](../../scripts/audit_maintainability/baselines/generated/module_layers.json)이다.
초기 측정 소스 SHA는 `0aeb8fc5996b790b3dabc1f788ea9a6982dcde1b`이고,
schema 3 준비 입력 SHA/base `6d7c1851e5a127d153b0193bab54c0a32e7c391e`에서 #7749(`90fe`)의 `src/services/discord/health/recovery/carried_turn_close.rs:293`에 추가된 `discord::schedule_deferred_idle_queue_kickoff_immediate` 참조(`use super::*` 뒤 unknown path root), uncertainty ID `42bf71b862c248fc38e947e6246e23801bcef7d3e8c35815f5c4572576f6b8cf`의 발생 1회를 승인해 기록했다.
최종 활성화 기준은 착지 직전 main `75726b3b1dfd5ea53aa9164324ed54030194a7cb`이며, 역방향 부채의 신규·증가가 0임을 확인했다.
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

## 최초 활성화 기록

schema 3 활성화 정본은 착지 직전 main에서 생성했다. 준비 기준 6d7 이후 main에 들어온
변경은 판정 불가 의존·uncertainty만 늘렸으며 역방향 부채는 130 ID/316회로 같았다.
다음은 승인된 활성화 생성의 재현 기록이다. 활성화 뒤 변경을 수용하는 방법이 아니다.

```sh
python3 scripts/audit_maintainability/checks/module_layer_baseline.py \
  --input-sha 75726b3b1dfd5ea53aa9164324ed54030194a7cb \
  --output scripts/audit_maintainability/baselines/generated/module_layers.json
```

기존 정본에 실행하면 동일 bytes replay만 허용하고, 새 output은 exclusive-create로
쓴다. 활성화 뒤에는 정본을 삭제·재생성하지 않고 아래의 감소 전용 갱신을 사용한다.

`--upgrade-records`는 기존 schema 2 정본과 역방향·불확실성 ID/count 및 집계 수가
모두 같은 경우 판정 불가 record만 보강하는 제한된 기능으로 남아 있다.
이번 최초 활성화에는 이 옵션을 사용하지 않았고, 해당 equality guard도 유지한다.

## 감소만 반영하는 갱신

소스를 commit한 뒤 해당 SHA로 실행한다. 아래 SHA는 현재 활성화 기준점이며,
이후 감소를 반영할 때는 감소가 들어간 commit의 SHA를 넣는다.

```sh
python3 scripts/audit_maintainability/checks/module_layer_baseline.py \
  --input-sha 75726b3b1dfd5ea53aa9164324ed54030194a7cb \
  --output scripts/audit_maintainability/baselines/generated/module_layers.json \
  --update-reductions
```

이 명령은 기존 정본과 비교해 ID별 `min(accepted, current)`로 감소·삭제만 반영한다.
warning 증가는 흡수하지 않고, 미분류 모듈 합계도 두 값의 최솟값을 유지한다.
역방향 신규·증가와 숨김, 설정·측정 오류는 쓰기를 막는다.
매핑이 달라져 역방향에서 사라졌지만 현재 그래프에 같은 source·target·kind가
남아 있으면 거절한다. 검토된 known 재분류만 `--reclassify <prefix>`로 명시한다.
prefix는 source 또는 target의 논리 모듈 namespace 경계에 일치해야 하며 생성 명령에도 기록된다.
삭제된 부채는 zero-rule로 남긴다.
동시 갱신은 exclusive lock 획득에 실패하면 거부하고, 허용된 변경만 같은 디렉터리의
임시 파일에서 atomic replace로 반영한다. 일반 생성기의 덮어쓰기 거부는 유지한다.

## 분류 편입

분류 편입은 `--admit-classification`으로만 한다. 허용 범위는 같은 source anchor에서 "판정 불가 → 층 역방향"으로 바뀐 정확한 ID와 그 count다. 기존 역방향 양수 records의 한도 인상과 zero-rule의 한도 인상은 금지한다. 편입 PR 본문에는 보고서 TSV의 `accepted_indeterminate_count`와 `grown_since_accepted` 합계를 적는다.

```sh
python3 scripts/audit_maintainability/checks/module_layer_baseline.py \
  --input-sha <source-commit> --admit-classification \
  --previous-mapping-rev <previous-mapping-commit> \
  --dry-run --report target/classification.tsv > target/classification-baseline.json
```

이전 매핑의 의미 기반 해시는 accepted 기준선의 `mapping_sha256`과 같아야 한다.
현재 `src/**`·`Cargo.toml`은 input SHA와 같아야 하며 이전 known 층과 층 rank는 불변이다.
같은 그래프의 이전 투영 비교가 error 0이어야 한다. `--reclassify`도 이 known 불변 검사를 면제하지 않는다.
편입 집합은 투영 후 역방향 ID에서 투영 전 역방향 ID를 뺀 정확한 차집합이다.
각 ID·count는 이전 투영의 판정 불가 record와 같아야 하며 이동 mapping의 old/new와 충돌할 수 없다.
기존 판정 불가 양수 record는 zero-rule로 바꾸고 정확한 역방향 record를 추가한 뒤
일반 감소 갱신을 적용한다. 별도 manifest는 두지 않는다.

TSV는 ID·count와 두 endpoint의 규칙 prefix·사유를 제공해 규칙별 비용을 합산할 수 있다.
`accepted_indeterminate_count`는 accepted에 없으면 0이고, `grown_since_accepted`는
`max(0, current count - accepted_indeterminate_count)`다. 현재 count를 편입하되 과거 경고 증가량을 드러낸다.
`--dry-run`은 기준선을 바꾸지 않고 예상 JSON을 stdout에 쓴다. 이 옵션을 빼면 검증한 결과를 atomic replace한다.
편입 ID가 없으면 일반 감소 갱신과 동일한 바이트·생성 명령을 사용한다.
성공 뒤 accepted 매핑 해시가 바뀌므로 같은 이전 매핑으로 재실행하면 거절한다.
K 같은 known 재분류는 먼저 `--update-reductions --reclassify <prefix>`로 반영하고
새 accepted 매핑을 anchor로 미분류 편입을 진행한다. 실패하면 기준선 바이트는 보존된다.

## 검토된 분류 기록

`services::discord::tmux_error_detect`는 긴 prefix로 foundation에 분류한다.
`src/services/discord/tmux_error_detect.rs:1`의 prompt 길이 판독과 :46의 provider 진단 분류,
:61·:83의 인증·과부하 문자열 판독은 I/O 없이 문자열과 정적 진단 값만 다룬다.
기존 known 층의 정교화이므로 `--update-reductions --reclassify services::discord::tmux_error_detect`로
기록하고, 이후 미분류 편입의 이전 매핑은 이 갱신을 담은 commit으로 고정한다.

`services::tmux_common` 본체와 `host_marker`·`session_markers`는 adapter다.
본체는 pane capture(`src/services/tmux_common.rs:539`), lazy 디렉터리 보장(:1061),
source authority 아래 파일 정리(:1183), JSONL 기록(:1315)을 실행한다.
`claude_footer`는 문자열과 task count만 판독하므로 foundation override를 함께 둔다.
두 규칙으로 논리 모듈 4개를 분류하고, `services`의 상위 미분류 규칙은 유지한다.
같은 source에서 K commit의 매핑을 이전 매핑으로 고정해 `--admit-classification`으로
기록한다. 새 역방향 편입은 0 ID·0회이며 기존 역방향 130 ID·316회 한도는 유지한다.

### Claude/Codex TUI와 tmux wrappers

Claude/Codex TUI 본체와 tmux wrappers는 provider 입력 transport·세션 실행·파일 관측을
소유하므로 adapter다. 화면·문자열·경로 판정 leaf는 긴 foundation prefix로 구분한다.
foundation leaf의 상위 실행 모듈 참조는 배치 부채로 기록하며 추출 가능으로 선언하지 않는다.
`services::claude_tui::hook_server`와 그 하위는 provider hook 수신 transport·event bus·값 계약인
adapter다. `src/server/web_surface.rs:36`이 router를 장착하므로 assembly→adapter 정방향이다.
HTTP handler를 조립하는 `services::claude_tui::tui_relay`만 assembly로 둔다.
`services::claude_tui::owner_input`은 C6에서 함께 분류(진행 중 입력 레인이 이 모듈을 만짐)한다.
이 모듈은 명시적 미분류로 남기고 `services` 상위 미분류 규칙도 유지한다.

편입 ID·count·accepted 이후 증가량은 생성기의 `--report` 출력인
[`module-layer-c2-admission.tsv`](module-layer-c2-admission.tsv)에 기록한다.
이는 검토 증거이며 생성기 입력 manifest나 별도 편입 정본이 아니다.
기존 역방향 양수 record와 zero-rule 한도는 유지하고, 같은 source anchor의 정확한
판정 불가→역방향 ID·count만 편입한다.

### Dedupe·input·O와 prompt/turn 제어

`services::tui_prompt_dedupe`는 prompt 상관 cache·binding log·runtime binding과
source custody를 소유하는 adapter다. `synthetic_prompt`는 foundation,
`binding_events::log_record`와 `runtime_binding::codex_verified::provenance`의 값 계약은
vocabulary다. provenance 아래 admission·permission·live_grant와
`dormant::construction`의 registry·파일·capability 실행은 adapter override로 함께 둔다.
값 계약이 상위 실행 모듈의 타입을 참조하는 부분도 정확한 역방향 한도로 기록한다.

`services::tui_input`의 pane 관측·tmux·submission 실행은 adapter다.
`ansi`와 `actor::gate`의 문자열·화면 판정은 foundation, `input_key`의 정적 ID namespace는
vocabulary다. 진행 중인 `fix/i7755-owned-input-stop`과 겹치는
`human_frame::history` 및 하위 append·compat·screen·v2·v2::collision·v2::migration은
명시적 미분류로 남기고, 해당 변경 착지 뒤 함께 검토한다. 그 아래에 더 긴 분류 규칙을
두지 않는다. owner_input 계열의 C6 보류와 `services` 상위 미분류 규칙도 유지한다.

`services::tui_o`의 channel 선택·cutover·turn admission·alarm 제어는 discord다.
기존 writer·repost·store 규칙은 유지한다. shadow 값·설정 계약은 vocabulary,
파일 capture·binding reader는 adapter, identity·seal·unit plan 계산은 foundation이다.
`exact_episode`는 DTO뿐 아니라 `resolve_strict`의 authority·settlement 업무 정책을
소유하므로 application이다. exact_pg·exact_submission·ownership 실행은 adapter로 둔다.
`services::tui_prompt_control`의 문자열 판독은 foundation이고, 실제 입력을 실행하는
`tui_steering`과 JSONL 파일을 관측하는 `tui_turn_state` 및 두 자식은 adapter다.

편입 증거는 생성기의 `--report` 출력인
[`module-layer-c3-admission.tsv`](module-layer-c3-admission.tsv)에 기록한다.
C2와 같은 동일 source의 정확한 판정 불가→역방향 편입 계약을 적용하며,
기존 역방향 양수 record와 zero-rule 한도를 올리지 않는다. TSV는 생성기 입력이 아니다.

### Cluster·Herdr 실행과 provider hosting

`services::cluster`의 node 배치·용량·라우팅 정책은 application이다.
SQL 저장·process registry·환경 및 config snapshot·machine 관측·relay 실행은
더 긴 adapter prefix로 구분한다. intake worker의 Discord turn 실행과 fallback 안내는
별도 discord 경계다. `intake_router_hook::model`, attachment uploads,
stream frame·terminal resolution은 vocabulary 값 계약이며, 상위 실행 타입을 참조하는
부분은 정확한 역방향 부채로 남긴다. owner_record의 PG/CAS 실행과
`owner_record::fallback`의 입력 배치 정책(application)도 구분한다.

`cluster::session_matcher`는 adapter다. rollout path를 만드는
`src/services/cluster/session_matcher.rs:409`·:436이
`src/services/tmux_common.rs:1131`의 `session_temp_path`를 거쳐
같은 파일 :1068·:1076의 lazy 디렉터리 보장 I/O로 이어진다.
주석의 순수 함수 표현이나 문자열 계산만으로 모듈 전체를 foundation으로 내리지 않는다.
stream relay의 `source_read_proof`와 metadata·enqueue가 섞인 identity도 adapter다.

`services::herdr_launch`의 PG/CAS·pane·launch evidence 실행, `herdr_admission`의
stop-file·환경 관측, `provider_hosting`의 registry·driver 선택은 adapter다.
`herdr_launch::policy`의 요청 값·permit future·grant store trait은 vocabulary이며,
미도입 구현은 오류만 반환한다. provider_hosting의 같은 prefix 미분류 항목은 제거한다.

편입 증거는 생성기의 `--report` 출력인
[`module-layer-c4-admission.tsv`](module-layer-c4-admission.tsv)에 기록한다.
진행 중 레인 ref를 시작과 최종 검증 때 대조하여 C4 대상과 겹치지 않음을 확인했다.
C2·C3 known 층, owner_input의 기존 분류와 C6 보류, history 7개 보류는 유지한다.
`services::tui_o::turn_mode`의 application 정교화는 후속으로 남기고 이번에는 적용하지 않는다.
동일 source의 정확한 판정 불가→역방향 ID·count만 편입하며, 기존 역방향 양수 record와
zero-rule의 한도를 올리지 않는다. TSV는 검토 증거이고 생성기 입력이 아니다.

C3 squash 결과는 검토한 C3 accepted의 매핑·부채 records·zero-rule과 대조하고 provenance 차이는 별도로 기록한다.

### Lifecycle 값 소유권 이동

`InflightRestartMode`의 실제 소유는 `domain::lifecycle`이다. 두 variant와 serde 이름,
수치 1/2, invalid→None, label은 기존 정의와 동일하다. Discord root와
`services::discord::restart_mode`는 같은 타입을 위에서 아래로 재노출한다.
domain은 services 실행 타입을 참조하지 않으며 기존 known 층과 rank는 유지한다.

`services::provider`의 4참조, `services::queue`의 2참조,
`services::turn_lifecycle`의 1참조만 정본 경로로 바꿨다.
시작 소스 e285a5cff1의 W595/G878에서 W588/G871로 줄었고,
각 source의 canonical target도 restart_mode에서 domain::lifecycle로 이동했다.
내부 legacy 소비자 경로와 CancelToken의 AtomicU8·0 sentinel·Relaxed ordering은 유지한다.
동일하면 검토된 C4 canonical과 C3·C4 기록을 보존하여 충돌을 풀고, 다르면 `DESIGN_DELTA`로 멈춘다.
새 병합 source anchor에서 `--update-reductions`와 일반 `--check`를 실행하며, 과거 C3 매핑으로 C4 편입을 다시 승인하지 않는다.
C5·C6도 이전 조각의 실제 squash와 검토한 accepted를 대조하는 같은 착지 절차를 적용한다.

### Utility bot 역할 값 소유권 이동

`UtilityBotRole`의 실제 소유는 `domain::bot_role`이다. 기존 enum·정적 구현·T2 시험을
바이트 동일하게 이동했으며 Announce=0/Notify=1, ALL 순서, alias·credential label·emoji·
oversize attachment 정책과 Display/FromStr 계약을 보존한다. 기존 serde 구현은 없다.
`services::discord::bot_role`은 같은 타입을 재노출하는 단일 `pub use` 호환 경로이며,
domain의 생산 의존은 표준 라이브러리뿐이다. 기존 vocabulary 층과 rank는 유지한다.

바깥 18 consumer 파일의 작성 경로 25개만 정본으로 바꿨다. S01 source
`da4f9e6ad1b5ed0c8ab2faa1788c1e4bbdf0ae1a`와 같은 도구로 비교한 결과
W588→563, G488→464 ID·871→836회이며, canonical target도
`services::discord::bot_role`에서 `domain::bot_role`로 이동했다.
기존 Discord·legacy 소비자의 작성 경로와 동작은 유지한다.
`cli/direct.rs`의 역할 경로 한 줄은 같은 이슈 S4 조각의 startup 변경과 구간이 분리된
공동 파일 사용 승인을 받아 치환했다. 기준선은 source 커밋 뒤 공식 감소 갱신만 적용한다.

### Config·runtime·core와 model/org 저장 경계

`config` 본체와 `runtime_layout`, live reload·credential·eventbus·logging은
파일·환경·process snapshot 실행을 소유하는 adapter다. 기존 config 값 계약 세 규칙과
`runtime_layout::paths` foundation은 유지한다. config DTO는 vocabulary,
받은 snapshot의 구조·경로 계산은 foundation, reload·runtime profile·hosting 검증과
migration 허용 정책은 application으로 구분한다. `config_path::selection`은 실제
`Path::is_file`을 수행하므로 adapter를 상속한다. approval snapshot과 consent hold의
실제 파일·저장 실행에는 긴 adapter override를 둔다.

`config::session_hosts`는 platform·provider/TUI 적격성과 restart 필요성 정책의
application 경계다(`src/config/session_hosts.rs:180`, :264, :296).
실행 핸들을 보유하는 `services::agent_protocol`과 provider model catalog·org loader·
Discord runtime store는 adapter다. GitHub process 실행과 sync/triage 업무,
supervisor 복구 정책, utils의 계산·파일 실행·HTTP 응답 경계도 구분한다.
`ui::ai_screen`은 DTO 외 문자열 정제를 수행하므로 foundation override를 함께 두며,
`manual_intervention`의 개입 필요 판정은 application으로 둔다.

이 조각은 논리 모듈 70개를 규칙 43개로 분류한다. 편입 60 ID·127회의 증거는
[`module-layer-c5-admission.tsv`](module-layer-c5-admission.tsv)에 기록한다.
`accepted_indeterminate_count` 합계는 126, `grown_since_accepted` 합계는 1이다.
증가한 path ID `97f26d5fe215da0ee672dc095e808ee5c3f3517771e3a353d6bb3e3dcd72605c`는
`session_host::herdr::launch_host` → `config::session_hosts`이며 accepted 판정 불가 한도는
3회다. 같은 source anchor의 이전 매핑에서는 현재 판정 불가 4회이고, 새 매핑에서
역방향 4회로 바뀐다. 기존 역방향 양수 record·zero-rule에는 없으며 편입 count는
현재 판정 불가 exact count와 같다. 원인은 #7808 commit
`b0bab6ebf693502f86c0b2e2c0307033fa411c29`이 추가한
`src/services/session_host/herdr/launch_host.rs:289`의 `EndpointMode::UserDefault` 참조다.
기존 역방향 양수 record·zero-rule 한도는 올리지 않고, 나머지 경고 증가도 흡수하지 않는다.

진행 중 ref와 C5 대상의 겹침은 시작과 최종 검증 때 다시 대조한다.
`services` 상위 미분류, owner_input의 기존 분류와 C6 보류, history 7개 보류와
C2·C3·C4 known 층은 유지한다. `tui_o::turn_mode` application 정교화는 C6에서 다룬다.

### Turn·recovery·session과 routine/scheduling

`services::tui_o::turn_mode`는 retire가 성공한 채널만 turn 전환을 확정하는
application 정책이다(`src/services/tui_o/turn_mode.rs:94`). 기존 known 재분류이므로
먼저 `--update-reductions --reclassify services::tui_o::turn_mode`로 M1에 기록했다.
M1은 역방향 281 ID·629회와 미분류 295개, 기존 positive·zero 기록을 그대로 유지한다.
이후 unknown 편입은 C5 매핑 대신 M1의 매핑·accepted를 비교 기준으로 사용한다.

turn orchestrator·lifecycle·host 선택·취소 queue 보존의 업무 정책은 application이다.
pending queue 파일 저장, mailbox snapshot·signal transport·wall-clock 관측과
legacy/V2 snapshot 복원은 긴 adapter prefix로 구분한다. snapshot 복원의 generation 0
경로는 runtime_store의 boot 미할당 fallback에서 디스크 counter를 읽으므로
순수 parser로 선언하지 않는다. reply 결과 DTO는 vocabulary, 관측된 stop result의
표현·거절 사유·queue depth 계산은 foundation이다.

recovery 상태·takeover·owner 복귀와 routine/예약 실행 조립은 application이다.
checkpoint·durable PG/process 실행, routine loader·store·provider 전송·파일 탐색은
adapter이고, routine Discord 로그는 discord다. context/action의 값 계약은 vocabulary,
받은 binding/checkpoint의 fallback plan 구성과 shared scheduling 계산은 foundation이다.
scheduled timing의 종결·재시도 정책은 application을 상속한다. session forwarding은
route handler를 조립하지 않는 transport adapter, HTTP resume handler는 assembly이며
transcript catalog 탐색은 adapter override로 둔다.

진행 중 입력 실패 레인과 겹친 routines/예약 owner_input 및 queued_head V2 경계는
그 후손을 포함한 15개 논리 모듈을 각각 명시적 미분류로 유지한다. 그 아래 새 known
규칙을 두지 않는다. Claude owner_input은 live owner/writer lease/payload callback의
adapter로 분류하며, 기존 Discord·Codex owner_input 분류와 history 7개 보류는 유지한다.
C2 hook_server adapter, C3 exact_episode application 및 C4·C5 분류도 유지한다.

M1 뒤 논리 모듈 105개를 규칙 42개로 편입한 증거는
[`module-layer-c6-admission.tsv`](module-layer-c6-admission.tsv)에 기록한다.
새 역방향은 97 ID·240회, `accepted_indeterminate_count` 합계 239,
`grown_since_accepted` 합계 1이다. 증가한 path ID
`8dd9c815ca11f5f0c4ce3cc88b0fc72f548711a70a4a13cfdf006f62c63a2e42`는
`routines::legacy_body_census` → `cluster::node_registry`의 1회 참조다.
M1 accepted에 없으므로 accepted count는 기본값 0이며, 기존 zero-rule도 없다.
같은 source의 이전 매핑에서는 판정 불가 1회이고 새 매핑에서 역방향 1회가 된다.
이전 source에 있던 참조의 도입은 #7783 commit
`bfa5c7171c223e7ad3c09f299ee72f749c17f582`의
`src/services/routines/legacy_body_census.rs:61`이다. accepted에 없던 ID의 exact 편입도
TSV에 드러내며 기존 역방향 양수·zero 한도와 나머지 경고 증가를 흡수하지 않는다.

### 나머지 서비스 가족

C7은 C6의 매핑·accepted를 기준으로 남은 167개 모듈을 규칙 96개로 분류한다.
관측·git·인증 profile·메시지 outbox·memory transport·process boot의 실제
파일·PG·HTTP·process·cache 실행은 adapter다. dispatch·calendar·SLO·입력 readiness,
종료 warrant·review 상태·memory scope 선택과 outbox 복구 정책은 application이다.
manual steer·task completion·분석 DTO·remote profile의 값 계약은 vocabulary이고,
문자열·identity·marker·증거/관측 window의 계산 leaf는 foundation으로 구분한다.
실제 entropy를 획득하는 manual steer action handle과 시계·파일을 읽는 summary quota는
adapter override를 둔다. foundation이라는 이유로 즉시 추출 가능하다고 선언하지 않는다.

review decision·dispatched session·onboarding의 HTTP 최종 응답 조립은 assembly다.
kanban 서비스와 공개 façade는 application이며, 일반 인증 helper의 HTTP 값만으로
handler라고 분류하지 않는다. imsg admission은 외부 직렬화 callback을 사용하며
승자만 전달하고 admission 결과를 정착시키는 application 정책이다.
Discord issue 카드와 outbox session 안내 표현은 discord로 구분한다.
부모와 필요한 leaf 규칙을 이번 편입에 함께 넣었고 기존 known 층은 바꾸지 않았다.

편입 증거는 [`module-layer-c7-admission.tsv`](module-layer-c7-admission.tsv)에 둔다.
신규 역방향은 126 ID·258회, `accepted_indeterminate_count` 합계는 243,
`grown_since_accepted` 합계는 15다. 증가한 10행은 모두 C6 accepted의 양수·zero에
없던 ID다. #7783의 census 관측에서 도입됐으며 같은 현재 source의 이전 매핑에서는
판정 불가 15회, 새 매핑에서는 정확히 역방향 15회다. 기존 역방향·zero 한도를
올리지 않고 나머지 판정 불가·uncertainty 증가도 기준선에 흡수하지 않는다.

시작과 최종 시점에 진행 중 ref의 source diff를 다시 대조한다. C7의 직접 겹침은
없으며, C2~C6 분류와 history 7개·C6 보류 15개는 유지한다. 최신 main의
queued_head V2 start_failure도 상위 미분류 규칙을 상속한다. 따라서 남은 미분류는
services namespace 1개와 위 보류 23개를 합친 24개다. 새 병합 뒤에는 C4 착지 계약에
따라 검토된 canonical을 보존하고 일반 감소 갱신·감사를 수행한다.

### 보류된 history·owner/V2 queue 편입

C8은 C7에서 보류한 23개와 같은 history 가족에 새로 생긴 3개를 함께 검토한다.
history의 native_row·notice_owner·v2::carried는 파일·metadata·registry 실행을
소유하므로 adapter다. history 저장 본체와 실행 자식도 adapter이며, 주어진 snapshot을
비교하는 collision과 raw 문자열을 decode·변환하는 migration은 foundation으로 둔다.
이 두 leaf의 파일·lock 실행은 부모 registry가 소유한다.

routine/예약 owner_input과 routine custody는 admission 상태별 실행 유지·관찰·재시도·
종결을 조립하는 application이다. queued_head V2의 우선순위·권한·원본 보존·시작 실패
정책도 application이다. 실제 writer lease/capability, CAS·fsync 저장, origin 관측과
clock·generation/history 복원은 attempt·commit·origin·sources adapter로 구분한다.
v2_types는 serde 값 계약을 정의하는 vocabulary이며 실행 capability를 발급하지 않는다.
기존 가족 책임 원칙으로 경계를 재구성했고 원설계 전수표의 exact leaf값을 복구했다고
주장하지 않는다. 새 가족에 대한 층 선택은 없고 기존 known 층과 rank는 모두 유지한다.

26개 모듈을 규칙 11개로 편입한 증거는
[`module-layer-c8-admission.tsv`](module-layer-c8-admission.tsv)에 기록한다.
새 역방향은 14 ID·20회, `accepted_indeterminate_count` 합계 17,
`grown_since_accepted` 합계 3이다. 증가한 3행은 모두 accepted의 positive·zero에
없던 missing-ID다. v2_types → db::turns와 v2::origin → v2는
`959999d883611d97b7854b05a1d96f7971d6cfe4`, notice_owner → writer::input_notices는
`9d371204d86389f9e757bc6341b7c10c6ee74c51`에서 도입됐다. 같은 현재 source의
이전 투영에서는 각각 판정 불가 1회이고 새 투영에서는 정확히 역방향 1회다.

기존 현재 역방향 504 ID·1,126회는 분류 전후 동일하다. 과거 accepted 1,127회 중
routines::migrated → error가 23→22회로 줄어든 것은 PR #7994의
`68b23f7d`가 삭제한 AppError::conflict 참조이며 분류 효과가 아니다.
이 기존 역방향 감소도 일반 감소 규칙으로 고정하며 positive·zero 한도를 올리지 않는다.
남은 미분류는 의도한 services namespace 1개다. 진행 중 ref와의 겹침은 시작과
최종 main 병합 뒤 다시 대조하며, 편입 뒤에는 과거 매핑으로 재승인하지 않는다.

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
`old_id`는 지정 section의 기존 양수 부채 또는 zero-rule에 있어야 한다.
`new_id`는 모든 section의 양수 부채·zero-rule ID와 충돌하면 error이며,
section이 달라도 같은 목적지를 중복 지정할 수 없다. 같은 ID로의 대응도 error다.
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

`domain::provider`가 ProviderKind enum과 정적 catalog·이름·별칭·CLI 순서·counterpart·공개 serde 값을 소유한다. 준비 조각, 추출 미완료 — runtime 책임은 S04가 adapter 경계로 전환한다. 기존 services::provider 경로는 같은 타입을 재수출하며 config·auth·cache·registry resolver와 runtime inherent impl은 adapter에 유지한다.
`domain::file_sync`는 parent directory fsync·flush 판정과 시험 probe의 adapter 소유자이며, Discord runtime_store는 같은 함수를 재수출한다. S07의 13 consumer 파일만 정본 경로를 사용하고 syscall 순서·오류·Windows의 unflushed 성공·상위 ENOSPC 관측은 유지한다.
`domain::tool_text`는 foundation에서 민감값 마스킹과 allowed-tools 정규화의 실제 구현·정적 tool catalog를 소유하며, 기존 Discord formatting 경로는 같은 함수·catalog를 재노출한다.
`domain::human_input`이 HumanInputDelivery·HumanInputError·ExternalHold와 정적 hold 사유를 실제 소유하며 vocabulary다. 기존 health 경로는 같은 타입을 재노출하고 Registry·HumanInputRequest·전달 실행은 기존 Discord owner에 유지한다.

S04에서 정적 catalog helper의 실제 구현은 `domain::provider`로, 문자열 naming 계산은 foundation child `domain::provider::naming`으로 옮겼다. config·auth·cache·runtime registry 조회는 `services::provider::ProviderRuntimeExt` adapter 경계가 소유한다. 기존 서비스 경로는 같은 값·함수·trait의 재수출이며 legacy 소비자는 유지한다. 별도 crate 추출 완료를 선언하지 않는다.

### Session identity 소유 경계

`domain::session_identity` vocabulary가 legacy/namespaced session key의 값과 parse, tmux tail 추출을 소유한다. `domain::session_identity::naming` foundation은 정적 provider catalog로 tmux provider/channel과 legacy default를 계산하고 `SessionIdentity::provider_and_channel`의 순수 impl을 소유한다. provider catalog 순서·기본값, 마지막 colon·slash alias·trim·malformed 처리 결과는 보존한다.

기존 Discord identity 경로와 services provider parser 경로는 같은 타입·함수의 재수출이다. 사용되지 않는 `legacy_key` 문자열 렌더링 accessor는 기존 호환 모듈에 남기며 새 suppression을 추가하지 않는다. DB·config·auth·runtime registry와 fallback 실행 순서는 이동하지 않는다. identity A consumer 12파일과 identity B consumer 11파일은 domain 정본 경로를 직접 사용하며 legacy 소비자는 기존 호환 경로를 유지한다.

`domain::message_split`은 foundation에서 UTF-16 단위·fit predicate·chunker·code-fence 및 문장 경계 계산을 실제 소유한다. Discord formatting·semantic_boundaries는 같은 함수를 재수출하고 HTTP 전송·상태 전이는 기존 owner에 유지한다. S12의 shadow/unit_plan·writer/round_trip·writer/pieces 문자열 consumer 3곳만 정본 경로로 전환한다.
