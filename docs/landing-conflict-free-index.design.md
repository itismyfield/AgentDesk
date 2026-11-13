# 착지 색인·기준선 충돌 제거 설계 (#8131)

<!-- doc-path-refs: proposed docs/architecture/layer-records/ scripts/audit_maintainability/layers.d/ -->
<!-- doc-path-refs: external prepush_gates.sh merge_main_regen.sh merge_autoresolve.py -->

설계 기준: `59bc11edf28411304cee5e75cc2fc4fdc051c483` (2026-11-13 KST).
이 문서는 설계와 저장소 밖 실험의 결과다. 제품 코드·설정·파일 형식은 변경하지 않았다.
구현과 적용 뒤 5일 관측은 후속 조각의 완료 조건이며, 이 설계 완료로 이슈를 닫지 않는다.

## 결정과 범위

생성 결과는 **전체 소스 병합 뒤** 공식 생성기로 동기화한다. 감소 전용 기준선은 양쪽의
승인된 상한과 zero-rule을 먼저 보존한다. 두 텍스트 목록에는 키 기반 병합 검사를 먼저
배선한 뒤 `merge=union`과 정렬을 함께 적용한다. 손수 쓴 색인은 항목별 원본으로 분리한다.
Rust 선언과 책임 분류 설정에는 일반 텍스트 union을 적용하지 않는다.

사용자 결정대로 형식 전환은 착지 대기가 적은 창에 **단독 PR**로 넣는다. 코디네이터가
#7353 S4-17·S14a·S14b·S14c·S14d의 실제 착지 여부를 확인하고, 전환 직후 모든 활성 레인에
새 main 병합과 생성 명령을 알린다. 이 설계 레인은 고정 base를 유지하며 main을 병합하지 않는다.
#8121은 공통 합성 병합 실행기의 정본을 소유하고, 이 이슈는 파일별 정책과 시험을 제공한다.

8개 대상 밖의 `owner_input.rs`·`effect_tests.rs` 의미 충돌은 그대로 실패시킨다.
`.github/workflows/statecharts-tlc.yml`의 timeout 30↔45도 실행 예산의 의미 충돌이다.
현재 base의 값은 45분(:33)이므로 union·최댓값 선택 대상으로 추가하지 않는다.
상태 차트는 해당 없음: 파일 형식과 착지 도구의 설계이며 제품 상태 전이를 바꾸지 않는다.

## 파일별 처방표

| 파일 | 부류 | 처방 | 적용 순서·근거 |
|---|---|---|---|
| `scripts/audit_maintainability/baselines/generated/module_layers.json` | 생성된 **승인 기준선** | ① 착지 단계에서 승인 상한 합성 → source anchor 확정 → `--update-reductions`; custom 재생성 driver는 채택하지 않음 | C1b. #7353의 매핑·이동 승인과 구별. 양쪽 zero-rule 소실 금지 |
| `src/domain/mod.rs` | 손수 편집 코드 | ④ 현재 파일 유지, 모듈명 정렬·명시 선언 유지 | 이미 21개 정렬 선언(:1). 빈 glue·하위 파일화만으로 공유 선언점이 없어지지 않음 |
| `scripts/audit_maintainability/layers.toml` | 손수 편집 책임 설정; rank·prefix 우선순위가 의미를 가짐 | ② 정적 rank는 유지, 책임 기록은 키별 `layers.d`로 이동; 전환 전에는 ④ | C3b. #7353 대기 조각 착지 뒤. 배열 행 순서는 무의미하나 같은 키의 책임 변경은 의미 충돌 |
| `ARCHITECTURE.md` | 손수 문서 + 생성 구간 | ① 표시된 두 블록만 충돌 해소하고 전체 병합 뒤 공식 생성 | C1a. 기존 해소기 재사용. 블록 밖 충돌은 실패 |
| `docs/architecture/layers.md` | **전부 손수 편집 문서** | ② 검토 기록을 항목별 문서로 분리, 안내 본문 유지 | C3a. 현 base에는 생성 표식·생성 경로가 없다. 전체 파일 재생성 금지 |
| `docs/statecharts/expected.tsv` | 순서가 무의미한 cfg 키 목록; 기대 위반은 검사 정책 | ③ 키 검사 + 정렬 + `merge=union` | C2a. 기존 중복·cfg 누락 검사 보존, 필수 CI에 가벼운 검사를 새로 연결 |
| `scripts/ci/test-removal-justifications.txt` | 순서가 무의미한 예외 목록 | ③ 범위가 겹치는 키 검사 + 정렬 + `merge=union` | C2a. 삭제·수정 경합을 파서만으로 잡을 수 없어 별도 병합 검사 필요 |
| `docs/statecharts/README.md` 색인 | 손수 편집 색인 + 설명 | ② `index.d/<stable-key>.md`를 모아 색인 블록 생성 | C2b. 현 문서는 중복·충돌 행 검사가 없으므로 union 금지. 설명은 보존 |

