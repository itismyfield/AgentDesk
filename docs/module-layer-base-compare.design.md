# 모듈 레이어 기준선: 커밋하지 않고 main과 PR을 각각 계산 (#8131 결정 B)

<!-- doc-path-refs: removed scripts/audit_maintainability/baselines/generated/module_layers.json scripts/audit_maintainability/layer_moves.toml -->
<!-- doc-path-refs: external coord-tools/ agentdesk-issue-pipeline/ /home/adk/ -->

설계 기준: `56fad666785bcdb8c28324d019067163db31a257` (2026-11-16 KST).
이 문서는 설계와 레인 폴더 실험의 결과이며 제품 코드·설정·파일 형식은 바꾸지 않았다.
`docs/landing-conflict-free-index.design.md`의 C1b(착지 단계 재생성 병합)를 이 설계가 대체한다.
B가 착지하기 전까지는 지금의 재생성 방식(C: 생성 명령으로 기준선 재생성)으로 운영한다.
B1·B2 구현 뒤 코드 인용은 구현 상태에 맞췄다. 지운 코드는 `(B2 전 :N)`으로 적는다.

## 1. 결정과 범위

- `scripts/audit_maintainability/baselines/generated/module_layers.json`을 저장소에서 지운다.
  필수 CI는 병합 기준(main)의 소스와 PR 병합 결과를 **같은 실행에서 각각 측정**해
  (section, id)별로 비교하고, 새로 늘어난 위반만 실패로 본다.
