# T5 → T6 철거 인벤토리 — #5464 (#5071 T5)

> **이 리포 사본이 정본이다.** §12-2 기록은 이 파일에서만 갱신한다.
> 워크플로 초안 `/private/tmp/adk-b0814/t6-removal-inventory.md` 는 S1 r1 시점에
> 동결됐고 더 이상 갱신하지 않는다 — `/private/tmp` 산출물은 T5 배포~T6 삭제
> 과도기를 넘겨 살아남지 않으므로, T6 진입 시 이 파일이 존재한다는 가정이
> 성립해야 한다.
>
> Last refreshed: 2026-08-21 (against #5464 T5 S2 r2 — 축 A 관측 절 + r2 무결성 정정,
> rowless 지표 S7a 재이관, 후속 이관 2번 판정·재이관).

정본 계약: `design-t5-r3.md` §12-2. **T5의 각 구현 슬라이스는, 레거시 경로를 대체할 때마다 그 자리에서
대체된 경로를 이 파일에 기록하고 코드에 deprecated 마킹을 남긴다.** 목적 이중:
① T6 철거를 새 census 없이 마킹 grep의 기계적 작업으로 만든다,
② T5 배포~T6 삭제 과도기에 에이전트가 강등된 코드를 권위로 오인·참조하는 것을 차단한다.

기록 형식: 슬라이스별 1절. 대체가 없으면 **"대체한 레거시 경로 없음(순수 추가)"** 를 명시 기록한다
(빈 절은 "아직 안 썼다"와 구별되지 않으므로 금지).

---

## S1 — cohort infra (배포 no-op) · 브랜치 `feat/5464-t5-s1-cohort`

**S1: 대체한 레거시 경로 없음(순수 추가).**

- 철거 대상 **0건**. deprecated 마킹 **0건**.
- 근거 (열거, 이 슬라이스 diff 전량):

  | 추가물 | 성격 | 대체한 것 |
  |---|---|---|
  | `config::RelayAuthorityMode` + 3술어 | 신규 enum. 기존 enum의 variant를 흡수하거나 대체하지 않는다 | 없음 |
  | `runtime.relay_authority_mode` / `runtime.relay_authority_cohort_percent` | 신규 YAML 키 2개. 기존 키의 rename·alias·deprecation이 아니다 | 없음 |
  | `relay_recovery::cohort`(신규 파일) | 신규 모듈. `session_relay_sink::journal::cohort_bucket`(Uuid 키, delivery-journal 전용)와 **키·소비처가 모두 다르므로 대체가 아니다** — 그 함수는 obligation UUID를 받고 이 함수는 채널 id를 받는다. 두 롤아웃은 독립이고 journal 쪽은 존치 | 없음 |
  | `relay_recovery.rs`의 `#[path]` `mod cohort` (+2줄) | 신규 선언 | 없음 |
  | `DiscordHealthSnapshot.relay_authority_rollout` (detail 전용) | 신규 필드. 기존 필드를 대체·이름변경하지 않는다 | 없음 |
  | `health_api::relay_authority_rollout_health_json` (standalone 분기) | 신규 함수 | 없음 |

- **프로덕션 소비처 — S1 시점 2곳, 둘 다 읽기 전용이고 detail 게이트 뒤에 있다:**
  1. `health::snapshot`의 registry 분기 — `DiscordHealthSnapshot.relay_authority_rollout` 을
     `include_mailbox_details` 로 게이트해 `cohort::rollout_report` 로 채운다.
  2. `server::routes::health_api::relay_authority_rollout_health_json` — standalone 분기,
     `health_response` 의 `if detailed` 블록 안.

  `cohort::admits` 와 S1이 추가한 술어(`records_authority_observations` /
  `governs_destructive_authority` / `consults_cohort`)의 프로덕션 호출자는 **0**이다
  (`admits` 는 그래서 `#[allow(dead_code)]`). 즉 S1은 어떤 판정 경로도 갈아끼우지 않았고,
  따라서 강등되어 "권위로 오인될 수 있는" 구 경로가 생기지 않았다.

  *r1 초안 정정:* 초안은 이 소비처를 "health 발행 경로 1개"로 적었다. 같은 문서의 추가물 표와
  `impl-t5-s1.log` 는 두 지점을 모두 열거하고 있었으므로 문서 내부 불일치였다. 이 수치는 T6 grep
  작업의 출발점이므로 2로 정정한다 — 판정(읽기 전용 / detail 게이트 / 대체 0)은 바뀌지 않는다.
- **AC2-R 위반 7부류(§3.4 + ERRATUM R3-E3)의 구조 판정기는 T6 대상이 아니다** (§12-3):
  후보-지목자 코드는 T5 최종 아키텍처의 영구 구성품이다. S1이 그 어느 것도 만지지 않았다는 사실은
  이 판정과 일관된다.

### S1이 T6에 남기는 유일한 항목 (철거가 아니라 **회수**)

§6.1 **S9(핀 회수, 선택)** 행이 이 슬라이스의 산출물 일부를 −230 prod 후보로 이미 예약하고 있다.
S1 지분은 다음과 같고, **T6 철거 대상이 아니라 S9 회수 대상**이므로 deprecated 마킹을 하지 않는다
(마킹은 "지금 강등됐다"는 뜻이고, 이들은 롤아웃이 끝날 때까지 유효한 권위다):

- `RelayAuthorityMode` enum + 3술어, `runtime` 2필드
- `relay_recovery/cohort.rs` 전량
- health detail `relay_authority_rollout` 발행 스캐폴딩 2곳

회수 진입 조건은 S9의 것과 동일하다 — cohort 100% 승격이 확정되고 롤백 계획이 닫힌 뒤.

---

## S2 — 축 A 관측 (순수 + JSONL, 배포 no-op) · 브랜치 `feat/5464-t5-s2`

**S2: 대체한 레거시 경로 없음(순수 추가).**