`.gitattributes`는 위 8개를 처리하는 정책 파일이다. C2a의 정확한 두 경로에만 union을 추가한다.
현재 등록된 `regen-inventory` 두 경로(:22)는 유지한다. `*.md`, `*.json`, `*.toml`, `*.rs`
와일드카드는 추가하지 않는다. `.gitattributes` 자체의 충돌도 정책 리뷰 대상이다.
8개 중 물리적인 행 순서를 실행 순서로 읽는 목록은 확인되지 않았다. 층의 rank 값과 prefix
우선순위에는 의미가 있으므로 이를 배열의 물리 순서와 혼동해 union 가능 목록으로 분류하지 않는다.

## 생성물과 감소 전용 래칫

### 현재 계약과 결정성

`module_layer_baseline.py:228`은 키와 id를 정렬해 한 레코드를 한 줄로 출력한다.
:210의 payload에는 `input_sha`, 그 SHA를 포함하는 `generation_command`, 매핑 hash가 들어간다.
:455는 인자가 commit으로 해석되는지 확인하고, :459는 그 commit과 작업 소스
(`src`, `Cargo.toml`)의 일치, :462는 미추적 소스 부재를 검사한다.
따라서 **같은 소스 트리라는 조건만으로 바이트 결정성이 성립하지 않는다**.
같은 source commit·매핑·이동 manifest·승인 기준선·옵션·생성기 버전이 입력이어야 한다.
다른 commit에 같은 소스를 담으면 provenance 두 필드가 달라진다. SHA를 지우거나 고정하지 않는다.
실제 빈 commit 실험에서도 tree `8fbbc2642b246f53a5362eac40f88e841f552cc5`는 동일했고,
base와 `677e520b6c39267e270029fe906a17c72cb83446` 사이 결과 차이는 정확히 이 두 필드뿐이었다
(`d1-same-tree-different-sha.json`).

실험 사본 `source-copy`에서 실제 base SHA를 입력으로 dry-run 두 번과 갱신 두 번을 실행했다.
네 결과의 SHA256은 모두
`596d0ef6bce79c04b30b36d00bdc0b5ca3a3a79d74f91707c5bb0d4a7a015f4d`였다.
각 실제 측정은 약 33초였고 cargo는 호출하지 않았다.
이는 고정 입력의 반복·갱신 멱등성 증거이며, 모든 Python/OS 조합의 결정성 증명은 아니다.
원시 명령·stderr·바이트는 외부 로그 `d1-determinism-summary.json`과 `d1-module-*`에 있다.

`module_layer_ratchet.py:141`은 id별 새 역의존·증가·zero-rule 부활을 차단한다.
판정 불가 edge·uncertainty·미분류의 일반 증가는 기존대로 경고다(:3, :169).
숨긴 역의존과 연관된 불확실성 증가는 차단한다. 이를 모두 오류로 바꾸거나 모두 흡수하지 않는다.
`reduced_baseline`(:214)은 승인된 각 count와 실측의 최솟값만 보존하고, 사라진 id를 zero-rule로
기록한다. 새 경고 debt를 승인 상한으로 흡수하지 않는다(:224).
`write_baseline`(:244)의 신규 생성 모드로 파일을 지웠다가 다시 만드는 처방은 금지한다.

### 착지 단계에서의 안전한 처리

공통 실행기는 base·ours·theirs의 commit과 tree를 고정한다. 다음 순서를 합성 검사와 실제 착지에
동일하게 적용한다. 여기서 source anchor는 합쳐진 소스를 담은 commit이며 최종 생성물 commit과 다를 수 있다.

1. 일반 코드·설정·수동 문서 충돌을 먼저 판정한다. 알려진 생성 경로라는 이유만으로 성공 처리하지 않는다.
2. 기준선 세 버전을 JSON으로 읽고 기존 `_validate`로 검증한다. 양쪽이 같은 기준에서 감소한 경우
   `(section,id)`별 상한의 최솟값과 zero-rule 합집합을 seed로 만든다. 한쪽의 zero는 양쪽보다 우선한다.
   양쪽 positive/zero identity 집합의 누락·추가, schema/count-unit 차이, endpoint/kind 차이는 실패한다.
   경고 section도 승인 id만 유지하고 상한을 올리지 않는다. 합계는 개별 기록으로 다시 계산한다.
3. **1차 자동 범위는 공통 매핑 hash와 변경 없는 이동 manifest인 순수 감소**다.
   매핑·이동·admission·record-upgrade가 함께 바뀌면 `BASELINE_POLICY_CONFLICT`로 돌려준다.
   옵션을 양쪽 `generation_command`에서 주워 자동 승인하거나 임의의 `--reclassify`를 붙이지 않는다.
   #7353처럼 매핑을 바꾸는 착지는 코디네이터가 검토된 prefix·이동·admission을 명시해 처리한다.
   더 넓은 자동화는 해당 조합의 fixture와 독립 리뷰를 추가한 뒤 별도 조각으로 연다.
