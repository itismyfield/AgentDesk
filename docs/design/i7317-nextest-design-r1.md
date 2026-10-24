…
더 보기
29m 동안 처리함
ChatGPT 답변:
AgentDesk #7317 — CI 시험 실행을 cargo nextest 단일 단계로 단순화

설계 결정: 일반 Linux 라이브러리 시험을 하나의 nextest 실행 단계로 통합한다. 초기 구성은 시간 가중 분할을 사용하는 6개 worker, worker 내부 직렬 실행, worker별 독립 PostgreSQL 서버다. 현재의 PG·non-PG 실행 레인, 디렉터리 기반 선택, 반복되는 targeted 실행, 손으로 관리하는 전체 시험 등록 목록을 제거한다. PG 15 호환성, OS별 실제 실행, mutation, doctest, 비 Rust 검증은 각각의 고유한 보장을 유지한다.

여기서 “단일 단계”는 하나의 공통 실행 방식과 하나의 matrix 단계를 뜻한다. 물리적으로 runner 하나에 모든 시험을 직렬 배치한다는 뜻은 아니다. 사용자가 허용한 시간 가중 partition 방식을 채택한다.

분석 기준은 지정된 
고정 SHA faf4955f448a058bc8be6d0b9253f602f1bf138f와 
이슈 #7317이다. 아래 줄 번호는 모두 이 SHA에 대한 것이다. 이 문서는 설계이며, 실제 실행 집합 동등성·안정성·성능을 이미 측정했다는 의미는 아니다. 구현 과정에서 확인할 항목은 **[U]**로 표시한다.

1. 보장 대조표
1.1 시험 집합을 읽는 기준

현재 구조에서 다음 세 가지는 구분해야 한다.

선언된 시험: Rust 소스와 등록 파일에 존재하는 시험.
선택된 시험: 해당 플랫폼·target·filter에서 선택되는 시험.
실제로 실행된 시험: ignored·필터 제외·조기 종료 등을 제외하고 실행 결과가 나온 시험.

현재 lib_test_inventory_manifest.txt에는 시험 행이 15,099개 있지만, 이것을 특정 Linux PR에서 실제 실행한 개수로 사용하면 안 된다. 플랫폼 조건, ignored, PR별 선택이 개입한다. 현재 선택기는 PR에서 변경 디렉터리와 관련 소스를 기준으로 부분 집합을 고르고, main·강제 전체 실행·해석 실패·빈 선택·40% 이상 선택 등에서는 전체 집합으로 돌아간다. 
test-selection.py:2–11, 53–78

현재 전체 실행의 라이브러리 집합은 다음처럼 이해할 수 있다.

L: 해당 플랫폼에서 발견되는 전체 라이브러리 시험 ID.
Q: 기존 PG 선택식이 고르는 집합. 이름·모듈 단위 선택 때문에 순수한 PG 의존 시험보다 넓을 수 있다.
R: 넓은 PG 제외식에 함께 걸린 non-PG 시험을 복구하는 replay 집합.
기존 전체 실행의 합집합은 Q ∪ ((L − Q) ∪ R) = L.
실제 기본 실행 대상에서는 #[ignore] 시험을 별도로 제외한다.

따라서 전환 비교는 같은 SHA에서 강제 전체 실행한 기존 구조와 새 구조를 대조해야 한다. 임의의 과거 부분 선택 PR과 새 전체 실행을 비교하면 정상적인 커버리지 확대를 “extra”로 오판한다. 
test-selection.py:122–128

1.2 실행 범위와 환경 보장
현재 보장·실제 범위	고정 SHA 근거	nextest 전환 판단
Linux PR: 공통 lib 빌드 이후 PG 8개 숫자 shard와 continuity leg, library sweep 최대 4개 shard가 실행된다. 일반 PR에는 부분 선택이 적용될 수 있다.	
ci-pr.yml:1321–1519, 
1804–1891, 
membership.py:1833–1859	통합. 컴파일된 전체 --lib 집합을 하나의 nextest matrix에서 실행한다. PG/non-PG 레인과 변경 디렉터리 선택은 삭제한다.
PG 직렬성: 숫자 shard는 --test-threads=1로 실행한다. continuity도 각 명령이 직렬이다.	
justfile:197–222, 
test-selection.py:428–449	유지·명료화. worker 전체를 직렬 실행하고 worker마다 독립 PG 서버를 둔다. 프로세스 내부 mutex를 프로세스 간 lock으로 간주하지 않는다.
PG fixture 강제: 명시적 fixture base와 AGENTDESK_REQUIRE_PG=1이 fixture 미설정·생성·연결 실패를 실패로 만든다. ambient host 선택을 막는 별도 검증도 있다.	
postgres.rs:907–930, 
1152–1167, 
fixture_target.rs:123–189	유지. 서비스 기동 성공만 확인하지 않고, fixture 경로와 실패 계약도 검사한다.
환경 차이: 기존 PG·high-risk job은 DATABASE_URL을 설정한다. library sweep은 live PG와 REQUIRE 설정을 가지면서 DATABASE_URL은 설정하지 않는다. 일부 targeted 실행은 AGENTDESK_ROOT_DIR을 제거한다.	
ci-pr.yml:1392–1404, 
1635–1646, 
1803–1829	공통 환경으로 통합하되 [U]. fixture base+REQUIRE를 사용하고 DATABASE_URL, AGENTDESK_ROOT_DIR은 제거한다. 기존 PG/high-risk가 실제 fixture 작업을 계속 수행하는지는 별도 증명한다.
env·DB mutex: env lock, DB lifecycle lock, 재진입 금지, env→DB 순서, create/drop 보호가 존재한다. 이것은 Rust 프로세스 내부 보장이다.	
config.rs:3023–3102, 
postgres.rs:946–1011, 
1092–1126, 
1240–1255	Rust helper는 유지. nextest의 프로세스 분리만으로 이 lock들을 삭제하지 않는다. 실행기 쪽 직렬화와 독립 서버가 프로세스 간 자원 충돌을 막는다.
PR targeted 회귀: footer/discrete marker, session forwarding, terminal/inflight identity, continuity, high-risk recovery 계열을 별도 호출한다. high-risk에는 recovery·health·relay·Herdr·alert/intake/outbox·turn lifetime/cancel/queue·watchdog·Codex resume 관련 selector가 포함된다.	
ci-pr.yml:1460–1498, 
1688–1758, 
justfile:212–222	별도 정상 실행은 삭제. 각 selector의 실제 실행 ID가 새 전체 lib 집합에 포함됨을 확인한 뒤 한 번씩 실행한다. assertion 자체는 유지한다.
PG 15 floor: 실제 서버 버전이 15인지 확인하고 exact ID 20개로 owner proof를 수행한다.	
ci-pr.yml:1521–1623, 
pg15_required_ids.txt:1–20	유지. 별도 PG 15 nextest 실행과 버전 확인을 남긴다. 최신 PG에서 전체 시험을 통과했다는 사실로 대체할 수 없다. 이 20개는 호환성 계약이다.
Relay authority: 정상 target 시험 외에 mutation 3개 shard의 검출 보장이 있다.	
ci-pr.yml:2374–2550, 
check_relay_authority_contract.py:328–440	정상 target은 통합, mutation은 유지. 변형한 소스를 시험하는 작업은 정상 archive 실행과 합칠 수 없다. target 존재·최소 개수·mutation 연결 검증은 nextest list를 입력으로 바꾼다.
Windows PR: workspace/all-targets cargo check와 writer namespace exact 시험 2개를 실제 Windows에서 실행한다. 추가 Win32 build-token context도 있다. macOS PR 전체 실행은 없다.	
ci-pr.yml:1050–1222	유지. check와 exact 2개를 native Windows에서 수행한다. Linux cross-compile로 대체하지 않는다.
main: 전체 library 실행, 별도 lint, non-lib selector 7개, ClaudeBinary doctest가 있다. 다만 non-lib 7개 selector는 고정 SHA의 main.rs·e2e 함수명과 겹치지 않아 소스상 실행 집합이 비어 보인다.	
ci-main.yml:29–280, 
main.rs:1–7, 
smoke_test.rs:139–161, 
666–839	library 통합, doctest 유지. non-lib의 실제 compiled list를 확인한다 [U]. 공집합이면 7개 빈 실행을 빌드 검증으로 정리한다. 새 PR suite를 무심코 --all-targets로 넓히지는 않는다.
nightly macOS·Windows: native all-targets non-PG 실행과 lib replay가 있다. Windows에는 discord_thread_create 직렬 실행이 추가된다.	
ci-nightly.yml:84–220	플랫폼별 실제 ID 집합 유지. 자동 PG 분류를 이용해 library 집합을 만들고, 비 lib target은 기존 선택 결과와 대조한다. 초기 native 전환은 자원 안전성을 위해 직렬 profile로 검증하고 기존 timeout 내 완주를 확인한다 [U].
nightly Linux: PG library, PG가 있는 e2e, multinode, high-risk, CLI smoke, 전체 mutation backstop을 실행한다.	
ci-nightly.yml:222–460, 
540–626	각 보장 유지. 일반 lib 중복은 합치고, e2e target·실제 PG 환경·CLI smoke·mutation은 남긴다.
trusted macOS: 6시간 주기/수동 실행, 변경 판정 실패 시 실행하는 보수적 filter, targeted Rust 시험, fresh-user portable smoke가 있다.	
ci-macos-trusted.yml:3–51, 
84–120	유지. 이 workflow의 현재 selector 합집합을 별도 정책으로 옮긴다. nightly의 전체 non-PG 정책으로 바꾸지 않는다.
main Windows cache warm: all-targets check와 test --lib --no-run이다. 실제 시험 실행이 아니며 advisory다.	
ci-main.yml:593–651	성격 유지. OS runtime 커버리지 증거로 세지 않는다.