- 철거 대상 **0건**. deprecated 마킹 **0건**.
- S2가 계산하는 `*_new` 술어는 **어느 판정 경로도 대체하지 않는다.** 모든 기록 지점이 `()` 를
  반환하므로 호출자는 관측값을 소비할 수 없고, 배송되는 판정은 전량 기존 `*_old` 경로다.
  즉 "강등되어 권위로 오인될 수 있는 구 경로"가 생기지 않는다 — 구 경로가 여전히 **유일한**
  권위다. `entry_gate_new` / `stream_gate_new` 는 S4·S7a가 집행을 가져갈 때 비로소 대체자가
  되며, **그 시점의 인벤토리 기록은 그 슬라이스들의 DoD**다.
- **no-op 주장의 범위 (r2 정정)**: 이 주장은 **판정·배달 값**에 대한 것이다 — 어느 호출자도
  관측값을 소비하지 않으므로 값은 불변이다. **timing 은 포함되지 않는다**(legB 판정): 기록 ON
  상태에서 매 stream tick 이 전역 mutex 를 동기 획득하고, loop exit 과 **후임 턴의 진입 게이트**가
  동기 JSON 직렬화 + 파일 append 를 수행한다. 특히 후임 진입의 선행 IO 는 그 턴의 lifecycle
  게이트·앵커 생성보다 **앞서므로** 느린 파일시스템은 새 턴의 첫 가시 편집을 지연시킨다.
  배송 다이얼(`Legacy`/`0`)에서는 틱당 relaxed load 1회가 전부이고 나머지는 실행되지 않는다.
- 근거 (열거, 이 슬라이스 diff 전량):

  | 추가물 | 성격 | 대체한 것 |
  |---|---|---|
  | `relay_recovery/authority_observation.rs`(신규 파일) | 신규 모듈. 기존 관측·메트릭 모듈을 흡수하지 않는다 — `discord::metrics` 는 턴 소요/토큰을 계속 자기 파일에 쓰고, 이 모듈은 `relay_authority/` 라는 별도 디렉터리를 쓴다 | 없음 |
  | `relay_recovery.rs` 의 `#[path] mod authority_observation` (+2줄) | 신규 선언 | 없음 |
  | `entry_gate_old` / `stream_gate_old` | 배송 중인 술어의 **거울**. 원본을 대체하지 않으며, 두 파일의 mirror 테스트가 원본과의 일치를 전 입력 도메인에서 강제한다 | 없음 (거울은 대체가 아니다) |
  | `entry_gate_new` / `stream_gate_new` | 아직 소비처 0. 대체는 S4/S7a에서 발생 | 없음 (**S4/S7a 예약**) |
  | `lease_range_shape` + `LeaseRangeShape` | 신규 순수 분류기. `BridgeLeaseAcquire::NoRange` 판정을 복제하지 않고 그 정의역만 3분류한다 | 없음 |
  | JSONL 이벤트 로그 `relay_authority/YYYY-MM-DD.jsonl` | 신규 sink. `metrics/` 의 파일을 대체·병합하지 않는다 | 없음 |
  | `DiscordHealthSnapshot.relay_authority_observation` (detail 전용) | 신규 필드. S1의 `relay_authority_rollout` 을 대체하지 않고 **옆에 선다** — S1 필드는 다이얼(mode/percent/fingerprint), S2 필드는 링+카운터다 | 없음 |
  | `health_api::relay_authority_observation_health_json` (standalone 분기) | 신규 함수. S1의 `relay_authority_rollout_health_json` 과 같은 이유로 같은 자리에 선다 | 없음 |
  | `scripts/relay_authority_rollout_report.py`(신규, prod 0) | 신규 집계 스크립트. 기존 리포트 스크립트를 대체하지 않는다 | 없음 |

- **S1 코드 변경 1건 — 대체가 아니라 S1이 명시적으로 위임한 후속 처리:**
  `cohort::admits` 의 `#[allow(dead_code)]` 를 제거했다. S1의 그 속성 독스트링이
  *"S2 is the first caller; dropping this attribute belongs to that slice"* 라고 스스로
  위임하고 있었고, S2의 `authority_observation::observing_dial` 이 그 첫 호출자다.
  판정 로직·시그니처·해시 벡터는 불변이므로 T6 대상이 아니다.

- **프로덕션 소비처 — S2 시점 5곳.** 기록 3곳은 전부 `()` 반환, 발행 2곳은 읽기 전용·detail 게이트 뒤:
  1. `turn_bridge::bridge_entry_persist::establish_bridge_entry_authority` — 진입 게이트 기록.
     `bridge_entry_lifecycle_can_continue(outcome)` 호출 **직전**에 삽입했고 `outcome` 은 불변이다.
  2. `turn_bridge::stream_tick::guarded_persist::visible_mutation_authority_after_guarded_save`
     — 스트림루프 게이트 기록 1곳으로 `authorize_visible_mutation!` 16개 사이트 전량을 덮는다.
     반환값 `authority` 는 기록 전에 계산되어 그대로 반환된다.
  3. `turn_bridge::post_loop_finalize::run_post_loop_finalize` — loop-exit 기록 + 턴당 단일 flush.
  4. `health::snapshot` registry 분기 — `include_mailbox_details` 게이트.
  5. `server::routes::health_api::relay_authority_observation_health_json` — standalone 분기.

  `turn_bridge/mod.rs` 는 **무접촉**이다(핫파일 raw 헤드룸 0). 접촉한 turn_bridge 하위 3파일은
  핫파일 목록에 없고 `check_hotfile_ratchet.py` green 으로 확인했다.

- **AC2-R 위반 7부류의 구조 판정기는 여전히 T6 대상이 아니다**(§12-3). S2는 그중 어느 것도
  만지지 않았다.