4. 유효한 seed와 표시된 문서 블록을 index에 올려 미해결 stage를 없앤다. 소스가 완전히 합쳐진
   **정상 커밋**을 격리 합성 사본에서 만든다(일반 hook 유지). 이 SHA를 source anchor로 사용한다.
   driver 실행 시점의 `HEAD`, merge-base, 아직 없는 merge commit, tree SHA는 대용할 수 없다.
   이미 결과 기준선이 있고 그 anchor가 현재 소스와 일치하는 무변경 재실행이면 기존 anchor를
   재사용한다. 매번 최종 `HEAD`로 provenance를 다시 찍어 무한 갱신하지 않는다.
   같은 입력 pair의 재시도는 source tree·seed·도구 버전을 확인한 뒤 기록된 anchor를 재사용한다.
5. 그 anchor에 대해 다음 공식 명령을 실행한다. 먼저 `--dry-run` 결과를 보존하고 실제 갱신과
   바이트 일치를 검사한다. 최종 결과의 `input_sha`는 anchor이며 자기 자신인 최종 commit SHA를 요구하지 않는다.

   ```sh
   python3 scripts/audit_maintainability/checks/module_layer_baseline.py \
     --input-sha <source-anchor-commit> --update-reductions --dry-run
   python3 scripts/audit_maintainability/checks/module_layer_baseline.py \
     --input-sha <source-anchor-commit> --update-reductions
   python3 scripts/generate_inventory_docs.py --review-docs-only --sync-giant-docs
   python3 scripts/audit_maintainability.py --check
   python3 scripts/generate_inventory_docs.py --review-docs-only --sync-giant-docs --check
   ```

6. 양쪽 승인 상한·zero-rule을 최종 결과와 id별로 다시 비교해 완화가 없음을 확인한다.
   그 뒤 정상 생성물 커밋을 만든다. seed만 담은 중간 SHA를 review/착지 ref의 head로 공개하지 않는다.
   source anchor는 최종 head의 조상으로 함께 전달되므로 그 중간 diff도 리뷰 artifact에 포함한다.
7. 예상하지 않은 경로 변경, 생성 오류, 남은 conflict marker, 검사 실패는 정상 실패다.
   raw 충돌은 기록하되 **재생성·검사 성공 후에만** 기계적 충돌을 복구 완료로 센다.

한쪽 기준선만 택한 뒤 재측정하면 다른 쪽의 **역사적 0**을 잃을 수 있다.
실험에서는 한쪽 seed가 부활한 역의존을 허용했고, 최솟값 seed는 `zero-rule` 오류로 거부했다.
두 독립 감소를 합친 fixture는 positive 0·zero-rule 2를 보존하고 기존 비교기를 통과했다.
이는 실제 API를 쓰는 레코드 실험이며, 완성된 범용 기준선 병합기의 구현 증거는 아니다.

### 재생성 diff를 리뷰에서 숨기지 않기

생성 경로의 diff를 무조건 버리고 PR을 자동 승인하지 않는다. 착지 도구가 PR 본문에 다음을
필수 첨부하고, 갱신 실패·누락이면 착지 검사가 실패하도록 한다.

- base/head/source-anchor SHA, 생성기·옵션, 생성 전후 파일 hash와 raw diff artifact.
- 파일별 추가·삭제 줄, id별 감소·새 zero-rule·경고 변화와 양쪽 승인 seed의 출처.
- 수동 영역 diff가 없다는 검사 결과 또는 별도 수동 변경 목록.
- 소스 변경 없이 provenance만 바뀐 경우 그 사실과 두 provenance 필드의 diff.

