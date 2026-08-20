# T5 → T6 철거 인벤토리 — #5464 (#5071 T5)

> **이 리포 사본이 정본이다.** §12-2 기록은 이 파일에서만 갱신한다.
> 워크플로 초안 `/private/tmp/adk-b0814/t6-removal-inventory.md` 는 S1 r1 시점에
> 동결됐고 더 이상 갱신하지 않는다 — `/private/tmp` 산출물은 T5 배포~T6 삭제
> 과도기를 넘겨 살아남지 않으므로, T6 진입 시 이 파일이 존재한다는 가정이
> 성립해야 한다.
>
> Last refreshed: 2026-08-20 (against #5464 T5 S1 r2 — 리포 이관, 소비처 수 정정, 후속 이관 등재).

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

## S2 이후

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