- **파일 집합 (git 실측).** r1의 "설계 S2 행 9파일과 일치, 초과 0"과 r2의 정정("코드 7 … 총계
  15") 둘 다 부정확했다(legA P2-2 → P2-4로 재발). 손으로 세는 것을 그만두고
  `git diff --name-status` 실측으로 고정한다. rc에서도 같은 방식으로만 갱신한다.

  아래 r1~r3 행은 **리뷰 캡을 소진한 작업 브랜치**(`feat/5464-t5-s2`, HEAD `6bd7b66b2`)의 측정이며
  그 커밋들은 이 커밋의 조상이 **아니다** — rc가 `origin/main` 위에서 재구성했기 때문이다.
  라운드별 증감의 이력으로만 읽고, 이 커밋의 정본 측정은 **rc 행**이다.

  | 범위 | 명령 | 파일 | 구성 |
  |---|---|---:|---|
  | r1 (이력) | `git diff --name-status af89885c8 1bc554cd7` | **13** | 코드 8 + 생성물 3 + 문서 1 + 스크립트 1 |
  | r2 (이력) | `git diff --name-status 1bc554cd7 537267768` | 11 (신규 2) | 신규 = `guarded_persist_tests.rs`, `tests/test_relay_authority_rollout_report.py` |
  | r3 (이력) | `git diff --name-status 537267768 6bd7b66b2` | 5 (신규 0) | 코드 1 + 스크립트 1 + 파이썬 테스트 1 + 문서 1 + 생성물 1 |
  | rc 델타 (이력) | `git diff --name-status 6bd7b66b2 HEAD -- <S2 슬라이스 17경로>` | 7 (신규 1<sup>‡</sup>) | 코드 2 + 스크립트 2 + 파이썬 테스트 1 + 문서 1 + 생성물 1<sup>†</sup> |
  | **S2 누계 (정본)** | `git diff --name-status 945d70a07 HEAD` | **17** | 코드 9 + 생성물 3 + 문서 1 + 스크립트 3 + 파이썬 테스트 1 |

  <sup>†</sup> rc 행의 생성물 1 = `lib_test_inventory_manifest.txt`. 내용 차이는 `origin/main`
  이 `af89885c8` → `945d70a07` 로 전진하며 바꾼 8줄이고, **rc 자신이 더한 lib 테스트는 0건**이다
  (rc의 신규 회귀는 전부 파이썬이다). S2 누계가 이 매니페스트에 더하는 항목은 r1~r3의 16건 그대로다.

  <sup>‡</sup> rc 신규 1 = `scripts/test_only_module_skip_pin.py`. **재착지가 드러낸 선재 게이트
  결손이다** — r2가 만든 전체-테스트 파일 `guarded_persist_tests.rs` 가 writer-gate 의 whole-file
  skip 집합에 핀되지 않아 census 가 `96 skipped / 95 pinned` 로 어긋났고, 이를 읽는 파이썬 게이트
  4건(`test_durable_frontier_writer_call_sites` 2, `test_intake_outbox_done_writer_call_sites` 2)이
  red 였다. r1~r3 의 게이트 실행이 이 4건을 targeted 목록에 넣지 않아 놓쳤다. 핀 1줄로 닫았고
  4건 모두 green 이다. 경로 목록이 16 → 17 로 늘어난 유일한 이유다.

  r1의 코드 8 = 설계가 열거한 5(`authority_observation.rs`, `bridge_entry_persist.rs`,
  `post_loop_finalize.rs`, `guarded_persist.rs`, `snapshot.rs`) + `health_api.rs` +
  `relay_recovery.rs`(mod 선언) + `cohort.rs`(S1 위임 건) — 설계의 5는 mod 선언 파일과 S1
  위임 건을 계상하지 않았다. 생성물 3 = `ARCHITECTURE.md`,
  `docs/generated/route-inventory.md`, `scripts/lib_test_inventory_manifest.txt`. 누계의
  코드 9 = 위 8 + r2 신규 `guarded_persist_tests.rs`, 스크립트 2 =
  `relay_authority_rollout_report.py` + `ci-script-checks.sh`. r2가 적은 "총계 15"는 바로
  그 `guarded_persist_tests.rs` 를 빼먹은 값이며, 같은 커밋의 `ARCHITECTURE.md` 가 그 파일을
  트리에 추가하고 있었으므로 내부 모순이었다. **"초과 0"은 여전히 사실이 아니다** — 초과분은
  전부 의무 산출물(생성물·게이트 배선·회귀 테스트)이지만 0은 아니다.

### S2 r2 — 관측 무결성 결손 정정 (dual r1 리뷰 P1 6건, ERRATUM R3-E4)

r1의 기록기는 판정·배달을 바꾸지 않았지만 **기록된 데이터가 승격 게이트에 쓸 수 있는가**에서
결손이 있었다. r2가 닫은 것과, 닫지 않고 **재이관**한 것을 구분해 남긴다.

- **재이관 1건 — `rowless_no_range_share` 는 S7a 소유**(legA P1-1, E4-6). `Missing`-at-entry
  턴은 배송 진입 게이트가 `End` 로 판정해 `turn_bridge/mod.rs` 에서 즉시 반환하므로
  `run_post_loop_finalize` 에 **구조적으로 도달할 수 없다**. 따라서 rowless × loop_exit = ∅ 이고
  r1의 `LoopExitObservation.rowless_continuation` 은 프로덕션에서 영구 `false` 였다. 항상-거짓
  필드를 방출하는 것은 false-green 벡터이므로 **필드를 삭제**했고, 스크립트의 미측정 필드는
  `frontier_already_covers` / `unbound_anchor_left` / **`rowless_no_range_share`** 3개로 정정했다.
  측정 가능 시점은 S7a 집행 착지 이후 — **그 시점의 재도입은 S7a의 DoD**다.
- **관측 시각 필드 추가**(legA P1-2, E4-5). `ts` 는 발행 시각이므로 축출-발행되는 목표 모집단이
  후임 턴 도착 시각으로 오귀속됐다. 방출 스탬프에 `observed_at`(턴의 `started_at`)을 추가하고
  당시 스키마를 `relay_authority.axis_a.v2` 로 bump했고, 스크립트의 window·지문 구간 산정·`--days`
  필터를 전부 관측 시각 기준으로 전환했다. S3의 completion scope 레코드 추가로 현재 스키마는
  `relay_authority.axis_a.v3`이다. 출하 다이얼이 `Legacy`/`0`이라 v2 기록은 생성되지 않았으므로
  기존 레코드를 재분류하는 마이그레이션 부담이 없다.
- **턴 신원 4축**(legB P1-1). 버퍼 매처가 `(user_msg_id, started_at)` 2필드였다 — 같은 초에
  시작한 `user_msg_id == 0` TUI-direct 연속 턴을 구분하지 못해 전임의 지연 tick/loop-exit이
  후임 버퍼를 오염·소멸시켰다. 버퍼는 이제 정본 `InflightTurnIdentity` 를 들고, 턴 도중 불변인
  3축(`user_msg_id`/`started_at`/`turn_start_offset`)으로 판정한다. 4번째 축
  `tmux_session_name` 은 **런타임 핸드오프가 턴 도중 재대입하는 축**이므로(프로덕션도
  `refresh_stream_tick_expected_identity_after_handoff` 로 기대 신원을 재유도한다) 엄격 비교가
  아니라 채택(adopt)한다 — 엄격 비교는 tmux 런타임을 띄우는 모든 턴을 stranded 로 만든다.
- **stranded·유실 관측**(legA P1-3, E4-4). 축출-flush 는 "지연 발행"이 아니라 **조건부 발행**이다
  (후임 부재 / 다이얼 하강 / 프로세스 재시작 시 유실, 상한 채널·프로세스당 1턴). 모듈독의
  *"published late, never lost"* 를 정직 문면으로 교체하고, health 트리아지에
  `resident_buffers`(미발행 잔류)와 `sink_dropped_records`(JSONL 유실 누적)를 노출했다.
  종료 flush 훅은 **의도적으로 만들지 않았다** — 새 배선 기계 없이 카운터로 관측한다.
- **승격 스크립트 fail-closed**(legB P1-2 + legA P2-4). `loop_exit_coverage` 는 항진명제
  (`loop_exit <= bridge_entry`)여서 loop-exit 0건도 PASS였다. 이제 `loop_exit`/`bridge_entry`
  와 `stream_loop`/`bridge_entry` **비율 하한 0.5**, 그리고 unusable 라인 비율 **상한 1%** 가
  기준이다(초과 시 warning 아니라 FAIL). 하한을 1.0 으로 두지 않은 근거: 목표 모집단은
  loop-exit 에 정당하게 미도달하고, visible mutation 이 없는 턴은 stream 레코드가 정당하게
  없다. 그러나 표본의 절반이 결손인 window 는 "브리지가 S4 를 막을 비율로 실패 중"이거나
  "sink 가 유실 중"이며 둘 다 승격을 멈춰야 한다.
- **지문 재진입 구간 분리**(legB P1-3). 단일 bucket 합산을 관측 시각 gap(48h) 기반 구간 분리로
  교체하고, 승격 기준 전량을 **최신 연속 구간 하나**에서만 평가한다. — **이 절의 두 주장은
  r3에서 정정됐다**: (i) "합산되지 않는다"는 48h 미만 다이얼 이탈에서 거짓이었고 (ii) 48h의
  "빈 달력일" 근거는 분할 판별이 못 된다. 아래 "S2 r3" 절이 정본이다.
- **스트림 거울 16셀 전수**(legA P1-5). r1 mirror 테스트는 12셀만 돌았고 빠진 열
  `(authority_unchanged=false, bridge_owns_relay=true)` 이 바로 이 fence 의 주 시나리오였다
  (`intended_authority` 는 저장 전, `authority_unchanged` 는 저장 후 계산). 4셀을 추가해
  "full three-operand product" 문면을 사실로 만들었다.
- **`TRIAGE.channels` 유계화**(legA P2-3). 링 깊이만 유계였고 채널 엔트리는 영구 누적이었다.
  채널 상한 32 + least-recently-recorded 축출.

### S2 r3 — 승격 게이트의 구간 정직성 (dual r2c 리뷰 P1 3건)

r2가 닫은 것은 "기록된 데이터"였고, r3가 닫는 것은 **그 데이터를 읽는 승격 게이트**다. 세 P1은
전부 "구간(segment) 하나에서만 평가한다"는 문면이 코드와 어긋난 자리였다.

- **다이얼 경계는 시계가 아니라 입력의 증언**(legA/legB r2c P1-1). r2는 지문별로 버킷을 나눈
  **뒤에** gap을 봤으므로, F 두 구간 사이에 다른 지문 G의 표본이 끼어 있어도 그 증거를 버리고
  F의 gap이 48h 이하면 병합했다. 재현: 120턴/4일 + 100턴/4일 사이에 22h36m 다이얼 이탈 →
  220턴/8일 단일 구간으로 `promotion_ready=True`. 이제 `segment_events` 는 **전역 관측시각
  순서**로 걸으며 다음 표본의 지문이 다르면 그 자리에서 자른다 — 끼어듦은 다이얼이 움직였다는
  직접 증거이고 이미 입력에 있다. gap 48h는 **보조 휴리스틱**으로 남긴다(다이얼이 관측 집합을
  아예 떠나 표본이 0인 구간은 끼어듦으로 증언되지 않는다). 경계 비교는 `>` → `>=` 로 고쳐 정확히
  48h가 병합되지 않게 했다. r2 문면의 "빈 달력일 ≥1 ⇒ 연속 불가" 논증은 **철회**한다: 성질
  자체는 참이지만 gap ∈ (24h, 48h) 의 **비분할** 쌍도 같은 성질을 가지므로 분할 판별 근거가 못
  되고, `window_days` 는 distinct day 를 셀 뿐 연속일을 요구하지 않는다(legA r2c §6).
- **`line_integrity` 를 나머지 5기준과 같은 스코프로**(legB r2c P1-2). 6개 기준 중 이것만
  `load_events` 의 전역 tally를 썼고 `--days` 이전에 계산됐다. 재현: 대상 구간의 3.08% 손실이
  이력 10,000줄과 나뉘어 0.19%로 통과. 유실 라인은 **날짜를 알 수 없으므로**(그것이 unusable의
  정의) 구간에 시각으로 귀속할 수 없다 → tally를 **파일 단위**로 보관하고, 대상 구간의 레코드가
  실제로 읽힌 파일들만 분모로 쓴다. 전역 tally는 `line_integrity_all_files` 로 **표시만** 한다.
  잔여(숨기지 않고 선언): 대상 구간의 사용 가능 레코드가 **하나도 없는** 파일은 스코프 밖이므로,
  통째로 깨진 하루치 파일은 전역 tally에만 나타난다.
  ⚠️ **이 파일 단위 분모는 아래 "S2 rc" 절에서 다시 교체됐다** — 희석의 절반만 닫혔음이
  재현됐다. 현행 정본은 그 절이다.
- **stranded 유실을 정본 안에서 가시화**(legA r2c P1-3). `publish` 가 한 턴의 3개 site 레코드를
  한 번에 쓰므로 전체-턴 유실은 site 비율의 분자·분모를 함께 줄인다 — entry-only 목표 모집단에
  대해서는 커버리지를 **0.667 → 1.000 으로 개선**시킨다(재현). 따라서 site-coverage 하한은
  이 부류의 유실을 **원리적으로 볼 수 없다**. 방출 레코드에 `publish_reason`
  (`loop_exit` | `evicted`)을 추가해 스크립트가 축출-발행 비율을 표시하게 했다. 스키마는
  additive라 v2 유지(미배포, 마이그레이션 0), 키 세트 12→13→**14**.
  **새 승격 기준은 만들지 않았다** — 아래 이관 항목 참조.
- **모듈독 정정 (거짓 안전 논거 제거).** r2 모듈독의 두 절 — 유실이 *"counted rather than
  argued away"* 이고 *"site-coverage floors … block promotion instead of pass on a thinner
  denominator"* — 은 stranded 모집단에 대해 **둘 다 거짓**으로 판정됐다. `sink_dropped_records`
  는 sink가 **시도했다 실패한** 레코드만 세므로 stranded 유실은 어느 카운터도 올리지 않고,
  커버리지 하한은 위 이유로 반대 방향으로 움직인다. 그 자리에는 이제 코드가 실제로 보장하는
  것만 쓴다: 유실 상한(채널·프로세스당 1턴), 편향 방향(목표 모집단 쪽), 카운터가 세는 것과 못
  세는 것, 게이지의 비내구성(프로세스-로컬, 재시작 시 소멸, 승격 정본에 없음), 그리고
  coverage floor가 전체-턴 유실을 못 보는 이유. r1에서 붕괴된 "thinner denominator" 논거는
  **되살리지 않았다** — `append_jsonl` 독스트링의 같은 논거는 sink 유실 모집단에 대해서는
  성립하므로 그 자리에만 남아 있다.
- **죽은 4번째 축 제거**(legA r2c P2-1). `identity.tmux_session_name` 은 `adopt_runtime_handoff`
  자신의 가드에서만 읽히는 write-only 축이었다(`is_turn` 은 3축, `TurnStamp` 에 없음,
  `TurnObservation` 은 `Serialize` 미파생, 트리아지 미노출) — 제거하면 출력이 바이트 동일하다.
  adopt 기계를 삭제하고 버퍼는 3축을 인라인으로 든다. 잔여 충돌 정의역 2개를 독스트링에 선언:
  (1) 이 완화가 여는 「3축 동일 ∧ session 상이」는 프로덕션 타이밍에서 구성 불가(핸드오프는
  초 단위, `started_at` 은 1초 해상도), (2) 「4축 전부 동일」은 정본 자신의 선재 성질이며 엄격
  4축 비교로도 닫히지 않는다.
- **`SITE_COVERAGE_FLOOR` 자기차단 한계 선언**(legA r2c P2-5). 목표 모집단이 window의 50%를
  넘으면 하한은 **영구 FAIL**이고, 그 실패는 S4가 고치려는 현상 그 자체다. 임계값은 바꾸지
  않는다(fail-closed 유지) — 탈출구는 더 낮은 숫자가 아니라 S4가 더 좁은 모집단이나 모집단별
  분모를 들고 오는 것이다. 스크립트 상수 독스트링에 그대로 적었다.
- **반례 테스트 검출력 정렬**(legA r2c P2-2). 후임 턴을 2틱으로 만들어 `ticks` 단정이 문면대로
  검출자가 되게 했다 — 2필드 매처로 되돌리면 전임의 지연 tick 1개가 후임 버퍼에 들어가므로
  `ticks == 1` 단정은 우연히 통과했고, 실제 검출자는 `old_ended_lifecycle` 하나뿐이었다.
- **`--days` 비용 특성**(legA r2c P2-8②). 필터 축은 관측시각이지만 `load_events` 는 항상 전체
  파일을 읽는다 — 판정 window만 좁히고 I/O는 줄이지 않는다는 사실을 도움말에 명시했다.
  (`line_integrity` 가 파일 단위 스코프가 되면서 `--days` 가 분모를 무시하던 P2-8① 도 닫혔다.)

**r3에서 만들지 않은 것**: 새 승격 기준 0, 새 health 카운터 0, 종료 flush 훅 0, 임계값 변경 0.

### S2 r3가 S4로 이관하는 항목 1건 (코드 변경 없음)

**축출-발행 비율을 승격 기준으로 삼을지는 S4 소유다.** r3는 `publish_reason` 을 정본에 넣고
스크립트가 `publish_reasons` / `evicted_publication_share` 를 **표시**하게 했을 뿐이고, 이 값에
대한 하한·상한은 두지 않았다. 근거: (a) 정당한 축출 비율의 실측 분포가 없다 — 목표 모집단
(`Missing`-at-entry, `AuthorityLost`)은 **정의상 축출-발행되므로** 건강한 window에서도 이 비율은
높다. (b) 표시된 비율은 "유실될 수 있었던 표본의 비율"이지 "유실된 표본의 비율"이 아니다 —
후자는 정의상 로그에 없다. 임계값을 지금 정하면 근거 없는 숫자가 fail-closed 로 굳는다.
결정해야 할 것: (i) 이 비율에 상한을 둘지, (ii) 축출 모집단을 분리한 별도 커버리지 분모를 쓸지,
(iii) 아니면 S4의 집행이 목표 모집단을 loop-exit 도달 가능하게 만들어 이 비율 자체를 떨어뜨리는
것으로 갈음할지. 소유: **S4** (§8 L-8 보존 정책과 같은 런북에서 결정되어야 한다).

**rc 보강(r3c P2-6) — 이 비율은 피해와 역행한다.** stranded 로 실제 유실된 턴은 로그에 아예
없으므로 비율을 **올리는 게 아니라 내린다**. 전량 유실된 window 는 `0.0` 을 보고하며, 이는
"축출이 한 번도 없었던 건강한 window" 와 구별되지 않는다. 즉 **낮은 값은 안심 신호가 아니고,
정보를 담은 것은 높은 값뿐이다**. 상한을 검토하는 (i) 이 방향 문제를 그대로 물려받는다 —
상한은 높은 쪽만 잡으므로 전량 유실을 통과시킨다. 스크립트 모듈독에 같은 문장을 적었다.

### S2 rc — origin/main 위 재구성 재착지 + 잔여 P1 폐쇄 (dual r3c 리뷰)

r3까지의 작업 브랜치는 리뷰 캡 3라운드를 소진했고, r3c dual 리뷰는 **그 내용 전체가 검증 완료이되
잔여 P1 정확히 1건**으로 판정했다(양 leg 수렴, 각자 독립 재현). 라운드를 하나 더 여는 대신
`origin/main` 위에서 **재구성**해 재착지했다 — cherry-pick 이 아니라 재작성이며, 위 r1~r3 절의
내용은 그대로 검증 자산으로 유지된다. 아래 세 가지만이 r3 브랜치 HEAD 대비 실질 델타다.

- **잔여 P1 폐쇄 — `line_integrity` 분모를 대상 구간 자신의 레코드로 교체**(legA/legB r3c P1-1).
  r3의 파일 단위 분모는 희석의 **절반만** 닫았다. daily 파일명은 **발행일**이고 다이얼은 하루
  중간에 움직이므로, 대상 구간의 **첫 파일에는 직전 구간의 그날치 레코드가 동거**한다 — 구간
  탄생의 예외가 아니라 기본값이고, 대상 표본이 가장 얇을 때(하한이 가장 필요할 때) 동거가 가장
  심하다. 재현(r3 자신의 F→G→F 형태): 대상 630줄 + unusable 20줄 = 3.08% FAIL 이어야 할 것이
  동거 1,530줄 때문에 `20/2180 = 0.92%` PASS 로 읽혀 `promotion_ready` 가 뒤집혔다. 뒤집기
  임계도 특정된다 — 동거 1,350줄(450턴)에서 정확히 1.00%. 넓게 올렸다 좁게 되돌리는 표준
  롤아웃이 이 비율을 만든다.
  **교체**: 분모 = 「대상 segment 의 usable 레코드 수 + 대상 파일들의 unusable 라인 수」.
  동거 usable 라인은 분모에서 빠지고 `cohabiting_usable_lines` 로 **표시**돼 배제가 감사 가능하다.
  **보수성 선언(문면에 명시)**: 유실 라인은 날짜를 알 수 없어 동거 구간들 사이에 나눌 수 없으므로
  **대상에 통째로 부과**된다 → 남의 오염이 깨끗한 대상을 FAIL 시킬 수 있다(**잔여 ①**, false-red
  방향 — 승격 게이트가 틀려도 되는 방향). r3 문면이 배제 방향만 선언하고 **포함 방향(fail-open)을
  선언하지 않았던 것**도 이때 함께 정정했다(r3c P2-1). `scoped_integrity` 독의
  *"Same scope as the other five criteria"* 는 문자 그대로 거짓(초집합)이었으므로 삭제했다.
  **정정(rc r2, legA rc P1-1)**: rc 문면 3곳이 「잔여 ①과, 대상 레코드가 하나도 없는 파일이
  스코프 밖이라는 **잔여 ②** 는 둘 다 false-red」라고 적었으나 **잔여 ②는 방향이 반대다 —
  fail-open 이다**. 스코프 밖 파일의 unusable 라인은 분자와 분모에서 **함께** 빠지므로
  `U/(R+U)` 를 `(U−u)/(R+U−u)` 로 **낮추는** 연산이고, 판정을 완화한다. 이 라벨은 r3c P2-1이
  같은 잔여를 "배제 방향(fail-closed)"으로 적은 것을 승계한 결과이며, 잔여 자체는 rc가 만든 게
  아니라 파일 단위 스코프 시절부터 있던 것이다 — 그러나 그것을 "선언 완료"로 주장한 것이 rc이므로
  여기서 정정한다. 도달 형태: 스키마 bump 후 mixed-version 운용으로 대상 구간 9일 중 하루치
  파일이 통째로 `schema_mismatch` 가 되면 `scoped 0/810 = 0.0% PASS` 인데 전체 로그의
  `1000/1810 = 55.25%` 가 unusable 이고 `promotion_ready=True` 다. 정상 운영 형태로는 이력 파일에
  쌓인 junk 36줄이 통째로 기준 밖으로 나가 `0/630 = 0.0%`(전체 `36/846 = 4.26%`)가 된다.
  **잔여 ②는 여섯 기준 어느 것도 보지 못하므로 런북 독자가 표시로 감시해야 한다** — 이 라운드에
  `cohabiting_usable_lines` 와 **대칭인 표시 필드** `out_of_scope_unusable_lines`(스코프 밖
  unusable 라인 수)를 추가해 배제량을 감사 가능하게 했고, 그 필드와 `line_integrity_all_files`
  비율이 감시 수단이다. 둘 다 **표시일 뿐 판정 입력이 아니며 새 기준은 추가하지 않았다**(기준은
  여전히 정확히 6종). 잔여 ②의 근본 폐쇄는 잔여 ①과 마찬가지로 유실 라인 위치 기반 날짜 추정이고,
  S4 소관이다.
- **선재 자기결함 폐쇄 — `load_events` 의 비-dict JSON 라인**(legA r3c P2-2, r1 유래).
  `3` / `"tail"` / `true` / `null` / `[1,2]` 는 전부 유효한 JSON 이라 decode 를 통과한 뒤
  `event.get("schema")` 에서 `AttributeError` 로 **프로세스를 죽였다**. 방향은 fail-loud 라
  조용한 오판정은 아니지만, 밟히는 입력이 하필 `MALFORMED_LINE_CEILING` 이 **존재 이유로
  선언한 바로 그것**(교차프로세스 인터리빙 — 한 writer 의 라인 조각이 단독으로 파싱됨)이라,
  방금 스코프를 고친 기준이 **아예 평가되지 못하는** 경로가 열려 있었다. `isinstance(event, dict)`
  가드로 다른 쓰레기 라인과 같은 unusable tally(`schema_mismatch`)에 흡수한다. 회귀 1건.
  r3c 가 "내구 산출물 어디에도 소유자가 없다"고 지적한 항목이며, 이관이 아니라 **폐쇄**로 처리했다.
- **P2 7건 처분.** 문면·낡은 상호참조는 이 커밋에서 정정하고, 코드 성격은 아래 이관 절에
  소유자를 명시해 넘겼다. 처분표:

  | 항목 | 처분 | 자리 |
  |---|---|---|
  | P2-1 파일 스코프 잔여가 한 방향만 선언됨 | **정정** (P1 폐쇄에 흡수) | 스크립트 모듈독 + `scoped_integrity` 독 |
  | P2-2 `load_events` 결함에 소유자 없음 | **폐쇄** (결함 자체를 수정) | `load_events` + 회귀 1건 |
  | P2-3 지문이 증언하는 것은 "호스트 간 불일치" | **정정 + 이관** | `segment_events` 독 · `cohort.rs` 독 / 소유 S4 |
  | P2-4 gap 휴리스틱이 덮는 경우 열거 1갈래 누락 | **정정** | `segment_events` 독 |
  | P2-5 `open_turn` 의 *"flushing, never dropping"* 절대 표현 | **정정** | `open_turn` 독 (층위 한정 병기) |
  | P2-6 `evicted_publication_share` 의 방향 미문서화 | **정정 + 이관** | 스크립트 모듈독 / 소유 S4(기존 이관 항목에 병합) |
  | P2-7 키 세트 6개 "단정으로 고정"이 실제로는 없음 | **폐쇄** (단정을 추가) | `test_the_criteria_key_set_is_exactly_the_six_axis_a_gates` |
  | P2-8 `cohort.rs` L-7 상호참조가 두 라운드째 낡음 | **정정** | `cohort.rs` 독 (정본 판별자 = 끼어든 지문) |

**rc에서 만들지 않은 것**: 새 승격 기준 0, 새 health 카운터 0, Rust 판정·배달 경로 변경 0,
임계값 변경 0. Rust 델타는 독스트링 2곳뿐이다.

### S2 rc가 S4로 이관하는 항목 2건 (코드 변경 없음)

1. **유실 라인의 날짜 귀속은 S4 소유.** rc 의 분모는 unusable 라인을 파일 단위로만 귀속하므로
   동거 구간의 오염이 대상에 부과된다(false-red). 완전한 해는 파싱 실패 라인을 **앞뒤 datable
   라인 사이 위치로 날짜 귀속**하는 것이고, 그것은 이 슬라이스가 얹을 기계가 아니다. 지금
   방향이 보수적이라 승격을 잘못 열지는 않으므로 긴급하지 않다. 소유: **S4**.
2. **`cohort_fingerprint` 의 호스트 무관성은 S4 소유**(r3c P2-3). 지문은 `mode` + `percent`
   만 담고 host 를 담지 않는다. 따라서 끼어듦은 "다이얼이 움직였다"가 아니라 "관측 모집단 안에서
   지문이 갈렸다"를 증언하며, config 배포가 호스트별로 어긋난 동안에는 두 호스트의 표본이 초
   단위로 교차해 구간이 파편화되고 target 이 영구히 작아진다. 방향은 fail-closed(승격이 막힐
   뿐)라 지금 고치지 않는다 — 고치려면 지문에 host 를 넣어야 하고, 그러면 **호스트마다 별도
   구간**이 되어 승격 판정 자체를 다시 설계해야 한다. 결정해야 할 것: 호스트별 판정으로 갈지,
   배포 수렴을 전제로 둘지. 소유: **S4** (위 축출-비율 항목과 같은 런북).

### S2가 T6/S9에 남기는 항목 (철거가 아니라 **회수**)

§6.1 **S9** 행이 `authority_observation.rs` 전량을 −230 prod 회수 후보로 이미 예약하고 있다.
S2 지분은 그 파일 전체 + 두 health 발행 지점 + 기록 3지점이며, **회수 진입 조건은 S1과 동일**하다 —
cohort 100% 승격이 확정되고 롤백 계획이 닫힌 뒤. deprecated 마킹은 하지 않는다(롤아웃이 끝날
때까지 유효한 권위다).

추가로 T6가 아니라 **운영**에 남는 항목 1건: JSONL 이벤트 로그에 **보존 정책이 없다**(설계 §5.4,
§8 L-8). 승격 판정에 ≥7일 window가 필요하므로 파일이 남아야 하고, 그 이상은 무한 누적이다.
S2는 정리 책임자를 만들지 않았다 — 설계가 그것을 S2 범위로 두지 않았기 때문이며, 소유는 §8 L-8에
그대로 남는다.

### S1 리뷰 후속 이관 2번 ("소유: S2 착수 전")에 대한 S2의 판정

아래 "후속 이관" 2번은 *mixed-version whole-config rewrite 가 신규 다이얼 키를 유실한다* 이고
소유가 **S2**로 지정돼 있었다. S2의 판정: **호환 계약을 세우지 않고 진행한다.**

근거는 S2가 이 knob를 쓰는 방식이 관측 전용이라는 점이다. 키가 유실되면 다이얼은 `Legacy/0` 으로
돌아가고 그 효과는 **관측이 멈추는 것**이 전부다 — 판정·배달 경로는 S2에서 다이얼을 읽지 않으므로
불변이고, 유실 방향은 fail-safe(더 적게 관측)이지 fail-open이 아니다.

**r2 정정 — 이 fail-safe 논거가 무엇에 의존하는지.** r1은 "표본이 줄면 ≥200턴/≥7일 바닥에서
걸리므로 얇은 분모로 통과할 수 없다"고 적었다. legB P1-2가 이 논거를 붕괴시켰다: 턴 개수만 세는
바닥은 **다른 노드/다른 구간의 entry-only 200턴**으로 충족될 수 있고, 그 window 에는 S4가 승격될
근거인 stream-gate 증거가 한 건도 없다. 즉 "관측 중단 → 승격 차단"은 자동으로 성립하지 않는다.
논거는 이제 **항목별 completeness 게이트에 의존한다** — 위 "S2 r2" 절의 site-coverage 비율 하한
(loop_exit/bridge_entry, stream_loop/bridge_entry ≥ 0.5), unusable 라인 상한 1%, 그리고 승격
기준을 최신 연속 구간 하나에서만 평가하는 segmentation. 이 세 가지가 있을 때 비로소 부분 유실이
"얇은 분모로 통과"가 아니라 **FAIL**이 된다. 이관받는 S4/S6는 이 게이트가 여전히 살아 있음을
전제로만 fail-safe 를 주장할 수 있다.

**r3/rc 정정 — 그 세 게이트 중 둘은 r2 시점에 실효가 없었고, 하나는 r3 시점에도 아직 없었다.**
segmentation 은 48h 미만 다이얼 이탈에서 두 window 를 병합했고(r2c P1-1, 재현), unusable 라인
상한은 전역 분모라 이력으로 희석됐다(r2c P1-2, 재현). r3가 segmentation 은 닫았으나 라인 상한은
**절반만** 닫았다 — 분모를 파일 단위로 좁혔을 뿐이라 같은 daily 파일에 동거하는 다른 구간의
라인이 그대로 희석에 참여했고, 3.08% 손실이 0.92%로 읽혀 승격이 뒤집혔다(r3c P1-1, 양 leg 수렴·
재현). **rc가 분모를 대상 구간 자신의 레코드 수로 교체해 닫았다** — 아래 "S2 rc" 절. 따라서 이
논거가 세 게이트 전부에 대해 실효를 갖는 것은 **rc 시점부터**다. 단 **전체-턴 유실은 여전히 이 세
게이트 밖이다** — site 비율은 원리적으로 그것을 볼 수 없고, 위 "S2 r3" 절의 `publish_reason`
표시가 그 자리를 대신하되 기준은 아니다. S4/S6는 이 한계까지 함께 전제해야 한다.

**따라서 이 항목의 소유는 S4/S6(집행 슬라이스)로 이관한다.** 거기서는 키 유실이 집행 중단을
뜻하므로 관측 중단과 달리 계약이 필요하다. 후속 이관 절의 1번(폭 클램프 fail-open)과 소유가
같아지며, 두 항목은 같은 롤아웃 런북에서 함께 결정되어야 한다.

---

## S3 — completion ownership 관측·억제

**S3: 대체한 레거시 경로 없음(순수 추가 + 기존 completion 효과 게이트).**

- L-21 옵션 A인 TUI-direct synthetic mailbox token/nonce의 bridge 운반은 이번 슬라이스에서
  구현하지 않았다. synthetic 턴 수명을 바꾸는 라이브 경로 리스크보다 안전성을 우선했다.
- 대신 옵션 B로 non-permitting completion을 다이얼·코호트와 무관한 경고 및 health 누적
  `completion_suppressions`로 관측하며, `turn_source`를 completion 기록에 추가한다.
- 실측 triage에서 TUI-direct `Foreign` 억제와 namespaced session의 `TURN_ACTIVE` 잔류 빈도·영향을
  확인한 뒤 옵션 A 또는 동등한 episode-bound witness 운반을 재평가한다. 이 재평가는 T6 철거가
  아니라 후속 설계 항목이며, 채택 전까지 현재 잔여를 해결됐다고 간주하지 않는다.

## S4 이후

미작성. 각 슬라이스 구현 시 이 파일에 절을 추가한다 (§12-2 = 슬라이스 DoD).

---

## 후속 이관 (S1 리뷰에서 확정, S1 스코프 밖 — 코드 변경 없음)

S1 dual 리뷰(legA/legB)에서 나왔고 S1에서 결정하지 않은 항목이다. 각 항목의 **소유 슬라이스**가
결정 주체이며, S1 r2 수리 커밋은 이 두 항목에 대해 코드를 바꾸지 않았다.

1. **cohort 폭 클램프의 극성이 fail-open이다 — 롤아웃 런북 결정 사항.**
   `cohort::admits` 와 `cohort::rollout_report` 는 `percent.min(100)` 으로 범위 초과값을 100으로
   **올린다**(코드와 두 곳의 독스트링이 일치하므로 정직성 문제는 아니다). 결과: `Enforce` 에서
   `20` 을 `200` 으로 오타하면 소비자가 배선된 뒤에는 즉시 전 채널 등록이고, health 블록은
   `cohort_percent` 필드에 **클램프된 값만** 발행하므로(리포트 필드 독스트링이 그 선택을 명시)
   오퍼레이터에게 "오타"라는 단서가 남지 않는다. `u8` 파싱이 256 이상을 거르므로 도달 가능한 오타
   구간은 `101..=255` 다. 설계에 극성 규정이 없어 S1 계약 위반은 아니다.
   결정해야 할 것: (a) 집행 전 validation으로 `>100` 을 거부할지, (b) raw / effective 를 health에
   함께 노출할지, (c) fail-open 을 명시적으로 승인하고 런북에 오타 복구 절차를 넣을지.
   소유: S4/S6 집행 슬라이스의 롤아웃 절차.
2. **mixed-version whole-config rewrite가 신규 dial 키를 유실한다 — S2 착수 전 호환 계약 필요.**
   구 바이너리는 `RuntimeSettingsConfig` 에 `deny_unknown_fields` 가 없어 신규 키 2개를 무시하고
   읽을 수 있지만, `discord::settings::write::save_bot_settings` 가 부르는
   `persist_bot_auth_to_yaml_checked` 는 `config::load_from_path` 로 typed `Config` 를 읽고 수정한 뒤
   `serde_yaml::to_string` 결과로 `agentdesk.yaml` **전체를 다시 쓴다** — 구 parser가 무시한 키는 그
   rewrite에서 **사라진다**. Discord의 여러 config/control 커맨드가 이 writer를
   호출하므로 downgrade 또는 mixed-version 운영에서 도달 가능하다. 이는 기존 typed whole-file
   writer의 일반적인 forward-field 보존 한계이고 S1이 새 writer를 도입한 것은 아니다. S1 시점의
   결과는 health 다이얼이 `Legacy/0` 으로 되돌아가는 관측 변화에 그친다(admission 소비자 0).
   결정해야 할 것: unknown-YAML 보존 또는 mixed-version 금지 계약 중 어느 쪽을, S2가 이 knob를
   관측/집행에 쓰기 전에 세울지. 소유: S2 착수 전.