추가로, native OS에서 e2e 시험 이름이 실행 결과에 나타난다고 해서 실제 PG 동작까지 검증한 것은 아니다. e2e fixture는 환경에 따라 조기 반환할 수 있다. 실행 ID 동등성과 환경·행동 동등성은 별도 증거가 필요하다. 
smoke_test.rs:35–52

1.3 등록 목록·실행 wrapper가 막는 실수
현재 막는 실수 또는 제공하는 보장	근거	대체·유지·삭제
잘못된 target과 filter 조합으로 0개를 실행하고 성공하는 오류, 잘못된 module 경로	
check_test_target_integrity.py:2–25, 
2099–2207	nextest list의 실제 집합, worker별 비어 있지 않은 배정, --no-tests fail, 실행 결과 대조로 대체한다. 현재 검사도 모든 빈 호출을 실패시키지는 않는다. 비선택 target에만 이름이 있는 일부 name sweep은 보고만 한다.
소스와 lib manifest의 불일치, 잘못된 플랫폼·include ID 예외, 선언했지만 연결되지 않은 시험	
check_test_target_integrity.py:111–158, 
1877–1907, 
2285–2375	manifest는 삭제한다. mod·#[path]·include!·cfg를 따라가는 소스 연결 검사는 추출해 유지한다. compiled list만으로 orphan source를 검출할 수 있다고 주장하지 않는다.
새 시험이 required PR/main의 실제 실행 경로에 활성화되지 않는 오류, 목록 출력만 하고 실행하지 않는 오류, 실패를 삼키는 실행	
check_test_lane_coverage.py:895–991, 
lane_source_parity.py:15–26, 97–106, 
lane_new_id_registration.py:592–740	전체 compiled 집합 자동 실행과 required aggregate의 결과 검증으로 대체한다. helper-only 모듈까지 등록 행을 요구하는 정책은 삭제한다.
PG가 필요한 시험을 non-PG OS 레인에 넣는 오류. 시험 이름만이 아니라 helper 호출과 body를 분석한다.	
check_pg_test_lane_membership.py:582–650, 
841–855, 
1810–1830	Linux 분할에는 불필요해진다. native OS 선택용 자동 분류 핵심은 유지한다. 해석 불명은 위치와 함께 실패시키고, 단순 pg_ 이름 추정으로 대체하지 않는다.
계획 ID와 실행 ID의 차이, 누락·추가·잘못된 skip 인자, 전체 ignored·빈 실행, nested libtest stdout의 중복 집계	
run_test_lane.py:527–638, 
780–814, 
nested_libtest.py:200–278	nextest가 만든 최상위 결과와 worker별 배정을 대조한다. child stdout은 별도 시험 결과로 세지 않는다. 부모의 child 실행·효과·종료 검증은 유지한다.
ignored 집합의 무단 증가·감소와 child entry의 구분	
known_test_failures.txt:195–212, 
nested_libtest.py:37–89	사유·child 소유 관계를 소스 가까이에 두고 자동 추출한다. runnable→ignored 변화는 반드시 diff와 summary에 나타나며, 정당화되지 않은 변화는 실패시킨다.
기존 실패 부채 처리	
known_test_failures.txt:109–187, 
run_test_lane.py:653–691	현재 flaky 15개는 실패 허용 ledger이며, nextest retry와 같은 의미가 아니다. exact ID·사유·이슈·만료를 검토해 최대 1회 retry 정책으로 옮기고, 재시도 후 실패는 반드시 red로 만든다.
known-flaky job 재실행과 infra 재실행	
known-flaky-tests.json:1, 
known-flaky-rerun.py:83–194, 
infra-failure-rerun.sh:422–499, 
566–694	JSON은 현재 비어 있다. 시험 retry는 nextest exact override로 통일한다. infra retry는 별도 유지하되 first attempt·명백한 infra 원인·실제 시험/설정/미상 실패 veto·최신 PR HEAD 조건을 보존한다. #7315 착지 후 재확인한다 [U].
공유 artifact의 checkout/build/host/run 일치 확인. artifact가 불완전하면 재빌드할 수 있지만 시험 실패 자체를 fallback으로 숨기지는 않는다.	
lib-test-artifact.py:39–57, 
124–148, 
199–219	nextest archive와 SHA·digest·빌드 옵션을 결합한다. worker의 임의 재빌드 fallback은 제거하고 artifact 실패로 처리한다. 재실행은 검증된 build/infra 경로로 수행한다.
1.4 nextest가 대체하지 않는 독립 검증