레인이 리뷰한 source에서 착지 source가 달라졌으면 해당 source delta를 리뷰한다.
자동화가 정책 충돌을 해소하거나 새로운 책임 분류를 승인할 수 없다. 템플릿 문구만으로
가드를 선언하지 말고 실행기가 artifact 생성·본문 반영 여부를 판정해야 한다(#8121 계약).

### ARCHITECTURE와 수동 문서 경계

`ARCHITECTURE.md`의 허용 경계는 다음 두 쌍뿐이다.

```text
<!-- BEGIN GENERATED: SRC TREE --> … <!-- END GENERATED: SRC TREE -->
<!-- BEGIN GENERATED: TOP LEVEL MODULE MAP --> … <!-- END GENERATED: TOP LEVEL MODULE MAP -->
```

현재 생성기는 `generate_inventory_docs.py:1880`의 치환과 :1901의 두 호출을 쓴다.
공통 실행기는 표식별 정확히 한 쌍·정방향·비중첩을 먼저 확인한다. 현 치환 함수는 첫 위치를
사용하므로 중복 표식 검사는 C1a에서 별도로 보강한다. 표식 자체가 충돌했거나 블록 밖
충돌이면 그대로 실패한다. 수동 구간을 포함하는 전체 파일 `--ours/--theirs` 선택은 하지 않는다.
`--sync-giant-docs`는 `docs/agent-maintenance/change-surfaces.md`의 `GIANT PATHS` 블록도
갱신하므로 그 경로를 정확히 생성 산출물 목록에 포함한다(:2045).

**`docs/architecture/layers.md`는 생성물이 아니다.** :1의 설명·책임 표와 :37 이후의 실행 계약,
분류 승인 및 이동 기록을 손수 쓴다. `generated_documents`(:2059)에도 이 경로는 없다.
실제 생성 명령 두 번과 `--check`에서 이 파일의 hash는 모두
`7d23454a4d0b1eae2ddbfbd4882976b93e4e19044c7378144ef71dfc40596d12`로 보존됐다.
같은 세 실행의 ARCHITECTURE hash도 `ca1d0908fdfc21bbc627ad51faf337ec6369910a29ddc765a2e7b025c603824c`로
같았고 route·worker·GIANT PATHS 산출물도 각각 같았다. 생성기의 기존 giant 등록 경고는
stderr에 보존했으며 경고를 없애기 위해 registry나 날짜를 바꾸지 않았다. C3a는 안내·현재 계약은
유지하고 검토 기록만 `docs/architecture/layer-records/<stable-key>.md`로 옮긴다.
기존 링크 anchor는 안내 본문에 연결을 남겨 보존하고, 새 기록은 새 파일로 추가한다.
목록을 자동 생성한다면 별도의 새 표식과 전용 생성기를 명시하며 기존 inventory 명령이
이 파일을 생성한다고 문서에 쓰지 않는다. 같은 항목의 책임 변경은 계속 수동 리뷰 대상이다.

## 목록·코드·설정의 병합 계약

### 두 목록: union은 가드와 함께 적용

`statecharts_tlc.py:308`은 cfg 경로를 dict key로 읽으며 :323에서 중복 cfg를 거부한다.
:473은 디스크의 cfg와 목록의 누락·초과를 검사한다. 선택기는 정렬하며(:373), expected 행의
본문 변화는 실행 대상으로 삼는다(:387). 행 순서는 의미가 없지만 기대 위반을 바꾸는 행 편집은
의미가 있다. rule 이름은 set으로 읽으므로 한 셀의 반복 이름은 현재 오류가 아니며,
이번 변경이 기대 위반을 늘리는 근거가 될 수 없다.

`nextest_ci.py:691`은 `(binary-id test, kind, platform)`으로 읽고 :710에서 같은 범위 또는
unscoped와 scoped가 겹치는 중복을 거부한다. 서로 다른 명시 platform은 허용한다.
`cmd_base_diff`(:753)는 손실에 사유가 없으면 오류(:803), 오래된 unscoped 사유 중 완전히
착지한 삭제는 경고로 처리한다(:833). 목록 순서는 검사 결과에 영향을 주지 않는다.
행 누락은 실제 시험 손실과 비교할 때 검출한다. **파서만 호출해서 누락을 검출했다고 하지 않는다.**

C2a는 Git 속성을 넣기 전에 다음 가벼운 필수 검사를 CI contracts와 로컬 착지에 연결한다.
statecharts workflow는 현재 :4에 `Alert only: not a required check`라고 명시돼 있으므로,
그 workflow만으로 union의 필수 보호를 충족했다고 볼 수 없다.

1. 세 입력과 결과를 기존 파서로 읽고 논리 키별 정상 3-way 결과와 실제 병합 결과를 비교한다.
   양쪽이 같은 키를 다르게 바꿨으면 실패한다. 삭제↔수정·서로 다른 기대 위반·다른 사유의
   동시 추가도 포함한다. 동일한 변경은 한 항목이어야 한다. scope 겹침도 기존 규칙으로 검사한다.
2. 정렬은 그 뒤 실행한다. expected는 cfg 경로, 사유는 `(test,kind,scope)`로 정렬한다.
   `sort -u`로 중복 정책을 숨기지 않는다. 행의 safety/liveness 열과 사유 내용은 변형하지 않는다.
3. 주석을 무작정 데이터와 함께 정렬하지 않는다. expected의 첫 6줄은 헤더이고, :37·:277에도
   설명이 있다. 전환 때 해당 설명을 명시적 cfg 키를 적은 헤더 설명으로 이동해 범위를 보존한다.
   이후 새 설명은 개별 사유/보고서에 둔다. 삭제 사유 파일은 첫 6줄 헤더를 유지한다.
4. cfg 온디스크 대조, 변경 cfg 선택, 기존 TLC 실행과 시험 base-diff를 그대로 유지한다.
   syntax/정렬 검사가 TLC·실제 시험 손실 검사를 대체하지 않는다.

`expected.d/*.tsv`는 키별 원본 분리로 같은 끝 구간 충돌을 없앨 수 있지만, 현재 371개 행을
파일로 만들고 모든 읽기·이전 commit 조회·변경 선택(:387, :442)·workflow 경로 필터를
한꺼번에 이행해야 한다. flat 목록을 생성물로 남기면 생성 충돌도 여전히 처리해야 한다.
중복 검사와 키 기반 검사를 이용할 수 있는 현재에는 C2a를 택한다. 사유 목록의 디렉터리 분리도
880개 기존 항목과 `nextest-base-diff.sh:35` 등 소비자 이행 비용 때문에 보류한다.
동일 cfg/동일 예외를 고치는 충돌은 분리 후에도 의미 충돌로 남는다.

### README 색인

base의 색인은 46행이며 cfg가 있는 area는 14개다. 한 area에도 서로 다른 주제가 여러 개다.
영역별 14파일만으로 나누면 같은 인기 영역에서 다시 충돌하므로 **색인 항목별 stable key**를 쓴다.
각 `index.d/<key>.md`는 key와 기존 표 한 행의 내용을 소유한다. 생성기는 파일을 key순으로 읽고
중복 key·빈 행·잘못된 표 행·깨진 로컬 링크를 거부한 뒤 README의 새 `STATECHART INDEX`
표식 내부만 만든다. 제목·When to add·Run 및 색인 뒤 설명은 수동 영역으로 보존한다.
같은 항목의 상태를 두 파일로 복제하지 않도록 key 목록을 전환 PR에서 명시적으로 리뷰한다.
빌드 없는 drift 검사를 필수 CI에 배선하고, 표식 밖 충돌은 실패시킨다.

현재는 이 색인의 키 중복이나 모순을 검사하는 전용 검사가 없다. 링크 존재 검사는 내용의
정합성을 증명하지 않는다. 실험의 README union은 같은 key의 서로 다른 두 행을 남기고 성공했다.
따라서 README 전체에 union을 켜는 안은 채택하지 않는다.

### Rust glue와 책임 매핑

`src/domain/mod.rs:1`은 이미 `domain/<x>.rs` 또는 `domain/<x>/mod.rs`를 가리키는
21개 정렬 선언이다. Rust는 파일을 만들기만 해서는 module을 발견하지 않는다.
빈 glue, 각 leaf의 디렉터리화, 알파벳 순서 고정은 이 등록점을 없애지 못한다.
현재 정렬을 유지하고 동일 인접 구간의 선언 충돌은 양쪽 모듈을 확인해 수동으로 해소한다.
build.rs·include 기반 자동 발견은 cfg·visibility·도구의 정적 모듈 분석까지 바꾸므로
이 21줄의 충돌 감소만을 위해 도입하지 않는다. #7353 뒤에도 이 파일이 상위 병목이면 별도 측정한다.

매핑은 `module_layer_baseline.py:67`의 최장 prefix·명시 root 우선 규칙이다.
같은 우선순위에서 서로 다른 층이면 미분류(:85)이지 마지막 행 승리가 아니다.
현재 TOML에는 규칙 367개·unclassified 1개·rank 6개가 있고 `(root,prefix)` 중복은 없다.
행 순서 정렬은 가독성을 높이지만 끝의 공통 괄호 충돌을 제거하지 못한다.

C3b는 `layers.toml`에 schema와 rank만 두고, 책임 원본을
`scripts/audit_maintainability/layers.d/<root-key>--<prefix-key>.toml`로 분리한다.
leaf 파일에는 원래 root·prefix·layer·reason·evidence와 category를 그대로 둔다.
현재 책임 키를 정규화해 **기존 `_ordered(mapping)` hash가 정확히 같게** 다시 모은다.
파일명 인코딩 충돌과 중복 논리 키는 오류다. 임의 로드 순서로 한쪽을 덮어쓰지 않는다.
폴더별 분리는 services 규칙 276개가 한 파일에 몰리므로 채택하지 않는다.

읽기 경로는 `load_mapping`(:60)만 바꾸면 끝나지 않는다. admission의
`git show <previous-rev>:layers.toml`(:479)과 historical replay, 문서·시험 fixture를 함께 이행한다.
이전 revision은 flat 형식만 있으면 flat으로 읽고, 새 형식은 manifest와 해당 revision의
`layers.d`를 `git ls-tree`/`git show`로 읽는다. 신형 표시가 있는데 shard가 없거나
두 형식에 동일 key가 있으면 실패한다. 현재 작업 디렉터리 shard로 과거 매핑을 재구성하지 않는다.
분할 자체가 책임 승인·rank 변경·기준선 인상으로 계산되지 않음을 hash·분류·ratchet 비교로 증명한다.

368개 항목을 한 번에 옮기는 대신 C3b1에서 이중 **읽기**와 검사를 배선하고(단일 key 중복은 금지),
C3b2 이후는 원본 행 제거와 shard 추가를 동일 commit에서 최대 20키씩 수행한다.
마지막 조각이 flat rules를 비운다. 신규 키는 전환 시작 뒤 shard에만 쓴다.
분할 수를 줄이려고 prod 30파일 상한에서 데이터 파일을 제외하지 않는다.

## merge driver·착지 도구 비교와 실험

[Git 공식 gitattributes 문서](https://git-scm.com/docs/gitattributes)의 driver 계약은
`%O/%A/%B`의 임시 파일을 받고 `%A`에 결과를 쓰는 것이다. custom 명령의 등록은 clone의
git config에 있으므로 속성만 배포해서는 실행되지 않는다. union은 내장 driver지만 결과 순서는
보장되지 않는다. 이 때문에 목록 정렬·검사가 필요하다.

| 방식 | 실제 확인·제약 | 결정 |
|---|---|---|
| 내장 union | 독립 expected/사유 추가는 성공. 같은 cfg 두 행은 기존 파서 RED. 삭제↔수정 사유는 부활하고 파서 GREEN | 두 정확한 목록만, 필수 키 검사와 함께 |
| custom 재생성 driver | 등록된 fixture 병합은 성공. 등록 제거 시 같은 두 입력은 충돌. driver가 본 source B는 병합 전 `base b` | 새 생성 파일에는 채택하지 않음 |
| `git merge-tree` + custom | driver가 읽은 `HEAD`·작업 소스는 실행 사본의 기존 값. 합성 결과 tree의 소스가 아님 | 순수 tree 검출 결과를 곧바로 재생성 성공으로 세지 않음 |
| 병합 뒤 착지 단계 | 전체 merged source를 사용할 수 있고 parser·래칫·drift 검사와 diff를 한곳에서 실행 가능 | #8121 공통 실행기로 통합 |

기존 `scripts/git-merge-regen-inventory.sh:19`도 ort driver 시점에는 소스가 완전히
물질화되지 않을 수 있다고 설명한다. 기존 등록 스크립트 `scripts/setup-merge-drivers.sh:18`을
CI runner·각 개발 clone에서 실행하지 않으면 custom driver는 동작하지 않는다. 링크된 worktree의
`--local` config는 공유될 수 있으므로 실험에서 이 레인의 config를 바꾸지 않았다.
실험은 독립 clone의 `.git/config`만 사용했다. clone에는 hook을 끄는 설정을 넣지 않았다.

현재 `batch_conflicts.sh`는 저장소 파일이 아니라 파이프라인 스킬 scripts에 있다(:14).
`git merge-tree --write-tree` 실패를 전부 `CONFLICT` pair로 센다. `prepush_gates.sh:119`는
격리 worktree의 정상 merge가 실패하면 곧바로 합성 실패를 등록한다(:147).
`merge_main_regen.sh:36`과 `merge_autoresolve.py:41`에는 이미 ARCHITECTURE 생성 블록의
자동 해소가 있다. 따라서 그 기능을 재구현하지 않고 공통 실행기에 연결한다.
`coord-tools` 원본은 이 레인 허용 경로에 없어서 조사하지 않았다([U1]).

#8121은 세 호출자가 같은 정책을 사용하게 한다. pair 검사에서는 먼저 순수 merge-tree로
충돌과 tree를 받고, 필요할 때만 격리 사본에 물질화해 정책을 적용한다. `merge-tree` 안의 driver에서
실제 생성을 실행하지 않는다. 해석 불가 ref·Git 오류는 conflict와 별도 error로 센다.
성공한 text merge도 생성물 freshness를 확인한다. 생성 파일의 **충돌 유무만** 검사하면
충돌 없이 섞인 오래된 값이 남을 수 있기 때문이다. 이 레인의 적용 실험은 하지 않았다.

실험 사본은 `/Users/itismyfield/.adk/lane-runs/i8131/exp/`에 보존한다.
`source-copy`는 실제 생성 명령, `merge-lab`은 일반 Git 병합·driver fixture를 담는다.
실행 스크립트는 같은 exp 디렉터리, 원시 로그는 형제 `logs/d1-*`에 있다.

| 실험 | 관측 | 구현 조각의 CI 이행 |
|---|---|---|
| E1 목록 독립 추가 | 기대 행 373·사유 882; union merge rc=0, parser·cfg 대조·선택기 GREEN | C2a: 일반 병합과 역순 병합의 정규화 바이트 비교, 실제 base-diff·TLC도 실행 |
| E2 목록 반례 | 중복 cfg·중복/겹친 scope 사유 RED, 사유 삭제↔수정은 복원되어 parser GREEN | C2a: 삭제↔수정·누락·수정↔수정 키 guard RED 필수 |
| E3 README union | 동일 key에 서로 다른 두 행이 남아 rc=0 | C2b: shard 동일 key 중복 RED, 서로 다른 key 추가 GREEN |
| E4 custom 등록/미등록 | 등록 rc=0, 미등록 rc=1; driver에서 ours HEAD·미병합 B 관측 | C1: runner 설정 유무와 무관하게 착지 단계 결과가 같음을 검사 |
| E5 감소 기준선 | 두 감소 자동 병합, zero-rule 2 보존, 부활 입력은 기존 비교기 RED | C1b: 작은 committed Rust fixture의 실제 CLI·양쪽 zero·경고 no-absorb·매핑 정책 충돌 추가 |
| E6 혼합 문서 | 생성 블록 충돌을 기존 해소기+공식 명령으로 처리, 수동 변경 보존; 수동 제목 충돌은 실패 | C1a: 표식 중복/삭제/블록 밖 충돌·재실행 바이트 시험 |

E5의 fixture driver는 **동일 매핑·동일 id집합의 감소 결합만** 구현한 실험물이다.
운영용 driver가 아니며 저장소에 편입하지 않는다. E4의 첫 미등록 대조는 이미 병합된 branch를
가리켜 `Already up to date`가 나왔다. 그 결과는 성공으로 세지 않고, 기록된 병합 전 SHA로
다시 대조해 rc=1을 확인했다(원시 로그에 정정 보존).
로컬 `/usr/bin/java`에는 Java runtime이 없어 이 설계 레인에서는 TLC를 실행하지 않았다.
TLC가 GREEN이라고 보고하지 않으며, C2a는 CI의 고정 jar·Java로 E1 두 cfg를 실제 실행해야 한다.

## 효과 측정: 같은 5일 비교

이슈 본문의 전측정은 코디네이터 로그 43개에서 집계한 파일별 출현 수다.
한 병합에서 여러 파일이 충돌하므로 합계를 실패 병합 수로 쓰지 않는다.
11-12 합성 실패 10건과 1112b/c의 JSON 2·expected 2·timeout 1은 별도 관측 집합이다.
서로 더해 하나의 baseline으로 만들지 않는다.

미니에서 읽은 status·notices·archive·coord-pids에는 관련 문자열 128줄이 있었지만,
오래된 진행 기록·보고서 diff·중복 전달도 포함돼 있다. 이를 128건의 충돌이라고 해석하지 않는다.
맥의 원시 43개 로그와 정확한 기간·중복 제거 키는 접근할 수 없으므로 [U2]로 인계한다.

전환 시각 T를 KST로 고정하고 `[T-5일,T)`와 `[T,T+5일)`를 비교한다.
혼합 이행 기간은 별도로 기록한다. 원시 로그 집합·hash·시간 경계·집계 명령을 함께 보존한다.
코디네이터는 맥과 미니 양쪽 로그에서 `SYNTH_MERGE_CONFLICT`, `BATCH_FIX`, Git
`CONFLICT (content): Merge conflict in <path>`, `BATCH_CONFLICTS`·`MERGE_CONFLICT`를 수집한다.
메시지 전송 사본·본문의 인용·diff는 제외한다. 이벤트가 없는 옛 로그는 시간·base·head·파일과
원시 위치를 대조해 수동 정규화하고 불확실한 건수를 따로 표시한다.

신규 공통 실행기 이벤트의 필드는 시각·host·caller·attempt-id·base/head/tree·path·정책·결과·소요시간이다.
결과는 `RAW_CONFLICT`, `REGEN_OK`, `POLICY_CONFLICT`, `CHECK_FAIL`, `GIT_ERROR`를 구분한다.
이들은 **추가할 관측 필드**이며 현재 `BATCH_FIX`/`SYNTH_MERGE_CONFLICT`에 모두 존재한다고
가정하지 않는다. 같은 attempt-id/path 재출력은 1회, 다른 재시도는 별도 1회다.
전체 시도 수와 고유 base/head 조합 수도 함께 센다. raw 충돌을 숨겨 최종 실패가 줄었다고 하지 않는다.

| 대상 | 이슈 전측정 | T 이전 5일 raw / 최종 실패 | T 이후 5일 raw / 복구 / 최종 실패 | 시도 수·해석 |
|---|---:|---|---|---|
| module_layers.json | 15 | 미집계 | 관측 전 | mapping 정책 충돌 별도 |
| src/domain/mod.rs | 12 | 미집계 | 관측 전 | 유지 처방 효과 별도 |
| layers.toml 및 후계 shards | 12 | 미집계 | 관측 전 | 새 경로 포함 |
| docs/architecture/layers.md 및 후계 기록 | 11 | 미집계 | 관측 전 | 새 경로 포함 |
| ARCHITECTURE.md | 11 | 미집계 | 관측 전 | 수동/생성 분리 |
| 시험 삭제 사유 목록 | 8 | 미집계 | 관측 전 | 의미 충돌 별도 |
| statecharts README 및 index.d | 5 | 미집계 | 관측 전 | 후계 원본 충돌 포함 |
| expected.tsv | 3 | 미집계 | 관측 전 | cfg 정책 충돌 별도 |
| 전체 합성 실패 시도 / 전체 시도 | 원본 필요 | 미집계 | 관측 전 | 실패율, 고유 pair당 실패도 병기 |
| 전체 batch pair 실패 / 전체 pair | 원본 필요 | 미집계 | 관측 전 | 배치 크기 차이 보정 |

DoD 3의 기록은 5일이 지난 뒤 코디네이터가 채운다. 트래픽·#7353 완료로 충돌 기회 자체가
줄었을 수 있으므로 같은 ref 집합을 정책 전후로 재생한 결과도 보조 지표로 남긴다.
동일 파일의 raw 충돌이 0이 아니어도 기계적 복구 후 최종 실패가 줄 수 있으며, 두 값을 함께 제시한다.

## 구현 조각·크기·착수 조건

아래는 prod 추정이다. docs·시험·generated 제외는 현재 `pr_cap_prod.py` 그대로 적용한다.
도구 정본과 저장소의 변경을 한 묶음으로 계수하며, 실제 commit 사이 실측으로 보정한다.
상한 prod 30파일/순 코드 +1500, PLAN 정지선 +1200을 유지한다. 추정은 실측 승인값이 아니다.

| 조각 | 변경·예상 prod | 착수·완료 조건 |
|---|---|---|
| C1a | 공통 후처리 계약, ARCHITECTURE 기존 해소기 배선·표식 검증·diff artifact: 4~7파일, +250~450 | #8121 실행기 경계 확정. E6와 생성기 check, 수동 충돌 RED. 기존 기능 재구현 금지 |
| C1b | 동일 매핑 감소 seed·source anchor·공식 명령·양쪽 비교: 3~5파일, +300~550 | C1a 뒤, #7353 S14d 등 착지 확인. E5 CLI fixture와 정책 충돌 RED. prod 합계가 +1200 예상이면 분할 |
| C2a | 두 목록 union·키 3-way 검사·정렬·필수 CI/prepush 배선: 4~7파일, +250~450 | 가드 먼저 들어가는 단독 전환. E1/E2·실제 TLC/base-diff, 기존 scope·행 누락 의미 보존 |
| C2b | README 46행을 원본 fragments로 이동·생성/drift 검사: 3~5 prod파일, +180~320; 문서 약 47개 | C2a와 별도 단독 창. 의미·링크·수동 본문 보존. docs 수를 prod 파일 수로 혼동하지 않음 |
| C3a | layers 수동 검토 기록 분리·기존 anchor 연결: 0~2 prod파일, +0~180 | #7353 기록 착지 뒤, 문서 생성 범위 오인 제거. 수동 내용은 기존 diff와 대조 |
| C3b1 | flat/shard 현재·과거 읽기와 중복/hash 검사: 3~6파일, +220~380 | #7353 매핑 조각 뒤. flat↔shard 동일 hash·분류·이전 revision 복원 시험 |
| C3b2… | 매번 최대 20키 원본 이동: ≤21 prod파일, 순 코드 약 +20~100 | C3b1 뒤. 368키 기준 약 19조각; 순 코드만 작아도 30파일을 넘기지 않음 |
| C3c | domain/mod.rs 유지 | 현재 정렬·명시 선언 증거로 재구현 없음. #7353 뒤 관측만 |

C3b의 항목별 분리는 가장 확실히 공통 편집점을 없애지만 약 19개 이동 조각의 비용이 크다.
따라서 먼저 C1·C2로 실측하고, #7353 이후 layers가 병목에서 사라지면 C3b를 강행하지 않고
기존 파일 유지의 관측 근거를 남긴다. 채택 시 파일 수 상한을 넘는 일괄 전환은 하지 않는다.
기능 PR에 형식 변경을 섞지 않고 각 전환 PR의 착지 직후 main 병합을 알린다.

## 완료 증거와 인계

설계 레인의 완료는 문서 1개·외부 원시 로그·실험 사본·고정 base 위 커밋 1개와 review ref read-back이다.
코드 변경·cargo·운영 설정 접근·배포·PR/이슈/DAG 쓰기는 하지 않는다.
기존 감소 래칫·두 파서·ARCHITECTURE 해소기는 이미 base에 있으므로 새 구현으로 세지 않는다.
기존 Python 시험 119개를 24.653초에 실행해 실패 0·skip 0을 확인했다. 범위는 감소·분류·기준선
레코드, expected 파서·선택기·CLI 선택, nextest base-diff다(`d1-existing-tests.log`).

집중 리뷰 항목은 다음과 같다.

- 경계 시각: 5일을 KST 반열림 구간으로 고정, 정확한 기존 구간은 [U2]. 날짜 의존 giant 경고는 보존.
- 동시성: driver의 미병합 소스 읽기 재현, 고정 source anchor 사용. 현재 updater의 lock·원자 교체(:330)는 유지.
- 옛 형식 복원: C3b1이 revision별 flat/shard 읽기와 hash 동일성 시험을 소유한다. 현 코드에 없는 능력을 완료로 쓰지 않음.
- 문서뿐인 가드: 키 검사·필수 CI 배선·artifact/본문 검증은 구현 DoD로 명시. 설계 실험물을 운영 보장으로 승격하지 않음.
- 잘못된 0/PASS: Java 부재·TLC 미실행, parser만의 한계, 관측 전 수치는 명시. 재생성 실패·정책 충돌을 0건으로 숨기지 않음.

[U1] 코디네이터: 허용 경로 밖 `coord-tools`의 실제 합성 병합 진입점·#8121 담당 경계와
PR 본문/artifact 검증 위치를 이 설계의 공통 실행기 계약에 연결한다.

[U2] 코디네이터: 맥 원본 43개 로그·정확한 전측정 5일 경계·중복 제거 기준과
`BATCH_FIX`/`SYNTH_MERGE_CONFLICT` 실제 예시를 제공하고, T 이후 5일 관측을 맡는다.
이는 설계 결론을 막는 운영 설정 질문이 아니며 배포·운영 DB 조회가 필요하지 않다.
