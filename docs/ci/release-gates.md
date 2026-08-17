# Release Candidate CI Gates

이 문서는 AgentDesk `main` 및 PR 레벨에서 release candidate 자격을 보증하는 3개 CI gate를 명시한다. Gate는 "이 job이 red면 릴리즈 후보가 아니다"를 의미하며, 우회 금지(branch protection에서 required로 등록 또는 자동 triage로 동등 효과를 보장).

> 상위 소스-오브-트루스: [`docs/source-of-truth.md`](../source-of-truth.md)
>
> 관련 문서: [`docs/high-risk-recovery-lane.md`](../high-risk-recovery-lane.md)

## 1. 3개 Release Gate

| Gate | ci-main.yml job | ci-pr.yml job | ci-nightly.yml 대응 | 실행 조건 |
| --- | --- | --- | --- | --- |
| **Full tests** | `full_non_pg` | `library_sweep` (+ `check_fast` compile/policy) | `full_macos` + `full_windows` | main/nightly always run non-PG tests. PR side: `library_sweep` runs the whole `--lib` harness minus the `_pg`/`pg_`/`postgres` id filters on the broad `rust_or_policy` filter (#5185), **with its own PostgreSQL service** — those filters are substring matches over ids and 61 PG-dependent tests carry none of them; `check_fast` stays compile/policy only. |
| **PostgreSQL tests** | `postgres` | `test_fast`의 PG 서비스 | `postgres_full` | main/nightly는 항상 실행. PR의 `test_fast`와 selection observer는 `pg_db` path filter가 true일 때만 실행하며, false이면 required mirror가 명시적으로 green을 반환. |
| **High-risk recovery** | `high-risk-recovery` | `high-risk-recovery` | `high_risk_recovery_full` | path filter hit 시에만 실행. nightly full job은 무조건. |

Selection observer required gate가 red로 만드는 observer 사망은 **프로세스 수준
사망**이다. observer의 비정상 종료 코드나 시그널, summary 0줄 또는 2줄 이상,
로그 파일 부재나 `tee` 실패, verifier 자체 사망은 required lane을 red로 만든다.
상세 관측과 다른 5필드 summary도 verifier가 red로 만든다. 반면 observer가 내부
예외를 잡아 `execution_errors`와 `findings`로 truthful하게 보고하면 observer는 0으로
종료하고 verifier도 통과한다. 이 verifier는 summary가 상세 evidence에 비추어
**참인지**만 검사하며 관측이 충분한지는 검사하지 않는다. invocation 하한을
제거했으므로 truthful all-zero summary도 통과한다.

### PR 측 library sweep (#5185)

`library_sweep` job은 `--lib` 하네스 전체에서 `--skip _pg --skip pg_ --skip
postgres`가 제거하는 것만 뺀 나머지를 `scripts/run_test_lane.py`를 통해 실행한다.

> ⚠️ **이 잡은 PostgreSQL을 필요로 한다.** 이름의 skip 패턴은 **test id에 대한
> 부분문자열 필터**이고 PG 의존성은 **테스트 본문의 성질**이라 둘이 일치하지
> 않는다. `scripts/pg_test_lane_manifest.txt`가 PG 의존으로 분류한 460건 중
> **정확히 61건**이 그 세 부분문자열을 id에 갖지 않아 이 레인에 선택된다.
> 실측: fixture base를 닫힌 포트로 향하게 하면 **61건 중 57건 FAILED**이고
> 소요 **915.7초**(연결 재시도로 각 테스트가 60초 이상 매달린다), 살아 있는
> 서버에 대해서는 `AGENTDESK_REQUIRE_PG=1`·병렬로 **61 passed / 0 failed,
> 24.8초**다. 따라서 서비스가 없으면 이 required context는 **구조적으로 초록이
> 될 수 없다**. 서비스를 붙임으로써 어떤 PR 레인도 실행하지 않던 56건
> (`scripts/pg_test_lane_baseline.txt`의 `[rule1]`, 이제 비어 있다)이 모든 Rust
> PR에서 실제 DB를 상대로 돌기 시작한다.
>
> 이 성질은 `scripts/check_pg_test_lane_membership.py`의 **`[rule5]`**가
> 기계적으로 지킨다: `ci-pr.yml`의 어떤 잡이 서비스를 시작하지 않은 채
> `cargo test`로 PG 의존 테스트를 선택하면 **id를 전수로 지목하며 rc=1**이다.
> baseline 관용이 없는 규칙이라 재생성으로 흡수되지 않는다. `rule2`는 이것을
> 잡지 못한다 — `--all-targets` 커맨드만 읽어서 `--lib` 레인을 보지 못하고,
> 같은 61건이 이미 nightly macOS/Windows 레인 몫의 debt로 등재돼 있어 **PR 레인을
> 추가해도 숫자가 움직이지 않는다.**

이 wrapper는 **실행 건수 임계를 쓰지
않는다.** 임계는 집합의 스칼라 요약이고, 실측된 두 회피가 모두 임계를 통과했다:
한 모듈을 402건 축소하면 `executed=6539`, 213건 모듈을 비활성화하면
`executed=6708`로 둘 다 floor 6500 위에 남아 **GATE_RC=0**을 반환했다.

대신 wrapper는 `scripts/lib_test_inventory_manifest.txt`에서 이 레인이 실행해야
할 **test id 집합**을 유도하고(플랫폼별 static-only / cargo-only 보정은 #5144의
`--verify-lib-inventory`와 같은 상수를 재사용한다), 선언된 `--skip` 패턴을 뺀 뒤
실제 보고된 집합과 비교해 **양쪽 차분을 id 이름으로 출력하며** 실패한다.
`--skip`은 wrapper와 cargo 양쪽에 선언되어야 하고 둘이 다르면 레인 실행 전에
실패한다. `#[ignore]`는 선택 집합을 유지한 채 실행만 줄이는 우회로이므로
원장(`ignored` 모드)에 집합으로 핀된다. libtest 요약줄이 `--max-summaries`를
넘으면 실패하며(테스트 바이너리를 재실행하는 테스트가 자식 요약을 상속 stdout에
쓴다), 주 요약줄은 유도된 기대 집합과 교차검증된다.

libtest는 병렬 모드에서 `test <id> ... `과 판정을 **동기화되지 않은 두 번의
write**로 내보내고, 그 stdout은 모든 테스트 스레드와 상속받은 자식 프로세스가
공유한다. 실제 스윕에서 launchd-plist 헬퍼의 write가 이름 앞·이름과 판정 사이
양쪽에 끼어드는 것이 관측됐다. 그래서 wrapper는 한 줄을 통째로 매치하지 않고
`test <id> ... ` 조각을 `finditer`로 훑은 뒤 조각 사이 구간의 **맨 앞 또는 맨
끝**에서만 판정을 인정한다(경로 중간의 `ok`는 판정이 아니다).

한 줄에 이름이 둘 이상 미결로 남을 때의 LIFO 귀속은 **증명이 아니라 선택**이다.
rust 1.94 `library/test/src/formatters/pretty.rs`는 병렬 모드에서 콘솔 스레드
하나가 이름과 판정을 연달아 쓴다는 것만 보장하며, 그래서 **한 libtest
프로세스는 이름을 둘 미결로 둘 수 없다**. `test A ... test B ... ok`는 부모와
중첩 자식이 상속 stdout을 공유할 때만 나오고, 그 경우 두 가지 순서가 **바이트
단위로 동일**하다. LIFO는 그중 하나를 답으로 고정한다.

**이 wrapper가 막는 것과 막지 못하는 것.** 이전 판본은 "파서 오류가 false
green으로 바뀌지 않는다"고 적었고, 그 전칭은 **실측으로 반증됐다**. `executed`,
`failed`, `selected`는 모두 **스칼라**이므로 **서로 상쇄되는 두 개의 오독**은 셋을
동시에 만족시킨다: 실패한 테스트가 외래 텍스트의 끝 `ok`를 가져가고 통과한
테스트가 다른 외래 텍스트의 끝 `FAILED`를 가져가면 rc=0, unexpected=0, stale=0
으로 **미등재 실패가 조용히 초록**이 된다.

그래서 판정 귀속은 더 이상 개수로 판단하지 않는다. libtest는 실패한 id를
`failures:` 블록에 스스로 나열하므로, wrapper는 그 **집합**을 파싱 집합과
대조하고 양쪽 차분을 id로 출력한다. 집합 차분에는 상쇄가 없다. 정확히:

* FAILED가 유실·날조되거나 **LIFO 선택이 틀려** 다른 id에 귀속되면 레인이 id를
  지목하며 실패한다,
* 요약이 실패를 보고하는데 `failures:` 블록을 복구할 수 없으면 실패한다(어느
  id가 실패했는지 판정 자체가 불가능하므로),
* `ignored` 오독은 원장 대비 양방향 집합 핀에 걸린다,
* 테스트 자신의 stdout이 만들어낸 id는 `lane-extra`, 판정을 보고하지 않은 id는
  `lane-missing`으로 실패한다.

**아직 닫히지 않은 것 — 전수 5건.** 이전 판본은 아래 1·2만 적었고, 그것은 코드보다
좁은 서술이었다.

1. 통과한 두 판정끼리의 뒤바뀜. 양쪽 모두 executed·pass로 남으므로 무해하다.
2. `failures:` 블록 자체가 인터리빙으로 오염되는 경우. 이름이 붙은 집합 차분,
   즉 **false green이 아니라 false red**로 나타난다.
3. **어떤 테스트든 상속 stdout에 `failures:` 한 줄만 써도** 스캐너가 블록 모드에
   들어가고, 뒤따르는 4-스페이스 들여쓴 `a::b` 줄들이 `declared_failures`가 된다.
   전부 초록인 런이 `declared-not-parsed`로 red가 된다. fail-closed이고 현재 이
   레인에 그런 테스트는 없지만, 금지하는 장치도 없다.
4. **중첩 자식 libtest**가 같은 상속 stdout에 자기 `failures:` 블록을 쓰면 그 id가
   부모 것과 **union**된다. 자식이 부모가 선택하지도 않은 id로 실패하면 레인은
   매니페스트에 없는 id를 지목하며 red가 된다. `--max-summaries 2`를 선언하게 만든
   바로 그 재실행 경로이므로 가정이 아니라 **구성상 도달 가능**하다. fail-closed.
5. 세그먼트 **시작**이 판정 단어이고 그 뒤가 **non-word 문자**인 외래 텍스트는
   여전히 판정을 훔친다. lookahead가 `\W` 후속을 허용하므로 `ok: connect refused`는
   `ok`로, `ignored, using default`는 `ignored`로 읽힌다 — 후자는 실제
   `ignored, <#[ignore] 이유>`와 구분 불가하고, `drain_verdicts`의 `,` 분기가
   바로 그 형상을 위해 있다. 즉 잔여는 `ok|FAILED|ignored`로 **끝나는** 외래
   텍스트에 한정되지 않는다.

   그 잔여에서 **빠진 것은 판정 단어 뒤가 word 문자인 경우**다. 경계가 아예 없던
   시절 `okhttp: connect`·`FAILED_upload_error`는 판정으로 읽혔으나,
   `VERDICT_AT_START`의 lookahead `(?=ok|FAILED|ignored|\W|$)`가 둘을 거부한다.
   `tests/test_run_test_lane_5185.sh` §4g가 세 방향을 핀한다 — 4g-1이 `okhttp:`
   거부, 4g-2가 `FAILED_upload_error` 거부, 4g-3이 `okok`의 두 판정 파싱 유지.

   **`\b` 형태의 워드 경계는 여기서 유일하게 배제된 형태다**: `ok` 뒤의 `\b`는
   다음 문자가 non-word이기를 요구하는데, 이 파서가 존재하는 이유인 병합 write
   `okok`(개행이 유실된 연속 두 판정)은 다음 문자가 word라서 두 번째 판정이
   유실되고 그 id가 `lane-missing`으로 레인을 red로 만든다. required context에서
   false red는 그것이 막는 false green보다 나쁘다. 다음 판정 단어 자체를 경계로
   인정하는 lookahead는 이 논거의 대상이 아니고, 그래서 그 형태가 적용돼 있다.

   `VERDICT_AT_END`는 **의도적으로 경계를 두지 않는다**. 손실이 경계 위치에
   달렸기 때문이다: 판정 단어 **앞**에 두면 실측된 `/var/….plist: OKok` 형상을
   잃는다(`OKok`의 `ok` 앞 문자가 word인 `K`). **뒤**에 두면 잃지 않는다(매치가
   이미 세그먼트 끝에서 끝난다). 선행 경계 해석에서만 손실이 성립하므로 end
   앵커는 그대로 두고, §4h-6이 `OKok`의 판정 유지를 핀한다.

   두 앵커 모두 false green 방향은 위 `failures:` 집합 대조로 좁혀져 있고,
   그래서 이 잔여는 닫지 않고 안고 간다.

2의 폐쇄 논증에는 명시되지 않은 전제가 있다: **블록 오염은 삽입 전용**이라는 것.
블록 내용을 파싱 집합과 일치하도록 **치환**하면 rc=0이 나오지만, 인터리빙은 다른
write 사이에 끼어드는 것이라 바이트를 **추가**할 수 있을 뿐 libtest 자신의 write를
**대체할 수 없다**. 그 블록을 제자리에서 다시 쓸 수 있는 메커니즘이 생기면 이
집합 대조는 더 이상 폐쇄가 아니다.

이 레인의 아카이브 전사 76개(73건 실패의 poison cascade 1개 포함)를 재생했을 때
블록 집합과 파싱 집합은 전부 일치했으므로, 이 검사의 실측 false-red 비용은 0이다.

#### required context 등록 절차 (#5185)

`library_sweep`을 branch protection의 required check으로 등록할 때:

1. 먼저 이 PR을 머지한다. 등록은 머지 **후**다.
2. **잡이 필요로 하는 서비스를 실제로 갖고 있는지 확인한다.** 등록 여부와
   무관하게 잡은 실행되므로, 등록은 red를 만드는 것이 아니라 **red가 머지를
   막게 만들 뿐**이다. 즉 이 단계를 건너뛰면 등록 전에 이미 모든 PR이 red다.
   확인 방법은 두 가지이며 **둘 다** 한다:
   - `python3 scripts/check_pg_test_lane_membership.py`가 `rule5=0`인지 본다.
     0이 아니면 `ci-pr.yml`의 어떤 잡이 PG를 시작하지 않은 채 PG 의존 테스트를
     선택하고 있다는 뜻이고, 출력이 그 id를 전수로 지목한다.
   - main의 실제 런에서 그 잡의 `Start PostgreSQL service` step이 존재하고
     성공했는지 본다. rule5는 **선택 집합**을 검사할 뿐 서비스가 실제로 떴는지는
     모른다.
   ⚠️ 이 단계는 #5185가 **거의 놓친 것**이다. 잡 이름과 `--skip` 패턴이 모두
   "non-PostgreSQL"이라 아무도 PG를 의심하지 않았지만, 그 패턴은 id 부분문자열
   필터였고 61건이 통과했다.
3. main에서 `Library test sweep`이 **N회 연속 green**인지 확인하고 **false-red
   비율을 실측**한다. 1회 green은 근거가 아니다: 이 레인이 관측하는 stdout
   오염은 확률적이고, 실제로 5회 스윕 중 1회 오탐이 관측된 적이 있다.
4. 등록할 컨텍스트 이름은 **`Library test sweep (ubuntu-latest)`**
   (= `library_sweep_required_context` job)이다. sweep 잡 본체인
   `Library test sweep`을 등록하면 `rust_or_policy` path filter가 false인 PR에서
   잡이 skip되어 **pending으로 영구 블록**된다. mirror job은 `if: always()`로
   돌면서 skip을 명시적 green으로 변환하고 upstream 실패/취소에는 fail-closed다.

`scripts/check-ci-runner-hardening.sh`의 `targets`에 등재된 `test_fast`,
`high-risk-recovery`, `check_fast_cross_os`, `library_sweep`에는 **강도가 다른 두
층**이 있다.
섞어 읽으면 안 된다.

1. **whole-job semantic hash — 변경 탐지기이지 보장이 아니다.** 같은 diff 안에서
   재핀하면 무엇이든 통과한다. `test_fast` 실측: 미등록 step 추가, `Start
   PostgreSQL service` 삭제, 캐시 step 2개 순서 변경이 모두 기존 pin 대비 rc=1
   이지만 정상 재핀 후 rc=0 이고, expected-step 계약을 고칠 필요조차 없었다.
   리뷰를 부르는 트리거로만 취급한다.
2. **그 밖의 모든 단언 — 재핀과 무관하게 유지된다.** hash 는 스크립트에서 단 한
   곳에서만 비교되고, 나머지 단언은 전부 hash 와 독립이다. 실측: `just
   test-postgres` step 삭제는 재핀 후에도 rc=1.

**2번이 무엇을 덮는지는 이 문서에 적지 않는다.** 다섯 라운드 연속으로 불완전하거나
틀린 목록이 나왔다. 특정 조작이 잡히는지 알아야 하면 **그 조작을 넣고 hash 를 재핀한
뒤 스크립트를 돌려라** — 그 답만이 낡지 않는다. 독립적인 명시적 step inventory 층은
없다. 다만
`targets`에 없는 신규 job 경계, job ID 인용이나 표현 변형으로 인한 추출량 붕괴,
invocation 하한은 이 게이트가 보장하지 않는다.

`scripts/ci-script-checks.sh`가 독립 실행하는
`check_test_target_integrity.py --verify-lib-inventory` 검사는 Rust 소스에서
정적으로 수집한 lib 테스트의 전체 이름 집합을 정렬된
`scripts/lib_test_inventory_manifest.txt`와 양방향 비교하고, 같은 트리에서
`cargo test --lib -- --list`가 연 집합과의 차이가 명시된 platform/include 차이인지
비교한다. 새 테스트를 추가하거나 기존 테스트를 삭제·이름 변경하면 manifest와
static 집합의 차집합에 실제 test ID가 이름 그대로 출력되어 red가 된다. 실패
출력에는 검사 시점에 파생한 count/digest와 함께 다음 재생성 명령이 그대로 나온다:

```sh
python3 scripts/check_test_target_integrity.py --write-lib-inventory-manifest
```

명령은 사람이 의도적으로 실행해 매니페스트를 다시 쓰는 절차다. 생성된
`[tests]` 행을 소스 diff와 함께 검토한 뒤 `--verify-lib-inventory`를 다시 실행한다.
검사는 자동으로 매니페스트를 고치지 않는다. 행은 중복 없이 locale-independent
bytewise UTF-8 오름차순이어야 하고, 파일은 UTF-8/LF/최종 LF 형식이어야 한다.
따라서 64-hex/count 한 줄보다 추가·삭제된 test ID 자체가 리뷰 diff에 노출된다.
소스에서 cfg-분기 때문에 같은 full ID가 반복되면 스캐너가 위치를 진단하고
집합에는 한 번만 canonicalize한다. 매니페스트의 중복 행은 거부한다.

이 inventory 검사는 테스트 identity만 확인하며 다음을 보장하지 않는다.

- 대상 테스트에 `#[ignore]`를 붙여 실행에서 제외하는 조작은 identity를 유지하므로
  이 inventory 검사가 거부하지 않는다.
- 컴파일되고 통과하지만 아무것도 검증하지 않는 빈 테스트 본문은 정적 identity
  검사로 원리적으로 판별할 수 없다.
- workflow의 실제 test step에 `if: false`를 넣고 해당 job의 semantic hash를
  재핀해도 Rust test identity는 바뀌지 않는다. 이 inventory 검사는 그 step의
  실행 여부를 검증하지 않는다.
- path filter에 `!src/...` 부정 패턴을 넣어 변경을 lane 선택에서 제외해도 Rust test
  identity는 바뀌지 않는다. 이 inventory 검사는 path-filter의 lane 선택 의미론을
  검증하지 않는다.
- 매니페스트 자체는 같은 PR에서 갱신할 수 있으므로, 그 갱신을 동반한 삭제·이름
  변경을 이 검사만으로 막는 것은 보장하지 않는다. 반드시 소스 diff와 이름 diff를
  함께 리뷰해야 한다.

`--verify-lib-inventory`는 `cargo test --manifest-path Cargo.toml --lib -- --list`를
실행하므로 전체 lib 크레이트 컴파일이 필요하다. 이를 호출하는 PR `Script checks runner`와
main `Main script checks` job은 모두 Rust 1.94.1 toolchain, sccache, Cargo dependency
cache를 먼저 설치한다. 이 wiring을 바꾸면 해당 workflow setup과 이 문서의 재현 명령을
함께 검토한다.

### Gate ↔ 실제 커맨드

| Gate | main 커맨드 | 재현 커맨드 (로컬) |
| --- | --- | --- |
| Full tests | `full_non_pg`의 `just check` step: `just check` | `just check` |
| Full tests (PR) | `library_sweep`의 `Library sweep (selection-set gated)` step | 도달 가능한 PostgreSQL과 `AGENTDESK_REQUIRE_PG=1` 아래에서 `python3 scripts/run_test_lane.py --lane non-pg-sweep --max-summaries 2 --skip _pg --skip pg_ --skip postgres -- env -u AGENTDESK_ROOT_DIR cargo test --lib -- --skip _pg --skip pg_ --skip postgres` (⚠️ 레인 이름과 달리 PG가 필요하다 — 위 §PR 측 library sweep 참조) |
| PostgreSQL tests | `postgres`의 `just test-postgres` step: `just test-postgres` | workflow와 같은 PostgreSQL 환경에서 `just test-postgres` |
| High-risk recovery | `high-risk-recovery`의 `High-risk recovery lane` step: `cargo test --lib high_risk_recovery:: -- --test-threads=1` | 동일 |

## 2. Path Filter Policy

### Always-on (필터 없음)

- **Full tests** / **PostgreSQL tests** 은 path filter 없이 `main` push 시 무조건 실행. 이 두 gate는 `changes` job의 outputs에 의존하지 않으며 `if:` 조건 없이 정의.
- 즉, 커밋이 어떤 파일만 건드리든 Full/PG는 실행되고 red면 merge 차단에 준하는 신호다.

### Conditional (`high_risk_recovery` path filter)

`ci-main.yml`과 `ci-pr.yml`의 `high-risk-recovery` job은 `needs: changes` +
`if: needs.changes.outputs.high_risk_recovery == 'true'` 로 실행된다. 두 workflow의
`changes` job / `Detect changed areas` step에 공통인 필터는 다음과 같다:

```yaml
high_risk_recovery:
  - '.github/workflows/**'
  - 'policies/auto-queue.js'
  - 'policies/kanban-rules.js'
  - 'policies/timeouts.js'
  - 'policies/timeouts/**'
  - 'policies/lib/**'
  - 'policies/__tests__/**'
  - 'src/db/**'
  - 'src/dispatch/**'
  - 'src/engine/**'
  - 'src/high_risk_recovery.rs'
  - 'src/kanban/**'
  - 'src/reconcile.rs'
  - 'src/server/routes/auto_queue.rs'
  - 'src/server/routes/dispatched_sessions.rs'
  - 'src/server/routes/dispatches/**'
  - 'src/server/routes/scheduled_messages.rs'
  - 'src/server/worker_registry.rs'
  - 'src/services/auto_queue.rs'
  - 'src/services/auto_queue/**'
  - 'src/services/scheduled_messages.rs'
  - 'src/services/discord/**'
  - '!src/services/discord/placeholder_live_events/**'
  - 'src/services/message_outbox.rs'
  - 'src/services/platform/tmux.rs'
  - 'src/services/tmux_common.rs'
```

`ci-pr.yml`의 같은 filter에는 PR required lane이 소유하는 아래 경계가 추가로
등재돼 있다:

```yaml
high_risk_recovery: # ci-pr.yml only additions
  - 'src/server/routes/message_outbox.rs'
  - 'src/services/scheduled_messages/**'
  - 'src/services/discord/outbound/source_registry.rs'
  - 'src/services/message_outbox_recovery.rs'
  - 'src/services/message_outbox_recovery_support.rs'
  - 'src/services/message_outbox_recovery_tests.rs'
```

중요: `src/services/auto_queue.rs` (파일)과 `src/services/auto_queue/**`
(디렉터리), `src/services/scheduled_messages.rs`와
`src/services/scheduled_messages/**`는 서로 다른 경로다. `src/kanban/**`와
`src/services/message_outbox.rs`도 recovery 경계로 포함한다.

### Generated docs / architecture drift

- Ordinary generated markdown freshness drift is **warning-only** for PR work.
  Stale `ARCHITECTURE.md` or `docs/generated/**` output is not equivalent to
  Full/PG/High-risk release-gate failure unless the PR is itself changing the
  generator, generated report wording, or the maintainability invariant that the
  report represents.
- `ci-pr.yml` and `ci-main.yml` run `scripts/ci-script-checks.sh`, which invokes
  `scripts/generate_inventory_docs.py` in the CI workspace. That command may
  update generated markdown locally for downstream checks, but generic markdown
  freshness drift is not the hard gate. The hard failures are the generator's
  source-of-truth invariants, such as giant-file registry drift, missing
  metadata, parse errors, or other explicitly coded maintainability errors.
- `ci-nightly.yml`의 `scripts` job은 `Generated tracked-doc drift (warn)` step에서
  `python3 scripts/generate_inventory_docs.py`를 실행하고 inventory docs가 stale이면
  명시적으로 GitHub warning을 내보낸다. 이 step은 `--check`를 사용하지 않는다.
- `.github/workflows/regen-docs.yml` owns the scheduled refresh path. It runs
  weekly, commits regenerated `ARCHITECTURE.md` / `docs/generated/**` output to a
  maintenance branch, and opens a reviewable PR. This keeps generated docs useful
  without forcing unrelated feature/fix PRs to carry mechanical report churn.

### Script checks Python runtime

- `scripts/ci-script-checks.sh` 는 Python 3.11+ 를 최소 런타임으로 요구한다. 이는 `tomllib` 같은 Python 3.11 표준 라이브러리 사용과 `scripts/audit_maintainability.py` 정책에 맞춘다.
- CI의 PR `Script checks runner`와 main `Main script checks` job은 `actions/setup-python`으로 Python 3.11을 명시적으로 설치한다.
- 로컬에서 `python3` 이 3.10 이하이면 `PYTHON=/path/to/python3.11 ./scripts/ci-script-checks.sh` 로 같은 정책을 재현한다. 지원하지 않는 Python 은 check 본문 실행 전에 명확한 오류로 실패해야 한다.

## 3. High-risk recovery lane test axes

`#1011`/`#974` 감사로그는 release gate 의 high-risk recovery lane 이 아래 **4 축**을 회귀 방지선으로 유지해야 한다고 명시한다. 레거시 SQLite 기반 `src/integration_tests/tests/high_risk_recovery.rs` 시나리오 하네스는 #3035 Phase 1 에서 제거되었으며, PG-only 회귀 보호는 `src/high_risk_recovery.rs` 로 이전된다. 아래 시나리오 매트릭스(`failure_recovery` / `outbox_boundary` / `delayed_worker` / `idle_session_cleanup` 축)는 제거된 레거시 하네스 기준 기록이며 PG 스위트 재매핑은 후속 Phase 에서 진행한다. 축별 대표 시나리오는 [`docs/high-risk-recovery-lane.md`](../high-risk-recovery-lane.md#release-gate-축-매핑) 참고.

| Axis | What it guards | Representative scenarios (cargo test filters) |
| --- | --- | --- |
| **Live turn 보존** | restart 직후 in-flight turn / dispatch 가 손실되거나 broken pointer 로 복원되지 않도록 | `high_risk_recovery::failure_recovery::scenario_3_restart_recovery_reconciles_broken_state`, `failure_recovery::scenario_667_restart_recovery_reconciles_duplicate_review_dispatches` |
| **Watcher reattach** | tmux 출력 watcher / deadlock watchdog 가 재시작 후 정상 재부착되고 stale 입력에 잘못 알림 보내지 않도록 | `high_risk_recovery::delayed_worker::scenario_421_deadlock_recent_output_extends_watchdog`, `delayed_worker::scenario_421_deadlock_stale_output_only_marks_suspected_deadlock`, `delayed_worker::scenario_421_long_turn_alerts_start_at_30_minutes` |
| **Dispatch/outbox idempotency** | notify outbox 가 정확히 1회 전달되고 fallback / duplicate / mixed action / completed 상태가 깨지지 않도록 | `high_risk_recovery::outbox_boundary::scenario_160_1_outbox_batch_delivers_exactly_once`, `outbox_boundary::scenario_160_2_recovery_fallback_completes_dispatch`, `outbox_boundary::scenario_160_4_outbox_processes_all_entries_including_duplicates`, `outbox_boundary::scenario_160_6_notify_success_keeps_completed_dispatch_terminal` |
| **Queue loss 방지** | boot reconcile 이 누락된 review dispatch / notify outbox / 깨진 auto-queue entry 를 backfill 하고, idle 세션 정리가 active dispatch 를 잘라먹지 않도록 | `high_risk_recovery::failure_recovery::scenario_251_boot_reconcile_backfills_missing_notify_outbox`, `failure_recovery::scenario_251_boot_reconcile_refires_missing_review_dispatch`, `failure_recovery::scenario_251_boot_reconcile_resets_broken_auto_queue_entries`, `idle_session_cleanup::scenario_492_idle_session_with_active_dispatch_uses_180_minute_safety_ttl` |

이 4 축 중 하나라도 시나리오가 0 개로 줄어들면 lane 자체가 release gate 자격을 잃는다고 본다. 새 시나리오는 위 표 + `docs/high-risk-recovery-lane.md` 동시 갱신 후 PR 에 동봉.

## 4. Resource Contention Policy

`PostgreSQL tests` 와 `High-risk recovery` 는 **각자의 job 에서 `Start PostgreSQL service` 를 따로 실행한다**(`ci-main.yml:153`, `ci-main.yml:234`) — 컨테이너를 공유하지 않는다. 공유되는 것은 job 내부에서 여러 테스트가 같은 PG 인스턴스를 CREATE/DROP DATABASE 로 나눠 쓴다는 점이고, 아래 정책은 그 job-내 경합을 다룬다.

### Serial execution

- `postgres` job의 `just test-postgres` step은 `just test-postgres`를 실행한다. 이 recipe는 `cargo test --lib -- _pg pg_ postgres --nocapture --test-threads=1`로 세 필터를 한 번에 선택하고 **단일 스레드**를 강제한다.
- `high-risk-recovery` job의 `High-risk recovery lane` step은 `cargo test --lib high_risk_recovery:: -- --test-threads=1`을 실행한다 — 동일.
- 이유(#974, `683db919f`): `PgRecoveryTestDatabase::create()` 가 시나리오마다 admin PG connection 을 열어 새 DB 를 만드는데, 기본 병렬 executor 에서는 **admin pool 이 고갈**되어 `pool timed out while waiting for an open connection` 으로 실패했다. `--test-threads=1` 이 순차 실행을 강제해 admin connection 을 재사용하게 한다. 즉 원인은 "테스트 간 lifecycle race" 가 아니라 **connection pool 고갈**이다.
- #973(`24a0e1cb0`)은 이 항목의 근거가 아니다 — non-PG lane 의 skip 필터가 너무 좁아(`_pg_`, `postgres_`) `*_pg`/`pg_*`/`*postgres` 가 새어 들어간 것과 brittle assertion 을 고친 건이다.

### Fixture isolation

- `PgRecoveryTestDatabase::create` 는 test마다 `agentdesk_pg_recovery_<uuid>` 데이터베이스를 신규 생성 → 독립 pool → drop 순으로 정리.
- `crate::db::postgres::lock_test_lifecycle()` lifecycle guard 로 동시 create/drop 직렬화.
- `seed_*` 헬퍼(`seed_agent_pg`, `seed_card_pg` — `src/high_risk_recovery.rs:207`, `:232`)는 `&sqlx::PgPool` 을 받아 **PostgreSQL 에 직접 INSERT 한다.** SQLite `test_db()` fixture 를 쓰지 않는다.

### Pool sizing

- pool=1 은 `src/db/postgres.rs`의 공용 상수 `TEST_POSTGRES_POOL_MAX_CONNECTIONS`가 강제한다. 단일 connection 이므로 startup reconcile 이 runtime pool 을 점유한 채 끝나면 곧바로 pool timeout 으로 드러난다.
- ⚠️ 이전 판이 인용하던 `pg_recovery_test_config` 와 `scenario_969_pg_boot_reconcile_uses_startup_pool_without_pool_timeout_logs` 는 **코드에 존재하지 않는다**(`git grep` 0건). 이 문서에 심볼을 적을 때는 실재를 확인하고 적는다.

## 5. Triage 분류 규약

`scripts/main-ci-triage.sh` 는 `CI Main` 이 2회 연속 red일 때 test identifier 또는 `job::<name>` 단위로 ci-red 이슈를 생성/갱신한다. Release gate 별 분류 계약:

| 실패 형태 | identifier 패턴 | 재현 커맨드 (issue body 에 기록) | Follow-up owner label |
| --- | --- | --- | --- |
| Full tests 개별 케이스 red | `<mod>::<test>` (e.g. `pipeline::tests::…`) | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |
| PG tests 개별 케이스 red | `<mod>::…_pg_…` / `postgres_…` | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |
| High-risk recovery job 자체 red (로그에서 test id 추출 실패) | `job::High-risk recovery` | `_job-level failure; see failing workflow job_` | `agent:project-agentdesk` |
| High-risk recovery 개별 시나리오 red | `high_risk_recovery::<submod>::scenario_…` | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |
| 인프라 종료(job-level, test id 추출 실패 + SIGTERM/signal 15/exit 143/cancel, **real-failure 신호 없음**) | **미기록 — flaky skip** | (없음, ci-red 미승격) | (없음) |
| Job-level red + 공유 술어 `scripts/ci/real-failure-predicate.sh` 의 real-failure 신호 — SIGTERM 노이즈 혼재 여부 무관 | `job::<name>` | `_job-level failure; see failing workflow job_` | `agent:project-agentdesk` |

### SIGTERM / 인프라 종료 = flaky skip (ci-red 미승격) — #3991 / #3996

`job::<name>` 폴백은 실패 job 로그에서 `test … FAILED` assertion 을 하나도 못 뽑았을 때만 발생한다. 이 폴백 로그가 **인프라 레벨 종료** 패턴(러너 OOM/축출로 인한 `signal 15` / `SIGTERM` / `SIGKILL`, `exit 143`, GitHub Actions `The operation was canceled` / `runner has received a shutdown signal`)을 담고 있으면, 이는 코드 회귀가 아니라 flaky 러너 압박이므로 식별자를 **기록하지 않고 skip** 한다 (`log_has_infra_termination`). 따라서 2회 연속 red 여도 ci-red 이슈로 승격되지 않는다. 이 필터는 **오직 job-level 폴백에만** 적용된다 — 실제 `test … FAILED` 가 하나라도 있으면 (SIGTERM 노이즈가 같은 로그에 섞여 있어도) 그 test 식별자는 정상적으로 ci-red 승격된다.

**Real-failure 우선 규약 (#3996):** 인프라 종료 skip 은 **인프라 종료가 유일한 실패 신호일 때만** 적용된다. job-level 폴백 로그에서 공유 술어 `scripts/ci/real-failure-predicate.sh` 의 `log_has_real_failure` 가 잡는 결정적 실패 신호가 하나라도 있으면 — 같은 로그에 SIGTERM/exit 143 노이즈가 섞여 있어도 — 그 job 은 **정상 승격**된다. 공유 술어가 지원 신호의 정본이며 이 문서는 닫힌 마커 목록을 중복하지 않는다. 즉 skip 조건은 `log_has_infra_termination && ! log_has_real_failure` 로, real 신호가 인프라 노이즈보다 항상 우선한다. 이 가드가 없으면 `test … FAILED` 를 남기지 않는 컴파일 회귀(job-level 폴백 경로)가 SIGTERM 문자열 혼재만으로 flaky 오분류되어 조용히 묻히는 false-negative 가 발생한다 (flaky 필터의 최악 실패 모드).

PR 레벨 `Fast check` (`ci-pr.yml check_fast`) 의 signal-15 는 이 triage 대상이 아니다 (triage 는 `CI Main` on `main` 만 처리). PR 측은 재실행 또는 근본완화(러너 자원/컴파일 병렬도 캡, #3658 계열)로 커버한다.

Self-test (`bash scripts/main-ci-triage.sh --self-test`) 는 위 분류가 red → red 2회 연속, recovery, existing issue comment-only, cancelled run skip, skipped lane non-closure 등 엣지 케이스 모두에서 유지됨을 검증한다. 또한 `scenario_three_gate_failures_produce_distinct_identifiers` 가 Full / PG / High-risk recovery 3개 gate 동시 실패 시 서로 다른 식별자 + 서로 다른 issue 가 생성됨을, `scenario_sigterm_job_failure_is_skipped_as_flaky` 가 2회 연속 SIGTERM job-level 실패(real 신호 없음)는 이슈화되지 않음을, `scenario_sigterm_noise_with_real_test_failure_still_creates_issue` 가 SIGTERM 노이즈가 섞여도 실제 test 실패는 여전히 이슈화됨을, `scenario_compile_error_with_sigterm_noise_still_creates_issue` (#3996) 가 SIGTERM 노이즈가 섞인 **컴파일 에러 job-level 폴백**도 real-failure 가드 덕에 ci-red 로 정상 승격됨을 확인한다.

## 6. 누가 소유하는가

- 3개 gate 의 red 신호 → `agent:project-agentdesk` label 로 자동 triage 배정.
- Gate red 가 2회 연속 재현되면 `[ci-red] <identifier> 실패 (main)` 제목의 이슈가 `ci-red` + `agent:project-agentdesk` label 로 생성/업데이트된다.
- 2회 연속 green 이면 자동 close.

## 7. 변경 이력 힌트

- #973 / #974: release gate B-12 도입.
- #1011 (이 문서): path filter gap 보강 (`src/kanban/**`, `src/services/auto_queue.rs`, `src/services/message_outbox.rs`), triage classifier self-test 확장, 4 축 (live turn / watcher reattach / dispatch-outbox idempotency / queue loss) 명시.
- #3991: job-level 폴백에서 인프라 종료(SIGTERM/signal 15/exit 143/canceled) 로그를 flaky 로 분류해 ci-red 미승격 (`log_has_infra_termination`), self-test 2건 추가.
- #3996: flaky skip 에 real-failure 우선 가드 추가 (`log_has_real_failure`) — 인프라 종료 skip 은 `log_has_infra_termination && ! log_has_real_failure` 일 때만 적용. 공유 술어 `scripts/ci/real-failure-predicate.sh` 가 정본인 결정적 실패 신호가 섞이면 (SIGTERM 노이즈 무관) 정상 승격. 컴파일 회귀 job-level 폴백이 SIGTERM 혼재로 오분류되던 false-negative 차단, self-test 1건 추가 (`scenario_compile_error_with_sigterm_noise_still_creates_issue`).

## 8. Operational Post-Deploy Smoke

The CI gates above cannot use live Discord credentials, so live relay continuity
is an operational release smoke instead of a required CI job. For relay-adjacent
or restart-adjacent deploys, run the post-deploy TUI relay continuity smoke
from the release worktree after the build is ready:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py \
  --cell claude-tui \
  --confirm-live \
  --deploy-command 'AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 scripts/deploy-release.sh --skip-review'
```

The same script has CI-safe validation modes:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py --self-check
python3 scripts/e2e/post_deploy_relay_continuity.py --fixture pass
```

Full runbook:
[`docs/runbooks/post-deploy-relay-continuity-smoke.md`](../runbooks/post-deploy-relay-continuity-smoke.md).

## 9. Untrusted `deploy-gate` rollout boundary (#4898)

Migration `0100_block_untrusted_deploy_gate.sql` is the authoritative rollout
containment while trusted typed deployment evidence is unavailable. Its validated
PostgreSQL `CHECK` constraint rejects `deploy-gate` case-insensitively after
trimming ASCII space, tab, newline, carriage return, form feed, and vertical tab.
`NULL`, blank legacy provenance, and `pr-confirm` remain valid. `NOT VALID` is not
permitted: an existing normalized `deploy-gate` row must fail the migration rather
than be silently converted, passed, or left outside enforcement.

The release script stages and signs the candidate binary, proves the PostgreSQL
tunnel, and then runs the candidate's hidden `release-migrate-postgres` command
before requesting `restart_pending` or any drain acknowledgement. If migration
fails, no restart marker or self-exit trigger has been issued and the old process
remains running. Only after migration succeeds does the script drain admissions,
receive durable restart persistence, and proceed to launchd bootout.

This boundary is forward-only. After migration 0100 commits, a binary embedding
only migrations through 0099 cannot restart because SQLx startup validation rejects
the newer database migration. A post-migration activation failure must therefore
fail forward with a 0100-aware binary; it must not auto-restart a pre-0100 rollback
binary. Preflight counts are diagnostic only and are never authority because an old
or concurrent node could insert after a count. PostgreSQL DDL locking plus the
validated constraint serialize legacy writers and enforce the boundary for every
node after commit.

A future trusted deployment-evidence capability must ship a coordinated migration
that explicitly replaces or removes this constraint in the same rollout that
introduces the typed evidence authority. Configuration, policy payloads, or agent
results alone cannot enable `deploy-gate`.