- 정당한 증가(#7353 이동, 분류 편입, 매핑 재분류)는 PR마다 새로 만드는 승인 파일에 항목별로 적는다.
  승인은 **그 승인을 처음 들여온 PR의 비교에서만** 효력이 있다(5절의 델타 의미).
- 파일 삭제와 새 검사 장치는 **한 착지**(한 main commit)에 들어간다. 리뷰 조각은 B1·B2로 나누지만
  어느 한쪽만 먼저 착지하는 분할은 금지한다(9절).
- 결정 2에 따라 이 착지는 대기열이 빌 때를 기다리지 않고 일반 착지 차례에 개별 PR로 넣으며,
  머지 직후 모든 레인에 main 병합을 알린다(9.4절).
- 상태 차트: 해당 없음. CI 검사와 파일 계약 설계이며 런타임 상태 전이를 만들지 않는다.
  승인 항목의 생애만 5.4절에 표로 적는다.

## 2. 두 안 비교 (한 라운드)

| | 안 1: 위반 키 집합 비교 (**채택**) | 안 2: 구획별 개수 비교 + 키 표본 |
|---|---|---|
| 흐름 | base·head를 같은 생성기로 측정하고 기존 `compare`(`module_layer_ratchet.py:132`)로 (section,id)별 count를 비교 | 구획마다 발생 수 합계만 비교하고, 늘어난 id 몇 개를 표본으로 출력 |
| 비용 | 측정 2회(병렬이면 벽시계 1회분+α). 비교 자체는 1초 미만 | 측정 비용은 같다. 비교만 조금 싸다 |
| 위험 | 측정 결정성에 의존(같은 실행·같은 코드로 양쪽을 재므로 통제 가능) | 한 id의 증가가 다른 id의 감소로 상쇄되면 통과. 항목별 승인 불가 |
| 실측 | S2(증가1+감소1) **오류 1**, 1116b의 불확실성 새 id 12건을 경고로 분리 | S2 역의존 합계 0으로 **통과(놓침)**, 1116b 불확실성 +4로 새 id 12건·감소 15건이 서로 가려짐 |

안 2는 "감소가 무관한 증가를 사지 못한다"는 현행 래칫 원칙(`module_layer_ratchet.py:1`)을 깬다.
측정 비용이 같으므로 안 1을 택한다. 표본은 안 1의 CI 요약에 그대로 쓴다(6.3절).

## 3. 실측: 현행 생성기로 본 판정

실험은 레인 폴더 `exp-b/`에서만 했다(원시 JSON `exp-b/out/*.json`, 스크립트 `exp-b/scripts/`).
측정은 모두 origin/main의 생성기 코드(`56fad66678`)로 했고 Python 3.14.10, 맥에서 실행했다.
"현행"은 각 head에 커밋된 `module_layers.json`과 `layer_moves.toml`로 `compare`를 돌린 결과다
(`audit_maintainability.py --check`의 module_layers 판정과 같은 입력).

| base → head | B 안 1 판정 | 안 2(개수) | 현행(커밋 파일) 판정 |
|---|---|---|---|
| main `56fad66678` → `refs/review/batch-1116b` `e6d4ae3e48` | 오류 0 · 경고: 불확실성 새 id 12 · 감소: 역의존 1 id/1회, 불확실성 15 id/27회 | 역의존 −1, 불확실성 +4 (증가 12건 가려짐) | 오류 0 · 경고 1,335건(불확실성 새 id 1,128·증가 175 등 누적) · 감소 0(착지 준비가 재생성 commit `c928d796d1`로 이미 기록) |
| `d40c06eb47` → `refs/review/batch-1115h` `91be6ed099` | 오류 0 · 경고 0 · 감소 0 (생성기 수리 #8188은 측정값을 바꾸지 않음) | 전부 0 | 오류 0 · 경고 1,327건(누적) |
| `d40c06eb47` → origin/main `56fad66678` | 오류 0 · 경고: 불확실성 증가 1 | 불확실성 +1 | 오류 0 · 경고 1,327건(누적) |
| 합성 S1: main + 어휘 층 `config::agent_memory`가 `services::tui_o::channel_policy`를 새로 import | **오류 1** (`new-id` reverse) | 역의존 +1 | (해당 없음) |
| 합성 S2: S1 + 기존 역의존 1회 제거 | **오류 1** + 감소 1 | 역의존 합계 0 → 통과 | (해당 없음) |
| 합성 S3: 소스 그대로, `config::agent_channels` 층만 vocabulary→adapter | 비교기만으로는 오류 0·감소 1 → **기존 매핑 가드**가 거부, `reclassify` 승인 시 통과 | 역의존 −1 통과 | (현행도 `--reclassify` 요구) |

추가 관측:

- 결정성: main을 작업 트리에서 두 번, `git archive` 경로(`historical_measurement`)로 한 번 측정했고
  세 결과의 provenance 제외 본문 SHA256이 모두 `f45470d206aac416…`로 같았다.
- 실행 시간: 한 트리 측정 44~50초(맥, 단일 프로세스). B는 측정이 2회다.
- 현행 `layer_moves.toml`의 세 이동은 새로 계산한 main base에 대해 `_moves`가
  `mapping old ID is absent from the accepted baseline`로 거부했다. 이동 기록은 PR 델타로만 쓸 수 있다.
- 분류 해제(known→unknown)와 known 층 변경: 실제 세 쌍 모두 0. 1116b는 매핑을 바꿨지만(새 모듈 규칙)
  해제·층 변경 0이었다. S3만 known 층 변경 1.
- 6.2절의 "역방향 모양 미해결 참조" 규칙에 걸리는 새 불확실성: 실제 세 쌍 모두 0.
- 1116b에 기존 매핑 가드(`module_layer_baseline.py`의 B2 전 `_guard_mapping_reductions`)를 B 방식(base=main 측정)으로 돌리면 통과,
  S3는 `mapping-only reverse reduction requires --reclassify` 거부였다.

현행의 "경고 1,3xx건"은 경고 증가를 기준선이 흡수하지 않아 쌓인 수다. B에서는 PR이 새로 만든 것만 보인다.

## 4. 계산 계약

### 4.1 base 위치

base는 **항상 main 이력 위의 commit**이다. 결정 규칙은 다음 순서이며, 실패하면 skip이 아니라 오류다.

| 실행 맥락 | base | 검증 |
|---|---|---|
| CI `pull_request` (`ci-pr.yml:1380` Script checks runner) | 병합 commit의 첫 부모 `HEAD^1` (= 그 실행 시점 main tip) | HEAD가 부모 2개, `HEAD^2` = 이벤트 payload의 `pull_request.head.sha`, `HEAD^1`과 payload의 `pull_request.base.sha`가 서로 조상 관계(둘 다 main 이력). 원격 추적 ref 유무에 기대지 않는다 |
| CI `push` main (`ci-main.yml:462` Main script checks) | 이벤트 payload의 `before` | `before`가 HEAD의 조상, 0이 아닌 SHA |
| CI 기타(`workflow_dispatch` 등) | HEAD 자신 | 측정·승인 형식 검사만 하고 비교는 "자기 비교"로 표시. nightly(`ci-nightly.yml:639`)는 현행 `SCRIPT_CHECK_SELF_TESTS_ONLY` 경로라 이 검사를 **실행하지 않는다**(`scripts/ci-script-checks.sh:178`) |
| 로컬(레인·prepush·착지 도구) | `MODULE_LAYER_BASE_REF` 지정값, 없으면 `git merge-base HEAD origin/main` | 지정값도 `origin/main`의 조상이어야 한다. `origin/main`이 없으면 오류 |

- CI 맥락(`GITHUB_ACTIONS=true`)에서 `MODULE_LAYER_BASE_REF`가 설정돼 있으면 오류로 거부한다.
  workflow env 수정으로 base를 HEAD 등으로 바꿔 비교를 비우는 길을 막는다.
- base·head 판정은 Actions가 자동으로 주는 `GITHUB_EVENT_NAME`·`GITHUB_EVENT_PATH`로 한다.
  따라서 workflow·hardening pin을 고치지 않고 `ci-script-checks.sh`의 호출 경로도 그대로 둔다
  (시험 묶음 목록에 새 시험 모듈 이름만 더한다. 9.2절의 빈 구간 논증의 핵심).
- PR CI에서 merge-base가 아니라 `HEAD^1`을 쓰는 이유: GitHub가 시험하는 것은 "현재 main + PR"이다.
  main 쪽 매핑 변경과 PR 쪽 새 참조가 합쳐져 생기는 역의존은 merge-base 비교로는 보이지 않는다.
  기존 시험 레인 래칫도 같은 이유로 `TEST_LANE_BASELINE_REF: HEAD^1`(`ci-pr.yml:1474`)을 쓴다.

### 4.2 측정 입력과 결정성

- base: base commit의 추적된 `src`·`Cargo.toml`을 `git archive`로 임시 디렉터리에 풀어 측정한다
  (`module_layer_gate.py`의 `_materialize`. 설계 때 쓰려던 `historical_measurement`는 쓰지 않아 B2에서 지웠다). 매핑은 `git show base:layers.toml`이다.
  작업 트리 상태를 읽지 않는다.
- head: CI에서는 작업 트리(= HEAD) 측정 전에 기존 검증과 같은 조건(`module_layer_gate.py`의 `_dirty`:
  `src`·`Cargo.toml`이 HEAD와 같고 미추적 소스 없음)을 요구한다. 로컬은 작업 트리를 재되
  더러운 트리면 결과에 `dirty`를 표시한다(로컬 신호일 뿐 착지 근거가 아니다).
- 양쪽 모두 **head의 측정 코드·같은 인터프리터·같은 실행**으로 잰다. Python 버전·OS 차이는 양쪽에
  똑같이 걸리므로 거짓 증가를 만들지 않는다. 지금은 레인이 로컬 Python 3.14로 만든 파일을
  CI Python 3.11이 비교하므로 오히려 B가 더 결정적이다.
- 측정 코드가 바뀐 PR은 측정 결과 차이가 양쪽에서 함께 사라질 수 있다. 이 경로는 6.2절의
  분석기 변경 검사로 막는다.
  측정 코드는 `module_layers.py`·`module_layer_baseline.py`·`service_server_backflow.py`·
  `audit_maintainability/common.py`·`audit_maintainability/rust_accessor_scan.py`·`checks/__init__.py`·`scripts/ci/rust_test_sources.py`와 그 지역 import
  (`rust_production.py`·`rust_lex.py`·`rust_cfg.py`·`generate_inventory_docs.py`)다. 목록이 실제 import를
  모두 덮는지는 시험이 고정한다.
- provenance(`input_sha`·`generation_command`)는 비교 대상이 아니다. 비교는 record 본문만 쓴다.

### 4.3 비교 키와 판정

키와 count 단위는 지금과 같다: edge는 `_id("edge", source, target, kind)`, 불확실성은
`_id("uncertainty", category, source, path, 정규화 reason)`이고 count는 발생 수다(`docs/architecture/layers.md:95`).

1. 이번 PR의 `[[move]]`를 **원시 base·head**로 먼저 검증한다(old_id는 base에 있고 head에 없음,
   new_id는 head에 있고 base에 없음, 같은 구획).
2. base 측정에 **이번 PR의 승인 델타**(5절)를 적용해 `accepted`를 만든다. `[[increase]]`의 효력·상한은
   **이동 정규화 뒤 같은 부채 id**에 대해 판정한다. 승인 파일에는 candidate의 id·끝점을 적고, 그 id가
   이동 목적지이면 내부적으로 base의 old_id record count를 `max_count`로 올린다(목적지 id를 `accepted`에
   넣지 않으므로 기존 `_moves`의 충돌 검사와 맞는다). 분류 편입은 B2 전 생성기의 `_admit_classification`과
   같은 조건으로 판정 불가 record를 역의존 record로 옮긴다(`module_layer_gate.py`의 `apply_approvals`).
3. `compare(accepted, head, 이번 PR의 [[move]], head sources)`(`module_layer_ratchet.py:132`)를 그대로 쓴다.
   역의존 신규·증가·숨김은 오류, 판정 불가·불확실성 증가는 경고(숨김 관련이면 오류)다.
4. 매핑이 바뀌었으면 B2 전 가드 `_guard_mapping_reductions`와 같은 판정을
   base=main 측정과 이번 PR의 `[[reclassify]]` prefix로 한다(`module_layer_gate.py`의 `layer_rules`).
5. 6.2절의 추가 규칙(분류 해제, 역방향 모양 미해결 참조, 분석기 변경, 묘비)을 적용한다.
6. 승인 항목 검증(5.3절): 쓰이지 않은 승인, 상한 초과, 착지된 승인 파일 수정은 오류다.

감소는 막지 않고 기록도 하지 않는다. 다음 PR의 base가 그 감소를 이미 포함하기 때문이다.

### 4.4 uncertainty

- 판정 단위는 지금과 같이 id별 count다. 일반 증가는 경고, 기존 역의존을 숨기거나 역의존이 줄어든
  source에서 늘면 오류다(`module_layer_ratchet.py:167`).
- 지금의 "역사 증명"(`_record_proven_uncertainties`, B2에서 제거)은 필요 없다.
  그 장치는 커밋 기준선이 경고 증가를 흡수하지 않아 main에 이미 있는 불확실성을 새것처럼 보던 문제를
  풀려고 있었다. B의 base는 main 측정 자체이므로 main에 있는 것은 처음부터 base에 있다.
- 총계(`unclassified_modules` 등) 증가는 지금처럼 경고다.

### 4.5 실행 시간

| 맥락 | 지금 | B |
|---|---|---|
| CI Script checks runner | 측정 1회 + 비교 | 측정 2회. base 측정을 하위 프로세스로 병렬 실행하면 벽시계 +수 초~1회분. 측정 코드가 바뀐 PR만 3회 |
| prepush 합성 병합(`agentdesk-issue-pipeline/scripts/prepush_gates.sh:129`) | 측정 1회 | 측정 2회(병렬 bg 작업 안). base는 로컬 캐시 가능 |
| 레인 로컬 `audit_maintainability.py --check` | 측정 1회 | 같은 main SHA면 캐시로 측정 1회 |

맥 실측은 한 트리 44~50초다. CI 러너(3.11)의 값은 구현 조각에서 잰다([U3]).
로컬 캐시 키는 (base commit의 `src`·`Cargo.toml` tree hash, base 매핑 blob, 측정 코드 blob hash)이고
`$(git rev-parse --git-common-dir)/adk-module-layer-cache/`에 둔다. **CI는 캐시를 읽지 않는다**
(캐시 오염이 필수 판정에 닿지 않게).

## 5. 승인 파일

### 5.1 형식과 위치

`scripts/audit_maintainability/layer_approvals/<issue>-<slug>.toml`. PR(조각)마다 **새 파일 하나**를 만든다.
한 항목을 한 줄 inline table로 써서 prod 줄 수 계산(`pr_cap_prod.py`는 `.toml`을 prod로 센다)을 작게 유지한다.

```toml
schema_version = 1
issue = 7353
summary = "S14d: services::x 분류 편입과 이동"
increase = [
  { section = "reverse_dependencies", id = "<sha256>", source = "src/lib.rs::crate::a", target = "src/lib.rs::crate::b", kind = "import", max_count = 2, reason = "admit-classification" },
]
move = [
  { section = "reverse_dependencies", old_id = "<sha256>", new_id = "<sha256>", reason = "a::x를 b::x로 이동" },
]
reclassify = [
  { prefix = "services::discord::tmux_error_detect", reason = "known 층 정교화" },
]
analyzer = [
  { digest = "<sha256>", reverse_removed = 0, reverse_added = 0, reason = "#8188 생성기 수리" },
]
```

- `increase.reason`은 `admit-classification`(판정 불가→역의존 편입) 또는 `accepted-debt`(검토된 새 부채)다.
  edge의 id는 source·target·kind와 일치해야 한다(`_record` 검증, `module_layer_ratchet.py:18`).
  불확실성 id도 올릴 수 있지만 대상은 count 증가에 걸린 판정(6.2절 `unresolved-reverse`, 증가 기반 숨김)뿐이다.
  기존 `hidden-reverse`(역의존이 줄었는데 같은 source의 미해결 참조가 여전히 대상을 가리킴,
  `module_layer_ratchet.py:184`)는 count와 무관한 판정이라 **승인 대상이 아니다**. 코드를 고친다.
- 파일 이름은 `^[0-9]+-[a-z0-9-]+\.toml$`, 같은 파일 안에서 (section,id)·old_id·new_id·prefix 중복은 오류다.

### 5.2 델타 의미

비교 base→candidate에서 효력이 있는 승인은 **candidate의 승인 디렉터리에는 있고 base에는 없는 파일**의 항목뿐이다.

- base에 이미 있는 승인(착지된 승인)은 다시는 효력이 없다.
- base에 있는 파일을 candidate가 **수정하거나 삭제**하면 오류다(착지된 승인 파일은 불변, 정리 없음).
  삭제를 허용하면 "정리 → 같은 파일 재추가"로 옛 승인이 새 델타가 되어 되살아난다(설계 리뷰 P2-1).
  착지된 파일이 영원히 base에 남으므로 같은 경로·같은 내용의 재추가는 델타가 될 수 없다.
  그래서 "예전 승인이 나중에 같은 부채의 재등장을 조용히 통과시키는 길"이 없다.
- base 쪽 승인은 `git ls-tree base -- layer_approvals/`와 `git show`로 읽는다. 작업 트리를 쓰지 않는다.
- 착지 묶음은 각 기능이 만든 파일을 그대로 모으므로 묶음 PR의 델타에 모두 들어간다.

### 5.3 승인 항목 검증

| 항목 | 효력 조건 | 오류 |
|---|---|---|
| `increase` | 이동 정규화 뒤 같은 부채에서 base count < head count ≤ `max_count` | head count > `max_count`(상한 초과), head count ≤ base count(쓰이지 않음), 정규화 전 목적지 id에 적용(구현 오류로 간주해 시험) |
| `increase`(admit) | base에서 같은 id가 판정 불가 record로 같은 count, 매핑 hash 변경, known 층·층 rank 보존(`apply_approvals`) | 조건 불일치 |
| `move` | old_id가 base에 있고 head에 없음, new_id가 head에 있고 base에 없음(`_moves`·`_normalized` 재사용) | 조건 불일치, 쓰이지 않음 |
| `reclassify` | 매핑이 바뀌었고 그 prefix로만 설명되는 매핑 감소 또는 분류 해제가 실제로 있음 | 매핑 무변경, 쓰이지 않음 |
| `analyzer` | 측정 코드가 바뀌었고 base 소스를 두 코드로 잰 차이의 digest·개수가 일치 | digest 불일치, 측정 코드 무변경 |

"쓰이지 않음"을 오류로 하는 이유: 착지 뒤에는 어차피 효력이 없으므로, 쓰이지 않는 승인은 미리 받아 둔
백지 승인일 뿐이다. main이 먼저 같은 증가를 들여와 승인이 쓸모없어지면 작성자가 지운다(fail-closed).

### 5.4 누가 언제 쓰는가와 승인 항목의 생애

- 작성: 증가가 필요한 레인(#7353 조각 등)이 도우미 `module_layer_gate.py --draft-approval <issue>-<slug>`로
  현재 오류가 된 증가만 담은 초안을 만들고 `reason`을 채운다. 지금의 `--admit-classification --report`
  TSV 역할을 초안이 대신한다.
- 검토: 승인 파일은 코드와 같이 독립 리뷰 대상이다. 코디네이터는 착지 단계에서 승인을 새로 쓰지 않는다
  (자동 승인 금지, 기존 설계 C1b의 "자동화가 정책 충돌을 해소하지 않는다" 원칙 유지).

| 상태 | 들어가는 사건 | 효력 | 나가는 사건 |
|---|---|---|---|
| 초안 | 레인이 브랜치에 파일 추가 | 그 브랜치의 로컬·PR 비교에서 유효 | 리뷰 수정, 쓰이지 않으면 CI 오류로 삭제 |
| 착지 | 그 PR이 main에 들어감 | **없음**(이후 모든 비교의 base에 포함되므로 델타가 아님) | 없음(불변, 삭제·수정은 CI 오류) |

승인된 증가가 나중에 줄면: 그냥 감소다. 승인 파일은 이미 효력이 없으므로 고칠 것이 없다.
파일은 감사 기록으로 영구히 남는다. 디렉터리 크기가 문제가 되면 경로 재사용을 git 이력으로 거부하는
검사를 먼저 넣은 뒤에만 정리를 허용한다(별도 설계).

### 5.5 승인 파일이 충돌원이 되는가

- PR마다 새 파일만 더하고(이름에 이슈·조각), 착지된 파일은 수정·삭제하지 않는다.
  두 PR이 같은 줄을 고칠 일이 없으므로 내용 충돌이 생기지 않는다. 남는 것은 같은 이름을 고른
  add/add뿐이고, 이름 규칙(이슈 번호 + 조각)으로 피한다.
- 기존 C2a(정렬 + `merge=union` + `list_merge_guard.py`)도 쓸 수 있지만, 델타 판정이 항목 정체성을
  다시 계산해야 하고 `.gitattributes`·병합 가드 배선이 늘어난다. 파일 단위 추가만으로 충돌이 없으므로
  C2a 계약을 쓰지 않는다. `.gitattributes`는 바꾸지 않는다.

## 6. 래칫 의미 보존

### 6.1 현행 보호와 B의 대응

| 현행 보호 | 근거 | B에서 |
|---|---|---|
| 역의존 신규·증가 차단 | `module_layer_ratchet.py:167` | 같다(base=main은 승인 없이는 역의존이 늘지 않은 상태) |
| zero-rule 재등장 차단(역의존 75건) | `module_layer_ratchet.py:160` | 같은 id가 역의존으로 돌아오면 `new-id` 오류로 여전히 차단 |
| zero-rule을 숨김 판정에 포함 | `module_layer_ratchet.py:144` | 과거 0이 된 역의존이 판정 불가 edge나 미해결 참조로 숨어 돌아오는 경로가 약해진다 → 6.2의 분류 해제·역방향 모양 규칙으로 보완 |
| 판정 불가(6,290)·불확실성(318) zero-rule | 경고 구획 | 경고 코드 이름만 `zero-rule`→`new-id`. 차단 여부는 같다 |
| 경고 증가를 상한에 흡수하지 않음 | `module_layer_ratchet.py`(B2 전 :217) | 착지 뒤 base에 흡수된다. 경고는 원래 막지 않으므로 차단 의미는 같고, PR마다 새 경고만 보인다 |
| 매핑만으로 줄인 역의존은 `--reclassify` 필요 | `module_layer_baseline.py`(B2 전 :271) | 같은 판정을 gate `layer_rules`가 한다, `[[reclassify]]` 승인 |
| 분류 편입은 명시 명령으로만 | `module_layer_baseline.py`(B2 전 :288) | 같은 조건을 gate `apply_approvals`가 검사, `increase(admit)` 승인 |
| 이동은 `layer_moves.toml` | `module_layer_ratchet.py:71` | PR 델타의 `[[move]]`(3절: 과거 manifest는 새 base에서 거부됨) |
| 동시 착지의 의미 충돌 | 커밋 파일에 없는 역의존 → main과 이후 모든 PR이 빨강 | main 사후 검사가 빨강 + 착지 도구가 다음 착지를 거부(6.2 다섯째 줄) |

### 6.2 증가를 숨기는 길과 막는 방법

| 길 | 막는 방법 | 남은 반례 |
|---|---|---|
| base를 main이 아닌 곳에서 계산(HEAD 자기 비교, 브랜치 중간 commit, 오래된 commit) | 4.1의 검증: CI는 이벤트 payload로만 base를 정하고 override 거부, PR은 `HEAD^2`=PR head·`HEAD^1`⊂main 확인 | 없음(PR base가 그 실행 시점 main tip보다 뒤질 수 있는 것은 strict 꺼짐의 일반 성질이며 다섯째 줄로 다룬다) |
| 승인 파일 남용(백지 승인, 옛 승인 재사용, 상한 부풀리기) | 델타 의미, 착지 파일 불변, 쓰이지 않음 오류, `max_count` 정확 상한, edge id-끝점 일치 | 리뷰어가 나쁜 부채를 승인하는 것은 사람의 판단(지금의 편입 명령과 같음) |
| 분류 해제로 숨김(known 모듈을 미분류로 만들어 과거 0인 역의존을 판정 불가로 되살림) | **새 규칙**: base에서 분류된 모듈이 head에서 미분류·충돌 분류면 오류, `[[reclassify]]`로만 허용. 실측 실제 3쌍 0건 | 이름이 같은 모듈을 지웠다가 미분류로 다시 만드는 경우(긴 prefix 규칙이 보통 덮는다) |
| 미해결 참조로 숨김(과거 0인 역의존을 경로만 남은 참조로 되살림) | **새 규칙**: 새로 늘어난 불확실성의 적힌 경로가 가리키는 분류 모듈의 rank가 source보다 높으면 오류(`unresolved-reverse`), `increase`로만 허용. zero-rule 집합보다 넓은 상위 집합. 실측 실제 3쌍 0건 | 재수출·별칭으로만 닿는 경로(지금도 알려진 한계, `module_layer_ratchet.py:111`) |
| 동시에 초록인 두 PR이 합쳐져 생긴 역의존(strict 꺼짐, `agentdesk-issue-pipeline/SKILL.md:403`) | main push CI가 `before`→HEAD를 비교해 빨강. 착지 도구 `coord-tools/open_landing_pr.sh`가 다음 조건을 **모두** 만족하는 run 하나만 통과 근거로 쓴다: `event == push`, `head_sha ==` 현재 origin/main, 그 run attempt에 속한 완료된 `maintainability-audit-main` 산출물에 module_layers 오류 0. 산출물 누락·판독 실패·검사 대기·dispatch 자기 비교·이전 SHA의 초록은 통과 근거가 아니며 착지 PR 개설을 거부 | 사후 검사가 빨강이 된 뒤 도구를 거치지 않고 다른 PR이 머지되면 그 뒤 base에 흡수된다. 착지 PR 동시 2개 제한에서 발생 조건이 좁다. PR CI에 "main tip 자기 검사"를 더하는 안은 1 commit 창만 덮고 측정 1회가 늘어 채택하지 않음 |
| 측정 코드 변경으로 양쪽에서 함께 사라짐 | **새 규칙**: base·head의 측정 코드 blob이 다르면 base 소스를 base 코드로도 한 번 더 재고, 역의존 구획 차이가 있으면 `[[analyzer]]` digest 승인 필요. 차이가 없으면(예: `_run`만 바뀐 B 자체) 승인 없이 통과 | 불확실성 구획의 차이는 요약에만 표시 |
| 결정성 깨짐(트리 상태, 캐시, 인터프리터) | CI는 깨끗한 트리 요구, base는 git 객체에서만, 양쪽 같은 실행·같은 코드, CI는 캐시 미사용 | 없음 |
| 커밋 파일 부활(도구가 modify/delete를 잘못 풀어 다시 추가) | **묘비 규칙**: `module_layers.json`이 base 트리에는 없는데 head 트리에 있으면 오류(부활). B2 전에는 양쪽에 다 있으므로 B1 단독 트리와도 공존한다. B0 도구 수정 | 없음 |
| 검사 자체가 건너뛰어짐 | "Maintainability audit"은 `SELF_TEST_INPUTS`(`scripts/ci/script_check_inputs.py:39`)에 없어 입력 경로로 건너뛰지 않는다. 시험으로 고정 | 없음 |

### 6.3 CI 요약의 감소 수치: 채택

채택한다. 근거:

- 커밋 파일이 없으면 감소가 diff로 보이지 않는다. 감소는 판정에 쓰이지 않으므로(막지 않고 저장도 안 함)
  표시는 새 실패 경로를 만들지 않는 순수 관측이다.
- 같은 입력이면 같은 결과이므로 아무 commit 쌍에서나 다시 계산할 수 있다. 영구 저장이 정확성에 필요 없다.

출력 위치: `audit_maintainability.py` YAML 산출물(이미 PR·main CI가 올린다, `ci-pr.yml:1531`),
stderr 한 줄, `GITHUB_STEP_SUMMARY`가 있으면 표 한 개. 예:

```text
module_layers base=56fad66678 head=e6d4ae3e48 errors=0 warnings=12
  decreases: reverse 1 id/1 occ, uncertainties 15 id/27 occ; approvals used: 0; analyzer: unchanged
```

착지 PR 본문에는 `coord-tools/build_bundle.sh`가 같은 줄을 붙인다(도구 쪽 선택 사항).

## 7. 진입점 전수표

"B뒤"의 조각은 9절. R=저장소, E=저장소 밖 코디네이터·스킬 도구.

| # | 위치 | 지금 하는 일 | B 뒤 |
|---|---|---|---|
| 1 | R `scripts/audit_maintainability/baselines/generated/module_layers.json` | 커밋된 승인 기준선 2.8MB | 삭제(B2). 묘비 규칙이 부활을 막음 |
| 2 | R `scripts/audit_maintainability/baselines/generated/README.md:1` | 이 파일의 생성·감소 갱신 규칙 | module_layers 문장 삭제, B 안내로 교체(B2) |
| 3 | R `scripts/audit_maintainability/checks/module_layer_baseline.py:16` | `BASELINE` 경로 상수 | 묘비 검사 경로로만 남김(B2) |
| 4 | R `module_layer_baseline.py:209` | payload의 `generation_command`가 커밋 경로를 가리킴 | 진단 스냅숏(`target/`)용 명령으로 바꿈(B2) |
| 5 | R `module_layer_baseline.py:239` | `write_baseline` exclusive-create | 진단 출력 전용, 커밋 경로 기본값 제거(B2) |
| 6 | R `module_layer_baseline.py`(B2 전 :271) | 매핑만 감소 가드 | B1 gate의 `layer_rules`가 `[[reclassify]]`로 같은 판정을 해 호출처가 없어져 B2에서 제거 |
| 7 | R `module_layer_baseline.py`(B2 전 :288) | 분류 편입으로 기준선 수정 | B1 gate의 `apply_approvals`가 같은 조건으로 admit 승인 검증, 함수와 기준선 쓰기 제거(B2) |
| 8 | R `module_layer_baseline.py`(B2 전 :364) | 미기록 불확실성·역사 증명 | 제거(4.4절)(B2) |
| 9 | R `module_layer_baseline.py`(B2 전 :389) | `update_baseline` 잠금·원자 교체 | 제거(B2) |
| 10 | R `module_layer_baseline.py`(B2 전 :480) | 과거 commit 소스 재측정 | B1 gate는 `_materialize`(`git archive`)로 base를 재서 호출처가 없어져 B2에서 제거 |
| 11 | R `module_layer_baseline.py:255`(B2 전 :513) | CLI: `--update-reductions`·`--upgrade-records`·`--admit-classification`, `--output` 기본 = 커밋 경로 | 세 모드 제거. `--input-sha X --output target/…` 진단만(B2) |
| 12 | R `scripts/audit_maintainability/checks/module_layer_ratchet.py`(B1 전 :11) | `BASELINE` import | 제거(B1) |
| 13 | R `module_layer_ratchet.py`(B1 전 :22) | `load_inputs`: 커밋 파일 + `layer_moves.toml` 읽기 | 제거, gate가 base 측정·승인 델타를 넘김(B1). `MOVES` 상수는 생성기 CLI(`module_layer_baseline.py`, B2 전 :553)가 써서 B2에서 제거 |
| 14 | R `module_layer_ratchet.py:71` | `_moves` 이동 검증 | 유지. 입력이 PR 델타 `[[move]]`(B1) |
| 15 | R `module_layer_ratchet.py:132` | `compare` | 변경 없음(B1에서 재사용) |
| 16 | R `module_layer_ratchet.py`(B2 전 :207) | `reduced_baseline` 감소 기록 | 제거(B2) |
| 17 | R `scripts/audit_maintainability/checks/module_layers.py:416` | `_run`: 커밋 파일과 비교 | base 결정·측정, 승인 델타, 추가 규칙, 요약(B1) |
| 18 | R `module_layers.py:435` | `hard_gate=True` | 변경 없음(필수 판정 유지) |
| 19 | R `scripts/audit_maintainability.py:52` | check 등록 | 변경 없음 |
| 20 | R `scripts/audit_maintainability/layer_moves.toml:1` | 이동 3건 | 삭제(B2). 3건은 이미 main에 반영돼 델타 아님 |
| 21 | R `scripts/ci-script-checks.sh:877` | 래칫 시험 묶음 | `tests.test_module_layer_gate` 추가(B1) |
| 22 | R `scripts/ci-script-checks.sh:890` | "Maintainability audit" 실행 | 변경 없음 |
| 23 | R `.github/workflows/ci-pr.yml:1380` | Script checks runner, `:1457` 실행 단계, `:1030` `CI required`의 needs | 변경 없음. base는 이벤트 payload |
| 24 | R `.github/workflows/ci-main.yml:462` | Main script checks | 변경 없음. base=`before` 사후 검사 |
| 25 | R `.github/workflows/ci-nightly.yml:639` | nightly scripts(`SCRIPT_CHECK_SELF_TESTS_ONLY`) | 변경 없음. 현행 self-test-only 경로에서는 이 검사가 **실행되지 않는다**(필수 PR·main 경로가 검사) |
| 26 | R `scripts/ci/script_check_inputs.py:39` | 건너뛰기 가능한 자체 시험 목록 | 변경 없음. "Maintainability audit" 비포함을 시험으로 고정(B1) |
| 27 | R `docs/architecture/layers.md:83` | 정본 경로 링크(삭제하면 문서 경로 검사 실패) | B 계약으로 교체(B2) |
| 28 | R `docs/architecture/layers.md:121` | 최초 활성화·감소 갱신(:140)·분류 편입(:167) 절차와 명령(:128·:146·:172) | 승인 파일 절차로 교체(B2) |
| 29 | R `docs/architecture/layers.md:462` | `layer_moves.toml` 링크 | `[[move]]` 안내로 교체(B2) |
| 30 | R `docs/landing-conflict-free-index.design.md:31` | C1b 처방(같은 문서 :46 절, :350 조각) | 이 문서로 대체 표시(이 설계 commit) |
| 31 | R `tests/test_module_layer_ratchet.py`(B2 전 :574) | fixture가 커밋 기준선 + CLI 갱신(B2 전 :648) | base 2-commit fixture와 gate 시험으로(B1·B2) |
| 32 | R `tests/test_module_layer_measurement.py:218` | runner fixture가 기준선 커밋 | base commit 모델로(B1) |
| 33a | R `tests/test_audit_maintainability.py:102` | 공통 `_LayerSrcTree`가 기준선·이동 manifest를 씀 | base commit fixture로(B1, `_run`이 바뀌므로) |
| 33 | R `tests/test_audit_maintainability.py:2375` | `write_baseline`·`generation_command` 경로 시험(:2472) | 진단 출력 계약으로 수정(B2) |
| 34 | R `tests/test_giant_deadline_entrypoints.py:133` | fixture가 기준선을 커밋 | 기준선 커밋 단계 제거(B2) |
| 35 | R `tests/test_module_layer_classification.py:15` | 편입 API 시험(`tests/test_module_layer_baseline_records.py:15` 포함) | admit 승인 검증 시험으로 이전(B2) |
| 36 | E `agentdesk-issue-pipeline/scripts/prepush_gates.sh:129` | 합성 병합에서 audit `--check` | 그대로 동작(merge-base = origin/main). `MODULE_LAYER_BASE_REF=$base` 명시와 캐시(B0) |
| 37 | E `agentdesk-issue-pipeline/scripts/merge_main_regen.sh:41` | 이 파일 충돌은 `MERGE_CONFLICT`로 사람에게 | main에 파일이 없으면 `git rm`(modify/delete 수용, :29 선례)(B0) |
| 38 | E `coord-tools/build_bundle.sh:18` | 충돌 시 ours(`git show :2:`) 채택 | stage 2가 없으면 `git rm`(빈 파일 부활 방지)(B0) |
| 39 | E `coord-tools/build_bundle.sh:65` | 묶음마다 `--update-reductions` 재생성 commit | 파일이 없으면 건너뛰고 gate 요약을 로그·PR 본문에(B0) |
| 40 | E `coord-tools/settle_gen_conflicts.sh:17` | 충돌 해결 뒤 기준선 재생성 | 이 파일 처리 제거, modify/delete는 `git rm`(B0) |
| 41 | E `coord-tools/open_landing_pr.sh:19` | 생성물만 충돌이면 예외 개설 | 예외 경로 소멸. 6.2 다섯째 줄의 run 선택 조건(push·현재 main SHA·같은 attempt의 완료 산출물, 누락·대기는 거부)으로 사후 검사 확인 뒤에만 개설(B0). 시험: 같은 SHA push 빨강+dispatch 초록, 현재 SHA 대기+이전 SHA 초록, 산출물 누락 세 fixture 모두 거부 |
| 42 | E `/home/adk/adk/bin/review_wait_tests.sh` | 합성 병합에서 이 파일은 main 쪽 채택 | modify/delete에서 `git rm`인지 확인 필요([U1])(B0) |
| 43 | E `agentdesk-issue-pipeline/SKILL.md:87` | 리뷰 대기 시험의 기준선 처리(:393·:395 착지 예외 목록) | 착지 직후 문구 개정 |
| 44 | E 레인 브리프 관례(예: `coord-tools/` 아래 착지 준비·수리 브리프의 "`--update-reductions` 증가 0" 증거, "생성물은 생성 명령으로") | 증거 줄을 gate 출력 한 줄로 교체(착지 직후) |
| 45 | E `agentdesk-issue-pipeline/scripts/batch_conflicts.sh` | 파일 쌍 충돌 계수 | 변경 없음(이 파일 충돌이 사라질 뿐) |

`.gitattributes`는 바꾸지 않는다(union·driver 추가 없음).

## 8. 조각과 크기

`pr_cap_prod.py` 상한은 prod 30파일/순증 +1500이다. `*/generated/*`·tests·docs는 세지 않는다.

| 조각 | 내용 | 예상 prod 파일 / 순증 |
|---|---|---|
| B0 (저장소 밖) | 7절 36~42의 도구 호환 수정. main에 파일이 있는 동안은 무동작이라 **착지 전에 미리 배포**한다 | 저장소 prod 0 |
| B1 (리뷰 조각 1) | `module_layer_gate.py` 새 모듈(base 결정·측정·병렬 실행, 승인 로드·델타·검증, 추가 4규칙, 요약, `--draft-approval`), `module_layers.py` `_run`, `module_layer_ratchet.py` 입력 함수 제거. 시험: base 2-commit fixture, 승인 델타·불변·쓰이지 않음, 분류 해제, 역방향 모양, 분석기 변경, 묘비, base 결정 규칙(PR·push·로컬·override 거부), "Maintainability audit" 비건너뜀 | 3파일 / +300~+400 |
| B2 (리뷰 조각 2) | 커밋 파일·`layer_moves.toml` 삭제, 생성기 CLI 세 모드와 갱신 함수 제거, `module_layer_ratchet.py`의 `MOVES` 상수 제거(B1 동안은 생성기 CLI가 씀), README·`layers.md` 개정, 시험 정리, 승인 디렉터리 README, **두 설계 문서의 경로 선언 전환**(이 문서의 `proposed` 선언 제거, 두 문서에서 삭제된 두 파일 인용에 `doc-path-refs: removed` 선언) | 2파일 / −200~−260 (생성물·docs 제외) |
| 착지 | B1+B2를 한 브랜치로 합친 개별 착지 PR 하나 | 5파일 / 약 +60~+190 |

B1 단독 트리도 검사는 통과한다(묘비 규칙은 "base에 없고 head에 있음"만 보므로 B2 전의 커밋 파일과 공존하고,
새 검사는 그 파일을 읽지 않는다). 그래도 사용자 조건에 따라 따로 착지하지 않고 B1+B2 결합 트리로 검증·착지한다.
B2만 착지하면 검사가 파일을 못 찾아 모든 PR이 빨강이 되거나(묘비 없음) 비게 된다. 그래서 분할 착지를 금지한다.

## 9. 전환 계획

### 9.1 순서

1. B0을 배포한다(도구 시험 `test_merge_main_regen.py` 등에 modify/delete 사례 추가). 무동작임을 확인한다.
2. B1·B2를 각각 리뷰받는다(B1은 새 불변식이므로 첫 리뷰 pro). 합친 착지 브랜치에서 전체 시험과
   prepush static을 돌린다.
3. 일반 착지 차례에 **개별** 착지 PR로 연다(결정 2). 다른 기능과 묶지 않는다. 그 PR 자체의 CI가
   새 검사로 main tip 대비 비교를 한다(base 측정은 파일을 읽지 않으므로 main에 파일이 아직 있어도 무관).
4. 머지 직후 9.4의 알림을 보낸다.

### 9.2 빈 구간이 없다는 논증

- 필수 경로: `CI required`(`ci-pr.yml:1022`) ← needs `scripts`(:1030) = Script checks runner(:1380) ←
  "Run script checks"(:1457) ← `ci-script-checks.sh` cargo 샤드 "Maintainability audit"(:890) ←
  `audit_maintainability.py --check` ← module_layers `hard_gate=True`(`module_layers.py:435`).
- 트리거는 `pull_request` opened·synchronize·reopened·ready_for_review(`ci-pr.yml:5`), workflow 경로 필터 없음.
  job 조건은 draft·cancelled뿐이고(:1383), 이 검사는 입력 경로로 건너뛰지 않는다(6.2 마지막 줄).
- workflow와 셸 호출 경로를 고치지 않으므로 착지 PR 자체(pull_request 병합 ref = main + 착지 PR)에서 새 검사가
  돌고, 그 뒤 모든 PR의 병합 ref에도 main의 새 코드가 들어간다.
- 착지 전에 옛 검사로 초록이 된 PR이 착지 뒤 다시 돌지 않고 머지되는 경우(strict 꺼짐):
  그 PR이 커밋 파일을 고쳤으면 modify/delete 충돌로 머지 자체가 막혀 재병합·재CI를 거친다.
  고치지 않았으면 옛 검사가 유효한 보호였고, 머지 commit은 main push CI의 새 검사(`before` 비교)를 받는다.
- 따라서 "파일은 없는데 검사도 없는" 시점은 없다. main에 파일이 있는 동안은 옛 검사, 같은 commit부터 새 검사다.

### 9.3 전환 직후 진행 중 브랜치

| 브랜치 상태 | 겪는 일 | 처리 |
|---|---|---|
| 커밋 파일·이동 manifest·측정 코드를 건드리지 않음 | main 병합이 깨끗함 | 알림대로 main 병합만 |
| `--update-reductions`로 커밋 파일을 고침(대부분의 감소 브랜치, 착지 묶음 전부: `coord-tools/build_bundle.sh:65`) | modify/delete 충돌 | `merge_main_regen.sh`(B0)가 `git rm`으로 자동 수용. 감소는 B가 다시 계산하므로 잃는 것 없음 |
| `--admit-classification`·`--reclassify`로 증가·재분류를 기록 | 위 충돌 + 병합 뒤 gate 오류 | `--draft-approval`로 승인 파일을 만들어 리뷰(#7353 진행 조각) |
| `layer_moves.toml` 수정 | modify/delete 충돌 | 항목을 `[[move]]`로 옮김(사람) |
| 측정 코드(`module_layers.py` 등) 수정 | 내용 충돌 | 사람이 병합. 분석기 변경 승인 필요할 수 있음 |
| 열린 착지 PR·대기 묶음(`refs/review/batch-*`) | 모두 커밋 파일을 고쳤으므로 충돌, CI 미실행 | 묶음은 기능 head에서 `build_bundle.sh`로 다시 만든다. 열린 PR은 재병합 push 1회(CI 1바퀴) |

리뷰 대기 시험·합성 병합 도구는 B0 뒤에는 modify/delete를 자동 수용한다. 단 WSL 도구는 [U1]이다.

### 9.4 main 병합 알림

코디네이터가 머지 확인 즉시 `coord_notify.sh`로 활성 레인 전체와 착지 준비 레인에 보낸다.

```text
[main] #8131 B 착지 <sha>: module_layers.json 삭제, CI가 main을 매번 다시 계산
- 지금 main 병합: bash merge_main_regen.sh . origin/main (이 파일 modify/delete는 자동 삭제)
- --update-reductions/--admit-classification 명령 폐지. 증가 승인이 필요하면
  python3 scripts/audit_maintainability/checks/module_layer_gate.py --draft-approval <issue>-<slug>
- 확인: python3 scripts/audit_maintainability.py --check (요약 줄의 errors=0)
```

같은 시각에 SKILL.md 문구(7절 43)와 브리프 관례(44)를 고치고, 대기 묶음을 재구성한다.

## 10. 기존 설계 문서와의 경계

| 기존 항목 | B 뒤 |
|---|---|
| 처방표 `module_layers.json` 행(C1b) | **대체**: 커밋하지 않음, base 재계산 |
| "생성물과 감소 전용 래칫" 절의 착지 단계 seed·source anchor·양쪽 zero-rule 합성 | **대체**: 합칠 파일이 없다 |
| 같은 절의 "재생성 diff를 리뷰에서 숨기지 않기" | module_layers 부분은 6.3의 요약이 대신한다. ARCHITECTURE 생성 구간(C1a)에는 그대로 적용 |
| 구현 조각표의 C1b | **대체**: 8절의 B0·B1·B2 |
| "형식 전환은 착지 대기가 적은 창에 단독 PR" | 결정 2로 **변경**: 일반 차례의 개별 PR + 머지 직후 알림 |
| C1a(ARCHITECTURE 생성 블록), C2a(두 목록 union+가드), C2b(상태 차트 색인 분리), C3a·C3b·C3c | **그대로** |
| 효과 측정 5일 비교 | 그대로. module_layers 행의 T 이후 기대값은 파일이 없으므로 충돌 0 |

## 11. 리뷰 집중 항목 응답

- **삭제 순간부터 새 장치가 필수 CI에서 도는가**: 9.2. workflow·셸·pin 무변경이라 착지 PR 자체부터 돈다.
  남은 반례 없음(단 CI 러너 실행 시간 [U3]). 워크플로·pin 무변경이고 셸은 호출 경로가 그대로다.
- **증가가 조용히 통과하는 길**: 6.2 표. 남은 반례는 사람의 승인 판단과, 사후 검사 빨강 뒤 도구를
  거치지 않은 머지로 인한 흡수다.
- **승인 파일이 새 착지 충돌원인가**: 5.5. PR별 새 파일·착지 파일 불변으로 내용 충돌 없음. 같은 이름 add/add만 남는다.
- **전환 직후 레인·대기 브랜치가 막히는가**: 9.3. 커밋 파일을 고친 브랜치는 한 번 modify/delete가 나고
  B0 도구가 자동 수용한다. 열린 착지 PR은 CI 1바퀴를 더 쓴다.
- **코디네이터 도구가 깨지는가**: B0을 미리 배포하지 않으면 깨진다. `coord-tools/build_bundle.sh:18`은
  stage 2가 없는 `git show :2:`의 빈 출력으로 빈 파일을 부활시키고, `coord-tools/settle_gen_conflicts.sh:13`의
  `checkout --theirs`는 main 쪽 판이 없어 실패한 채 :15에서 브랜치 쪽 판을 다시 add해 파일 전체를 부활시킨다.
  `merge_main_regen.sh`는 `MERGE_CONFLICT`로 멈춘다(사람 개입, 부활은 없음). 묘비 규칙이 어떤 부활이든 CI에서
  잡으므로 조용히 통과하지는 않는다. WSL `review_wait_tests.sh`는 [U1].

## 12. 미확인

- [U1] WSL `/home/adk/adk/bin/review_wait_tests.sh`는 이 레인에서 읽을 수 없었다. 이 파일의 "main 쪽 채택"이
  modify/delete에서 `git rm`으로 동작하는지 코디네이터가 확인해 B0에 넣는다.
- [U2] 브랜치 보호 설정(필수 검사 `CI required`, strict 꺼짐)은 SKILL.md 기록(:403)에 따랐고 API로 다시 읽지 않았다.
- [U3] CI 러너(Python 3.11)의 측정 시간과 3.11/3.14 사이 결과 동일성은 B1 구현 때 CI에서 잰다.
  B는 양쪽을 같은 인터프리터로 재므로 판정에는 영향이 없다.