ci-script-checks.sh의 cargo/guards/contracts dispatcher에 등록된 검사 전체와 필수 shell suite 및 tests/*.sh 실행은 유지한다. 삭제 대상은 3절에 명시한 옛 시험 선택·등록·실행기 전용 부분으로 한정한다. 아래는 호출 구간별 보장 분류다. 
ci-script-checks.sh:102–202

독립 보장	근거	판단
shellcheck, PG 사용 감사, Windows fixture, migration checksum, outbox insert, directory fsync, O-shadow/authority/routing, policy DB capability	
ci-script-checks.sh:205–265	유지
SQL inventory, H2, destructive-call ratchet, reachability independence/equivalence	
267–303	SQL/H2는 별도 세션의 실제 착지 상태를 반영. 나머지 계약 유지
idle-kill JS, operator routines, toolchain, await-holding-lock, delivery journal/frontier/input census/dormant boot/intake-outbox/run-owner	
306–377	유지
comment-only 판정, hot-file·production cap, Clippy/dead-code/log-key/blind-save, launchd refresh, runner hardening, timeout·retry·swap·mutation·wiring	
379–458	유지. 옛 job 이름·DAG를 검사하는 부분만 새 구조로 수정
Python e2e harness unit modules, voice media smoke/scenario, relay authority floor	
459–487	유지
deadline/D4B, mutex poison·ROOT env lock, scheduled message·high-risk path, scratch/path/portable/docs/PG tunnel/watchdog/session anchor/release token	
498–688	시험 레인 전용 부분을 제외하고 유지. restart 관련 삭제는 별도 세션 결과에 맞춤
CLI 참조, production/test 분리, Clippy allow ratchet, API 문서·symbol sync·maintenance 감사, 필수 shell suite	
690–779	유지. 기존 warning과 hard failure의 구분도 보존
fmt, policy JS, workspace/all-targets/all-features Clippy와 warning ratchet	
ci-pr.yml:1921–1983	유지. all-features 시험 실행 보장으로 확대 해석하지 않음
Dashboard Node 22 설치·audit·build·test	
ci-pr.yml:1029–1046, 
verify-dashboard.sh:8–36	유지
PR cancellation 범위, PR cache 읽기/main 쓰기, swap·timeout·진단, tracked file 크기 검사	
ci-pr.yml:4–29, 
2123–2139	유지
1.5 required context 9개의 정확한 의미

아래에서 N은 공통 nextest build·계획·모든 worker·실행 집합 감사가 통과한 결과다. 허용된 docs/comment-only skip은 별도 검증을 거쳐 N에 반영한다.

required context — 이름 그대로 유지	현재 의미와 근거	전환 후 의미
Lint	lint runner 성공. 
ci-pr.yml:1921–2022	N AND lint
Script checks	cargo/guards/contracts 3개 결과와 mirror/hardening/writer wiring 검증. 
2300–2372	N AND 세 script 결과 AND 구조 검증
Fast check (ubuntu-latest)	별도 fast compile이 아니라 lint 결과의 mirror. 
1224–1255	N AND lint
High-risk recovery	high-risk runner 결과. 
1768–1802	N。기존 high-risk ID 포함 여부도 감사
Dashboard (Node 22)	dashboard runner 결과. 
1286–1319	N AND dashboard
Fast targeted tests (ubuntu-latest)	PG matrix와 PG 15 owner proof. 
1257–1284	N AND PG 15 proof
relay-authority-contract	relay target와 mutation 결과. 
2551–2618	N AND relay 계약 검사 AND mutations
Library test sweep (ubuntu-latest)	library sweep matrix 결과. 
1893–1919	N
Fast check cross OS required context (ubuntu-latest)	native Windows check와 writer namespace 시험 결과. 이름의 Ubuntu는 mirror runner다. 
2024–2061	N AND Windows check AND Windows exact proof

이름 변경은 필요 없다. 기능 이름을 정리하려고 branch protection까지 동시에 바꾸면 전환 위험만 늘어난다.

현재 mirror는 changes 성공을 요구하고, 실행 대상이면 upstream success만 허용하며, 정당하게 제외된 경우에만 skipped를 허용한다. 이 실패 처리 원칙을 유지한다. 
required-check-mirror.sh:16–49

2. 목표 구조
2.1 job 구성
구성	역할
changes	기존 docs/comment-only 및 workflow별 실행 조건을 보존. 판정 실패를 skip으로 취급하지 않음
rust_build	동일 Rust·feature·profile로 lib archive를 한 번 생성. 필터 없는 시험 목록, 소스 연결 검사, LPT 계획 생성
rust_tests	단일 matrix. 각 worker는 같은 archive, 자신의 generated config, 독립 PG 서버로 cargo nextest run 한 번 실행
test_evidence	if: always()로 결과 수집. worker별 ID·종료 코드·artifact identity·retry 정책을 검사
독립 검사 jobs	기존 lint/scripts/dashboard/Windows/PG 15/mutation 보장 유지
required_contexts	matrix 9개로 기존 이름을 정확히 발행. 각 행은 N과 자신의 원래 검증 결과를 AND

main·nightly·trusted macOS도 동일한 archive/list/run/audit 코드를 재사용한다. 실행 범위는 1절의 플랫폼·target 정책으로 결정한다.

2.2 6개 시간 가중 worker를 선택하는 이유

한 runner에서 모든 PG 시험을 직렬 실행하면 기존 여러 shard의 작업 시간이 누적된다. 반대로 한 runner에서 PG 시험을 무조건 병렬 실행하면 프로세스 내부 mutex로 공유 DB·파일 자원을 보호할 수 없다. 이 설계에서는 fixture 구조 전체를 바꾸는 작업을 추가하지 않는다.

따라서 초기값은 독립 서버 6개 × worker 내부 직렬 실행이다.

시간 분할은 다음과 같이 한다.

필터 없는 compiled list에서 실행 집합을 만든다.
최근 성공한 main의 nextest 결과에서 ID별 실행 시간을 읽는다.
오래 걸리는 ID부터 정렬한다.
현재 누적 예상 시간이 가장 작은 worker에 배정한다.
동률은 정렬된 ID로 결정해 재현 가능하게 한다.
새 ID는 보수적인 기본 가중치로 배정한다. 시간 정보가 없다는 이유로 제외하지 않는다.
시간 자료가 없으면 균등 초기 가중치로 calibration을 수행하고, 이후 실제 nextest 시간으로 갱신한다.

nextest의 내장 partition은 hash/slice 방식이다. 시간 가중 기능이 내장되어 있다고 가정하지 않는다. LPT 계획은 작은 외부 helper가 만들고, 실행은 nextest가 담당한다. 
cargo-nextest

수천 ID를 하나의 -E 인자로 넘기지 않는다. worker별 임시 TOML의 default-filter에 exact (binary_id, test_name) 식을 기록한다. 공통 profile·retry 정책을 합성한 최종 config를 다시 검사한다. exact matching은 명시적 =를 사용하고 filterset escaping을 적용한다. 
cargo-nextest
+1

6개는 성능 검증 전 초기값이다. #7303의 기존 6개 hash shard 추정은 critical path 46.0분이었으므로, 단순히 shard 수를 줄여 45분을 만족한다고 주장할 수 없다. 
#7303 측정 코멘트

검증에서 6개가 기준을 만족하지 못하면 같은 구조에서 7개, 필요하면 8개를 평가한다. 최종 worker 수를 정한 뒤 그 구성으로 DoD 표본을 모은다. build 이후 runner 대기는 여전히 존재하며, 27.8분 대기를 “제거했다”고 표현하지 않는다.

2.3 profile/config 예시

nextest는 검증한 버전으로 고정한다. 현재 공식 changelog에서 확인한 후보는 0.9.149다. Rust는 기존 1.94.1, debug 설정과 feature 범위를 그대로 유지한다. 
cargo-nextest

toml
# .config/nextest.toml

[profile.default]
retries = 0
fail-fast = false

[profile.default.junit]
path = "junit.xml"
report-skipped = "none"
store-success-output = false
store-failure-output = true
flaky-fail-status = "failure"

[test-groups]
shared-fixture = { max-threads = 1 }

[profile.ci]
# worker config 생성 시 자신의 exact ID 집합으로 교체한다.
default-filter = "all()"
test-threads = 1

# 느림 경고다. 134초짜리 정상 시험을 60초에 종료하지 않는다.
slow-timeout = { period = "180s" }

# 부모 종료 후 열린 출력 핸들이 남는 경우를 성공으로 숨기지 않는다.
# 실제 child harness와의 호환성은 shadow에서 검증한다.
leak-timeout = { period = "2s", result = "fail" }

[[profile.ci.overrides]]
filter = "all()"
test-group = "shared-fixture"

# 아래와 같은 항목을 검토된 known-flaky 정책에서 생성한다.
# 실제 binary_id와 ID 존재 여부는 compiled list로 검증한다.
[[profile.ci.overrides]]
filter = "binary_id(=agentdesk) & test(=voice::receiver::tests::receive_hook_observes_pcm_and_completed_utterance_with_control_channel)"
retries = 1

test-groups의 제한은 한 nextest 실행 안에서 적용된다. 여러 runner를 가로지르는 lock이 아니다. 이 설계에서 worker 간 격리는 독립 PG 서버가 제공한다. 
cargo-nextest

별도 ci-pg15, ci-native profile은 기본 retry 0을 상속하며 각각의 환경과 target 정책을 적용한다. 초기 native 이관 역시 직렬로 검증한다. 병렬도를 올리는 일은 공유 파일·포트·자식 프로세스 조건과 소요 시간을 확인한 뒤 별도로 판단한다.

실행 형태는 다음과 같다.

Bash
# build job: 기존과 같은 default features의 library만 archive
cargo nextest archive --lib \
  --archive-file "$RUNNER_TEMP/lib-tests.tar.zst"

# 전체 기준 집합: worker filter를 적용하지 않는다.
cargo nextest list \
  --archive-file "$RUNNER_TEMP/lib-tests.tar.zst" \
  --workspace-remap "$GITHUB_WORKSPACE" \
  --ignore-default-filter \
  --message-format json > "$RUNNER_TEMP/universe.json"

# 제안 helper: 계획과 worker별 config 생성
python3 scripts/ci/nextest_ci.py plan \
  --universe "$RUNNER_TEMP/universe.json" \
  --workers 6 \
  --output "$RUNNER_TEMP/plan"

# worker: generated default-filter를 그대로 적용한다.
cargo nextest run \
  --archive-file "$RUNNER_TEMP/lib-tests.tar.zst" \
  --workspace-remap "$GITHUB_WORKSPACE" \
  --config-file "$RUNNER_TEMP/worker.toml" \
  --profile ci \
  --no-tests fail

필터 없는 발견 단계에만 --ignore-default-filter를 사용한다. worker 실행에 넣으면 배정을 무시하므로 구조 검사에서 거부한다.

archive에는 소스가 포함되지 않는다. 모든 worker는 같은 SHA를 checkout하고 runtime fixture 파일을 보유해야 한다. --workspace-remap으로 해결되지 않는 컴파일 시점 절대 경로도 확인한다. 
cargo-nextest

2.4 PG·child·retry 조건

Linux 공통 환경은 다음을 기준으로 한다.

일반 텍스트
POSTGRES_TEST_DATABASE_URL_BASE=postgresql://postgres:postgres@127.0.0.1:5432
POSTGRES_TEST_ADMIN_DB=postgres
PGHOST=127.0.0.1
PGPORT=5432
PGUSER=postgres
PGPASSWORD=postgres
AGENTDESK_REQUIRE_PG=1
CI=1

DATABASE_URL과 AGENTDESK_ROOT_DIR은 제거한다. 기존 runner가 설치하던 tmux, ripgrep, PG 로컬 도구 등도 보존한다. 특히 reclaim 시험은 자체 cluster를 만들 때 initdb/pg_config를 사용하므로 컨테이너 PG만 준비해서는 충분하지 않다. 
test_db_reclaim.rs:1282–1338

test-threads=1은 부모 시험이 끝난 뒤 살아남은 child까지 직렬화하지 않는다. 부모는 child가 필요한 동작을 수행했는지와 정리·종료를 검증해야 한다. nextest도 출력 핸들을 상속하지 않은 child 누수까지 검출하지는 못한다. 
cargo-nextest

retry 정책은 다음으로 고정한다.

기본 retries=0.
검토된 exact ID만 최대 1회 retry.
이슈·근거·만료일을 가진 작은 known-flaky 정책 하나를 유지한다.
현재 ledger의 15개를 검토 대상으로 삼되, process-per-test로 원인이 사라진 항목은 제거한다.
module prefix 전체에 retry를 주지 않는다.
CLI --retries나 NEXTEST_RETRIES로 전역 정책을 덮어쓰지 못하게 한다. CLI/env가 config보다 우선하기 때문이다. 
cargo-nextest
retry 후에도 실패하면 red다.
retry로 통과한 ID, 실패 횟수, 최종 결과, 시간, 연결 이슈를 run summary에 표시한다.
infra retry는 시험 retry와 분리한다. worker 재실행 뒤 audit와 required context도 같은 SHA·계획 기준으로 다시 판정해야 한다.
2.5 required context 구현

GitHub Actions job 하나는 일반적으로 check 이름 하나를 만든다. 따라서 9개 이름을 보존하려면 공통 정의를 쓰는 9개 matrix instance가 적절하다.

YAML
required_contexts:
  name: ${{ matrix.context }}
  if: ${{ always() }}
  needs:
    - changes
    - rust_build
    - rust_tests
    - test_evidence
    - lint
    - scripts
    - scripts_guards
    - scripts_contracts
    - dashboard
    - windows
    - pg15
    - relay_mutations
  strategy:
    fail-fast: false
    max-parallel: 3
    matrix:
      context:
        - Lint
        - Script checks
        - Fast check (ubuntu-latest)
        - High-risk recovery
        - Dashboard (Node 22)
        - Fast targeted tests (ubuntu-latest)
        - relay-authority-contract
        - Library test sweep (ubuntu-latest)
        - Fast check cross OS required context (ubuntu-latest)
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - name: Validate required result
      env:
        CONTEXT_NAME: ${{ matrix.context }}
        NEEDS_JSON: ${{ toJSON(needs) }}
      run: python3 scripts/ci/nextest_ci.py required-context

이 예시는 골격이다. 구현에는 다음 조건이 포함되어야 한다.

matrix 전체의 fail-fast:false, 시험 profile의 fail-fast=false를 모두 설정한다.
needs에 검사할 job을 직접 선언한다.
matrix output의 단일 boolean을 “모두 성공” 증거로 사용하지 않는다.
계획된 모든 worker의 artifact·종료 코드·ID 대조를 확인한다.
예상하지 않은 skipped/cancelled/missing 결과는 성공으로 처리하지 않는다.
docs-only skip은 성공한 changes 판정과 예상 job 상태가 모두 맞아야 한다.
기존 Script checks·relay의 mirror/hardening hash pin과 독립 wiring 검증을 새 구조에 맞춰 함께 갱신한다.
동일한 required 이름을 다른 job에서도 발행하지 않는다.

GitHub에서 skipped job이 성공으로 보고될 수 있으므로 if: always() 자체가 보장은 아니다. 실제 판정 코드가 필요하다. required PR check는 pull_request 실행에서 발행하고, 수동 workflow_dispatch 결과로 대체하지 않는다. 
GitHub Docs

3. 삭제 목록과 남길 항목
3.1 확정적인 대체 대상

다음은 고정 SHA의 원문 줄 수다. 기존 코드 삭제량이며 새 helper 추가량을 뺀 net 값은 아니다.

파일	줄 수	삭제 전 이관할 의미
scripts/lib_test_inventory_manifest.txt	15,104	compiled list + source 연결 검사
scripts/pg_test_lane_manifest.txt	3,095	native OS용 자동 PG 분류
scripts/ci/non-pg-test-filter.sh	1,088	플랫폼별 자동 집합 및 기존 replay 포함 결과
scripts/test_lane_required_id_exceptions.txt	21	전체 실행으로 흡수되는 일반 등록 예외 제거
scripts/test_lane_pr_parity_exceptions.txt	93	required/main 실제 집합 감사
scripts/test_lane_required_id_exceptions.d/의 .txt 4개	4	일반 등록 예외 제거
scripts/test_target_integrity_allowlist.txt	20	현재 활성 예외 없는 주석 파일
scripts/test_target_integrity_source_floors.txt	5	명령 수 floor를 실제 집합 검사로 대체
scripts/test_lane_coverage_baseline.txt	3	소스→compiled→executed 연결 검사
scripts/test_lane_required_pr_baseline.txt	2	required aggregate 연결 검사
scripts/pg_test_lane_allowlist.txt	6	분류 false-positive 2건의 사유를 source-local 정책으로 이전
scripts/pg_test_lane_baseline.txt	5	자동 분류·실행 집합 검사
scripts/check_test_target_integrity.py	2,476	소스 연결·cfg/include 검사 핵심 추출
scripts/check_pg_test_lane_membership.py	2,362	자동 PG body/call 분류 핵심 추출
scripts/check_test_lane_coverage.py	1,023	소스 발견 핵심 추출
scripts/lane_source_parity.py	671	실제 실행 결과와 required 연결 검사
scripts/lane_new_id_registration.py	741	cfg 처리 재사용, 등록 행 요구 삭제
scripts/lane_retry_verdict.py	285	현행 사용 여부 확인 후 dormant 레인 코드 제거
scripts/ci/test-selection.py	566	전체 실행 + 시간 가중 계획
scripts/ci/lib-test-artifact.py	251	nextest archive
scripts/run_test_lane.py	824	nextest 결과·ID 감사

합계: 24파일, 28,645줄.

위 fragment 4개는 mac-mini-6772-act_d.txt, mac-mini-6871-b1.txt, mac-mini-7190-p1.txt, mac-mini-7234-p2.txt다. 관련 README 정리는 문서 변경으로 별도 집계한다. 파일 존재와 구성은 
고정 SHA scripts 트리를 기준으로 한다.

3.2 옛 구현 전용 시험
제거·재구성 대상	기존 줄 수
tests/test_check_test_target_integrity.py	3,363
tests/test_check_pg_test_lane_membership.py	2,391
tests/test_test_lane_coverage.py	824
tests/test_lane_source_parity.py	1,263
tests/test_lane_activation.py	108
tests/test_required_id_exception_fragments.py	330
tests/test_non_pg_test_filter.py	441
tests/test_target_empty_identity.py	96
tests/test_lib_test_artifact.py	234
tests/test_lib_test_inventory_sharing.py	110
tests/test_lib_inventory_fixture_environment.py	125
tests/test_test_selection.py	1,025
tests/test_run_test_lane_5185.sh	1,391
tests/test_lane_retry_verdict.py	355
합계	12,056

전부를 의미 없이 삭제하지 않는다. cfg/include, orphan module, PG helper 간접 호출, 잘못된 target, 빈 실행, 누락 결과처럼 계속 필요한 실패 사례는 새 검사 시험으로 이전한다. 파일명과 구성은 
고정 SHA tests 트리를 기준으로 한다.

핵심 삭제와 옛 시험의 원문 합계는 40,701줄이다. 새 코드와 이전할 시험을 반영한 최종 diff는 구현 시 산출한다.

3.3 의미를 옮긴 뒤 삭제할 추가 후보
scripts/known_test_failures.txt 212줄: flaky 정책과 ignored 사유·변화를 각각 새 정책/source 검사로 옮긴 뒤 삭제.
scripts/ci/known-flaky-rerun.py 217줄 및 전용 시험 314줄: nextest exact retry로 대체한 뒤 삭제.
scripts/ci/nested_libtest.py 299줄, 선언 JSON 208줄, 전용 시험 245줄: CI stdout 해석 부분은 제거한다. 부모→child 정확한 선택과 실제 동작 검증을 소스 검사 또는 test-side helper로 옮긴 뒤 나머지를 삭제한다.
옛 just PG shard/continuity/selection recipe와 workflow의 중복 정상 시험 호출: 공통 nextest 명령으로 교체한다.
3.4 남기는 것과 수정할 호출자

남긴다.

Rust PG fixture·env mutex·lifecycle helper와 해당 회귀 시험.
postgres-service.sh와 PG runtime 의존성 준비.
PG 15 exact 계약 20개.
relay authority target·mutation 계약 데이터.
rust_lex.py, rust_include_reads.py 등 실제 소스 해석 기반.
source 연결 검사와 native OS용 자동 PG 분류.
의미 있는 shell/Python/JS 검사, doctest, CLI·portable smoke.
registration_fragments.py: 다른 skip-pin registry에서도 사용하므로 함께 삭제하지 않는다.
ci_shell_lex.py: 남은 소비자를 확인한 후 판단하며 핵심 삭제 수에 포함하지 않는다.

수정한다.

ci-script-checks.sh, justfile, 관련 workflow, 품질 게이트 문서.
check-ci-runner-hardening.sh, test_fast_check_ci_wiring.py: 옛 DAG·명령 문자열 검사를 새 구조로 변경.
check_relay_authority_contract.py: lib manifest 입력을 nextest list로 교체.
infra-failure-rerun.sh: 삭제될 membership script의 --pg-job-names 의존 제거.
required-check-mirror.sh 또는 공통 aggregate helper와 관련 hash pin.

마지막 두 의존은 실제 코드에 존재한다. 삭제 순서를 뒤집으면 시험 자체와 무관하게 CI가 깨진다. 
check_relay_authority_contract.py:39, 320–325, 
infra-failure-rerun.sh:13–15

SQL inventory·H2·restart_mode/protocol_v2 삭제량은 위 수치에 포함하지 않았다. 별도 작업이 아직 착지하지 않았다면 현행 검사를 유지하고, 착지했다면 그 결과에 맞춰 호출자를 정리한다.

4. 0-test·누락 방지
4.1 전환 전후 실행 ID 비교

정규화 ID는 다음 정보를 포함한다.

일반 텍스트
(platform, target/features scope, binary_id, test_name)

비교 자료에는 Git SHA, archive digest, Rust/nextest 버전, 빌드 옵션, profile, plan digest도 기록한다. JUnit의 시험 이름만으로 플랫폼·feature·빌드 일치를 증명하지 않는다.

절차는 다음과 같다.

같은 SHA의 기존 구조를 강제 전체 실행한다.
기존 libtest 결과에서 최상위 실제 실행 ID를 수집한다. ignored와 nested child 출력은 따로 기록한다.

새 archive에서 필터 없는 전체 목록을 만든다.
cargo nextest list --message-format json으로 실제 시험 목록을 읽는다. binaries-only 목록을 시험 목록으로 사용하지 않는다. 
cargo-nextest

프로파일별 expected 집합을 결정한다.
Linux lib, Windows PR exact proof, nightly native, PG 15, nightly e2e, trusted macOS를 서로 다른 scope로 비교한다.

분할 자체를 검사한다.
전체 실행 집합을 U, worker별 배정을 A_i라고 하면 다음을 요구한다.

⋃
𝑖
𝐴
𝑖
=
𝑈
,
𝐴
𝑖
∩
𝐴
𝑗
=
∅
 
(
𝑖
≠
𝑗
)
,
∣
𝐴
𝑖
∣
>
0
i
⋃
	​

A
i
	​

=U,A
i
	​

∩A
j
	​

=∅ (i

=j),∣A
i
	​

∣>0

worker별 실제 결과를 검사한다.
non-skipped testcase 집합 E_i에 대해 E_i = A_i를 요구한다. 전체 합집합만 맞추어 중복 실행으로 누락을 가리지 않는다.

구·신 차이를 출력한다.
missing, extra, runnable→ignored, platform/target 변화, 재시도 통과, child proof 결과를 별도 출력한다. 정상 중복 호출이 제거된 것은 실행 횟수 변화로 보고하고 ID 누락으로 세지 않는다.

보고서를 PR에 붙인다.
성공 여부뿐 아니라 기계 판독 JSON과 사람이 읽는 요약을 남긴다.

nextest JUnit은 binary별 testsuite와 시험별 testcase를 만들고, retry는 그 testcase 내부에 기록한다. retry attempt를 새로운 시험 ID로 세면 안 된다. report-skipped="all"은 partition별 중복 skipped 항목을 만들 수 있으므로 여기서는 "none"과 별도 ignored inventory를 사용한다. 
cargo-nextest

4.2 상시 방어 장치
위험	상시 장치
filter가 0개를 선택	계획 단계의 비어 있지 않은 집합 검사 + --no-tests fail
worker 하나가 실행되지 않음	계획에 있는 worker artifact가 모두 있어야 통과
잘못된 archive·이전 실행 결과 혼입	SHA·archive/plan digest·scope·worker·attempt identity 검사
실패한 명령을 wrapper가 성공 처리	nextest 종료 코드와 job 결과를 JUnit과 함께 판정
목록에는 있지만 실제 실행하지 않음	worker별 expected == executed
새 시험을 등록하지 않음	compiled list에서 자동 발견·배정. 일반 시험용 등록 파일 없음
mod 연결 누락으로 컴파일조차 안 됨	소스 연결·cfg/include 검사
#[test] 제거·ignore 추가·플랫폼 축소	base와의 자동 ID/소스 변화 보고. 정당화되지 않은 제거는 실패
PG 시험의 soft-skip	REQUIRE/fixture-target 계약, 누락 base·접속 실패 주입 검증
expired/stale flaky 예외	exact ID 존재·scope·만료 검사. 잘못된 예외는 실패
audit·aggregate가 skip되어 green	always()와 명시적 결과 판정. missing/skipped를 무조건 성공 처리하지 않음

--no-tests fail은 빈 선택을 실패시킬 수 있지만, 일부 시험만 빠진 경우까지 해결하지 않는다. 전체·worker별 집합 감사가 함께 필요하다. 
cargo-nextest

자동 비교용 baseline은 성공한 main의 생성 artifact로 유지한다. 사람이 수정하거나 PR마다 충돌하는 파일로 커밋하지 않는다. 시간 가중치 artifact가 없으면 보수적으로 실행할 수 있지만, 커버리지 비교 baseline이 없으면 재생성하거나 검사를 실패시킨다. 두 artifact의 실패 정책을 구분한다.

4.3 ignored와 child 처리

현재 Linux ledger의 ignored 18개는 child entry 11개와 수동·외부 환경 시험 7개다. Windows에는 추가 child 선언이 있다. 일반 실행에서 --run-ignored all을 사용하지 않는다. 
known_test_failures.txt:195–212

수동·외부 환경 7개의 제외 이유는 유지한다.

시험	기본 CI 제외 이유
round_trip_on_an_approved_test_channel	승인된 실제 Discord 채널과 자격 정보 필요
herdr_real_server_launch_input_bytes_and_refusals	지정된 실제 Herdr binary 필요
a_p11_m2_last_pane_close_and_exact_retire	실제 Herdr root/PID harness 필요
a_p11_m2_confirmation_required_has_no_escalation	실제 Herdr activation harness 필요
th9w_real_h_post_timeout_and_force_after_f_pg	200초 이상 실제 시간 시험. 명시적 수동 recipe 유지
auto_queue_preflight_fixture_sandbox_roundtrip_pg	별도 sandbox fixture harness에서 실행
pg_reclaim_dry_run	수동 진단 dry-run

child entry는 독립적인 기본 시험으로 세지 않는다. 부모가 정확한 child를 선택하고, 필요한 handshake·효과·종료를 확인해야 한다. 잘못된 child 이름으로 0개를 실행한 child process의 exit 0을 성공 증거로 받아들이지 않도록 기존 nested 검증의 의미를 옮긴다.

사유 있는 #[ignore]를 붙였다는 사실만으로 새 제외를 자동 승인하지 않는다. runnable→ignored 변화 ID와 사유는 자동 보고서에 나타나고, 해당 PR의 의도적 제외 설명과 연결되어야 한다. 일반 시험 추가에는 이런 절차가 필요하지 않다.

5. 전환 계획
5.1 선행 PR 착지 후 기준 확정

이슈에서 지정한 #7312, #7313, #7315, #7316과 delegation rollback이 착지한 뒤 전환 PR을 만든다. 조회된 열린 PR은 번호가 있는 4건이며, 다섯 번째 rollback PR의 번호·착지 여부는 **[U1]**이다.

그 시점의 main을 전환 기준 B로 고정하고 다음을 다시 읽는다.

#7315의 최신 PR HEAD·retry 정책.
별도 SQL/H2/restart 삭제 결과.
시험 ID·ignored·flaky 정책 변화.
이 문서의 고정 SHA와 B 사이에서 바뀐 CI 보장.
5.2 단일 PR 내부 순서

새 실행·감사 코드를 준비한다.
archive/list/plan/run/audit, source 검사 추출, nextest config, 필요한 구조 시험을 작성한다. 이 단계에서는 main의 기존 workflow가 계속 작동한다.

같은 SHA에서 제한된 shadow 비교를 수행한다.
구 구조 강제 전체 실행과 새 구조의 ID·환경 동등성을 확인한다. 무제한 병렬 이중 실행을 유지하지 않는다. shadow의 추가 작업 때문에 늘어난 wall time은 성능 표본에서 제외한다.

특수 보장을 검증한다.
PG 15, Windows exact proof, native nightly/trusted scope, ignored child, doctest, mutation, fixture 실패 계약을 확인한다.

같은 PR의 최종 workflow를 새 구조로 바꾼다.
기존 9개 이름은 새 aggregate만 발행한다. 삭제 대상과 모든 호출자·hardening pin을 함께 변경한다.

새 구조만 실행하는 PR workflow에서 수용 표본을 모은다.
최종 worker 구성으로 유효 PR 실행 20회 이상, 그중 PG 안정성 10회 이상을 확보한다. 실행 취소·캐시 상태·변경 범위·동시 부하를 기록한다. 유리한 재실행만 골라 중앙값을 만들지 않는다.

DoD 증거가 갖춰진 최종 HEAD를 한 번에 merge한다.
그 전까지 main은 기존 CI를 사용한다. merge 후 CI Main 이름과 전체 green을 참조하는 배포 관계를 유지한다.

현재 main workflow는 배포가 green CI Main run을 참조한다는 전제를 명시하고 있으므로 workflow 이름과 성공 의미를 유지해야 한다. 
ci-main.yml:593–600

5.3 캠페인 정적 검사와 레인 브리프

지정 SHA에는 scripts/prepush_gates.sh와 캠페인 레인 브리프의 실제 관리 위치가 없다. 위치는 **[U2]**로 확인한다.

갱신 내용은 확정할 수 있다.

일반 Rust 시험 추가 시 lib/PG manifest·required-ID exception 행을 추가하라는 규칙을 삭제한다.
일반 시험은 표준 test 선언과 module 연결만으로 CI에 자동 포함되도록 안내한다.
--static-only에서는 Cargo 실행 없이 source 연결, cfg/include, ignore 사유, retry 정책, TOML/YAML, aggregate wiring을 검사한다.
compiled list·실제 실행 집합 대조는 CI에서 수행한다.
PG 15 호환성, mutation 계약, known-flaky, 수동·child 시험처럼 의미가 있는 예외는 각각의 목적과 변경 절차를 설명한다.
전환 기간에는 구/신 구조를 명시적으로 인식하는 정적 gate를 먼저 배포하고, repository merge 후 옛 분기를 제거한다.
최종 revision에서 scripts/prepush_gates.sh --static-only PASS 로그를 남긴다.
5.4 롤백

롤백 단위는 전환 PR 전체다.

workflow, 삭제한 파일, 호출자, config, hash pin을 같은 revert로 복원한다.
외부 캠페인 검사·브리프도 대응 revision으로 되돌린다.
required context 이름은 바뀌지 않았으므로 branch protection을 완화할 필요가 없다.
잘못된 duration artifact는 버릴 수 있다. 시험 집합은 항상 현재 소스·compiled list에서 다시 만든다.
부분적으로 옛 등록 파일만 복구하거나 새 aggregate만 남기는 혼합 상태는 만들지 않는다.
6. 크기·분할
6.1 예상 크기

기존 전용 파일의 원문 삭제 후보는 다음과 같다.

핵심 파일: 28,645줄
옛 구현 전용 시험: 12,056줄
합계: 40,701줄
추가 삭제 후보와 workflow 정리는 별도
새 planner/audit/aggregate helper, source 검사 추출, config는 대략 수천 줄 이내로 예상

따라서 전체 net은 상당한 음수가 될 가능성이 높다. 다만 전체 diff 줄 수, production code net, production 파일 수는 서로 다른 수치다.

현재 pr_cap_prod.py는 production 30파일 / 순증 +1500을 적용하지만, workflow YAML을 제외하는 규칙은 현재 EXCLUDE에 없다. 이번 요청의 “워크플로 YAML 제외”와 맞추려면 그 분류를 명시적으로 갱신하고 시험해야 한다. 
pr_cap_prod.py:1–22

6.2 한 PR 안의 리뷰 조각
리뷰 조각	범위	크기 관리
A. 실행·증거	archive 사용, LPT 계획, result audit, retry summary, config	production 약 4–6파일, 순증 +1500 이내
B. 소스·플랫폼	source 연결/cfg/include 및 PG 분류 핵심 추출, 옛 검사 의미 이전	production 12파일 이내 목표. 기존 코드 삭제와 함께 검토
C. workflow·required	9개 alias, PR/main/nightly/trusted 연결, infra retry, hardening pin	YAML 제외 후 production 15파일 이내 목표
D. 등록·wrapper 철거	manifest/filter/예외/옛 harness·시험·호출자 제거	production 30파일 이내, 순증 음수
E. 캠페인·문서	static-only gate, 레인 브리프, 운영·롤백 문서	별도 관리 위치에 맞춰 작은 변경으로 검토

이것은 리뷰 경계이며 여러 PR로 쪼개 merge하자는 제안이 아니다. 최종 전환은 한 PR이다.

또한 commit을 나눈다고 PR 전체 cap이 자동으로 통과하지는 않는다. 구현 후 전체 production 파일 수를 측정해야 한다 [U3]. 30파일을 넘으면 파일 재사용·선택적 삭제 범위 조정으로 먼저 줄인다. 그래도 요청된 단일 PR과 전체 PR cap이 충돌한다면 기존의 명시적 PR-CAP-EXEMPT 절차 사용은 사용자 결정 사항이다. 지금 이를 승인된 것으로 간주하지 않는다. 
pr_cap_check.sh:22–95

7. 이슈 DoD 7개와 증명 방법

이슈 #7317의 수용 기준을 다음 증거로 닫는다.

DoD	필요한 증거	통과 조건
1. 전후 실행 ID 집합 동등성	같은 SHA의 구 강제 전체 실행 결과, 새 nextest 결과, scope별 비교 JSON·요약	기본 실행 ID의 missing/extra 0. 정상 중복 실행 제거는 횟수 변화로 설명. ignored·수동·child·플랫폼 차이는 명시적 사유와 부모/별도 실행 증거로 구분
2. required 9개 이름 및 실제 실패 반영	실제 PR check 목록, 일반 non-flaky 시험 1개 실패 주입, artifact 누락·예상 밖 skip 검증	이름 정확히 9개 유지. 공통 nextest 실패 시 9개 모두 통과하지 못함. dashboard/Windows/PG 15/mutation 고유 실패도 해당 context가 실패
3. PG 직렬 조건과 n≥10 안정성	최종 구성 10회 이상 PR 실행, worker 자원 설정, fixture 실패 주입, child 종료·정리 확인	worker당 직렬·독립 서버 유지. 새로운 intermittent failure 0. 새 flaky를 임의 allowlist로 흡수해 통과시키지 않음
4. n≥20 성능	최종 worker 구성의 20회 이상 유효 PR 실행과 #7303 방식의 집계	PR wall 중앙값 ≤60분, executed critical path 중앙값 ≤45분. queue·retry·aggregate 시간을 숨기지 않고 기록
5. retry 정책·가시성	exact allowlist와 generated override 검증, 알려진 flaky의 retry-pass 시연, 미등록 실패 시연, run summary	기본 retry 0. known-flaky만 최대 1회. retry-pass ID가 summary에 나타남. exhausted retry는 실패
6. 옛 등록 목록·filter·checker 삭제와 무등록 신규 시험	삭제 파일 목록, 잔존 참조 검색, 정상 신규 시험 추가 증거	일반 시험을 소스에 추가하는 것만으로 자동 발견·배정·실행. 등록 파일 수정 0. 소스 연결 누락 시 gate 실패
7. 캠페인 정적 검사 PASS	최종 캠페인 revision과 scripts/prepush_gates.sh --static-only 로그	최종 repository 구조에서 PASS. 삭제된 파일을 요구하는 규칙과 레인 브리프 지시가 남아 있지 않음

성능 보고서는 사용자가 제시한 기준선 critical path 41.9분, PR wall 84분, lib 이후 shard 대기 27.8분과 같은 정의로 비교한다. 평균이나 가장 빠른 실행으로 중앙값 기준을 대신하지 않는다.

nextest는 시험마다 프로세스를 만들기 때문에 기존 프로세스당 한 번 실행되던 초기화가 반복될 수 있다. 실제 reclaim 코드에도 SWEPT_THIS_PROCESS가 있다. 따라서 기존 libtest 시간은 초기 가중치 자료로만 쓰고 실제 nextest 시간으로 보정한다. 
test_db_reclaim.rs:734–740

8. 미확정 항목과 확인 방법
항목	미확정 내용	확인 방법·결정 시점
[U1] 선행 다섯 번째 PR	delegation rollback PR 번호·착지 상태	구현 시작 전 #7317의 선행 조건과 실제 merge 이력을 대조. 다섯 개 착지 후 기준 B 고정
[U2] 캠페인 파일 위치	prepush_gates.sh, 레인 브리프의 실제 저장소·revision	운영자가 사용하는 캠페인 checkout에서 경로와 호출자를 검색. 전환 PR 최종 검증 전에 갱신
[U3] production cap	YAML 제외를 반영한 최종 production 파일 수·net	최종 base/head로 측정. 리뷰 조각과 전체 PR 수치를 함께 제시
[U4] branch protection 실설정	사용자 제공 9개와 관리자 설정의 최종 일치	조회 시 integration 권한으로 403이 반환됨. merge 전 관리자 readback으로 이름·App 연결·중복 context 확인
[U5] 최종 worker 수·시간 자료	6개 LPT가 두 성능 기준을 모두 만족하는지, 초기 시간 자료 위치	calibration 후 6→7→8 순으로 평가. 최종 구성 확정 후 n≥20 측정
[U6] 공통 PG 환경	DATABASE_URL 제거 후 기존 PG/high-risk가 실제 fixture 경로를 수행하는지	동일 SHA shadow, fixture base 누락·접속 실패 주입, soft-skip 없는 실행 확인
[U7] process-per-test 비용·child	반복 초기화 비용, child 정리, leak 설정, 절대 경로와 archive 호환성	reclaim·nested parent·파일 fixture 대표 사례와 전체 shadow에서 확인. 부모의 실제 child 실행·효과 검증을 보존
[U8] native OS 실행 시간·집합	자동 PG 분류 결과와 기존 OS 집합의 일치, 초기 직렬 profile의 timeout 적합성	native Windows/macOS compiled list·실행 결과 대조. nightly/trusted 한 주기 상당의 shadow 완주
[U9] main non-lib 공집합	7개 non-lib filter가 실제 compiled list에서도 빈 집합인지	같은 SHA에서 target별 list 확인. 공집합이면 빈 실행을 빌드 검증으로 정리하고 doctest 유지
[U10] #7315 이후 retry 코드	최신 PR HEAD 제한, job 재실행 후 audit/alias 의존 재실행의 정확한 동작	#7315 착지 코드 재독해 후 실패 주입. 같은 SHA·계획의 최종 결론으로만 required context 판정
[U11] 별도 삭제 세션 결과	SQL/H2/restart 관련 파일·호출자의 최종 상태	기준 B에서 재확인. 존재하는 계약은 유지하고 이미 삭제된 기능의 참조를 정리

이 미확정 항목들은 구현·전환 검증에서 닫을 항목이다. 설계의 핵심 결정은 고정할 수 있다. 실행 집합은 컴파일 결과에서 자동 생성하고, 소스 연결은 별도로 검사하며, 자원 직렬성은 worker와 독립 PG 서버로 보장하고, 9개 required context는 실제 결과를 판정하는 aggregate로 유지한다. 수용 기준을 만족하기 전에는 전환 PR을 merge하지 않는다.

VERDICT: DESIGN_READY
