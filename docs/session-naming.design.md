# 세션 이름 단순화 설계 1 — #8119

작성일 2026-11-12 KST. 조사 기준은 `origin/main`에서 분기한 `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`이다. 이 문서는 설계 리뷰 입력이며 구현 완료·독립 리뷰 통과·사용자 결정 확정을 뜻하지 않는다. 코드·운영 설정·DB·store는 변경하지 않았고 cargo/build/test/TLC는 실행하지 않았다. 문서 커밋만 이번 산출물이다.

## 목표와 원문 수용 기준

[#8119](https://github.com/itismyfield/AgentDesk/issues/8119)의 내용·DoD를 기준으로 한다. 시작 때 댓글 0개였으나 설계 재검토 시 [사용자 결정 댓글](https://github.com/itismyfield/AgentDesk/issues/8119#issuecomment-6816190818) 1개를 확인했다(`logs/issue-before-final-design.json`). **채널 이름**과 `<채널 이름>-<스레드ID>`가 확정됐다. 추가 요구인 DM·루틴 등 비채널 hint 생성 전수와 다른 길드·카테고리의 동명 처분도 포함한다.

| 원문 조건 | 설계의 증거 / 아직 남은 일 |
|---|---|
| 채널은 에이전트 이름, 스레드는 `<에이전트>-<스레드ID>` | 아래 명명 계약. 사용자 결정 1은 댓글로 확정됨. |
| provider·channel·thread·관리 판정을 이름에서 분리 | 부록 A 전수표와 각 행의 기존 저장 근거, 신규 descriptor 계약. |
| 한글·44byte·충돌·NFC/NFD·Herdr·인용·UI/API 실물 | 부록 D 명령/출력 원문과 격리 probe. 문제 3건(절단 충돌, 파일 정규화 충돌, UI parser miss)을 수리 계획에 넣음. |
| live 보존, old read/new write, 외부 의존 순서 | 부록 B, C와 전환 순서. |
| 외부 사용자 tmux와 충돌 처분 | 관리 증거 없으면 0 mutation, 자동 별도 이름 생성. |
| 독립 리뷰 통과와 결정 확정 | 결정은 댓글로 확정. 이 설계 ref의 최종 독립 리뷰 통과는 아직 남으며 이번 보고로 이슈를 완료 처리하지 않음. |

이미 main에 있는 canonical identity/alias, channel 기반 inflight 파일명, explicit host locator, owner/channel sidecar는 부록 B 좌표대로 재사용한다. 새 host 추상화·새 registry DB·운영자 설정 스위치를 만들지 않는다.

## 명명 계약

`SessionNameInput`은 display label, provider, token namespace, channel ID, parent channel ID, **Option<thread_id>**, identity kind를 별도 필드로 받는다. builder는 host를 고르거나 thread를 문자열에서 찾지 않는다. `PreferredSessionName`과 실제 I/O 대상인 `ResolvedSessionLocator`를 구별한다.

1. 채널 이름을 NFC로 정규화한다(`unicode-normalization`은 `Cargo.toml:87`에 이미 있음). Unicode letter/number와 `_`, `-`를 보존하고 그 밖의 문자는 지금처럼 `-`로 치환한다. 결과가 비거나 의미 없는 구분자뿐이면 `channel-<channel_id>`를 쓴다. 신규 허용 집합의 정본은 Unicode `L`/`N` category이며 그 밖의 combining mark는 치환한다. Rust `is_alphanumeric`과 완전히 같다고 가정하지 않는다. U+0345 같은 Alphabetic mark와 NFC 뒤 잔여 mark를 경계 fixture로 둔다. 기존 이름/키/파일에는 정규화를 소급하지 않는다.
2. 일반 채널 기본 이름은 이 label 전체다. 기본 agent 부분의 검증 정규식은 `^[\p{L}\p{N}_-]{1,100}$`이며 Unicode scalar 기준 100자/UTF-8 최대 400byte다. 더 긴 내부 routine/snapshot basis는 scope별 전용 builder가 ID를 보존해 축약한다. 새 일반 채널에는 **44byte 자르기를 없앤다**. 실물 tmux는 한글 100자(300byte), thread 포함 320byte, 4byte letter 100자(400byte), thread 포함 420byte, 충돌형 521byte를 받았다. Herdr workspace도 521byte label을 수용했다.
3. 스레드는 정규화한 **부모 채널 label** + `-` + `thread_id`의 10진수다. ID는 `u64`의 1..=MAX, 1~20자리(`^[1-9][0-9]{0,19}$` + 숫자 범위 검사)이며 기존 `-t`는 쓰지 않는다. 합계 최대 121 scalar/421byte. thread 제목은 ID 대신 쓰지 않는다. 출력 전체 정규식은 형태 검증용일 뿐 의미 판독에 사용하지 않는다.
4. tmux 예약 구분자인 `.`, `:`, `/`, `#`, 공백·제어문자를 출력에 넣지 않는다. provider명으로 시작하는 `claude-sales`, 숫자로 끝나는 일반 채널 `sales-1570308839910412348`도 정상 이름이다. 이것으로 provider/thread를 추정하지 않는다.
5. 다른 owner가 기본 이름을 선점하면 후보는 순서대로 `<agent>-<channel_id>`, `<agent>-<provider>-<channel_id>-<scope12>`, 마지막으로 이 이름에 `-<spawn_nonce32>`를 붙인 것이다. thread는 agent 대체 이름 뒤에 `-<thread_id>`를 붙인다. scope12는 runtime-root/host namespace SHA256 앞 12hex다. 단축 hash의 유일성을 신뢰하지 않고 actual descriptor를 대조한다. `new-session` 원자적 중복 거절 때 정상 다른 owner면 다음 후보, 자기 identity면 기존 actual 재획득으로 수렴한다. 후보를 모두 썼으면 retryable 생성 실패이며 다른 세션/manifest를 변경하지 않는다. 전송 불명·marker read failure·alias conflict는 새로운 후보 생성을 중단하고 보존한다. 남의 세션 rename/kill/표식 덮어쓰기와 선호명 복귀를 위한 live rename은 없다.

6. 이름 길이 상한은 기본형 421byte, 충돌형은 provider 32자·channel20·scope12·nonce32·thread20을 더한 **최대 521byte**로 제한한다. tmux/Herdr의 이 길이 지원은 activation 전 격리 host 계약시험으로 확인한다. 지원하지 않는 host에서는 agent 부분만 UTF-8 경계에서 줄여 이 식별 suffix를 보존하며 display label 전체는 구조 필드로 유지한다. host별 cap은 실측값으로 고정하고 조용히 44byte로 돌아가지 않는다.

비채널 hint는 별도 typed kind로 생성한다. DM은 `dm-<provider>-<DMchannelID>`, routine은 `routine-<provider>-<routineID>-<agentID>`, process routine은 `routine-process-<provider>-<attemptID>`, scheduled snapshot은 `scheduled-<provider>-<definitionID>`, recovery는 `recovery-<provider>-<channelID>`로 provider/안정 ID를 보존한다. **이 충돌 경로들에만** 구분자를 유지하며 실제 길드 채널에는 provider를 늘 붙이지 않는다. 내부 ID가 길면 사람이 읽는 부분만 줄이고 identity tuple의 digest를 보존한다. DM workspace hint `dm-<userID>`는 라우팅용으로 그대로 두며 physical label과 분리한다. DM+tmux routine의 현재 canonical DM 공유, process attempt 분리, scheduled 정의별 재사용·native continuity reset은 main 의미 그대로 유지한다. producer·caller 58곳/43파일의 전체 목록은 부록 F에 있다.

충돌 후보는 정상형의 예외이고 API의 display name에는 원래 agent 이름을 유지한다. 물리 locator와 display name은 다른 용도다. 동명 채널·provider·token namespace·다중 runtime의 동일 이름은 descriptor로 구별한다.

## 명시적 식별 정보와 권한

다음은 추가할 schema의 예시다. 필드 값은 fixture이며 운영 값이 아니다. 기존 sessions row와 immutable binding context가 정본이고 tmux option은 그 실행을 찾기 위한 host 표식이다.

```json
{"v":1,"owner_scope":"<sha256(runtime-root|execution-node)>","provider":"claude","token_namespace":"<existing token_hash>","channel_id":"1479671298497183835","parent_channel_id":null,"thread_id":null,"identity_kind":"discord_channel","name_kind":"channel","session_record_id":null,"logical_key":"<actual stored key>","spawn_nonce":"<32hex>","artifact_layout":"v2","display_name":"스마트홈-cc"}
```

- DB의 현 `identity_kind`는 `discord_channel`/`scheduled_snapshot` 두 가지다(`db/dispatched_sessions/canonical_identity.rs:4-27`). 신규 `name_kind`(channel/thread/dm/routine/process/scheduled/recovery)는 이름 입력·표식의 discriminant이며 DB enum 확대가 아니다. legacy row의 kind는 그대로 읽고 새로운 routine canonical 채널 합치기를 만들지 않는다. `session_record_id`는 기존 launch/hook 순서상 아직 없을 수 있으므로 optional이다. 기존 launch admission과 immutable binding context/nonce로 최초 소유를 입증하고 나중 row ID와 대조한다. 이를 위해 새 DB 선삽입 프로토콜을 만들지 않는다.
- tmux에는 **session option 한 개 `@agentdesk_identity`**로 compact JSON을 저장한다. 숫자 ID는 JSON 문자열이다. 여러 option을 순차로 읽어 일부 갱신된 tuple을 조합하지 않는다. 성공한 `new-session -P -F '#{session_id}'`가 돌려준 세션 ID와 서버 witness를 대상으로 set/read-back한다. 실측에서 `set-option -t '=한글명'`은 실패했으므로 `=name` helper를 모든 tmux 명령에 무조건 재사용하지 않는다. 새 작은 platform 모듈이 실패 가능한 Result를 반환한다(기존 `platform/tmux.rs:990`의 오류를 버리는 set_option을 쓰지 않음).
- descriptor의 own/foreign/unknown/conflict를 구분한다. version 미지원·truncated JSON·권한 오류·서버 교체·row/nonce 불일치는 unknown/conflict다. 이를 absent로 읽어 새로 만들거나 지우지 않는다. 실제 I/O는 검증한 actual locator와 기존 fence/nonce/CAS에 고정한다.
- 관리 판정은 owner scope + provider/token/channel + 현재 실행 nonce/실제 host locator가 저장 row/binding과 일치해야 한다. 이름 일치·`AgentDesk-`·`-e2e` 문자열은 mutation 권한이 아니다. 새 option을 foreign 세션에 먼저 써서 입양하지 않는다.
- legacy는 기존 `.owner`의 현재 runtime 일치, `.channel`, session row/provider/inflight 또는 binding nonce의 **서로 일치하는 기존 증거**를 사용한다. channel 증거가 없는 오래된 세션도 pinned inflight/immutable binding·동일 owner/실제 host 증거가 충분하면 현재 main의 허용 범위 안에서 유지한다. 어느 쪽도 입증 못하면 보존·진단하고 marker 강제 backfill/새 세션 대체/기존 세션 종료를 하지 않는다. legacy decoder는 발견/표시에만 쓰며 권한을 새로 주지 않는다.
- marker는 새 실행에서만 쓰고, 검증된 기존 실행의 metadata 보강은 선택적·자동·멱등이며 live name/key/path/nonce는 바꾸지 않는다. marker 준비가 실패하면 그 실행은 관리 입력에 공개하지 않고 기존 launch outcome의 Unknown/보존 경로로 간다. 성공을 날조하거나 다른 후보를 연속 생성하지 않는다. 이 사유만으로 새 kill 권한을 만들지 않는다.
- Herdr는 이미 `HostedOwner`·nonce·ServerWitness·pane ID를 보유한다. 이 tuple을 descriptor 원천으로 재사용하고 새로운 label parser를 만들지 않는다. 기존 `logical_key#nonce` workspace marker와 ACK-gap 복구는 보존한다. 기존 workspace label을 readable name으로 rename하지 않는다. 이번 새 실행의 logical name과 API/pane의 display label을 분리해 전달하며, readable workspace label로 소유 증거를 대체하는 일은 하지 않는다. workspace 표시까지 순수 agent명으로 바꾸려면 nonce 증거의 대체가 먼저여야 하므로 #7583의 전면 host 재설계에 남긴다. 한글 workspace/pane label의 host 수용성은 실물 검증했다.

## 기존 실행을 보존하는 탐색 순서

일반 channel/thread/DM 요청(현 canonical 지원 범위) → `(identity_kind, provider, token_hash, channel_id)` → canonical sessions/alias → pinned inflight·watcher·binding 및 host 증거 → **실제 stored key/name/path** 순서다. live old가 있으면 새 preferred를 계산했더라도 old를 쓴다. 실제 session 없음이 확인되고 old inflight/dispatch/host pending이 남지 않은 자연스러운 다음 실행에서만 신형 이름을 쓴다. probe 오류/충돌은 없음이 아니다.

routine·process는 기존 receipt/실행 pin의 exact actual key와 routineID/agentID/attempt scope를 먼저 사용하고 같은 채널의 canonical row로 합치지 않는다. scheduled는 기존 definition override와 scheduled_snapshot scope를 유지한다. 새 이름 kind가 있다는 이유로 DB identity_kind나 채널 alias의 범위를 넓히지 않는다.

`build_session_key_candidates`의 현 2개는 **같은 물리 이름의 namespaced/legacy key wire**다. 여기에 old/new 문자열을 무작정 추가해 4개로 만드는 안은 채택하지 않는다. canonical/alias가 서로 다른 sessions.id를 가리키면 Conflict이며, 살아 있는 두 terminal을 하나로 합치지 않는다. wire `provider/token_hash/host:<actual_name>` / `host:<actual_name>`는 이미 신형 tail을 파싱하므로 바꾸지 않는다.

canonical row의 primary `session_key`가 최신 물리 이름이라고 가정하지 않는다. 현 `canonical_identity.rs:252-265`는 같은 canonical row를 update한 뒤 신규 key를 alias로 보존하고, `:850` 이후 update는 primary key를 바꾸지 않는다. 따라서 resolver는 canonical row ID·lookup alias와 **현재 실행의 effective key/name**을 구분한다. 후자는 `session_info`의 optional managed descriptor 또는 pinned inflight/binding/host marker가 같은 nonce로 가리키는 actual locator에서 읽는다. 새 tmux option의 logical_key도 이 effective key다. legacy descriptor가 없을 때에만 기존 primary tail이 fallback이다. 자연 재생성 시 새 alias를 기존 row에 연결하고 현재 descriptor를 기존 upsert/CAS로 갱신하되 live old 실행을 rekey하지 않는다. 신규 DB enum·테이블·primary-key rewrite는 없다. RED는 old primary가 남고 new alias/current descriptor가 생긴 뒤 follow-up·restart·stop이 current physical name을 선택하며, old nonce의 지연 cleanup은 새 실행을 건드리지 않는 경우다.

descriptor 저장은 일반 `session_info` 교체에 맡기지 않는다. 현 `canonical_identity.rs:871`의 `COALESCE`는 전체 JSON을 바꾸고 일반 upsert(`:175-183`)는 actor pin 없이도 호출되므로, 새 작은 `db/dispatched_sessions/session_descriptor.rs`에서 descriptor 부분만 현재 row/실행 nonce에 대해 merge·CAS한다. descriptor 없는 일반 metadata hook는 기존 descriptor를 보존하며, descriptor를 바꾸는 hook는 expected execution nonce/actor CAS가 있어야 한다. primary/alias 변경으로 이 검사를 우회하지 않는다. 새 테이블·DB enum을 만들지 않고 기존 JSON 필드의 의미를 제한한다. 추가 RED는 (1) old primary/new alias→current actual 선택, (2) descriptor 없는 후속 hook→descriptor 유지, (3) 지연 old-nonce hook→새 descriptor 덮어쓰기 0, (4) 두 현재 descriptor 증거가 다른 nonce면 Unknown/Conflict 보존이다. 기존 metadata update 의미를 이름 변경을 빌미로 전부 재설계하지 않는다.

inflight identity equality·synthetic finalizer hash·cancel generation·binding nonce·watcher key를 느슨하게 하지 않는다. old와 new를 동시에 map에 복제하지 않고 resolver 진입 전 한 actual locator로 고정한다. old output/FIFO/offset/JSONL/launch artifact와 immutable binding context를 rewrite·rename하지 않는다. 새 실행에는 새 nonce를 부여한다. 재시작·Cancel·Terminate·clear·stop·reaper·GC도 같은 resolver가 정한 실제 대상과 현행 독립 guard를 사용한다.

입력 출처는 Discord queue(채널 tuple), provider hook(immutable binding context), restart restoration(pinned row/inflight), routine receipt(explicit kind/thread), CLI/API 요청(exact key + owner)로 구분한다. raw 사용자 입력·수동 생성 tmux·미표식 Herdr workspace는 관리 권한의 출처가 아니다.

## 파일 경로 호환 계약

### 결론

작은 독립 모듈 `services/session_artifacts.rs`에 **살아 있는 locator의 불변 layout**을 두고, 새로 생성하는 세션만 ASCII hash basename을 사용한다. 옛 이름/열린 FIFO/JSONL/스크립트/owner marker는 한 바이트도 이동·재작성하지 않는다. 이름의 NFC 정규화는 새 **표시 이름 writer**에만 적용한다. 파일 hash는 선택된 exact session name 원문 바이트를 입력으로 삼는다.

단, `session_temp_path(&str, &str) -> String`은 `Unknown`을 표현할 수 없다. 그 함수가 manifest를 매번 읽고 오류를 숨기는 설계는 승인하면 안 된다. 작은 변경으로 유지하려면 layout을 checked gateway에서 한 번 확정한 후 기존 String 함수가 **이미 검증된 layout의 순수 formatter** 역할만 해야 한다. 모든 310파일의 API 서명을 일괄 변경하는 대신, 신규 launch·source authority·정리·복구의 mutation 진입점을 명시적으로 보강한다. 이 경계를 통하지 않는 direct write가 남으면 “String API 유지 + Unknown fail-closed”는 성립하지 않으므로 다음 조각으로 분할해야 한다. 함수 내부 경고 후 legacy 경로 반환, 빈 문자열/`/dev/null`/임의 sentinel 반환, panic을 정합성 가드로 쓰는 안은 제외한다.

### 저장 형식과 파일명

- index/manifest 경로 자체도 Unicode 이름을 포함하지 않는다: `runtime/session-artifacts/<lookup_sha256>.json`.
- lookup digest 입력: 길이 구분 또는 NUL domain separation을 둔 `agentdesk-artifact-lookup-v2`, runtime identity, 안정된 host identity, **exact session name UTF-8 bytes**. NFC/NFD를 file hash 단계에서 합치지 않는다. 이름이 다른 두 실제 세션이 같은 artifact를 공유하지 않아야 한다.
- manifest v1 예: `{schema:1, layout:"hashed_v2", exact_name:"개발-담당", runtime_id:"...", host_id:"...", owner:{provider,token_namespace,channel_id}, basename:"adk-v2-<64hex>"}`. 저장하는 runtime/owner 값은 운영 자격증명이 아니다. basename은 payload를 믿지 말고 위 입력에서 다시 계산해 검증한다.
- legacy enrollment는 `{schema:1, layout:"legacy_v1", exact_name:"AgentDesk-claude-...", ...}`이며 **기존 prefix 계산 결과와 기존 persistent/temporary lookup 순서 그대로** 쓴다. metadata는 legacy의 파일을 수정하지 않고 별도 index에 둔다. 실제 기존 legacy layout 관측/owner 증거를 확인한 뒤에만 enrollment하며 새 이름의 manifest 없음은 legacy로 간주하지 않는다.
- v2 canonical 파일 예: `runtime/sessions/adk-v2-<64hex>.jsonl`, `.input`, `.owner`, `.claude-tui-settings.json`, `.runtime-kind` 등. plaintext hostname도 basename에서 제거한다. 가장 긴 기존 extension을 포함해 255byte NAME_MAX 아래인지 정적 assert/시험으로 고정한다. hash 충돌은 manifest exact tuple 불일치로 typed conflict가 되어야 한다.
- layout은 한 번 선택하면 세션의 생존/respawn 동안 바꾸지 않는다. generation/spawn nonce는 기존 incarnation 자료이며 파일 layout hash에 넣지 않는다. 신규 nonce마다 path를 바꾸면 watcher와 followup 연결이 다시 깨진다.
- v2 manifest는 보통 cleanup에 삭제하지 않는다. dead pane 재획득 중 layout이 사라지는 ABA를 피하며 작은 파일 1개만 남긴다. 별도 보존/GC 정책을 이번에 새로 추가하지 않는다.

### spawn 전 순서

1. canonical owner에 대한 기존 launch admission/serialization을 얻고, exact live legacy locator가 있으면 그것을 재사용한다. 다른 owner/무표식 동명 세션이면 기존 세션에 I/O·옵션·정리를 수행하지 않는다.
2. manifest를 checked load: `KnownLegacy` / `KnownV2` / `Missing` / `CandidateOccupied` / `Unknown` / `Conflict`. 기존 live locator와 증거가 일치한 legacy만 KnownLegacy로 고정한다. fresh allocation으로 확정된 Missing만 V2로 atomic create-new한다. tmp write+fsync+rename만으로 기존 manifest를 overwrite하지 않는다. 동시 생성자는 최종 existing tuple을 다시 읽고 같은 owner/layout일 때만 수렴한다.
3. **아직 artifact 파일을 하나도 만들기 전** manifest를 read-back하고 checked layout handle을 얻는다. launch 시작 시점에 tmux session이 아직 없어도 layout은 확정되어야 한다. tmux의 단일 `@agentdesk_identity` JSON 안 `artifact_layout` 필드는 세션 생성 뒤의 교차 확인 표식이며 path 선택의 유일한 근거가 될 수 없다.
4. 이 handle로 RuntimeHintFence, auth/host/runtime marker, prepared incarnation, JSONL/FIFO/prompt/script path를 같은 layout으로 준비한다. 기존 경로 캐시를 manifest enrollment 전 만들면 안 된다.
5. artifact durable preparation 후 tmux/herdr create. 새 세션에 관리 metadata와 path-layout option을 게시하고 현재 source authority를 read-back한다. 실패 시 **자신이 방금 만든 v2 artifact만** 기존 소유/인카네이션 권한 아래 치운다. 기존 세션/legacy artifact cleanup 권한은 늘리지 않는다.
6. 재시작 때 manifest와 session option/row가 모두 일치하면 같은 layout으로 회복한다. 손상/미지원 schema/모순인 Unknown/Conflict는 “없음”이나 “stale”이 아니며 recreate/kill/cleanup으로 이어지지 않는다. 다만 정상 검증된 manifest의 owner가 다른 경우는 CandidateOccupied로 구분한다. 이는 **fresh allocation의 후보 선택 단계에서만** 기존 파일을 건드리지 않고 자동 다음 이름을 고르는 근거다. 저장된 actual locator를 재획득하는 도중 다른 owner가 나타나면 Conflict로 보존하고 신규 후보 생성으로 우회하지 않는다. tmux 생성 전 두 owner의 manifest 선점 경합과 종료된 다른 owner의 예약 파일만 남은 경우도 같은 규칙이며 덮어쓰지 않는다. Unknown/Conflict인 경우에만 기존 live process를 둔 채 해당 턴을 진단 가능한 refusal로 반환한다. checked read가 보유한 immutable layout이 있으면 그 핸들 안에서만 읽기 관측을 계속한다.

### 정확한 삽입 지점과 비호환 함정

| main 좌표 | 현재 동작 | 필요한 최소 변경 |
|---|---|---|
| `tmux_common.rs:1113-1140` | raw host/name으로 prefix, String path 생성 | 새 모듈 formatter로 delegation만. giant에 manifest 책임 추가하지 않음. |
| `tmux_common.rs:1153-1176` | legacy `/tmp` 생성, `.exists()`로 canonical→legacy 선택 | checked layout에 따라 legacy에만 기존 순서. v2에는 legacy raw path fallback 금지. I/O 오류/Unknown을 Missing으로 압축하지 않는 checked resolver 추가. |
| `tmux_common/runtime_hint_fence.rs:379-389` | session_temp_prefix 직접 호출해 canonical와 `/tmp` 두 lock/marker path 구성 | 동일 checked layout에서 paths를 받아야 함. formatter만 바꾸고 여기를 빠뜨리면 reader/writer lock namespace 분리. |
| `claude/tui_session_launch.rs:43-62` | publisher observation/fence→auth read→cleanup→owner write | layout admission은 publisher observation capture보다 앞. 적어도 auth read/cleanup/owner write 이전에 layout 확정; 기존 순서의 fence 의미 유지. |
| `claude/tui_session_launch/prepared.rs:37-67` | PreparedIncarnation→host marker→launch files | 앞에서 선택한 layout handle 재사용. 이 단계에서 layout을 다시 고르지 않음. |
| `qwen/session_lifecycle.rs:25-49,51-111,116-140` | path 미리 계산, usability failure가 kill/recreate, 뒤에 output/FIFO/owner/script 생성 | layout checked load는 path 계산 **및 kill 판단 전**. Unknown이면 session_usable=false가 아니라 Err. |
| `codex_tui/session.rs:24-64` | canonical와 legacy path String을 구조체에 함께 보유, cleanup이 둘 다 삭제 | v2일 때 legacy path를 Option/typed path set에서 제외. 없는 legacy를 String sentinel로 채우지 않음. 이 구조체와 cleanup은 작은 별도 적응 범위. |
| `tmux_common.rs:1222-1266` | EXTS 전체에서 canonical/legacy 삭제 | checked path set을 받는 내부 정리 helper로 제한; Unknown이면 한 파일도 삭제하지 않음. |
| `herdr_launch.rs:772` | logical key로 host_kind marker path 생성 | workspace/pane 이름과 무관하게 동일 artifact layout resolver 사용. host 선택 전에 잘못된 tmux artifact를 만들지 않음. |

**주의:** 현재 Qwen은 source가 없을 때 stale cleanup으로 가는 경로가 main에 이미 있다(`:100-108`). 이번에 layout Unknown을 `.exists()==false`로 넘겨 그 권한을 넓히지 않는다. 기존 결함 전체 재설계는 범위 밖이지만 새 layout 실패가 그 경로에 들어가지 않는 RED를 둔다.

### gateway 전수와 크기 보정

`session_temp_path` 관련 정적 후보는 314파일/704행이고, 실제 checked mutation/authority gateway는 기존 prod 32파일, 추가 결정성 reader/복원은 19파일이다. 부록 E의 G01~G32/R01~R19를 모두 적응·검증하기 전에는 새 artifact writer를 활성화하지 않는다. 앞선 10~20파일 단일 조각 추정은 폐기하고 S7a~S7e로 나눈다. 새 layout 도입에 필요한 범위만 다루며 host abstraction·lease/consent/cleanup 권한을 새로 만들지 않는다.

legacy에서도 같은 checked API를 운영 호출하도록 연결해 휴면 코드/경고를 늘리지 않는다. 신형 v2 경로 선택은 마지막 활성화 조각에서만 켠다. `String` formatter는 이미 검증된 immutable handle의 경로만 포맷하며 unknown을 legacy·빈 문자열·sentinel·panic으로 숨기지 않는다. global cache는 권한 원천이 아니다. 상세 gateway·상위 kill 0 계약은 부록 E를 따른다.

## 경계와 전환 순서

[#8118](https://github.com/itismyfield/AgentDesk/issues/8118)에 남기는 것은 부모 채널의 host/endpoint/execution-node 상속과 Herdr tab 배치다. 이번에는 parent/channel/thread 필드를 builder/descriptor에 전달하고 old `-t` reader를 제한하는 계약만 공유한다. 이름으로 host를 선택하지 않는다. #8118의 살아 있는 tmux thread 보존 원칙과 같다.

[#7583](https://github.com/itismyfield/AgentDesk/issues/7583)에 남기는 것은 ProcessBackend 제거·Windows Herdr·Core parent/child·InteractiveSessionHost 전체 생성/입력/종료 확장이다. 이번에는 이미 있는 host locator와 owner를 읽는다. 설정 스위치·Core·운영자 이사 절차를 추가하지 않는다.

1. reader/descriptor/artifact 호환층을 먼저 넣되 현 writer는 old 유지. 동일 채널 live old를 실제로 찾는 계약을 고정한다. 새 표식은 현 owner·row·nonce를 검증한 경우에만 붙인다.
2. 전수표의 provider/channel/thread/managed 소비자를 표식/저장 필드로 옮긴다. unknown/conflict 보존을 모든 파괴 경로에 연결한다. 생성 이름을 바꾸기 전에 reaper의 wrapper 파일명 복원도 explicit context로 바꾼다. GC와 SQL은 새 thread 필드가 먼저 저장되도록 한다. 과거 migration 파일은 수정하지 않는다.
3. E2E 대상 writer/kill allowlist와 외부 스킬 보호 문구를 관리 증거 기준으로 함께 갱신한다. 이 레인은 외부 스킬을 수정하지 않는다. 코디네이터가 별도 스킬 변경으로 처리하며 그 전에는 새 writer 활성화 조각을 착지하지 않는다. 자동 reader와 코드 계약으로 보호하므로 운영자의 수동 tmux 이름 점검·rename을 요구하지 않는다.
4. UI와 policy의 의미 판독 reader도 먼저 착지한 다음 새 생성 writer를 기본형으로 바꾼다. 기존 live old/session key/inflight는 그대로 유지한다. routine/scheduled/DM basis는 별도 identity kind와 ID를 유지해 일반 채널과 합쳐지지 않게 한다. routine의 새 thread 호스트는 #8118에 맡긴다.
5. UI/API와 공개 문서를 맞추고 old 형식을 legacy 사례로 남긴다. old reader 제거 시점은 이번에 정하지 않는다. 설정·일괄 migration·DB backfill·live kill 없이 자연 교체로 이행한다. rollback도 actual stored locator 우선 reader를 유지한 채 **신규 writer만** 되돌리는 코드 변경이다. old 전용 바이너리로 내리는 운영 절차는 제안하지 않는다.

## 상태차트와 기존 결함 경계

0단계의 '해당 없음'을 뒤집는다. `tmux_reaper.rs:504`의 thread 분류는 delete/idle을 고르고 `watchers/lifecycle/restore.rs:45,128`은 재획득 대상을 고른다. 생성 충돌·foreign·marker unknown에서 입장 조건도 달라진다. 구현 조각에 `docs/statecharts/session-lifecycle/TmuxSession.tla`, `TmuxSession.cfg`, `HerdrSession.tla`, `HerdrSession.cfg`와 기존 view/report 갱신을 넣는다. 구별 변수는 old/new owned locator, foreign candidate, marker result, canonical ambiguity, actual execution nonce다. `ForeignNeverMutated`, `LiveLocatorPreserved`, `UnknownDoesNotMeanMissing`, `CleanupRequiresSameExecution` 불변식을 기존 모델에 추가한다. suffix-wide decoder·새 builder 우선·unknown→absent 뮤턴트에 반례가 나야 한다. 이번 읽기 전용 설계에서는 모델을 바꾸거나 TLC를 실행하지 않았다.

기존의 prefix-only `reset-tmux` 권한(`cli/utils.rs:185`)을 새 짧은 이름 전체로 넓히지 않는다. worktree sweep의 cwd 목록이 불완전하면 삭제를 보류한다. `dispatch_channel.rs:48-65` unified-thread false/빈 신호는 이번에 활성화하지 않는다. 기존 broad cleanup/status/lease/TTL 조건은 변경하지 않으며, unrelated dead-code/giant 정리를 끼워 넣지 않는다.

## 구현 조각과 크기

이 수치는 실제 변경 전 추정이다. 모든 조각은 prod **30파일 / 순증 +1500** 이내이고 **PLAN +1200에서 정지**한다. 총합을 한 PR로 묶지 않는다. 아래 파일 목록에서 시험·문서는 prod 추정에서 제외하며 부모 `mod.rs` 배선은 포함한다. 목록 밖 새 소비자가 나오거나 +1200 예상이면 해당 조각을 나누어 DESIGN_DELTA로 보고한다. 각 조각은 현재 직전 조각을 기준으로 실측하고 `pr_cap_prod.py <base> <head>` 출력을 남긴다.

| 조각 / 선행 | prod 파일·순증 추정 | 수정할 파일 집합·새 책임 배치 | 수정 전 RED와 대상 시험 / 원인 제거 뮤턴트 |
|---|---:|---|---|
| S1 descriptor·ownership / 없음 | 10~13 / +650~850 | 새 `domain/session_identity/managed.rs`, `services/session_identity_metadata.rs`, `services/platform/tmux/session_identity.rs`; `domain/session_identity.rs`, `services/mod.rs`, `platform/tmux.rs`, `tmux_common.rs`는 작은 delegate·기존 코드 추출만; `tmux_common/session_markers.rs`, `tui_prompt_dedupe/binding_context.rs`, `session_host/model.rs`, `session_host/resolve.rs` 연계 | 새 이름→관리 정보 조회, foreign/손상/다른 nonce/서버 교체, 실패한 set/read-back 미공개. `session_identity`·metadata mock-host + 격리 tmux 계약 / prefix만 승인, unreadable→absent, provider mismatch 무시 뮤턴트 |
| S2 actual locator resolver / S1 | 19~22 / +650~900 | 새 `discord/session_locator.rs`, `db/dispatched_sessions/session_descriptor.rs`; `adk_session.rs`, `host_key_derivation.rs`, `tmux_lifecycle.rs`, `idle_relay_drift.rs`, `recovery_engine/manual_rebind_output_path.rs`, `manual_rebind_override.rs`, `manual_rebind/mod.rs`, `session_runtime/restore_cwd.rs`, `watchers/lifecycle/{activity,restore,restore_core,restore_support}.rs`, `cluster/{session_registry,session_discovery,session_matcher}.rs`, `db/dispatched_sessions/canonical_identity.rs`, `discord/mod.rs` | old live+new preferred에서 12개 candidate 소비자가 old actual 사용; old primary+new alias/current descriptor의 effective locator 선택; canonical/alias 두 row 충돌; unknown host 보존. PG canonical 기존 시험 + resolver 대역 / builder 우선, 4 candidate 무조건 합치기, alias equality 완화 |
| S3 runtime provider 소비 / S1,S2 | 23~26 / +250~450 | `db/session_agent_resolution.rs`, `services/{auto_queue/runtime,claude_tui/tui_relay,dispatched_sessions,session_activity,session_resume,stale_turn_reconciler,tmux_turn_liveness}.rs`; `discord/{health/recovery/stall_alert,health/watcher_respawn/idle_relay_absence,host_defer_gate,host_teardown_gate,inflight_heartbeat_sweeper,org_schema,relay_recovery,tmux/monitor_auto_turn_inflight,tmux_restart_handoff}.rs`; `provider/cancel_token_cleanup/{authority,executor}.rs`, `dispatched_sessions/{force_kill,tmux_cleanup}.rs`, `message_outbox/{identity,session_notice}.rs` | explicit provider가 이름과 다를 때 구조 필드가 권위이고 foreign input/kill 0. existing guard suites + metadata fixture / 이름 Claude default 부활, channel 숫자 역추정 |
| S4 watcher·복원 / S2,S3 | 13~17 / +350~550 | `discord/recovery_engine/{herdr_row_end,restore_inflight,state_extractors}.rs`, `tmux_watcher.rs`, `tmux_watcher/{discrete_trigger_marker,entry,host_gate,provider_session_persistence,terminal_token_update}.rs`, `watchers/lifecycle/restore.rs`, `tui_prompt_relay/claude_idle_runtime.rs`, `tmux_watcher_registry/session_binding.rs`; 필요 로직은 각 소유 모듈 child에 배치 | old inflight·path·nonce·offset·hash를 보존한 boot/재획득/마지막 clear, 새 일반 숫자 꼬리를 thread로 안 읽음. restore fixtures + restart mock / missing name을 builder로 덮기, name CAS 완화 |
| S5 cleanup·inventory·상태차트 / S1~S4 | 13~17 / +450~700 | `cli/{utils,doctor/orchestrator}.rs`, `discord/{health/o_transition/codex_census,health/watcher_respawn,relay_recovery/decision,tmux_reaper,tmux_reaper/host_guard,tmux_kill_policy,turn_presence/activity/unbound,turn_presence/activity/unbound/hold_leaf,turn_presence/activity/unbound/completed_scheduled,turn_presence/activity/unbound/routine_owner}.rs`, `maintenance/jobs/worktree_orphan_sweep.rs`; wrapper discovery는 새 `tmux_reaper/wrapper_identity.rs`; 기존 statecharts 2모델·cfg·view/report는 별도 문서/모델 변경 | foreign `adk-cc`, fake `AgentDesk-*`, unknown census에서 kill/delete 0, old dead는 기존 범위만. cleanup/doctor/sweep/reaper fixtures + TLC / suffix-wide GC, missing marker→owned, wrapper filename 추정 회귀 |
| S6 explicit thread·non-channel kinds / S2~S5 | 19~23 / +300~500 | `db/{session_agent_resolution,session_evidence,dispatched_sessions/thread_gc}.rs`; `discord/session_runtime/channel_routing.rs`, `discord/router/message_handler/{headless_turn,intake_turn/adk_thread}.rs`, `dispatched_sessions/{hook,listing}.rs`, `discord/turn_bridge/{activity_heartbeat,completion_postlude,post_loop_finalize,stream_tick,turn_analytics}.rs`, `routines/{session_control,agent_executor}.rs`, `discord/tmux_lifecycle.rs`, `tmux_watcher/provider_session_persistence.rs`, `recovery_engine/restore_inflight.rs`, `watchers/lifecycle/restore.rs`; identity kind를 쓰는 hook 소유 지점도 같은 조각에 포함 | 명시 thread_id가 있는 신형과 없는 숫자 꼬리 일반 채널 분리, scheduled 격리, 서로 다른 routine ID. 기존 PG thread_gc/dispatch 보호/heartbeat/routine 시험 / `-t` 신규 writer, 숫자 suffix 범용 decoder, scheduled kind 생략 |
| S7a codec·공용 authority / S1,S2 | 13~15 / +650~900 | 부록 E G01~G09 9파일 + 새 codec/manifest/path-set/배선 4~6파일. parent giant는 추출/delegate. | legacy 모든 확장자/path byte-identical, Unknown callback 0, manifest 선점 경합·read-back / Unknown→legacy, authority/fence 경로 분리 |
| S7b Claude·Qwen·process / S7a | 13~15 / +450~650 | G10~G13/G23~G25, R03/R10/R11/R14 + child 추출 2~4파일 | live FD·continuation/settings/script inode/permission 보존, Unknown 때 기존 process kill 0 / layout gate를 usability 뒤로 이동 |
| S7c Codex launch·cursor / S7a | 12~14 / +500~750 | G14~G22/R02 + child 추출 2~4파일 | v2 legacy path-set 없음, home/fingerprint/snapshot/rollout/checkpoint 같은 layout, unknown clear/relaunch 0 / v2 cleanup에 legacy path 추가 |
| S7d Herdr·relay 복구 / S7b,S7c | 23~25 / +650~950 | G26~G32/R01/R04~R09/R12/R13/R15~R19 총21파일 + child/배선 2~4파일. +1200 예상 때 Herdr와 relay로 추가 분할 | old saved path 유지, raw/normalized replay·manual rebind·post-mortem unknown write/kill/truncate 0 / Unknown→NotApplicable, missing path를 truncate 시작으로 처리 |
| S7e v2 allocation 준비 / S7a~d | 6~10 / +150~300 | 기존 naming/launch admission 5~8파일 + allocator 배선 1~2파일. v2 발행은 S8c 최종 전환과 같은 착지 경계 | 모든 host의 ASCII basename≤255, raw NFC/NFD hash 분리, manifest-only foreign 선점 자동 대체 / enrollment를 artifact 생성 뒤로 이동 |
| S8a typed kind producer / S1~S6 | 13~17 / +350~550 | 새 `domain/provider/session_name.rs`, `domain/provider/naming.rs`, `discord/{adk_session,health/headless_turn,router/message_handler/headless_turn/routine_metadata,session_runtime/channel_routing}.rs`, `routines/agent_executor{,/launch,/execution,/reliability}.rs`, `routines/fresh_session_reaper.rs`, `scheduled_messages/context_snapshot.rs`, `agent_recovery/handoff.rs` 및 ID 전달 child/배선. 신형 발행은 아직 연결하지 않는다. | DM cross-provider 분리, routine 동일명/다른 ID, process attempt, scheduled 정의별 격리와 DM tmux 공유 유지 / synthetic kind를 Channel로 바꾸기·provider/ID 제거 |
| S8b1 제어·health lookup / S2,S8a | 18~22 / +300~500 | 부록 F의 `admin_host_guard`, `commands/*`, `health/recovery*`, `health/turn_deliver/inject`, `host_defer_gate`, `host_teardown_gate` caller를 actual resolver로 전환. 파일별 정확 좌표는 F 58행 표. 기존 consumer 중복 파일은 직렬 소유 | 제어·조회·relaunch/teardown이 old actual 유지, missing name default 생성 금지 / generator 우선·unknown→stale |
| S8b2 복원·routine lookup / S2,S4,S8a | 19~24 / +300~500 | 부록 F 나머지 `recovery_engine/*`, `session_runtime*`, `tmux_reaper`, `tmux_restart_handoff`, `tui_prompt_relay`, `turn_bridge`, `turn_presence`, `watchers/*`, `routines/session_control`의 이름 재구성. S2~S6에서 이미 바꾼 행은 재구현하지 않음 | old/new actual locator 대조, routine receipt·parent routing·dispatch 보호 / old 후보를 새 generator로 대체 |
| S8c 최종 새 writer·충돌 / S1~S7e,S8a,b1,b2,S9, 외부 보호 갱신 | 17~22 / +450~650 | `domain/provider/naming.rs`, `discord/adk_session.rs`, Claude/Codex prepared launch·각 provider launch 소유 지점·`herdr_launch/{prepared,runtime_marker}.rs`·`session_backend.rs`, `scripts/e2e/run_tui_relay.py`, `scripts/e2e/tui_relay/midturn_kill.py`. exact 파일은 launch gateway 표와 F의 생성 행으로 한정 | preferred 원자 생성·foreign 경합·동시 생성 패자 무조작·old-live 유지·전 host V2 artifact. 격리 tmux/Herdr/process + naming/launch fixture / truncate44 복원·duplicate 성공 취급·이름만 kill 승인 |
| S9 UI/policy/docs reader / S2,S6 | 6~10 / +150~300 | `dashboard/src/components/agent-manager/discord-routing.ts`, `session-panel/SessionPanelModel.ts`, `services/agents/serialization.rs`, `policies/lib/timeouts-helpers.js`, `policies/timeouts/idle-kill.js`의 SQL/부모/thread fallback, CLI 문구/API inventory 원본, `docs/session-binding.md`·보호 문서·공식 생성 문서. 외부 15스킬은 별도 변경(이 레인 소유 밖) | plain/한글/claude-sales 표시, thread 링크, 중지 API에 같은 key 전달, policy metadata 우선. vitest/policy node 계약 / parser 우선으로 복원·UI 축약값을 stop key로 전달 |

기존 giant 파일에는 새 책임을 얹지 않는다. 현재 `tmux_common.rs` **3840행**, `tmux_reaper.rs` **1547행**, `adk_session.rs` **1073행**, `canonical_identity.rs` **1103행**, `session_host/resolve.rs` **1363행**이다. 새 로직은 작은 child/독립 모듈에 두고 부모는 기존 책임 추출과 delegate만 맡아 행수 no-growth를 지킨다. `watchers/lifecycle/restore.rs`는 994행이므로 1000을 넘기지 않는다. baseline/allowlist/임계값은 바꾸지 않는다.

각 구현 레그는 대상 시험의 RED→GREEN을 얻고 이름만 열거한 시험을 실행 증거로 삼지 않는다. 기존 legacy fixture를 대량 치환하지 않고 새 형식 fixture를 더한다. required lane의 새 시험 ID 선택·실행 수/skip/PG 실제 실행 여부를 확인한다. 조각별 static-only, 묶음의 전체 clippy warning no-growth·mutant를 수행한다. 이번 설계 레인에는 build/시험 바이너리/SHA가 없으며 probe를 cargo GREEN이라고 부르지 않는다.

## 사용자 결정 필요

1. 에이전트 이름의 정의 — **결정 완료**, 추가 답변 불필요.
   A. 채널 이름(`adk-cc`, `adk-cdx`, `스마트홈-cc`). **사용자가 A 선택.**
   B. 역할 ID(`project-agentdesk`). 채택하지 않음.
   추천 A와 일치한다. provider별 채널을 구별하고 역할 ID 충돌을 피한다. 2026-11-12 사용자 댓글을 근거로 확정했으며 역할 ID 분기 구현은 계획에서 제외한다.

현재 열린 사용자 결정은 없다. 다른 길드·카테고리 동명 채널은 이름이 아니라 channel ID로 구별하고, 기존 live를 유지한 채 신규 충돌자만 자동 대체 이름을 쓴다. DM·routine 등 비채널 hint는 부록 F 전수표대로 충돌하는 경로에만 provider/안정 ID를 유지한다.

## 리뷰 집중 항목

- 경계 시각: 새 writer 착지 때도 old live/key/path/inflight를 고정하고 자연스러운 다음 실행만 바꾸는가. probe-error를 absence로 취급하지 않는가.
- 동시성: 외부 candidate 선점, new-session ACK 불명, option read-back 실패, 서버 교체 때 기존 대상에 표식/kill이 일어나지 않는가.
- 옛 형식 복원: 12 candidate 소비자, wrapper output 파일 역조회, scheduled SQL, Herdr ACK-gap label, registry/nonce/offset을 표와 대조한다.
- 문서로만 선언한 guard: foreign 보호를 주의 문구로 끝내지 않고 공유 descriptor/authority gate·negative 시험으로 연결하는가.
- 잘못된 0/PASS: 정적 후보 수≠실행 시험 수, fixture browser≠전체 app/운영 API, Python 전사≠Rust 바이너리 실행. 미실시 TLC/cargo/운영 관측을 통과했다고 쓰지 않는가.

이하에 전수표·key 경로·외부 의존·probe 원문을 수록한다. 표의 M은 **새 설계**이며 S/I/B/H 등 기존 source와 구현 여부를 구별한다.

<!-- doc-path-refs: proposed domain/session_identity/managed.rs services/session_identity_metadata.rs services/platform/tmux/session_identity.rs discord/session_locator.rs domain/provider/session_name.rs services/session_artifacts.rs session_artifacts/codec.rs session_artifacts/registry.rs session_artifacts/paths.rs tmux_reaper/wrapper_identity.rs db/dispatched_sessions/session_descriptor.rs -->

# 부록 A. 이름 판독 전수표

# #8119 이름 판독 전수조사

기준 `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`. 코드·운영 설정·운영 저장소 변경/접속 없음, cargo 미실행.
`git grep` 원문: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/naming-grep.log`(넓은 후보), `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/naming-direct.log`(직접 호출/상수), `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/suffix-grep.log`(꼬리), `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/naming-assumptions.log`(시험 계수).

## 대체 근거 범례

범례는 **이미 존재하는 읽기 원천**과 이번에 추가할 **M**을 구분한다. M은 설계안이며 main에 아직 없다. 각 행의 대안은 이름에서 사실을 추측하는 대신 이 명시적 원천을 사용한다는 뜻이다.

- **S** 저장 sessions/canonical identity: `src/db/session_agent_resolution.rs:170` SELECT agent_id/thread_channel_id/active_dispatch_id; `src/db/session_evidence.rs:60` provider/channel_id/session_key; `src/db/dispatched_sessions/thread_gc.rs:61` identity_kind/discord_token_hash/channel_id/hosted_execution를 이미 SELECT. canonical key alias를 통해 실제 row를 읽는다.
- **I** inflight: `src/services/discord/inflight/model.rs:122` provider, :123 channel_id, :124 channel_name, :137 thread_id, :204 tmux_session_name. channel ID로 읽는 기존 경로를 유지하고 저장 tmux 이름은 그대로 사용.
- **B** 실행 binding context: `src/services/tui_prompt_dedupe/binding_context.rs:21` schema/provider/execution_nonce/tmux_session/channel_id(:23-27). `src/services/tmux_common.rs:1324` durable `.channel` 바인딩과 watcher registry 소유 채널도 있음.
- **H** Herdr owner: `src/db/dispatched_sessions/hosted_execution.rs:36` HostedOwner{provider,discord_token_hash,channel_id,logical_key}; :91 HostedExecution. pane/workspace label은 식별 증거 하나일 뿐 이 owner·nonce·pane binding을 대체하지 않는다.
- **T** 저장 thread: S의 thread_channel_id, I.thread_id, dispatch thread_id. 이름 꼬리는 옛 형식 **fallback 전용**으로 남긴다. 새 이름의 끝 숫자는 ownership 증거로 사용하지 않는다.
- **D** channel directory: `src/services/cluster/session_matcher.rs:53` ChannelBinding(agent_id/provider/channel_id/tmux_segment), :196 exact binding lookup. 새/옛 alias 둘을 같은 identity에 매핑하고 M과 실행 fingerprint를 같이 확인한다.
- **R** routine receipt/identity_kind: `src/services/discord/turn_presence/activity/unbound/completed_scheduled.rs:108` routine receipt key들, sessions.identity_kind='scheduled_snapshot'. 이름의 scheduled-* 분류를 구조 필드로 대체.
- **M** 추가 예정 관리 표식: tmux session option의 schema/provider/channel_id/thread_id/owner runtime/session ID/nonce. `src/services/platform/tmux.rs:990` set_option은 현재 실패를 반환하지 않으므로 새 writer는 확인 가능한 Result+read-back 필요. 기존 `.owner`(`tmux_common.rs:1278,1334`), `.channel`(:1324), `.host_kind`(`tmux_common/host_marker.rs:12`)과 상호 대조. 이름이 같다는 이유만으로 미표식 세션을 claim/kill/attach하지 않음. 충돌/읽기 실패는 unknown이고 다른 사용자 세션은 그대로 둠.

## 1. 중앙 규칙과 직접 접두어 판정

| file:line | 지금 읽고 결정하는 것 | 대체 원천/처리 |
|---|---|---|
| `src/domain/provider/naming.rs:7` | AgentDesk 접두어 상수 | legacy 전용으로 격리; 새 이름 생성기에는 사용하지 않음 |
| `src/domain/provider/naming.rs:15` | 채널 문자를 alphanumeric/[-_]로 치환, 44바이트 절단, -t숫자 보존, provider 접두어 붙임 | 새 생성기는 agent label+명시적 thread ID를 별도 인자로 받음; provider는 M/S |
| `src/domain/session_identity/naming.rs:7` | prefix/provider/채널 segment 디코딩, provider 없으면 기본 Claude 계열 | legacy decoder만 유지; 새 이름은 M/S/I/B/H |
| `src/domain/session_identity/naming.rs:31` | SessionIdentity.provider_and_channel wrapper | caller가 S/I/B에서 명시적 provider·채널을 받게 함 |
| `src/cli/doctor/orchestrator.rs:2719` | prefix 있는 세션만 현재 runtime owner 파일 확인 후 server PID 수집 | M + `.owner`; PID 수집 범위만 치환 |
| `src/cli/utils.rs:178` | prefix면 host marker refusal 검사 | M/legacy owner 확인 뒤 동일 refusal 검사 |
| `src/cli/utils.rs:185` | prefix면 reset-tmux kill 대상 | M + 현재 runtime 소유 대조; 미표식 타인 세션 제외. 기존 prefix만으로 kill하던 main 결함을 넓히지 않음 |
| `src/services/discord/health/o_transition/codex_census.rs:47` | prefix 세션을 Codex census live 집합에 넣음 | M/B가 현재 runtime의 managed locator인지 대조 |
| `src/services/discord/health/watcher_respawn.rs:167` | prefix로 관리 tmux 여부/respawn eligibility 판단 | M + S/I/B; unknown은 기존 보존 분기로 |
| `src/services/discord/relay_recovery/decision.rs:252` | prefix로 관리 tmux 여부/복구 분류 | M + S/I/B; probe 실패를 unmanaged로 해석하지 않음 |
| `src/services/maintenance/jobs/worktree_orphan_sweep.rs:634` | prefix 세션의 pane cwd만 읽어 worktree 삭제 보호 | M이 관리 세션인 것을 입증; 표식 조회 실패 시 보호 집합 불완전으로 삭제 보류 |
| `src/services/message_outbox/session_notice.rs:62` | 동적 AgentDesk-provider- prefix로 channel locator 유효성 확인 | S.provider + identity_kind + B/M의 owner 일치 |
| `src/services/cluster/session_matcher.rs:365` | prefix 있음/없음으로 UnknownProvider/NotAgentDeskNamed 구분 | M의 schema/provider 오류로 구분, legacy는 현행 decoder 유지 |
| `src/services/cluster/session_matcher.rs:519` | prefix와 env suffix로 관리 이름 추정(현재 dead_code) | M 기반 조회; 문자열 helper는 legacy 전용 |
| `src/services/discord/recovery_engine/state_extractors.rs:139` | 저장 tmux_session_name 또는 channel_name을 prefix로 구분하여 이름 재생성 여부 결정 | 필드 자체를 구분: tmux_session_name 있으면 무조건 실제 locator 사용, channel_name만 있으면 S/B/M legacy/new lookup |
| `src/services/discord/tmux_reaper/host_guard.rs:155` | prefix + -t<thread> 정확한 suffix로 kill 신호 대상 검색 | T+S+M으로 대상 identity 조회 후 기존 teardown guard 유지 |
| `src/services/discord/turn_presence/activity/unbound.rs:463` | namespace 안의 unresolved owner가 있으면 absence 판단 Hold | M의 unresolved managed identity를 Hold; 조회 실패도 Hold |
| `src/services/discord/turn_presence/activity/unbound/hold_leaf.rs:70` | prefix가 맞는 unknown/unbound 개수를 진단에 기록 | M census의 managed/unknown 분류로 동일 진단 |
| `src/services/discord/turn_presence/activity/unbound/completed_scheduled.rs:116` | AgentDesk-provider-scheduled-smsg_ 이름으로 routine 후보 제한 | R/S; legacy decoder는 옛 receipt에만 |
| `src/services/discord/tmux_reaper.rs:834` | wrapper --output-file basename 중 AgentDesk- 이후를 session 이름으로 추출 | wrapper 실행 컨텍스트 B 또는 별도 --logical-session-name 인자; 옛 wrapper만 기존 경로 판독. **새 이름만 바꾸면 orphan wrapper 탐지가 끊김** |

## 2. provider/channel parser 전체 소비자

각 행은 직접 호출 지점 하나다. import/re-export, 중앙 decoder와 시험 호출은 위/아래 절로 분리했다. `src/` 내 현재 판독 함수를 새 이름도 파싱하도록 확장하는 방식은 금지한다: adk-cc처럼 provider를 담지 않는 이름은 그것만으로 역산할 수 없다.

| file:line | 현재 결정 | 이름 대신 읽는 원천(위 근거) |
|---|---|---|
| `src/db/session_agent_resolution.rs:19` | key의 tmux 꼬리→채널 이름→agent/thread 추정 | S/T |
| `src/services/auto_queue/runtime.rs:125` | 저장 provider가 없을 때 key 이름에서 provider 보충 | S/I |
| `src/services/claude_tui/tui_relay.rs:619` | 다른 provider pane 여부를 판정하여 Claude composer 읽기 방지 | B/M |
| `src/services/cluster/session_discovery.rs:391` | 세션 key의 이름에서 provider·채널 segment를 복원하여 directory 구성 | S/B/M |
| `src/services/cluster/session_matcher.rs:231` | 이름→provider·binding 채널 매칭; 관리 이름/unknown provider 거절 분기 | D/M |
| `src/services/cluster/session_matcher.rs:327` | 이름→provider·binding 채널 매칭; 관리 이름/unknown provider 거절 분기 | D/M |
| `src/services/cluster/session_matcher.rs:363` | 이름→provider·binding 채널 매칭; 관리 이름/unknown provider 거절 분기 | D/M |
| `src/services/discord/health/recovery/stall_alert.rs:26` | provider가 일치하는 tmux key만 stall 복구 후보로 선택 | S/I/M |
| `src/services/discord/health/recovery/stall_alert.rs:36` | provider가 일치하는 tmux key만 stall 복구 후보로 선택 | S/I/M |
| `src/services/discord/health/watcher_respawn/idle_relay_absence.rs:84` | provider별 watchdog이 자기 provider 세션만 관측 | I/B/M |
| `src/services/discord/host_defer_gate.rs:255` | idle mirror eviction에서 이름으로 provider row 선택 | S/B/M |
| `src/services/discord/host_teardown_gate.rs:199` | row provider 없을 때 teardown gate provider 보충 | S/M |
| `src/services/discord/inflight_heartbeat_sweeper.rs:97` | provider별 sweeper가 다른 bot watcher를 건드리지 않도록 제한 | I/B/M |
| `src/services/discord/org_schema.rs:447` | binding context channel 누락 시 숫자 채널명에서 channel ID 보충; auth overlay 라우팅 | B/S/M |
| `src/services/discord/org_schema.rs:467` | binding context channel 누락 시 숫자 채널명에서 channel ID 보충; auth overlay 라우팅 | B/S/M |
| `src/services/discord/recovery_engine/herdr_row_end.rs:361` | inflight channel_name 누락 시 이름으로 복구 routing 검사 | I/B/H |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:250` | 수동 rebind override 이름에서 channel_name 보충 | S/I/B/M |
| `src/services/discord/recovery_engine/restore_inflight.rs:672` | channel_name 누락 시 이름에서 보충하여 restart/복구 라우팅 및 watcher 복원 | I/S/M |
| `src/services/discord/recovery_engine/restore_inflight.rs:715` | channel_name 누락 시 이름에서 보충하여 restart/복구 라우팅 및 watcher 복원 | I/S/M |
| `src/services/discord/recovery_engine/restore_inflight.rs:942` | channel_name 누락 시 이름에서 보충하여 restart/복구 라우팅 및 watcher 복원 | I/S/M |
| `src/services/discord/recovery_engine/restore_inflight.rs:1826` | channel_name 누락 시 이름에서 보충하여 restart/복구 라우팅 및 watcher 복원 | I/S/M |
| `src/services/discord/relay_recovery.rs:372` | key의 provider/channel이 watcher snapshot과 일치하는지 destructive warrant 판정 | S/I/B/M |
| `src/services/discord/tmux/monitor_auto_turn_inflight.rs:26` | monitor synthetic inflight의 channel_name 생성 | I/B/M |
| `src/services/discord/tmux_kill_policy.rs:132` | stop tombstone에 Codex native 출력 offset을 넣을지 결정 | I/B/M |
| `src/services/discord/tmux_lifecycle.rs:72` | provider 및 thread ID를 복원하여 dispatch teardown 보호 대조 | T/S/I/M |
| `src/services/discord/tmux_reaper.rs:210` | provider별 orphan/dead 세션 정리 후보 필터; channel owner와 dispatch 보호 대조 | S/I/B/M |
| `src/services/discord/tmux_reaper.rs:232` | provider별 orphan/dead 세션 정리 후보 필터; channel owner와 dispatch 보호 대조 | S/I/B/M |
| `src/services/discord/tmux_reaper.rs:395` | provider별 orphan/dead 세션 정리 후보 필터; channel owner와 dispatch 보호 대조 | S/I/B/M |
| `src/services/discord/tmux_reaper/host_guard.rs:159` | unified thread kill 신호의 정확한 thread·provider 세션 선택 | T/S/M |
| `src/services/discord/tmux_restart_handoff.rs:497` | watcher 사망 시 provider별 restart recovery 실행 | I/B/M |
| `src/services/discord/tmux_watcher.rs:234` | watcher provider/channel 초기화; 실패하면 현재 Claude/빈 채널 기본값 | I/B/M |
| `src/services/discord/tmux_watcher/discrete_trigger_marker.rs:33` | suppressed monitor 알림의 표시 라벨 복원 | I/B/M |
| `src/services/discord/tmux_watcher/entry.rs:69` | provider/channel로 inflight 복원하여 watcher nonce/offset 기준 선택 | I/B/M |
| `src/services/discord/tmux_watcher/host_gate.rs:126` | watcher host guard provider 결정; 현재 실패 시 Claude 기본값 | I/B/M |
| `src/services/discord/tmux_watcher/provider_session_persistence.rs:91` | provider selector 저장 후 heartbeat 대상 thread ID 생성 | T/I/S/M |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:468` | unbound Claude channel 복원; parent/session segment 비교로 thread route 복원 | I/B/T/M |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:551` | unbound Claude channel 복원; parent/session segment 비교로 thread route 복원 | I/B/T/M |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:556` | unbound Claude channel 복원; parent/session segment 비교로 thread route 복원 | I/B/T/M |
| `src/services/discord/turn_presence/activity/unbound/completed_scheduled.rs:113` | scheduled-smsg 세션만 완료된 routine 소유 증거 후보로 수집 | S/R/M |
| `src/services/discord/turn_presence/activity/unbound/routine_owner.rs:72` | 이름의 provider로 routine owner receipt 대조 대상 선택 | S/R/M |
| `src/services/discord/watchers/lifecycle/restore.rs:45` | 살아남은 provider별 세션 검색, binding channel 복원, thread watcher 재획득 | S/I/B/T/M |
| `src/services/discord/watchers/lifecycle/restore.rs:79` | 살아남은 provider별 세션 검색, binding channel 복원, thread watcher 재획득 | S/I/B/T/M |
| `src/services/discord/watchers/lifecycle/restore.rs:128` | 살아남은 provider별 세션 검색, binding channel 복원, thread watcher 재획득 | S/I/B/T/M |
| `src/services/dispatched_sessions.rs:568` | provider별 native 출력 근거 경로 선택 | S/B/M |
| `src/services/dispatched_sessions/force_kill.rs:87` | force kill의 canonical row/provider 및 owner host 조회 | S/M |
| `src/services/dispatched_sessions/tmux_cleanup.rs:40` | cleanup row/provider 조회 또는 preserve 로그용 채널 표시 | S/M |
| `src/services/dispatched_sessions/tmux_cleanup.rs:526` | cleanup row/provider 조회 또는 preserve 로그용 채널 표시 | S/M |
| `src/services/message_outbox/identity.rs:88` | DM 이름인지 확인하여 delivery bot provider 선택 | S/B/M |
| `src/services/message_outbox/session_notice.rs:57` | provider/channel/namespace 검증하여 session notice bot fallback 선택 | S/B/M |
| `src/services/provider/cancel_token_cleanup/authority.rs:100` | published provider와 이름 provider 불일치 경고; 권한은 아님 | B/M |
| `src/services/provider/cancel_token_cleanup/executor.rs:327` | channel을 추출하여 unified-thread cleanup 억제 여부 조회 | T/S/M |
| `src/services/session_activity.rs:69` | provider/runtime_kind로 input-ready 판정기 선택 | B/I/S/M |
| `src/services/session_resume.rs:236` | resume의 canonical row/provider 및 owner host 조회 | S/M |
| `src/services/stale_turn_reconciler.rs:311` | DB provider와 이름 provider 일치 여부로 independent liveness/inflight 조회 허용 | S/I/B/M |
| `src/services/stale_turn_reconciler.rs:731` | DB provider와 이름 provider 일치 여부로 independent liveness/inflight 조회 허용 | S/I/B/M |
| `src/services/tmux_turn_liveness.rs:64` | idle cleanup 전에 provider별 native rollout 근거 검사 | S/B/I/M |

## 3. -t<숫자> decoder와 전체 helper 소비자

| file:line | 현재 결정 | 대체 원천 |
|---|---|---|
| `src/db/session_agent_resolution.rs:8` | 마지막 -t 뒤 15자리 이상 숫자면 parent/thread | T; legacy decoder 존치 |
| `src/domain/provider/naming.rs:28` | -t숫자를 절단 후에도 보존 | 새 생성기에 thread ID 별도 인자; legacy write는 더 만들지 않음 |
| `src/services/cluster/session_matcher.rs:213` | -t 뒤 숫자 u64면 thread binding을 parent binding에서 도출 | T/D/M |
| `src/services/discord/adk_session.rs:51` | -t 뒤 15자리 이상 u64로 thread 판정 | T; 호출부는 explicit thread 먼저 |
| `src/services/discord/tmux_reaper.rs:504` | thread면 row 삭제, 일반 채널이면 idle update 분기 | T/S; **수명 전이 영향** |
| `src/services/discord/tmux_reaper/host_guard.rs:153` | 정확한 -t<id><env> suffix로 kill 대상 찾음 | T/S/M |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:559` | parent name segment 뒤 -t 숫자가 thread channel인지 확인 | T/I/B/M |
| `src/services/discord/watchers/lifecycle/restore.rs:129` | binding 없는 생존 세션 -t ID를 watcher channel로 사용 | T/S/B/M |
| `src/services/routines/session_control.rs:564` | SQL session_key ~ '-t<id>(-dev)?$' fallback 조회 | sessions.thread_channel_id 우선 유지; legacy row fallback만 존치 |
| `src/db/dispatched_sessions/thread_gc.rs:28` | thread_channel_id NULL인 옛 row backfill | T/S/I/B, 옛 이름만 legacy fallback |
| `src/db/session_agent_resolution.rs:25` | key→채널 decoder 결과에서 thread ID 반환 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/db/session_agent_resolution.rs:215` | agent resolution에 key 유래 thread fallback 추가 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/db/session_agent_resolution.rs:239` | agent binding lookup 전에 parent segment 복원 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/recovery_engine/restore_inflight.rs:2267` | 복구 후 session status/hook에 thread ID 기록 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/router/message_handler/headless_turn.rs:835` | headless hook session row의 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/router/message_handler/intake_turn/adk_thread.rs:10` | thread intake 기존 채널이 thread였는지 판단 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/tmux_lifecycle.rs:77` | dispatch 보호용 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/tmux_lifecycle.rs:81` | dispatch 보호용 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/tmux_reaper.rs:532` | watcher session/status/heartbeat에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/tmux_watcher.rs:254` | watcher session/status/heartbeat에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/tmux_watcher/provider_session_persistence.rs:90` | watcher session/status/heartbeat에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/tmux_watcher/terminal_token_update.rs:43` | watcher session/status/heartbeat에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/turn_bridge/activity_heartbeat.rs:10` | heartbeat/status/analytics에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/turn_bridge/activity_heartbeat.rs:15` | heartbeat/status/analytics에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/turn_bridge/completion_postlude.rs:237` | heartbeat/status/analytics에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/turn_bridge/post_loop_finalize.rs:468` | heartbeat/status/analytics에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/turn_bridge/stream_tick.rs:676` | heartbeat/status/analytics에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/turn_bridge/turn_analytics.rs:374` | heartbeat/status/analytics에 기록할 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/discord/watchers/lifecycle/restore.rs:787` | 복구 watcher의 dispatch channel에서 thread ID 도출 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/dispatched_sessions/hook.rs:13` | hook 명시 thread 누락 시 session_info 이름/key에서 thread 보충 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/dispatched_sessions/hook.rs:16` | hook 명시 thread 누락 시 session_info 이름/key에서 thread 보충 | T/S/I/B, 옛 이름만 legacy fallback |
| `src/services/dispatched_sessions/listing.rs:95` | dashboard/API listing의 thread ID fallback 표시 | T/S/I/B, 옛 이름만 legacy fallback |

`src/services/routines/session_control.rs:595`는 위 regex의 SQL bind(별도 판독 아님). `src/services/discord/tmux_lifecycle.rs:77,81`과 activity_heartbeat.rs:10,15는 각각 hint/이름 fallback 두 호출이다.
`src/dispatch/dispatch_channel.rs:48,56,61`은 현재 unified-thread active=false, name-active=false, kill signal=[]이다. parser라는 주석만 남고 실제 판독은 하지 않는다. 이번에 기능을 되살리지 않는다.

## 4. Rust 밖의 운영 판독

| file:line | 현재 결정 | 대체 원천 |
|---|---|---|
| `dashboard/src/components/agent-manager/discord-routing.ts:112` | prefix 제거 후 provider-를 제거하여 Discord 채널 이름 표시 | API의 channel_name/channel_id/provider 필드 우선, 문자열 fallback은 legacy만. 새 실제 이름은 display field로 별도 전달 |
| `policies/lib/timeouts-helpers.js:54` | key 꼬리의 provider 접두어 제거; 없으면 regex /^AgentDesk-[^-]+-/ 제거 후 -dev 제거 | policy가 받은 row.channel_id/provider 및 명시 display/channel_name 사용; 구형 row만 현행 fallback |
| `src/db/session_evidence.rs:66` | scheduled 이름을 고정채널 evidence JOIN에서 제외 | identity_kind != scheduled_snapshot을 새 writer에서 보장; legacy 이름 필터는 그대로 이중 판독 |
| `src/services/discord/session_runtime/channel_routing.rs:3,11,22,264` | synthetic parent-tID 생성·정확 suffix로 합성 채널인지 판단해 복원 이름 선택; guild channel 이름과 nonGuild None 분기 | explicit parent/thread ID와 저장 channel metadata로 신형을 판정. 기존 -t helper는 old 복원에만 |
| `policies/lib/timeouts-helpers.js:67,73` | parent의 -t숫자 제거 / thread ID 추출 | row.thread_channel_id/channel_id/agent_id 우선, 옛 key만 legacy decoder. 신형 숫자 꼬리 범용 확장 금지 |
| `policies/lib/timeouts-helpers.js:178,189,190` | fallback thread와 parent 이름으로 agent/notice target 결정 | explicit row/dispatch/channel directory; 메타데이터 미확정은 unknown으로 보존 |
| `policies/timeouts/idle-kill.js:54,85` | SQL -t regex와 helper null로 일반 channel cleanup 분류 | 구조 thread/channel identity 우선. unknown을 일반 채널로 간주하지 않고 kill 0. writer 이전 reader 전환 |
| `migrations/postgres/0101_canonical_discord_session_identity.sql:204` | scheduled 이름으로 fixed-channel canonical 식별 제약 예외 부여 | 기존 migration은 수정하지 않음. 새 writer가 identity_kind를 명시하여 새 이름이 예외에 의존하지 않게 함; 필요 시 새 additive migration으로만 |

## 5. 시험 가정 계수

`git grep -n -E 'AgentDesk-|TMUX_SESSION_PREFIX|parse_provider_and_channel_from_tmux_name|parse_thread_channel|rfind\("-t"|strip_prefix\("-t"|thread_suffix_regex|provider_and_channel\(' -- src` 원문은 **2,172행 / 482파일**. 이는 주석·문서 예시·fixture도 포함한 후보이며 변경 파일 수가 아니다.

`/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/count.py`가 문자열·주석을 제외한 Rust brace 위치로 `#[cfg(...test...)]` item span, `#[test]/#[tokio::test]` function span 및 명시 시험 경로(*_tests.rs, tests/, test_support 등)를 분류한 결과:

- 시험 후보 **1,954행 / 416파일**. 그중 `AgentDesk-` literal이 직접 있는 행 **1,940행 / 412파일**. 이 이름을 전부 새 이름으로 일괄 바꾸면 legacy read 시험이 사라지므로 금지.
- 검색 토큰을 직접 담은 `#[test]/#[tokio::test]` 함수 **1,237개**. helper/fixture를 경유하는 간접 의존 시험은 이 수에 포함하지 않으므로 전체 영향 시험 수나 실행 수로 보고하지 않는다.
- 비시험 텍스트 후보 **218행 / 100파일**에는 import·주석·생성 API 예시도 들어 있다. 위 판독 표가 그 후보를 의미별로 직접 검토한 결과다.
- 직접 `starts_with("AgentDesk-")` 비시험 판정은 **7행 / 6파일**(doctor 1, cli utils 2, codex_census 1, watcher_respawn 1, relay decision 1, worktree sweep 1). 동적 provider 접두어 session_notice 및 TMUX_SESSION_PREFIX 사용은 별도 표에 포함.
- TMUX_SESSION_PREFIX는 **정의·import·re-export·문서 포함 13행 / 9파일**. 실제 판정은 표의 각 행을 사용한다.
- 수량은 실측 정적 후보 계수이며 실행 시험 수가 아니다. 새 계약 RED는 naming, unmanaged collision, restart old/new, cleanup ownership, thread metadata 우선, dashboard/policy fallback에 별도로 작성한다.

Rust 밖 시험 후보: dashboard `AgentDesk-` fixture **7행 / 2파일**, policy `AgentDesk-` fixture **28행 / 2파일** (정적 literal 계수; 원문 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/naming-nonrust.log`).

## 6. 상태차트 판정

0단계의 '상태 전이 해당 없음' 판정을 뒤집는다. 근거: tmux_reaper.rs:504의 thread 분류가 delete/idle 전이를 고르고, :210/:395의 provider 판독은 정리 후보를 결정하며 watchers/lifecycle/restore.rs:45/:128은 재획득 대상과 channel을 고른다. 새 관리 표식 미확정·충돌 시 보존하는 분기가 들어간다. `docs/statecharts/session-lifecycle/` 기존 모델에 old/new managed locator·foreign collision·marker unknown·alias ambiguity를 입력 조건으로 추가하고, 신규 cleanup 권한이 없는지 TLC+목표 시험으로 검증하는 계획이 필요하다. 조사 레인에서는 모델/코드 변경·실행 없음.

# 부록 B. 세션 키·inflight·바인딩

## 세션 키·inflight·바인딩 경로 (기준 5bcbb82297f5cc3ab8897b81ddb9891459e0321b)

원시 검색: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/identity-grep.log` (명령 `git grep -n -E 'session_name|tmux_session|inflight|binding' -- 'src/**/*.rs' ':!agentdesk.yaml' ':!launchd.env' ':!settings.json'`; 36,609행). 아래는 단순 인자 전달/로그/시험 fixture를 묶고, 이름이 키·증거·경로가 되는 각 소유 seam을 열거한 표다. 코드·운영 저장소는 변경하거나 실행하지 않았다.

| 소유 seam / 좌표 | 이름을 포함하는 값·판정 | 이름 변경시 끊기는 대조 | 이중 판독·live 보존 방법 |
|---|---|---|---|
| `src/domain/session_identity.rs:25`, `:32`, `:80`; `src/services/discord/session_identity.rs:8` | legacy `host:<actual_name>` 및 namespaced `provider/hash/host:<actual_name>`의 tail; legacy 역직렬화 | 새 포맷 key를 옛 key와 문자열 동일하다고 가정하면 누락 | 이 파서는 접두어와 무관하게 이미 양쪽 이름을 읽음. wire 문법은 유지하고 actual_name을 불투명 locator로 취급. 새 이름에서 provider·thread 역추정 금지 |
| `src/services/discord/adk_session.rs:59`, `:84`, `:135`, `:150`, `:155` | 채널 basis→build_tmux_session_name→namespaced/legacy key 후보 2개 | generator를 먼저 바꾸면 follow-up에서 기존 행과 다른 key 생성 | canonical `(provider, token_hash, channel_id)`로 기존 actual key를 먼저 찾고 그 실제 이름을 반환. absence를 확인한 새 생성에만 새 이름. 후보 2개는 **동일 actual name의 key wire 2종**이며 old/new name 지원과 다름 |
| `src/services/discord/host_key_derivation.rs:105`, `:117`, `:141` | canonical row의 실제 session_key→name; 다중 행/실패 Conflict/Unknown | 생성한 이름으로 덮으면 host·live locator 상실 | 이미 존재하는 canonical row resolver를 재사용. 반환한 실제 key/name을 이후 launch/reattach/stop까지 보존 |
| `src/db/dispatched_sessions/canonical_identity.rs:33`, `:265`, `:287`, `:297`, `:320`, `:396`, `:469`, `:492`, `:825` | canonical identity `(kind, provider, discord_token_hash, channel_id)`, exact primary·alias 및 ID pin | exact direct SQL만 쓰는 소비자는 alias를 못 봄; name collision을 동일 채널로 잘못 합칠 위험 | main에 identity/alias 해법 이미 있음. exact→alias→canonical 증거가 **한 sessions.id**로 수렴할 때만 사용. divergence/foreign owner는 fail closed. 기존 live 행 primary key/name 변경·무조건 alias 새 생성 금지. 자연 재생성 때 기존 identity/alias 경로만 재사용 |
| `src/db/dispatched_sessions/canonical_identity.rs:549` | 서로 다른 host:tmux는 서로 다른 terminal로 분류 | 이름만 rename해도 다른 terminal 승격으로 읽을 수 있음 | old/new 이름을 canonical identity만 같다고 살아 있는 한 terminal로 합치지 않는다. 신규 실행은 새 nonce로 새 lifetime, 옛 실행은 기존 lifetime 유지 |
| `src/services/discord/idle_relay_drift.rs:601`; `recovery_engine/manual_rebind_output_path.rs:289`; `recovery_engine/manual_rebind_override.rs:222`; `session_runtime/restore_cwd.rs:163`, `:200`, `:261`; `tmux_lifecycle.rs:88`; `watchers/lifecycle/activity.rs:61`, `:142`; `watchers/lifecycle/restore.rs:299`; `watchers/lifecycle/restore_core.rs:27`; `watchers/lifecycle/restore_support.rs:200` (모두 `src/services/discord/` 기준) | build_session_key_candidates 소비자 전수(비시험 호출 12곳) | 생성한 새 이름으로 candidate를 만들면 old row/CWD/override/protection 미조회 | shared resolver로 찾은 실제 name/key를 전달. 각 seam에 old live + new preferred가 다른 fixture. 배열 후보만 4개로 늘리는 방식은 다른 terminal을 alias로 오인하므로 피함 |
| `src/services/discord/tmux_lifecycle.rs:65`, `:72`, `:76`, `:101`, `:147` | active dispatch 보호: 실제 key 2개 및 legacy `-tID` 파싱, sessions.thread_channel_id와 namespace LIKE | 새 `<agent>-ID`를 옛 parse로 보면 dispatch 보호 누락; suffix를 넓히면 일반 채널 오인 | thread_id는 sessions.thread_channel_id/inflight.thread_id/명시 descriptor에서 받음. key는 resolved actual; legacy 이름 파서는 legacy prefix에만 적용. 보호 범위·status·기존 SQL 권한 확대 금지 |
| `src/services/discord/inflight/store.rs:90`, `:117`; `model.rs:122`, `:133`, `:135`, `:137`, `:203`, `:204`, `:205`, `:206`, `:225`, `:233` | 파일은 `<root>/<provider>/<channel_id>.json`, lock은 `.json.lock` — **이름 불포함**. 내용에 actual name/key/output/FIFO/host_locator/session-record-id/nonce | 이름을 바꿔 payload만 고치면 파일명은 같아도 active execution의 경로·authority 다름 | 파일 레이아웃 변경 불필요. old JSON은 그대로 역직렬화, old actual name과 pinned output/FIFO를 계속 사용. 신규 lifetime만 새 name 기록. 저장경로/lock migration 추가 금지 |
| `src/services/discord/inflight/model/identity.rs:36`, `:57`; `model.rs:70`, `:89`; `model/presence_identity.rs:110` | identity tuple에 name equality, synthetic finalizer ID hash에 name, presence CAS snapshot에도 name/key/path | 실행 중 rename은 same turn을 successor/authority mismatch로 오인·hash 변경 | equality/hash 완화하지 않는다. 살아 있는 이름을 보존해서 기존 fence 그대로 유지. new name은 new incarnation에서만 사용. RED: old-name inflight 저장→preferred new name 적용 후 old turn clear/heartbeat 완료 |
| `src/services/discord/inflight/store.rs:576`; `native_stop_pin.rs:49`, `:102`, `:125`; `managed_submission.rs:287`; `managed_submission/evidence.rs:40`; `status_panel_orphan_store.rs:120` | turn re-save / stop pin / admitted input / orphan panel의 actual name 동일성 | name alias를 무조건 허용하면 다른 pane/후속 turn stop·삭제 | 실제 name 유지, 기존 nonce/offset/name conjunction 유지. 신규 descriptor alias를 이 비교 함수로 주입하지 않음 |
| `src/services/discord/inflight/host_locator.rs:19`, `:32`, `:72` | explicit host kind/session ID/pane/execution_node. unknown JSON은 원문 보존 | unknown/present-null을 absent로 해석하면 tmux fallback 확대 | 이미 존재하는 Known/Unknown/Absent 의미 보존. #8119에서 hosted locator schema나 downgrade 정책 수정 불필요 |
| `src/services/tmux_common.rs:1113`, `:1134`, `:1153`, `:1167`, `:1233`, `:1278`, `:1283` | canonical persistent/legacy tmp 경로 prefix가 `<runtime hash>-<host>-<actual name>`; 확장자 jsonl/input/prompt/owner/sh/generation/auth_profile/auth_session/spawn_nonce/exit_reason/runtime_kind/channel/dead/hook settings/launch script/rollout/codex home | rename하면 open FD와 새 path가 갈라져 output·FIFO·auth·nonce 분실; cleanup도 옛 파일 누락 | 살아 있는 모든 path는 기존 actual name으로 계속 읽고 쓰기. 이미 canonical persistent + legacy tmp dual-location reader 있음; **이름의 migration과 위치 migration을 혼동하지 않음**. 새 이름은 신규 wrapper만. NFC normalize는 신규 name 생성에만, 과거 path/key는 byte 그대로 유지 |
| `src/services/tmux_common/session_markers.rs:9`, `:24`, `:37`, `:58`, `:86`, `:157`; `tmux_common/host_marker.rs:12` | auth/profile/native session/runtime/host sidecar가 이름으로 조회 | rename 또는 name-based 추정만으로 auth evidence 약화 | old actual name으로 읽고 새 spawn에서만 새 marker. missing/unreadable/foreign를 success로 바꾸지 않음; existing marker는 ownership 판정의 legacy 증거로 사용 가능 |
| `src/services/tmux_common/runtime_hint_fence.rs` (name keyed fence); `src/services/provider/cancel_token_cleanup/authority.rs:8`, `:46`, `:90` | source authority·runtime file fence·cancel generation slot `(provider, name)` | rename하면 두 슬롯/두 fence로 분열, stale cancel 허용 가능 | live name 변경 금지. 새 ownership 표식 검사 후 **실제 target name**으로 기존 fence를 잡음. provider parse 비교는 metadata 공급으로 교체하되 generation checks는 유지 |
| `src/services/tui_prompt_dedupe.rs:233`~`:291`, `:328`; `runtime_binding/binding_access.rs:71`, `:97`, `:150`; `state.rs:89` | name keyed pending/observation/channel/runtime/anchor/native-turn/relay lease/steer/handoff/restore maps; provider-session→name reverse map | rename은 pending·relay lease·binding·dedup를 분리, hook context mismatch | 모든 map은 actual name을 유지. new preferred→actual resolver는 map 진입 **앞**에서만; map 자체에 두 키로 복제 금지. 중복 전달·stale capability 방지 |
| `src/services/tui_prompt_dedupe/binding_context.rs:21`, `:68`, `:98`; `runtime_binding/binding_access.rs:71` | immutable context JSON path는 `<provider>/<nonce>.json` (name 불포함), 내용 tmux_session/owner root/channel/provider/host/expected native ID | live rename은 immutable context.tmux_session과 authority.session 불일치 | context 불변 유지. 새 name은 새 nonce context로만. provider/channel/ownership metadata의 좋은 대체 근거이나 context를 mutable alias registry로 변형하지 않음 |
| `src/services/tui_prompt_dedupe/binding_events.rs:72`, `:240`, `:480`, `:685`, `:956`; `src/services/tui_o/writer/binding.rs:71` | durable binding log filename channel_id (이름 불포함), record.tmux_session 및 fold panes의 key 이름 | old/new 이름을 같은 pane으로 fold하면 incarnation/seq 오염 | old logs 원문·old pane key 유지. 신규 lifetime 새 record. #8119에서 log rewrite 필요 없음; historical reads는 record 실제 name으로 조회 |
| `src/services/discord/tmux_watcher_registry.rs:210`~`:227`, `:404`, `:440`; `tmux_watcher_registry/session_binding.rs:20`, `:48`, `:66` | by_name watcher handles·channel reverse map·owner/restored-owner·reservation; live owner 우선 | generator 이름으로 restore하면 old watcher 두 개 생성/owner loss | registry 반환 actual name 우선, live claim > restored 유지. preferred name이 달라도 old watcher 살아 있으면 재사용. metadata mismatch는 소유 이전 허용 아님 |
| `src/services/cluster/session_registry.rs:103`, `:149`, `:191`; `session_discovery.rs:174`, `:280`, `:387` | per-node matched registry 이름 key; expected_session_name과 saved session_key→live name 교집합 | expected name 새 생성만 하면 old live inventory 안 맞아 Removed/Added 이벤트 | discovery 후보를 explicit descriptors + durable actual locators + legacy parse로 확장하고 동일 canonical owner에 old live 우선. 이름 parse 기반 `tmux_name_and_segment_from_session_key`는 row.provider/channel_id로 대체 |
| `src/services/discord/tmux_session_files.rs:20`, `:227`, `:433`; `tmux_reattach_offsets.rs:13`, `:55` | generation mtime path·panel instance `<name>#<spawn_nonce>`·offset resume에서 name equality | rename하면 panel reset, generation 0, offset 재송신 | old markers/name/nonce/offset 그대로. 새 이름으로 generation/nonce 복사하지 않음; 신규 lifetime만 새 marker |
| `src/services/session_backend.rs:109`, `:344`, `:368`, `:383`, `:507` | process fallback registry handle·stop marker·active-turn guard도 name key | tmux만 바꾸면 process mode에도 old follow-up miss | host 종류와 관계없이 actual name 불투명. process mode 별도 smoke 계약 포함. host 추상화 확대는 #7583으로 남김 |
| `src/services/provider_cli/paths.rs:23`, `:71`, `:76` | launch artifact filename = sanitized session_key + SHA digest | 표시명만 바꿔 새 key 만들면 old launch artifact 미조회 | artifact는 resolved actual key로 조회. 새 artifact만 새 key. 현재 digest가 raw Unicode 바이트에 민감하므로 old keys NFC 재작성 금지 |
| `src/db/session_agent_resolution.rs:7`, `:17`, `:23`, `:192`; `src/db/dispatched_sessions/thread_gc.rs:8`, `:28`, `:65`, `:95` | thread/agent 복원은 canonical fields 우선 + 이름 legacy fallback; GC backfill 옛 suffix 파싱 | 새 `<agent>-ID`를 일반 parser로 확대하면 채널명을 thread로 오인해 GC 대상 확대 | 신규 thread write는 thread_channel_id를 반드시 직접 저장. old `-tID` fallback만 보존. 새 이름의 마지막 숫자만 보고 thread 판정 금지. GC 범위/status·deletable·child guard는 그대로 |
| `src/db/dispatched_sessions/hosted_execution.rs:36`, `:47`, `:77`; `src/services/session_host/resolve.rs:70`, `:118`, `:156` | Herdr owner.logical_key·source_ref.logical_key는 name 포함 full key; location은 socket/named-session/pane ID; resolver row/host/inflight 증거 투표 | logical name rename만으로 owner/source_ref·host marker divergence | Herdr pane ID/host locator를 target 권위로 유지, logical_key old value 보존. 새 pane label은 display만. #8119에서 hosted execution schema·consent·termination authority는 건드리지 않음 |
| `src/services/termination_audit.rs:179` | audit legacy key = host:name | 새 이름 세션 audit는 새 key, historical joins 혼동 가능 | audit event가 가진 actual key/name을 그대로 기록. canonical session ID가 있으면 별도 추적, 과거 audit rewrite 불필요 |

### main에 이미 충족된 것 / 재구현하지 않을 것

- canonical tuple로 channel collision을 구분하고 exact·alias 증거 divergence를 막는 코드가 이미 있다: 위 SHA `canonical_identity.rs:320`, `:492`, `:825`; `session_canonical_identity.rs:42`의 sanitize/truncate collision 계약시험.
- inflight와 binding log의 **파일명**은 이미 channel ID 기반: `inflight/store.rs:90`, `binding_events.rs:240`. 새 이름 때문에 이를 다시 migration할 이유가 없다.
- unknown host locator 원문 보존과 fail-closed 판독은 이미 있다: `inflight/host_locator.rs:32`, `:72`; full session key를 그대로 받는 host resolver `session_host/resolve.rs:67`.
- durable owner/channel sidecar가 이미 있다: `tmux_common.rs:1271`~`:1335`. tmux `@agentdesk_*`는 아직 신규 **설계**이며 현재 구현됐다고 쓰면 안 된다.

### 권장 해법과 자동 ownership

1. `PreferredSessionName`(새 표시명)과 `ResolvedSessionLocator`(actual name/key, provider/token/channel/thread/host/nonce)를 구별한다. existing canonical row, watcher/inflight, ownership metadata가 서로 일치하면 actual name을 우선한다. live old name은 rename·rekey·파일 rename 하지 않는다.
2. 신규 tmux session은 원자적 `new-session` 성공으로 생성한 것에만 managed schema/runtime/provider/token/channel/thread/nonce 표식을 기록한다. 이름이 이미 있으면 metadata+기존 sidecar+DB canonical evidence의 소유 증거를 확인하기 전 read-only다. 상대방 session에 먼저 표식을 쓰고 "입양"하면 안 된다. exact name 경쟁은 tmux new-session의 atomic failure로 잡는다.
3. 새 선호 이름을 사용자가 선점했으면 사용자 세션은 그대로 두고 자동 충돌 이름(예: 채널ID·runtime digest suffix; 최종 naming 정책은 부모 통합)으로 **우리 세션을 새로 생성**한다. suffix가 어느 채널인지 읽는 권위는 표식이며 문자열 parser가 아니다. 충돌 candidate도 선점됐으면 다음 deterministic candidate를 시도하되 임의 세션 kill/rename 없음.
4. legacy `AgentDesk-*`도 prefix만으로 ownership을 인정하지 않는다. 기존 runtime-root `.owner`와 `.channel`, canonical row/provider/nonce가 일치할 때만 old-live 재사용/관리. 미표식·손상·다른 runtime 소유는 다른 사람 세션과 같이 처리한다. dual-read는 legacy 이름 **구문 판독**을 허용하는 것과 **mutation 권한**을 주는 것을 구별한다.
5. identity/hash/equality/fence를 alias 허용으로 느슨하게 하지 않는다. 이름을 canonical identity로 부르는 것은 탐색만, 실제 I/O·sidecar·turn CAS는 선택된 실제 locator에 고정한다. 기존 cleanup 범위/status/lease/nonce 검사는 유지한다.

### #8118 / #7583 경계

- 이번에 함: `<agent>-<threadID>` 생성에 explicit thread_id를 넣기, parent/provider/token/channel metadata 전달, old `-tID` compatibility reads, 새 이름에 thread suffix parser 의존 제거.
- #8118에 남김: 스레드가 부모 채널의 tmux/Herdr/process host·execution node/endpoint 선택을 상속하는 **배치 정책**. 이름 builder는 host를 결정하지 않는다. 상속된 parent descriptor를 입력으로 받는 계약만 공유한다.
- #7583에 남김: 전체 interactive session host abstraction과 모든 provider/ops API의 host-neutral routing 이관. 현재 `session_host` witness/locator를 읽어서 이름 추정을 줄이되 hosted execution schema·consent/termination gate를 재설계하지 않는다.

### 상태차트 판정

초기 "이름 문자열 변경만"이라면 비대상이다. 그러나 이번 수용조건은 prefix 판정 제거 후 외부 세션 비간섭·충돌 생성·옛 세션 재획득 조건을 바꾼다. 따라서 브리프의 예외가 적용된다: `docs/statecharts/session-lifecycle/` 기존 모델을 구현 조각에 포함해 `existing owned legacy → retain`, `existing foreign → choose another name`, `Unknown/Conflict → no mutation` admission 조건을 갱신해야 한다. 별도 턴/입력 모델 확대는 불필요하다.

### RED 계약·뮤턴트 제안

- old live key + old inflight/hash/nonce/FIFO/output + new preferred name을 넣어 restart discovery/queued follow-up/dispatch protection/stop/clear가 **같은 actual name**을 유지함. 뮤턴트: existing lookup보다 new builder 우선 → RED.
- same preferred name, 다른 provider/token/channel 또는 외부 미표식 session이 있을 때 0 mutation + own collision name 생성. 뮤턴트: name equality를 ownership으로 간주 → RED.
- 새 `<agent>-123...` **일반 채널명**인데 thread metadata absent: thread_gc/dispatch thread lookup 불발. 새 실제 thread는 explicit thread ID가 살아남음. 뮤턴트: 신규 이름 suffix 숫자를 thread로 추정 → RED.
- primary/alias/canonical이 다른 sessions.id면 Conflict, unreadable metadata는 Absent로 읽지 않음. 기존 main 계약을 재사용하고 새 resolver 소비자 경로만 추가.
- `InflightTurnIdentity`·cancel generation·binding nonce·panel instance 기존 비교에 actual name이 유지됨. 뮤턴트: alias 문자열 정규화로 equality 완화 → old/new 동시 pane fixture RED.

# 부록 C. 외부 의존 목록

# #8119 외부 표면 조사 — 읽기 전용

기준: `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`. cargo 실행, 운영 세션/설정/DB/store 접근, GitHub/DAG 쓰기, 스킬 수정 없음. 아래는 코드 근거이며 한글 실물 probe는 부모 담당이다.

## 전수 검색과 수

명령: `git grep -n -I 'AgentDesk-' -- scripts docs ':!**/agentdesk.yaml' ':!**/launchd.env' ':!**/settings.json'`.

- 전체 25파일/59행; `scripts/` 12파일/26행, `docs/` 13파일/33행. 문자 일치 수이며 `AgentDesk-side`, `AgentDesk-managed`, HTTP User-Agent 같은 비이름 용법을 포함한다.
- 원문 전부: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/external-grep.log`.
- `-t` 추가 검색에서 실제 thread 생성은 `scripts/e2e/run_tui_relay.py:466`; 명명 계약은 `docs/session-binding.md:63`. T5/T6 프로젝트명 등은 suffix 판독이 아니다.

| 파일 | 일치 줄 | 처분 |
|---|---|---|
| `docs/agent-maintenance/change-surfaces.md` | 929, 936 | AgentDesk-side/managed 등 일반 복합어. 세션 이름 무관, 변경하지 않음. |
| `docs/agent-maintenance/discord-outbound-migration.md` | 150 | DM legacy locator 예시는 legacy라고 명시; DM까지 새 이름 적용하는 범위와 분리. |
| `docs/agent-maintenance/multinode-two-node-smoke.md` | 38 | AgentDesk-* 보호 규칙을 owner metadata 확인 및 foreign 세션 무조작 규칙으로 대체. |
| `docs/agent-maintenance/opencode-usability-spec.md` | 292, 371, 410, 477, 522 | AgentDesk-side/managed 등 일반 복합어. 세션 이름 무관, 변경하지 않음. |
| `docs/claude-md-load-order.md` | 46 | AgentDesk-side/managed 등 일반 복합어. 세션 이름 무관, 변경하지 않음. |
| `docs/design/kakao-calendar.md` | 11 | AgentDesk-side/managed 등 일반 복합어. 세션 이름 무관, 변경하지 않음. |
| `docs/gemini-cli-0.38.1-review-issue-36.md` | 14 | AgentDesk-side/managed 등 일반 복합어. 세션 이름 무관, 변경하지 않음. |
| `docs/generated/cli-reference.md` | 75, 712, 1238 | reset-tmux 설명은 소스 수정 후 공식 생성기 재생성; --no-agentdesk 설명 무관. |
| `docs/plans/discord-tui-relay-unification.md` | 174, 176, 186 | 과거 path 사례는 legacy 예시로 남기고 신형 경로 계약 링크 추가. |
| `docs/relay-state-contract.md` | 1295 | grace의 managed-session 예시를 표식 기준 설명으로 갱신. |
| `docs/session-binding.md` | 15, 22, 23, 24, 25, 26, 33, 37, 41, 103, 141, 178 | 공개 계약 재작성. 44byte 충돌 불가 주장(68-70)이 실제 동작과 모순; canonical/alias 절은 재사용. |
| `docs/source-of-truth.md` | 91 | AgentDesk-side/managed 등 일반 복합어. 세션 이름 무관, 변경하지 않음. |
| `docs/statecharts/session-lifecycle/TmuxSession.tla` | 4 | 현재 모델의 이름 하나=채널 세션 하나 전제. ownership/dual-read가 수명 판정을 바꾸면 기존 모델 갱신. |
| `scripts/deploy-release.sh` | 396, 1277 | dcserver 수동 fallback 세션. worker 이름과 별도라 기본값 유지; 기존 kill 권한 확대 금지. |
| `scripts/e2e/post_deploy_relay_continuity.py` | 80, 86 | 문서형 before/after fixture. 옛/새 공존 예제로 교체. |
| `scripts/e2e/run_tui_relay.py` | 467 | 중앙 E2E 이름 writer. 신형 기본 이름 + 실제 바인딩이 선택한 옛 locator 허용. |
| `scripts/e2e/tui_relay/midturn_kill.py` | 168 | 파괴적 E2E 대상 allowlist. writer와 같은 조각에서 표식/정확 binding 확인으로 교체. |
| `scripts/e2e/tui_relay/test_cell_resolution.py` | 41, 45, 443 | 시험 fixture. 옛 형식 회귀는 보존하고 신형 fixture 추가; 순차 업데이트. |
| `scripts/e2e/tui_relay/test_driver_health.py` | 2584, 2657, 2658, 2659, 2660, 2668, 2738, 2745, 2750, 2756 | 시험 fixture. 옛 형식 회귀는 보존하고 신형 fixture 추가; 순차 업데이트. |
| `scripts/e2e/tui_relay/test_durable_delivery.py` | 26, 270 | 시험 fixture. 옛 형식 회귀는 보존하고 신형 fixture 추가; 순차 업데이트. |
| `scripts/e2e/tui_relay/test_midturn_kill.py` | 132 | 시험 fixture. 옛 형식 회귀는 보존하고 신형 fixture 추가; 순차 업데이트. |
| `scripts/e2e/tui_relay/test_post_deploy_relay_continuity.py` | 179 | 시험 fixture. 옛 형식 회귀는 보존하고 신형 fixture 추가; 순차 업데이트. |
| `scripts/e2e/tui_relay/test_post_deploy_turn_smoke.py` | 297 | 시험 fixture. 옛 형식 회귀는 보존하고 신형 fixture 추가; 순차 업데이트. |
| `scripts/test_p11_m2_activation_selftest.py` | 62 | 시험 fixture. 옛 형식 회귀는 보존하고 신형 fixture 추가; 순차 업데이트. |
| `scripts/toolchain_update.py` | 93 | User-Agent 문자열. session naming 무관, 유지. |

## 대시보드와 API

| 위치 | 현재 동작 | 수리 계약 |
|---|---|---|
| `dashboard/src/components/agent-manager/discord-routing.ts:105-123` | key의 첫 colon 뒤에서 `AgentDesk-`와 provider prefix를 제거하여 채널 이름 복원. plain `host:adk-cc`는 `null`, `host:claude-sales`는 `sales`로 오인 가능. | API의 channel/thread/name 필드를 우선. legacy key parser는 fallback에만 남기고 신형 이름에서 provider를 추론하지 않는다. |
| 같은 파일 `:185-188`, `:192-201` | fallback 순서 parser→session.name→session_key; canonical channel/thread 필드는 이미 링크 타깃에 우선. | fallback 순서를 metadata name→legacy parser→opaque locator로. 공급자명으로 시작하는 새 채널 회귀 시험. |
| `dashboard/src/components/agent-manager/discord-routing.test.ts:72,79,95,113,127` | legacy 이름 fixture 5곳. | 옛 fixture를 지우지 말고 새 한국어 이름/새 thread/dash provider 이름 fixture 추가. |
| `src/services/agents/serialization.rs:24-44`, `:115-134` | provider/channel/thread IDs와 deeplink를 구조화하여 JSON으로 전달. 이름을 ASCII 변환하지 않는다. | 기존 구조 필드 재사용; 별도 label 필요 시 optional display_name을 추가하고 session_key는 opaque 유지. |
| `src/services/agents/query.rs:194-200`, `:226-257`, `:280-294` | DB에서 session_key/provider/thread_channel_id를 읽고 직렬화. 이름으로 provider를 복원하지 않음. | 새 이름으로 key 전체를 재작성하지 않고 alias/canonical identity 기존 계약 사용. |
| `src/server/routes/agents.rs:289-300` | agent_name/provider/session_key/thread_channel_id JSON 제공. | UTF-8 보존 시험(serde round-trip); 운영 API 호출 불필요. |
| `dashboard/src/components/session-panel/SessionPanelModel.ts:41`, `:47-49` | name 우선. name 없으면 `Session ${key.slice(0,8)}`로 축약, full에는 key. | 실제 name/label 제공; React 문자열 그대로 표시. emoji supplementary char 포함 byte/UTF-16 절단 fixture 검토. |
| `dashboard/src/components/session-panel/SessionInfoCard.tsx:121` | 전체 세션 키 문자열 표시. | escape는 React 기본 적용. 한글 문자열 UI fixture 검증. |
| `dashboard/src/components/cluster/ClusterNodesPanel.tsx:81,84-86,96` | name/key 표시, 중지 호출에는 전체 session_key 그대로 사용. | display를 바꿔도 중지 key가 바뀌지 않는 시험. |
| `src/server/routes/docs/inventory/endpoints/part_01.rs:357`, `part_05.rs:458,465`, `part_06.rs:131` | API 예제의 AgentDesk-* 이름. | 새 예제와 legacy 호환 예제 구분; 생성 문서 경로로 반영. |

API·UI는 코드 조사만 수행했다. 실제 빌드/운영 API 접근을 했다고 주장하지 않는다. 부모의 격리 fixture 렌더와 함께 증거 수준을 구분해야 한다.

## herdr, Unicode, shell

- `src/domain/provider/naming.rs:16-50`: 현재 `char::is_alphanumeric()` 또는 `-`/`_` 유지, 나머지를 `-`로 치환. 문서의 `[A-Za-z0-9_-]` 설명은 실제와 다르며 한글은 유지된다. `safe_prefix(...,44)`는 **byte** 예산이고 `src/utils/format.rs:4-12,38-43`이 UTF-8 char boundary로 내리므로 panic은 피하나 앞 14음절(42byte)이 같은 두 15음절 이름은 충돌한다.
- `Cargo.toml:87`에 `unicode-normalization` 의존성이 이미 있다. 현 naming 모듈에 `.nfc()`/`.nfd()` 적용 없음. 신형 writer에는 NFC 정규화 후 허용문자 치환/길이 계산을 넣고, 옛 경로 lookup은 저장된 원문을 사용해야 한다. APFS가 정규화 등가 파일을 같은 inode로 처리하는지 부모 실측 결과에 맞춰 이름 기반 파일 충돌을 방지한다.
- `src/services/tmux_common.rs:1113-1140`: 경로 basename에 `session_name` 원문 삽입. NFC/NFD가 tmux에서 별개여도 파일시스템에서는 같은 경로일 수 있다. 신형 locator 전환과 함께 NFC 정규화 + canonical ownership 충돌 확인 필요. 살아 있는 옛 파일명은 rewrite/rename하지 않는다.
- `src/services/herdr_launch.rs:342-345`: workspace label은 `logical_key#nonce`. `prepared.rs:37,58`이 create 요청에 전달. `src/services/herdr_launch/unplaced.rs:100-112`는 정확한 logical_key 또는 `logical_key#32hex`만 자기 후보로 읽는다. **표시용 label을 단순 에이전트명으로 바꾸면 orphan/adopt 판정이 깨진다.** 새 display label을 identity marker와 분리하거나 이 naming 이슈에서는 workspace ownership label을 보존해야 한다.
- `src/services/session_host/herdr/model.rs:238-243,268-293`: label/cwd는 JSON String, pane 제어는 별도 `pane_id`/`workspace_id` 기반. 이 프로토콜에는 pane name이나 rename 요청이 없다. 한글 label 실물 생성은 부모의 격리 herdr에서 검증하며 사용자 herdr는 조작하지 않는다.
- `src/services/herdr_launch.rs:350-358`: launch command는 절대 cwd/한 줄 non-control 조건. shell line의 Unicode 자체를 거부하지 않음.
- `src/services/process.rs:596-600`: POSIX `shell_escape`는 단따옴표로 감싸고 내부 `'`를 `'\''`로 치환, Unicode 바이트 보존. `src/services/claude/tui_session_launch/prepared.rs:82`가 script 경로에 적용. 외부 명령은 argv 사용, tmux target은 `src/services/tmux_common.rs:1031-1038`의 `=name` exact target 재사용.
- `dashboard/src/components/settings/SettingsProvidersModel.ts:57-58`의 attach 문자열은 quoting이 없지만 **provider login 전용 `adk-login-*` 이름**이다. worker naming 변경에 확대하지 않으며 일반 사용자 이름 shell 문자열에 재사용하지 않는다.

## 외부 스킬 이름만 검색한 목록

검색 범위: `99_Skills/agentdesk-*/`, `watch-agent-turn/`, `adk-release/`, `agentdesk-runtime-ops/` 하위 파일. `grep -Il AgentDesk- <files...>`만 사용; 내용 열람·수정 없음. 일치 26파일, `.pre-` 백업 11을 제외한 현행 파일 15. backup은 현행 지침처럼 바꾸지 않는다.

현행 15파일(루트 `/Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/`):

- `adk-release/SKILL.md`
- `agentdesk-issue-pipeline/SKILL.md`
- `agentdesk-issue-pipeline/references/execution.md`
- `agentdesk-issue-pipeline/scripts/launch-headless-leg.sh`
- `agentdesk-issue-pipeline/scripts/reclaim_tmp.py`
- `agentdesk-issue-pipeline/scripts/stop_orphan_test_procs.py`
- `agentdesk-issue-pipeline/scripts/test_stop_orphan_test_procs.py`
- `agentdesk-relay-audit/SKILL.md`
- `agentdesk-relay-e2e/SKILL.md`
- `agentdesk-restart/SKILL.md`
- `agentdesk-restart/scripts/restart_agentdesk.sh`
- `agentdesk-runtime-ops/SKILL.md`
- `agentdesk-runtime-ops/references/recovery-playbook.md`
- `agentdesk-runtime-ops/references/topology.md`
- `watch-agent-turn/SKILL.md`

백업 11파일(목록만, 변경 제외):

- `agentdesk-issue-pipeline/SKILL.md.pre-20261026-batch`
- `agentdesk-issue-pipeline/SKILL.md.pre-20261026-giant`
- `agentdesk-issue-pipeline/SKILL.md.pre-20261026-weekday`
- `agentdesk-issue-pipeline/SKILL.md.pre-20261027-anchor`
- `agentdesk-issue-pipeline/SKILL.md.pre-20261031-speed`
- `agentdesk-issue-pipeline/SKILL.md.pre-20261104-p2judge`
- `agentdesk-issue-pipeline/SKILL.md.pre-20261104-statechart-liveness`
- `agentdesk-issue-pipeline/SKILL.md.pre-20261106-pstack`
- `agentdesk-issue-pipeline/SKILL.md.pre-7415-landed`
- `agentdesk-issue-pipeline/references/execution.md.pre-20261106-pstack`
- `agentdesk-issue-pipeline/scripts/launch-headless-leg.sh.pre-20261109-symlink-owner`

## 고칠 순서

1. ownership/canonical resolver/legacy dual-read부터 착지. `AgentDesk-*` prefix를 관리 권한의 근거로 계속 쓰는 외부 스크립트는 새 writer보다 먼저 전환해야 한다.
2. 새 writer와 `cell_session_name`, destructive E2E allowlist를 같은 조각으로 갱신. 새 이름과 기존 binding-selected locator 모두 시험하고 `-e2e` 문자열만으로 kill 대상 승인하지 않는다.
3. UI의 이름→provider/channel 역파싱 제거. 구조 필드·legacy fallback·표시를 분리하고 key를 중지 API에 그대로 보내는 계약 유지.
4. `docs/session-binding.md`, 보호 규칙/실행 문서, CLI 소스 설명·API inventory 예제 및 공식 생성 문서 갱신. historical plans에는 legacy 표기만 보강.
5. 코디네이터가 현행 스킬 15파일의 내용을 별도 승인된 스킬 작업에서 점검·수정·동기화. 이 레인은 목록만 제공. 대형 운영 수동 절차 없이 CLI/metadata query를 쓰게 전환. 외부 소비자 이행 전에도 살아 있는 옛 세션은 계속 지원.

## 계획 수리/시험 제안

- 대시보드와 E2E 조각: prod 5~8파일, 순증 약 +180~300, 시험 4~7파일/+200~350. 기존 giant `tmux_common.rs`에 새 Unicode/ownership 책임 추가하지 말고 naming 도메인과 작은 registry/metadata 모듈로 분리.
- RED: plain `adk-cc`, `claude-sales`, 한국어 `개발-담당`, NFC/NFD equivalent, `<agent>-<threadid>` fixture의 target label/provider/channel 복원; legacy key도 통과해야 함. old names-only midturn guard는 새 이름을 거부하는 RED를 먼저 확보.
- mutant: UI metadata 우선순위를 legacy parser 앞에서 뒤로 되돌림; new thread를 `-t`로 생성; E2E ownership guard 제거/AgentDesk-prefix만 확인. 모두 위 fixture 또는 foreign-collision 시험이 실패해야 한다.

# 부록 D. 실물 점검

## 한글 점검 실행 기록

격리 macOS 27.3 / tmux 3.8 / Herdr 0.9.5(protocol 22)에서 실제 생성·조회했다. 이름 계산은 Rust naming.rs+safe_prefix를 Python으로 전사한 것이며 cargo 실행이 아니다. `AgentDesk-*` 기본 운영 소켓은 사용하지 않았다. 최초 시도의 set-option 대상 `=name`과 Herdr 소켓 예상 경로 실패는 `logs/unicode-probe-attempt1.log`에 보존했고, session ID/실제 named socket으로 고친 최종 원문을 아래에 싣는다. 최종 rc=1 두 건은 의도한 duplicate create 거절이며 성공으로 세지 않는다.

Herdr의 설정·XDG 데이터·소켓은 생성한 임시 디렉터리에만 둔다. 모든 tmux create/stop은 `-L i8119-probe`, Herdr는 `--session i8119-probe`와 명시 격리 환경이다. 끝의 kill-server/server stop은 이 probe가 만든 격리 서버만 대상이다. file path의 raw NFC/NFD 충돌과 ASCII digest 대안은 **실제 macOS 파일**로 확인했다. 후자는 제안 검증이며 production 구현이 아니다.

```text
ROOT=/tmp/i8119-probe-l9zvdc4m
This Python model transcribes naming.rs + safe_prefix; no cargo executed.
$ sw_vers
rc=0
stdout=ProductName:		macOS
ProductVersion:		27.3
BuildVersion:		26C5035r
stderr=
$ tmux -V
rc=0
stdout=tmux 3.8
stderr=
$ herdr --version
rc=0
stdout=herdr 0.9.5
stderr=
{"input": "가가가가가가가가가가가가가가나-cc", "input_bytes": 48, "old_name": "AgentDesk-claude-가가가가가가가가가가가가가가", "old_tail_bytes": 42}
{"input": "가가가가가가가가가가가가가가다-cc", "input_bytes": 48, "old_name": "AgentDesk-claude-가가가가가가가가가가가가가가", "old_tail_bytes": 42}
{"input": "가가가가가가가가가가가가가가가가가가가가-t1570308839910412348", "input_bytes": 81, "old_name": "AgentDesk-claude-가가가가가가가-t1570308839910412348", "old_tail_bytes": 42}
$ tmux -L i8119-probe -f /dev/null new-session -d -s '스마트홈-cc' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s '스마트홈-cc-1570308839910412348' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s 'AgentDesk-claude-가가가가가가가가가가가가가가' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s '가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s '가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가-1570308839910412348' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s '𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s '𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-1570308839910412348' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s '𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-pppppppppppppppppppppppppppppppp-11111111111111111111-aaaaaaaaaaaa-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-22222222222222222222' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s 'AgentDesk-claude-가가가가가가가가가가가가가가' /bin/cat
rc=1
stdout=
stderr=duplicate session: AgentDesk-claude-가가가가가가가가가가가가가가
$ tmux -L i8119-probe -f /dev/null list-sessions -F '#{session_id}|#{session_name}'
rc=0
stdout=$2|AgentDesk-claude-가가가가가가가가가가가가가가
$3|가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가
$4|가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가-1570308839910412348
$0|스마트홈-cc
$1|스마트홈-cc-1570308839910412348
$5|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀
$6|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-1570308839910412348
$7|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-pppppppppppppppppppppppppppppppp-11111111111111111111-aaaaaaaaaaaa-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-22222222222222222222
stderr=
$ tmux -L i8119-probe -f /dev/null set-option -t '$0' @agentdesk_identity '{"v": 1, "provider": "claude", "channel_id": "1479671298497183835", "thread_id": null}'
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null show-options -qv -t '$0' @agentdesk_identity
rc=0
stdout={"v": 1, "provider": "claude", "channel_id": "1479671298497183835", "thread_id": null}
stderr=
$ tmux -L i8119-probe -f /dev/null list-sessions -F '#{session_id}|#{session_name}|#{session_name}|#{@agentdesk_identity}'
rc=0
stdout=$2|AgentDesk-claude-가가가가가가가가가가가가가가|AgentDesk-claude-가가가가가가가가가가가가가가|
$3|가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가|가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가|
$4|가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가-1570308839910412348|가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가-1570308839910412348|
$0|스마트홈-cc|스마트홈-cc|{"v": 1, "provider": "claude", "channel_id": "1479671298497183835", "thread_id": null}
$1|스마트홈-cc-1570308839910412348|스마트홈-cc-1570308839910412348|
$5|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀|
$6|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-1570308839910412348|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-1570308839910412348|
$7|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-pppppppppppppppppppppppppppppppp-11111111111111111111-aaaaaaaaaaaa-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-22222222222222222222|𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-pppppppppppppppppppppppppppppppp-11111111111111111111-aaaaaaaaaaaa-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-22222222222222222222|
stderr=
tmux_created_name_byte_lengths=[15, 35, 59, 300, 320, 400, 420, 521]
$ tmux -L i8119-probe -f /dev/null new-session -d -s '한글-cc' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s '한글-cc' /bin/cat
rc=0
stdout=
stderr=
{"nfc_hex": "ed959ceab8802d6363", "nfd_hex": "e18492e185a1e186abe18480e185b3e186af2d6363", "samefile": true, "nfc_read": "NFD", "nfd_read": "NFD", "dir_entries": ["agentdesk-probe-한글-cc.jsonl"]}
$ /bin/sh -c 'printf '"'"'%s\n'"'"' '"'"'스마트홈-cc; touch /tmp/i8119-probe-l9zvdc4m/INJECTION_MUST_NOT_EXIST'"'"''
rc=0
stdout=스마트홈-cc; touch /tmp/i8119-probe-l9zvdc4m/INJECTION_MUST_NOT_EXIST
stderr=
shell_sentinel_exists=False
$ tmux -L i8119-probe -f /dev/null new-session -d -s adk-cc /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s adk-cc /bin/cat
rc=1
stdout=
stderr=duplicate session: adk-cc
$ tmux -L i8119-probe -f /dev/null show-options -qv -t =adk-cc @agentdesk_identity
rc=0
stdout=
stderr=
$ herdr --session i8119-probe server # env HERDR_CONFIG_PATH=/tmp/i8119-probe-l9zvdc4m/herdr.toml HERDR_SOCKET_PATH=/tmp/i8119-probe-l9zvdc4m/herdr.sock
herdr server running; you can use any herdr CLI command in another terminal.
api socket: /tmp/i8119-probe-l9zvdc4m/config/herdr/sessions/i8119-probe/herdr.sock
client socket: /tmp/i8119-probe-l9zvdc4m/config/herdr/sessions/i8119-probe/herdr-client.sock
logs: /tmp/i8119-probe-l9zvdc4m/config/herdr/sessions/i8119-probe/herdr-server.log
did you mean to open the Herdr TUI? run `herdr`; you do not need `herdr server`.

herdr_server_pid=8445 ready=True
$ herdr --session i8119-probe workspace create --cwd /tmp/i8119-probe-l9zvdc4m --label '스마트홈-cc' --no-focus
rc=0
stdout={"id":"cli:workspace:create","result":{"root_pane":{"agent_status":"unknown","cwd":"/private/tmp/i8119-probe-l9zvdc4m","focused":true,"foreground_cwd":"/private/tmp/i8119-probe-l9zvdc4m","pane_id":"w1:p1","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":40},"tab_id":"w1:t1","terminal_id":"term_6600e554d30f91","workspace_id":"w1"},"tab":{"agent_status":"unknown","focused":true,"label":"1","number":1,"pane_count":1,"tab_id":"w1:t1","workspace_id":"w1"},"type":"workspace_created","workspace":{"active_tab_id":"w1:t1","agent_status":"unknown","focused":true,"label":"스마트홈-cc","number":1,"pane_count":1,"tab_count":1,"workspace_id":"w1"}}}
stderr=
$ herdr --session i8119-probe pane rename w1:p1 '스마트홈-cc-1570308839910412348'
rc=0
stdout={"id":"cli:pane:rename","result":{"pane":{"agent_status":"unknown","cwd":"/private/tmp/i8119-probe-l9zvdc4m","focused":true,"foreground_cwd":"/private/tmp/i8119-probe-l9zvdc4m","label":"스마트홈-cc-1570308839910412348","pane_id":"w1:p1","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":40},"tab_id":"w1:t1","terminal_id":"term_6600e554d30f91","workspace_id":"w1"},"type":"pane_info"}}
stderr=
$ herdr --session i8119-probe workspace list
rc=0
stdout={"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"active_tab_id":"w1:t1","agent_status":"unknown","focused":true,"label":"스마트홈-cc","number":1,"pane_count":1,"tab_count":1,"workspace_id":"w1"}]}}
stderr=
$ herdr --session i8119-probe pane get w1:p1
rc=0
stdout={"id":"cli:pane:get","result":{"pane":{"agent_status":"unknown","cwd":"/private/tmp/i8119-probe-l9zvdc4m","focused":true,"foreground_cwd":"/private/tmp/i8119-probe-l9zvdc4m","label":"스마트홈-cc-1570308839910412348","pane_id":"w1:p1","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":40},"tab_id":"w1:t1","terminal_id":"term_6600e554d30f91","workspace_id":"w1"},"type":"pane_info"}}
stderr=
$ herdr --session i8119-probe workspace create --cwd /tmp/i8119-probe-l9zvdc4m --label '𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-pppppppppppppppppppppppppppppppp-11111111111111111111-aaaaaaaaaaaa-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-22222222222222222222' --no-focus
rc=0
stdout={"id":"cli:workspace:create","result":{"root_pane":{"agent_status":"unknown","cwd":"/private/tmp/i8119-probe-l9zvdc4m","focused":false,"foreground_cwd":"/private/tmp/i8119-probe-l9zvdc4m","pane_id":"w2:p1","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":40},"tab_id":"w2:t1","terminal_id":"term_6600e554dc0b32","workspace_id":"w2"},"tab":{"agent_status":"unknown","focused":false,"label":"1","number":1,"pane_count":1,"tab_id":"w2:t1","workspace_id":"w2"},"type":"workspace_created","workspace":{"active_tab_id":"w2:t1","agent_status":"unknown","focused":false,"label":"𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-pppppppppppppppppppppppppppppppp-11111111111111111111-aaaaaaaaaaaa-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-22222222222222222222","number":2,"pane_count":1,"tab_count":1,"workspace_id":"w2"}}}
stderr=
$ herdr --session i8119-probe api snapshot
rc=0
stdout={"id":"cli:api:snapshot","result":{"snapshot":{"agents":[],"focused_pane_id":"w1:p1","focused_tab_id":"w1:t1","focused_workspace_id":"w1","layouts":[{"area":{"height":40,"width":120,"x":0,"y":0},"focused_pane_id":"w1:p1","panes":[{"focused":true,"pane_id":"w1:p1","rect":{"height":40,"width":120,"x":0,"y":0}}],"splits":[],"tab_id":"w1:t1","workspace_id":"w1","zoomed":false},{"area":{"height":40,"width":120,"x":0,"y":0},"focused_pane_id":"w2:p1","panes":[{"focused":true,"pane_id":"w2:p1","rect":{"height":40,"width":120,"x":0,"y":0}}],"splits":[],"tab_id":"w2:t1","workspace_id":"w2","zoomed":false}],"panes":[{"agent_status":"unknown","cwd":"/private/tmp/i8119-probe-l9zvdc4m","focused":true,"foreground_cwd":"/private/tmp/i8119-probe-l9zvdc4m","label":"스마트홈-cc-1570308839910412348","pane_id":"w1:p1","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":40},"tab_id":"w1:t1","terminal_id":"term_6600e554d30f91","workspace_id":"w1"},{"agent_status":"unknown","cwd":"/private/tmp/i8119-probe-l9zvdc4m","focused":false,"foreground_cwd":"/private/tmp/i8119-probe-l9zvdc4m","pane_id":"w2:p1","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":40},"tab_id":"w2:t1","terminal_id":"term_6600e554dc0b32","workspace_id":"w2"}],"protocol":22,"tabs":[{"agent_status":"unknown","focused":true,"label":"1","number":1,"pane_count":1,"tab_id":"w1:t1","workspace_id":"w1"},{"agent_status":"unknown","focused":false,"label":"1","number":1,"pane_count":1,"tab_id":"w2:t1","workspace_id":"w2"}],"version":"0.9.5","workspaces":[{"active_tab_id":"w1:t1","agent_status":"unknown","focused":true,"label":"스마트홈-cc","number":1,"pane_count":1,"tab_count":1,"workspace_id":"w1"},{"active_tab_id":"w2:t1","agent_status":"unknown","focused":false,"label":"𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀𐐀-pppppppppppppppppppppppppppppppp-11111111111111111111-aaaaaaaaaaaa-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-22222222222222222222","number":2,"pane_count":1,"tab_count":1,"workspace_id":"w2"}]},"type":"session_snapshot"}}
stderr=
$ tmux -L i8119-probe -f /dev/null kill-server
rc=0
stdout=
stderr=
$ herdr --session i8119-probe server stop
rc=0
stdout=
stderr=
herdr_server_exit=0
END ROOT=/tmp/i8119-probe-l9zvdc4m
```

추가 충돌·파일명·인용 실물:

```text
ROOT=/tmp/i8119-extra-523tru17
$ tmux -L i8119-probe -f /dev/null new-session -d -s '가가가가가가가가가가가가가가나-cc' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -s '가가가가가가가가가가가가가가다-cc' /bin/cat
rc=0
stdout=
stderr=
$ tmux -L i8119-probe list-sessions -F '#{session_id}|#{session_name}'
rc=0
stdout=$0|가가가가가가가가가가가가가가나-cc
$1|가가가가가가가가가가가가가가다-cc
stderr=
{"hypothesis": "raw UTF8 digest basename, not production implementation", "names": ["adk-v2-93db954d908ad6ee73601ff94ff75a0d4e5e53f1cfbf92cdcc1b773b2459e798.claude-tui-settings.json", "adk-v2-96d89741a675938857ec53e5036f05eee5e5e19905bd8fbc5626abf7001e5c08.claude-tui-settings.json"], "basename_bytes": [96, 96], "samefile": false, "roundtrip": ["한글-cc", "한글-cc"]}
$ /bin/sh -c 'printf '"'"'%s\n'"'"' '"'"'한글 '"'"'\'"'"''"'"'경로'"'"'\'"'"''"'"'; $(touch /tmp/i8119-extra-523tru17/UNEXPECTED) `touch /tmp/i8119-extra-523tru17/UNEXPECTED`'"'"''
rc=0
stdout=한글 '경로'; $(touch /tmp/i8119-extra-523tru17/UNEXPECTED) `touch /tmp/i8119-extra-523tru17/UNEXPECTED`
stderr=
shell_sentinel_exists=False
$ tmux -L i8119-probe kill-server
rc=0
stdout=
stderr=
```

대시보드는 전체 app 빌드 대신 **실제 `discord-routing.ts`를 Node stripTypeScriptTypes로 로드한 격리 브라우저 fixture**에서 확인했다. 127.0.0.1 임의 포트의 JSON 응답을 fetch하고 실제 helper가 반환한 한글 제목을 DOM에 표시했다. production API/DB 호출은 없다. `parsed:null`은 현재 parser가 새 이름을 읽지 못한다는 실패 근거다. 아래는 출력 원문이다.

```text
⚠ Chrome 157.0.8103.51 is behind the current stable release 157.0.8103.78, which fixes 83 CVEs (CVE-2026-126840, CVE-2026-126841, CVE-2026-126842, +80 more). Update the browser; for agent-browser's Chrome, run `agent-browser install`.
✓ i8119 Unicode display probe
  http://127.0.0.1:55622/
[agent-browser] launched browser
true
- heading "격리된 이름 표시 점검" [level=1, ref=e1]
- main
  - paragraph
    - StaticText "{\"name\":\"스마트홈-cc\",\"title\":\"#스마트홈-cc\",\"parsed\":null,\"bytes\":15}"
  - paragraph
    - StaticText "{\"name\":\"스마트홈-cc-1570308839910412348\",\"title\":\"#스마트홈-cc-1570308839910412348\",\"parsed\":null,\"bytes\":35}"
  - paragraph
    - StaticText "{\"name\":\"가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가\",\"title\":\"#가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가가\",\"parsed\":null,\"bytes\":300}"
✓ Browser closed
```

재현 스크립트: `logs/unicode-probe.py`, `logs/unicode-extra.py`, `logs/ui-probe.mjs`. 실행 중인 probe·browser·HTTP 서버는 없다. 원시 출력은 logs 아래 보존했다.

# 부록 E. artifact gateway 전수와 크기 보정

# #8119 ASCII artifact layout — checked gateway 전수 보강

기준 `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`. `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/artifact-design.md` 및 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/design-core.md`를 읽고 소스만 조사했다. 코드 변경·cargo·운영 조회 없음.

## 결론 / 크기 정정

`session_temp_path` 문자열 검색은 **314파일/704행**이다(주석·시험·`resolve_session_temp_path` 포함). 이것이 수정할 prod 파일 수는 아니다. 실제 책임을 기준으로 겹치지 않게 묶으면 **checked mutation/authority gateway 32개 기존 prod 파일**, 그 밖에 **결정성 reader/복원 19개 기존 prod 파일**을 확인했다. 즉 v2 활성화 전에 확인해야 할 명시적 적응 목록은 51파일이며, 작은 신규 layout 모듈·부모 배선은 별도다. S7의 기존 "10~15파일" 또는 B의 "12~20파일/+350~600" 일괄 추정은 사용하면 안 된다. 함수 전수 signature 이사는 필요 없지만 30파일 한 조각에도 들어가지 않는다.

원시 근거:

- `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/artifact-gateway-grep.log`: `git grep -n 'session_temp_path' -- 'src/**/*.rs'` (314파일/704행).
- `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/artifact-indirect-grep.log`: `git grep -n -E 'tmux_owner_path|tmux_channel_path|write_tmux_owner_marker|write_tmux_channel_binding|AuthMarkers::|PreparedIncarnation::|CodexTuiSessionFiles::|session_files\(\)|RuntimeHintFence::|TmuxPublisherObservation::|write_tmux_session_auth_profile|write_claude_session_auth|record_tmux_host_marker|cleanup_session_temp_files|clear_tmux_exit_reason|record_tmux_exit_reason' -- 'src/**/*.rs'`.
- 아래 표는 파일명 기반 시험 필터만으로 계수하지 않고 해당 함수의 production cfg와 효과를 읽어서 고정했다. `claude.rs`처럼 시험 module 사이에 prod 함수가 있는 giant를 통째로 시험으로 제외하지 않았다. `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/rust-prod.py`는 **탐색 보조 스크립트**이며 raw 문자열/cfg 전용 함수 때문에 자동 분류 오차가 있으므로 그 출력은 판정 증거로 사용하지 않는다.

## G: checked mutation / authority gateway — 32파일

좌표의 `src/services/`는 공통 접두어다. "추가 checked"는 기존 guard의 대체가 아니라 **그 guard가 사용할 artifact layout을 먼저 확인**한다는 뜻이다. Unknown/Conflict는 0 filesystem mutation·0 spawn·0 kill이며 기존 nonce/lease/source/CAS를 모두 보존한다.

| ID | 기존 prod 파일:좌표 | 현재 효과 / 필요한 layout 적응 |
|---|---|---|
| G01 | `tmux_common.rs:126,136,1113,1134,1167,1222,1278,1283,1295,1332,1908` | source authority lock key가 `session_temp_prefix`이며 formatter·canonical/tmp lookup·cleanup·owner/channel write·JSONL ownership 분류를 소유. layout을 authority handle에 고정하고 known path-set만 formatter/cleanup에 전달. String-only 함수가 manifest 오류를 legacy path로 숨기면 안 됨. giant에는 delegate만. |
| G02 | `tmux_common/runtime_hint_fence.rs:239,263,291,316,345,379` | publication snapshot / sync·async·try fence가 **prefix를 직접** 계산하고 canonical/tmp 두 marker/lock inode를 열어 잠금. layout handle에서 path-set을 얻고 capture 전 확정. v2에는 legacy path를 넣지 않음. |
| G03 | `tmux_common/session_markers.rs:9,24,37,58,70,86,99,146,157` | auth-profile/session-auth/runtime marker read·write·restore. missing을 default profile로 해석하는 branch 앞에 checked layout. 알 수 없는 layout을 `None`으로 넘기면 account mismatch kill/recreate가 확대됨. |
| G04 | `tmux_common/host_marker.rs:12,26` | host-kind marker read/write, 기존 write-failure warn 후 launch 지속. layout Unknown을 host-kind absent로 압축하지 않음. |
| G05 | `tmux_diagnostics.rs:7,137,141,182` | exit_reason write/atomic rename/remove/read. cleanup 외 post-turn direct writer이므로 layout gate에 포함. 실패를 성공/normal exit로 바꾸지 않음. |
| G06 | `discord/tmux_session_files.rs:222,296,316,326,367` | generation inode replacement·spawn nonce write/remove/read. marker failure arm이 **기존 destination 삭제**를 하므로 path 계산 전에 layout 검증 필수. |
| G07 | `tui_prompt_dedupe/binding_context.rs:98,300` | spawn nonce canonical/tmp 동시 판독 + PreparedIncarnation prepare/create/validate(immutable context 생성). 새 manifest enrollment를 prepare 전에 수행, v2에는 legacy nonce fallback 없음. context 파일 자체는 이미 nonce ASCII path여서 이사하지 않음. |
| G08 | `tui_prompt_dedupe/runtime_binding.rs:147,189,199,256` | binding publication과 channel marker 저장. Unknown layout을 binding absent로 오인하지 않게 기존 authority가 checked layout을 함께 보유. |
| G09 | `tui_prompt_dedupe/runtime_binding/binding_access.rs:55,71,83,150` | current context/nonce/rollout marker를 대조해 실제 binding consumer 권한 결정. marker path가 다른 layout이면 consumer_allowed 판정 오염; checked authority가 정한 동일 layout 사용. |
| G10 | `claude.rs:2407,2436,2449,2610,2634,2650,2714` | legacy wrapper의 usability→old kill/cleanup→JSONL/FIFO/prompt/owner/script write→spawn. layout gate는 auth/usability/경로 캐시 **이전**. output/input만 새 경로로 바꾸고 owner/script를 남기면 안 됨. |
| G11 | `claude/tui_session_launch.rs:12,43,56,62,103,112,137,154,194` | TUI admission·auth read·cleanup·owner write와 failure cleanup, Herdr script rewrite. capture 앞에서 layout 확정; created artifact path-set을 cleanup이 그대로 보유. |
| G12 | `claude/tui_session_launch/prepared.rs:18,37,59,67,78` | prepared context→host marker→settings/script 파일 생성, script rewrite. G11의 handle 재사용하고 재선택하지 않음. |
| G13 | `claude_tui/session.rs:23,75,90,95,155,201,297,326,399` | settings/script 생성·cleanup + continuation시 두 artifact atomic replace. launch만 gate해도 warm continuation writer가 남으므로 여기도 path-set/checked layout을 보유. |
| G14 | `codex.rs:568,704,1390,1430,1448,1459,1530,1534,1558,1574,1618` | Codex wrapper relay/binding·input mode 읽기·usability→cleanup→FIFO/prompt/owner/script→spawn. layout check는 profile/input-mode/usability 전에. |
| G15 | `codex/tui_session_launch.rs:30,67,109,169,222` | preflight와 prepared incarnation 생성·first_input publisher 위임. layout admission을 preflight 시작에 연결; 아래 first_input으로 동일 handle 전달. `:217` fence 코드는 cfg(test) helper이므로 prod 근거로 세지 않음. |
| G16 | `codex/tui_session_launch/first_input.rs:72,226,232,234` | first-input 준비와 auth/owner/script write. preflight의 선택을 재사용, 실패 cleanup도 같은 path-set. |
| G17 | `codex/tui_session_launch/launch_policy.rs:77,199,217` | TUI incarnation/context 및 Herdr launch script를 logical key의 artifact로 생성. Herdr name이 tmux name과 다른 것까지 exact-name input으로 명시. |
| G18 | `codex_tui/session.rs:25,64,74,102,125,190,210,271,331,554` | canonical/legacy home·fingerprint·snapshot path-set, cleanup, rollout marker publication. v2는 legacy paths를 Option/typed path-set에서 제거; sentinel 금지. |
| G19 | `tui_prompt_dedupe/runtime_binding/codex_cursor.rs:32,91,218,310,355,394` | marker migrate/restore/persist/advance, NamedTempFile→marker persist. authority layout으로 marker와 relay 경로를 같이 판독. |
| G20 | `tui_prompt_dedupe/runtime_binding/codex_verified.rs:193,466` | rollout marker 판독으로 **proof/publication/consumer 권한** 결정. read-only 파일 I/O지만 mutation admission을 좌우하므로 G에 넣음. Unknown은 marker absent 아님. |
| G21 | `tui_prompt_dedupe/runtime_binding/codex_policy/clear_relaunch.rs:127,206,222` | existing marker/launch context·auth 확인 뒤 clear/relaunch 허용. layout mismatch를 "unowned marker missing"으로 읽지 않음. |
| G22 | `tui_prompt_dedupe/runtime_binding/codex_policy/hold_detail.rs:69` | hold 이유를 판정하는 marker read. Unknown과 no marker를 분리해야 사용자에게 restart 허용 상태로 잘못 보이지 않음. |
| G23 | `qwen/session_lifecycle.rs:25,36,45,51,79,100,111,132,140` | wrapper usability·기존 kill/cleanup·owner/output/FIFO/script. path 계산/None→stale 판단 전에 checked layout. main의 기존 stale cleanup 조건 자체는 확대/재설계하지 않음. |
| G24 | `session_backend.rs:173,178,198` | process fallback이 `SessionConfig` output을 create/append하고 unused FIFO path를 wrapper에 전달. config가 checked layout path-set에서 왔는지 admission. tmux provider만 막으면 process write 우회가 남음. |
| G25 | `session_backend/auth_profiles.rs:10,20` | profile mismatch이면 기존 process terminate; 새 process auth marker 실패면 새 process terminate. layout Unknown이 default profile false로 흘러 기존 kill을 유발하면 안 됨. |
| G26 | `herdr_launch.rs:393,729,755,771` | Pending owner/immutable context 뒤 host_kind artifact 직접 write. pane/workspace ID와 독립인 logical artifact layout을 Pending 전에 확정. |
| G27 | `herdr_launch/runtime_marker.rs:11,24,31,44,66,105` | fresh/retired runtime marker cleanup; canonical/tmp path 2종 직접 구성 및 nonce 확인 후 unlink. v2에는 legacy path를 절대 추가하지 않음. |
| G28 | `discord/recovery_engine/producer_resume.rs:51,61,129,139,214,252,363` | saved relay path 비교, OpenOptions append로 wrapper를 다시 열고 producer를 재개. `prepare`에서 checked layout 고정; Unknown을 NotApplicable/새 bootstrap 경로로 낮추지 않음. |
| G29 | `discord/recovery_engine/rebind_runtime/codex_spawn.rs:3,19,21,36` | normalized relay path 계산, generation preparation이 truncate/open을 결정하고 binding 설치. typed layout이 없으면 prepare generation 전 거부. |
| G30 | `discord/recovery_engine/manual_rebind/mod.rs:510,512,516` | saved normalized relay 부재면 formatter path를 고르고 truncate 플래그 설정. Unknown→None이면 빈 spool 생성/원문 손실로 이어질 수 있어 새 checked result 필요. |
| G31 | `discord/tmux_watcher/post_stream_exit.rs:234,241,253,260,280` | teardown 뒤 post-mortem `death_pane_log` 직접 write 및 exit_reason/cleanup. best-effort 진단 writer도 foreign/unknown path에 쓰면 안 되므로 path-set에 포함. |
| G32 | `discord/turn_bridge/tmux_runtime.rs:824` | output/FIFO path tuple을 만들고 stop/input 경로가 소비. Result/typed path-set으로 unknown을 I/O 앞에서 거부. |

### R: 결정성 read / 복원 소비자 — G와 겹치지 않는 19파일

이들은 String formatter delegation만으로 알려진 layout의 경로는 얻을 수 있어도, **알 수 없음이 absent로 바뀌지 않는지** v2 활성화 전에 호출부 계약을 적응/검증해야 한다. 조회 실패 후 다른 source/adoption/idle 판정으로 갈 수 있으므로 단순 display reader로 제외하지 않는다.

| ID | 기존 prod 파일:좌표 (`src/services/` 기준) | 확인할 판정 |
|---|---|---|
| R01 | `cluster/session_matcher.rs:433` | expected rollout를 새 path-set으로 계산; legacy live면 old output 유지 |
| R02 | `codex_tui/session/checkpoint.rs:85,94` | generation/nonce missing fallback에 Unknown을 섞지 않음 |
| R03 | `discord/commands/control/relaunch.rs:171` | 기존 launch script의 fast-mode 증거 읽기 |
| R04 | `discord/delivery_lease_cell/source_epoch_observer.rs:75` | observer source witness; Unknown이면 unavailable, fresh/zero epoch 아님 |
| R05 | `discord/recovery_engine/manual_rebind/codex_tui_replay.rs:20,71,137,246,310` | raw/native와 normalized relay 구분 및 replay suppression |
| R06 | `discord/recovery_engine/manual_rebind_output_path.rs:211` | wrapper mtime와 transcript fresher 비교 |
| R07 | `discord/recovery_engine/rebind_runtime.rs:393` | saved output이 normalized relay인지 비교 |
| R08 | `discord/recovery_engine/terminal_watcher.rs:151` | restart report watcher baseline path |
| R09 | `discord/router/message_handler/provider_isolation.rs:364,381` | input artifact 존재로 runtime kind 분류 |
| R10 | `discord/tui_prompt_relay/claude_source.rs:142` | legacy source 증거 판독; unknown layout에서 default/adopt 금지 |
| R11 | `discord/tui_prompt_relay/launch_script.rs:40` | launch script 파싱/rehydration transcript 선택 |
| R12 | `discord/tui_prompt_relay/rehydration/codex_marker.rs:138,308` | relay checkpoint 및 launch mtime 기준 |
| R13 | `discord/watchers/codex_tui_restore.rs:372` | binding restore의 relay path |
| R14 | `discord/watchers/lifecycle/claude_restore.rs:77` | launch-marker mtime fallback transcript 탐색 |
| R15 | `discord/watchers/lifecycle/restore.rs:310,407` | output/FIFO restore; old actual layout 유지 |
| R16 | `discord/zombie_foreground_release.rs:203` | output 없음을 idle 증거로 오인하지 않음 |
| R17 | `dispatched_sessions.rs:502,564` | activity mtime·provider output evidence; unknown을 inactive로 해석하지 않음 |
| R18 | `stale_turn_reconciler.rs:744` | independent readiness/liveness 복원 |
| R19 | `tui_input/human_frame/history/append.rs:28` | human-frame incarnation generation/nonce 증거 |

그 밖의 간접 cleanup caller(`provider_teardown`, reaper, commands, force-kill, lifecycle/liveness 등)는 G01/G03/G05/G06의 checked mutation gate를 그대로 호출하되 **호출 전 kill 허용 판정**은 이름/metadata 소비자 조각 S3~S5에서 보강한다. gateway가 거부한 후에도 상위가 kill하는 오류는 low-level path API만으로 막을 수 없다. 해당 소비자 조각과 합친 negative 계약에서 0 artifact write **및** 0 kill을 동시에 단언해야 한다.

## 작은 조각으로 나누기

순증은 아직 구현 전 보수적 추정이며 **매 조각 30파일/+1500 및 PLAN +1200** 적용. 아래는 S7 전체의 하위 조각이다. 중복 파일은 직렬로 소유하고 parent giant는 코드 추출/delegate만 한다. 단계별로 기존 legacy lifetime도 checked API를 실제 호출하므로 dormant helper만 쌓지 않는다. 새로운 v2 allocator는 마지막 활성화 때 연결한다.

| 조각 | 기존 gateway / reader + 신규·배선 파일 추정 | prod 순증 추정 | RED / 뮤턴트 |
|---|---:|---:|---|
| S7a codec + common authority | G01~G09 9파일 + 신규 codec/manifest/path-set/배선 4~6 = **13~15파일** | +650~900 | legacy 모든 ext/path byte-identical, Unknown/Conflict의 callback 0회, manifest create 경쟁·read-back. 뮤턴트 Unknown→Legacy, authority/fence 경로 분리 |
| S7b Claude·Qwen·process | G10~G13·G23~G25 7 + R03/R10/R11/R14 4 + child 추출 2~4 = **13~15파일** | +450~650 | existing live FD·continuation/settings/script 동일 inode/permission, Unknown 시 old process kill0. 뮤턴트 layout gate를 usability 뒤로 이동 |
| S7c Codex launch·cursor | G14~G22 9 + R02 1 + child 추출 2~4 = **12~14파일** | +500~750 | v2 legacy Option 없음, home/fingerprint/snapshot/rollout/checkpoint 같은 layout, unknown held/clear/relaunch0. 뮤턴트 v2 cleanup에 legacy path 추가, corrupt marker를 absent 처리 |
| S7d Herdr·relay 복구·마무리 | G26~G32 7 + R01/R04~R09/R12/R13/R15~R19 14 = **21파일**, child/배선 2~4 포함 **23~25파일** | +650~950 | raw/normalized replay·restore·post-mortem·manual rebind 0 mutation, old saved path 유지. 뮤턴트 Unknown→NotApplicable, missing path에서 truncate 재시작 |
| S7e v2 allocator 연결·실물 | 기존 naming/launch admission 소유 5~8 + shared allocator 배선 1~2 = **6~10파일** | +150~300 | 모든 host의 새 이름/V2 ASCII basename, 한글 100자·NFC/NFD distinct tmux name·NAME_MAX·foreign collision, restart/followup/cleanup. 뮤턴트 allocator를 enrollment read-back 이전 artifact write 뒤로 이동 |

S7d도 +1200 예상에 닿으면 Herdr2+post-mortem와 Codex relay-recovery를 나눈다. "51파일을 한 번에 서명 변경" 또는 "함수 한 개를 바꿨으므로 모든 caller가 안전" 어느 쪽도 승인하지 않는다. 마지막 v2 writer 활성화는 **G/R 표 전부의 계약이 수렴하고 S3~S5의 ownership/kill gate가 연결된 뒤**다.

## String API 보존시 반드시 고정할 계약

- `session_temp_path(&str, ext)->String`을 남긴다는 말은 manifest Unknown을 삼키는 API를 정당화하지 않는다. 신규 mutation 경로는 checked `ArtifactLayout/ArtifactPaths` handle을 받아 **I/O 전 fallible admission**을 거친다. 기존 String 함수는 legacy formatter 또는 이미 검증한 layout handle의 formatter에 제한한다. 임의 이름을 넣어 on-demand manifest lookup 후 fallback하는 공용 함수로 남기지 않는다.
- global name→layout cache만으로 권한을 얻지 않는다. runtime/host/exact-name/owner가 붙은 검증 handle을 source authority/lifetime이 보유하고, 읽지 못한 manifest를 cache miss=legacy로 해석하지 않는다. old name의 NFC/NFD bytes나 key를 normalize하지 않는다.
- layout을 판단하지 못한 read-only helper가 `None`을 반환하더라도, 이를 받는 G/R gateway가 absence로 해석하지 않는 typed 상태가 있어야 한다. 이 방식의 구체 API/함수별 결과 타입은 S7a 계획에서 확정하고, 본 표 밖 production direct writer가 나오면 활성화 전에 목록에 추가한다.

# 부록 F. 이름 생성·재구성·비채널 hint 전수

# #8119 이름 writer·비채널 hint 추가 전수표

기준 `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`. 사용자 댓글 `issuecomment-6816190818`에 따라 **에이전트 이름=채널 이름**을 확정한다. 이 표는 **실제 Guild 채널 이름**과 이름을 대신하여 끼워 넣는 DM/routine/scheduled hint를 구분한다. 실제 길드·운영 세션 조회 없이 소스만 읽었다.

원시 `git grep -n build_tmux_session_name -- src dashboard policies`는 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/writer-grep.log`: 312행/153파일. 명시 cfg(test)·시험 경로와 부모 cfg(test) 아래 relay_e2e/fixture를 제외하고 **prod 호출 58곳 / 43파일**(중앙 정의 `domain/provider/naming.rs:15` 제외)을 아래 전수표에 적었다. 이 가운데 대부분은 새 세션 생성뿐 아니라 복구·탐색·정리용 **이름 재구성**이므로 쓰기만 새 이름으로 바꾸면 안 된다.

## 1. basis 소유 kind와 충돌

| kind | producer와 현재 basis | 같은 basis 가능 여부·현재 보호 | 새 생성 경로 |
|---|---|---|---|
| Guild channel | `session_runtime/channel_routing.rs:51,264`: Guild(channel).name. `adk_session.rs:84` 또는 `router/message_handler.rs:248`에서 provider builder 호출 | 같은 문자열이 다른 channel ID에 쓰이지 않는다는 전역 보장은 코드에 없다. 같은 길드/다른 카테고리/다른 길드는 모두 별개 channel ID여도 basis가 같을 수 있다. 같은 provider면 main도 충돌; provider가 다르면 기존 prefix가 분리 | 실제 채널 이름 `adk-cc` 기본. provider/역할 접두어 추가하지 않음. 충돌은 관리 identity로 구분하고 신규 충돌자에만 안정 suffix(아래 절) |
| Thread | `session_runtime/channel_routing.rs:3` parent 이름 + `-t<threadID>`, `session_runtime.rs:522` bootstrap | parent 이름은 같아도 thread ID가 다르면 분리. parent ID fallback(`session_runtime.rs:521`)은 채널명이 아님 | 실제 parent 이름+`-<threadID>`. thread ID 별도 인자. parent 이름 unavailable이면 저장 locator/관리 metadata 우선, 임의 이름을 실제 parent 이름으로 위장하지 않음 |
| DM | `health/headless_turn.rs:176` 직접 start, :239 reserved start, `routines/agent_executor/launch.rs:157` owner input intent 모두 `dm-<userID>` | **같은 사용자를 대상으로 Claude/Codex가 같은 basis**. 현재 AgentDesk-<provider>-가 분리한다. provider prefix만 삭제하면 `dm-123`을 공유한다. DM channel은 bot/user pair별 실제 ID가 있고 producer :174/:216에서 얻는다 | **이 경로만** synthetic `dm-<provider>-<DMchannelID>` 유지. user 기반 `dm-<userID>` hint는 workspace/role routing용으로 그대로 두고 physical label input에서만 분리. marker에는 kind=dm/provider/channelID 명시. 실제 DM channelID를 못 얻은 상태는 새 세션을 띄우지 않음(기존 locator는 읽음) |
| DM routine(tmux) | `agent_executor/launch.rs:151-152`는 `(dm_user_id_num.is_none() || process)`일 때만 routine label; DM+tmux는 **None**. :157 dm hint가 canonical DM 세션 선택 | routine임에도 DM tmux는 현재 의도적으로 canonical DM 세션 공유. `health/headless_turn.rs:255-262` 주석은 S2/S3 전 synthetic label 활성화 금지 | DM rule 그대로. 이번에 per-routine DM tmux 분리 활성화하지 않음. process 경우만 아래 attempt rule. main의 이 수명 계약을 이름 정리와 함께 바꾸지 않음 |
| Fresh/Persistent routine(tmux) | `routines/agent_executor.rs:1198` routine_agent_session_name = `routine <routine.name> - <agentID>` compact+90문자. `agent_executor/execution.rs:50-59` Tmux label. launch.rs:151 및 `headless_turn/routine_metadata.rs:83` 사용 | 이름만 다른 routine이 44byte prefix에서 충돌 가능. 같은 routine/agent의 provider 변경은 현 provider prefix가 분리. 서로 다른 routineID가 같은 display name도 가능(이 함수에 ID 입력조차 없음). fresh라도 per-attempt tmux 이름은 아님 | **이 경로만** kind/provider와 routineID+agentID를 유지하는 synthetic label. 예 `routine-<provider>-<routineID>-<agentID>`; 길이 한계 시 사람이 읽는 부분만 줄이고 식별 suffix는 전체 tuple의 고정 digest로 보존. immutable routineID/agentID/provider를 metadata로 전달. fresh/persistent의 기존 재사용·정리 타이밍은 그대로 |
| Routine(process) | `agent_executor/execution.rs:60-65` `routine-process <turnID 마지막 : segment> <routine label>` | 현재 turn/attempt를 앞에 넣어 44byte 절단에도 서로 다른 retry가 분리됨. provider prefix가 없으면 동일 문자열에서 provider 충돌 가능 | tmux를 새로 만들지 않음. 논리 key/binding 이름에 kind/provider/attemptID를 유지. `routine-process-<provider>-<attemptID>`+필요하면 scope digest; tmux 이름 규칙을 process identity 전체에 일괄 적용하지 않음 |
| Scheduled snapshot | `scheduled_messages/context_snapshot.rs:227-228` `scheduled:<definitionID>` → :290 label → `scheduled_messages.rs:386` metadata → `headless_turn/routine_metadata.rs:26,39` key override | definition ID가 label 기준이며 같은 정의의 매 fire는 같은 label을 재사용하고 headless_turn.rs:550 이하가 native session continuity를 끊음. provider prefix는 현재 분리. 새 이름을 단순 실제 채널 이름으로 돌리면 live channel row를 덮을 수 있음 | **이 경로만** `scheduled-<provider>-<definitionID>` 유지; kind=scheduled_snapshot 명시. definition scope를 그대로 유지하고 per-fire pane 수명 정책은 바꾸지 않음. 실제 channel 이름과 절대 같은 key로 생성하지 않음 |
| Recovery fallback | `agent_recovery/handoff.rs:39` fallback_provider + binding.channel_id(숫자)를 builder에 전달 | 원래 owner/fallback provider가 달라도 같은 raw channel ID. 현재 provider prefix 분리. role 이름은 기반이 아님 | **이 경로만** `recovery-<provider>-<channelID>` 또는 명시 fallback identity scope; original channel routing은 그대로. 저장된 actual locator는 우선하여 live rename/추가 kill 없음 |
| Name unavailable/numeric binding fallback | `cluster/session_matcher.rs:156,170` tmux_segment 없으면 channel_id; `:426` helper도 param 이름은 channel_id이나 raw name/id 모두 수용. 등록 fallback hint도 raw string | ID/alias를 실제 이름인 것처럼 처리하면 실제 숫자형 채널명과 혼동. 같은 raw ID를 provider가 함께 쓸 수 있음 | 실제 채널 name을 기존 directory/Discord projection에서 먼저 얻어 Channel typed input으로 전달. 얻지 못하면 existing locator만 탐색. 새 synthetic 필요 경로는 kind=fallback/provider/channelID 구분자를 유지하며 실제 채널명 함수에 넘기지 않음 |

`channel_routing.rs:51`은 Guild만 이름을 반환하고 비Guild는 None, :264도 Guild만 Some이다. 따라서 DM의 `dm-*`는 실제 Discord 채널 이름으로 분류할 근거가 없다. “channel_name” 필드가 string이라는 이유로 모든 caller를 새 규칙으로 돌리면 위 synthetic ID가 섞인다.

## 2. 종류를 받는 생성 계약

하나의 `build_tmux_session_name(&str)`를 prefix만 제거하도록 바꾸지 않는다. 새 입력은 `Channel{display_name,channel_id}`, `Thread{parent_display_name,thread_id}`, `Dm{provider,dm_channel_id}`, `Routine{provider,routine_id,agent_id}`, `RoutineProcess{provider,attempt_id,...}`, `ScheduledSnapshot{provider,definition_id}`, `RecoveryFallback{provider,channel_id}`처럼 scope를 명시한다. 실제 길드 채널만 채널명 자체를 사용하고, 나머지는 해당 kind에서만 provider/ID 구분자를 유지한다. synthetic label 이름으로 kind를 다시 추측하지 않으며 marker/session_info에 같은 kind와 원 ID를 저장한다.

기존 복구 caller는 저장 actual locator → canonical identity/alias → marker → 구형 이름 재구성 순서로 읽는다. 이름 생성은 신규 할당에만 사용하고 live tmux/Herdr label과 binding/pane IDs를 재생성하지 않는다. 같은 kind가 같은 identity인지는 provider/토큰scope/channel-or-routine ID/host 경계로 비교하며 display string 동등성으로 재사용하지 않는다.

## 3. 동명 채널: 같은 길드·다른 카테고리·다른 길드

- **같은 길드 내 다른 channel ID**, **같은 이름을 가진 다른 카테고리**, **다른 길드에서 같은 이름** 모두 실제 channel identity는 다르므로 같은 프로세스/host의 tmux 이름 충돌로 처리한다. category명은 display 조직일 뿐 identity에 넣지 않는다. guild ID만 넣는 것도 같은 길드 내 동명을 해결하지 못하므로 최종 구분자는 channel ID다.
- 기본 이름은 채널명 그대로. bare 이름이 이미 **다른 managed identity 또는 사용자 unmanaged 세션**에 쓰이면 그 이름은 claim/attach/send/rename/kill하지 않고 신규 세션을 `<채널명>-<channelID>`에 할당한다. 이 fallback마저 타인의 세션이면 본문 명명 계약의 provider/scope/nonce 후보 순서를 따른다. 전부 점유되면 생성만 실패시킨다. 기존 살아 있는 세션 이름은 바꾸지 않는다.
- 같은 채널이 여러 provider/runtime token에서 동시에 쓰이면 channel ID suffix만으로 부족하다. provider/token/host scope가 다른 충돌자에는 digest를 추가한다. 실제 일반 채널 기본 이름에 provider를 항상 붙이지 않으며 **충돌 경로에만** 구분한다.
- 중복 여부를 실시간 목록의 한 번 조회에만 기대지 않는다. `new-session`의 atomic 중복 거절 뒤 기존 세션 metadata를 다시 읽고 자기 identity면 재획득, 정상 외부 소유면 다음 후보를 시도하고, 조회 실패·모순인 unknown이면 보존한다. 우리 row/inflight/binding이 없고 정상 미표식 사용자 세션으로 확인한 경우는 외부 점유다. 현재 채널별 lease만으로 다른 채널의 같은 이름 동시 생성은 막히지 않는다.
- main `cluster/session_matcher.rs:170-190`은 같은 generated 이름에 서로 다른 binding이 들어오면 SessionNameCollision으로 거절한다. 이 기존 보호는 유지하고, 새 directory는 display name에 다중 identity가 매달려도 실제 할당 locator별 매핑이 유일해야 한다. “채널명은 유일”이라는 기존 주석(:24-26)을 사실로 가정하지 않는다.

## 4. build_tmux_session_name 모든 prod 호출

C=실제 채널/합성 thread/DM hint를 담을 수 있는 저장 문자열; R=routine; S=scheduled snapshot; F=fallback. C caller도 새 typed identity+저장 locator lookup으로 옮겨야 하며, 표의 '재구성'은 새 세션 생성 권한이 아니다.

| file:line | 현재 입력·용도 | 신규 경로 |
|---|---|---|
| `src/services/agent_recovery/handoff.rs:39` | F: binding.channel_id 숫자로 fallback spawn plan 이름 생성 | fallback provider/channelID 경로 |
| `src/services/cluster/session_matcher.rs:233` | C: parent segment에서 thread의 parent locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/cluster/session_matcher.rs:426` | C: public expected_session_name helper: 이름/숫자 raw basis | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/adk_session.rs:84` | C/R/S: override가 있으면 synthetic basis, 없으면 channel basis로 sessions key 생성 | 명시 kind/ID를 producer부터 전달; raw hint 분류 금지 |
| `src/services/discord/admin_host_guard.rs:165` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/admin_host_guard.rs:205` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/admin_host_guard.rs:320` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/control.rs:205` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/control.rs:537` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/control/managed_reset.rs:27` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/control/provider_reset.rs:34` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/control/relaunch.rs:71` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/diagnostics/reports.rs:209` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/diagnostics/reports.rs:314` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/tui_passthrough.rs:225` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/commands/tui_passthrough/herdr_settings.rs:37` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/health/recovery.rs:264` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/health/recovery/channel_runtime.rs:284` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/health/recovery/runtime_lookup.rs:130` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/health/turn_deliver/inject.rs:283` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/host_defer_gate.rs:71` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/host_teardown_gate.rs:105` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/host_teardown_gate.rs:302` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:265` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/recovery_engine/manual_rebind/watcher_claim.rs:74` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/recovery_engine/restore_inflight.rs:307` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/recovery_engine/restore_inflight.rs:938` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/recovery_engine/state_extractors.rs:143` | C/DM/R/S: stored tmux name 또는 channel_name에서 restart target 재구성 | 명시 kind/ID를 producer부터 전달; raw hint 분류 금지 |
| `src/services/discord/router/message_handler.rs:248` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/router/message_handler/headless_turn.rs:571` | C/DM: late owner read에 channel pane와 synthetic pane가 같은지 판정 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/router/message_handler/headless_turn/entrypoints.rs:124` | C/DM: 생성된 pane가 channel canonical pane와 같은지 own 판정 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/router/message_handler/headless_turn/routine_metadata.rs:83` | C/DM/R/S: label 우선, 없으면 channel_name으로 headless physical locator 생성 | 명시 kind/ID를 producer부터 전달; raw hint 분류 금지 |
| `src/services/discord/router/message_handler/tui_followup/nameless_gate.rs:93` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/session_idle_cleanup.rs:88` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/session_runtime.rs:359` | C: DB cwd와 live tmux pane cwd 재조정 대상 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/session_runtime/restore_cwd.rs:162` | C: DB cwd/JSONL selector의 channel-scoped key 또는 correction target 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/session_runtime/restore_cwd.rs:199` | C: DB cwd/JSONL selector의 channel-scoped key 또는 correction target 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/session_runtime/restore_cwd.rs:260` | C: DB cwd/JSONL selector의 channel-scoped key 또는 correction target 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/tmux_reaper.rs:227` | C: channel_name으로 owner 대조/idle-or-delete 상태 갱신 key 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/tmux_reaper.rs:420` | C: channel_name으로 owner 대조/idle-or-delete 상태 갱신 key 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/tmux_reaper.rs:493` | C: channel_name으로 owner 대조/idle-or-delete 상태 갱신 key 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/tmux_restart_handoff.rs:206` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:545` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/turn_bridge/tmux_runtime/owned_input_stop.rs:90` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/turn_presence/activity/unbound.rs:91` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/turn_presence/activity/unbound.rs:104` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/turn_presence/activity/unbound/completed_scheduled.rs:163` | S: scheduled:<definitionID>와 receipt.name 일치 검사 | scheduled definition ID/provider 경로, receipt key 우선 |
| `src/services/discord/turn_presence/supervisor.rs:919` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/watchers/lifecycle/restore.rs:64` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/watchers/lifecycle/restore.rs:111` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/watchers/lifecycle/restore.rs:818` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/watchers/lifecycle/restore_core.rs:26` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/watchers/lifecycle/restore_mapping.rs:36` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/discord/watchers/lifecycle/restore_support.rs:199` | C: core/등록 channel_name에서 조회·guard·복구용 locator 재구성 | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/routines/agent_executor/reliability.rs:245` | R: routine_name/agentID로 fresh provider liveness probe 대상 재구성 | 저장 actual locator 우선; routine ID/provider/agentID 경로 |
| `src/services/routines/fresh_session_reaper.rs:69` | R: routine_name/agentID로 fresh routine 정리 대상 후보 재구성 | 저장 actual locator 우선; routine ID/provider/agentID 경로 |
| `src/services/routines/session_control.rs:433` | C: 저장 session_key 없을 때 primary-name 또는 parent-tID fallback | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/routines/session_control.rs:904` | R: fresh teardown의 routine_name/agentID fallback locator | 저장 actual locator 우선; routine ID/provider/agentID 경로 |

중앙 정의는 `src/domain/provider/naming.rs:15`. 간접 소비자 `src/cli/run.rs:772`, `cluster/session_matcher.rs:156,170`은 expected_session_name_for→`:426`을 호출한다. parent cfg(test) 아래 `relay_e2e/registered_bootstrap_e2e.rs:108,335`, `relay_e2e/thread_guard_host_e2e.rs:195`, `watchers/lifecycle/watcher_install_fixture.rs:55`는 prod 호출 수에서 뺐다.

## 5. RED 계약·뮤턴트 보강

1. DM user가 같고 provider가 다른 두 typed 입력을 동시에 생성하면 서로 다른 locator·binding·sessions row. provider를 제거한 뮤턴트는 RED. dm workspace hint는 `dm-<userID>`로 그대로 전달되는 별도 단언.
2. 같은 label·서로 다른 routineID, 같은 routineID·서로 다른 agentID/provider, 이름이 44byte prefix에서 같아지는 routine을 분리. ID/digest를 제거한 뮤턴트 RED. DM+tmux routine의 synthetic label None은 그대로라 main과 같은 canonical DM 세션을 쓴다는 회귀 계약.
3. routine process retry turnID가 다르면 논리 key 분리; tmux 생성 호출 0. attempt 분리 제거 뮤턴트 RED.
4. scheduled snapshot은 live channel과 다른 key, 같은 definition의 현재 reuse scope와 native fresh reset 유지. kind를 Channel로 넘긴 뮤턴트 RED.
5. 같은 길드/다른 category/다른 guild의 동명 channel ID 각각 신규 충돌 fallback. bare 사용자 세션 PID·pane·option 변화 0, existing managed old/new locator 이름 변경 0. metadata provider/channel 불일치에도 attach하지 않음.
6. 동일 이름 동시 생성 두 채널 경합에서 하나의 세션을 공유하지 않음. atomic create 실패를 자기 managed 세션으로 간주한 뮤턴트 RED.
7. thread `is_synthetic_thread_channel_name`는 `session_runtime/channel_routing.rs:11`에서 `ends_with(-t<id>)`를 읽고 :22에서 restore 보존을 결정한다. 명시 thread ID 우선+legacy suffix read, parent 이름 미조회/숫자 fallback 회귀를 별도로 고정.
