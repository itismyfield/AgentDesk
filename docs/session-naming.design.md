<!-- S1 delta: session environment marker supersedes r2 option/created-session cleanup; historical probe transcripts below are unchanged. -->
# 세션 이름 단순화 설계 r2 — #8119

개정일 2026-11-12 KST. 소스 조사 기준 `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`, d1 설계 `8bcaeb4909225e2adef440fdc72050ccbda64d5f`. r2는 리뷰 `review-d1.out.md`와 코디네이터의 권고안 A 채택 지시를 반영한다. 변경은 문서·TLA+ 모델뿐이며 cargo·Rust 시험·운영 설정/DB/store·운영 세션은 건드리지 않는다. 최종 설계 리뷰 2라운드의 입력이고, 남는 지적은 구현 시험·수용 기준으로 넘긴다.

## 목표와 확정 사항

[#8119](https://github.com/itismyfield/AgentDesk/issues/8119)의 내용·DoD와 [사용자 결정](https://github.com/itismyfield/AgentDesk/issues/8119#issuecomment-6816190818)을 따른다. 에이전트 이름은 **채널 이름**, 스레드는 **`<채널 이름>-<스레드ID>`**다. r2 시작 때 외부 iMessage 폴백을 지적한 두 번째 댓글도 읽었다(`logs/issue-r2-start.txt`).

| 수용 조건 | 증거와 구현 계약 |
|---|---|
| 의미를 이름에서 분리 | 부록 A의 provider/channel/thread/관리 판독 전수와 기존 row/inflight/binding/host 증거. legacy decoder는 옛 형식 호환에만 사용. |
| 살아 있는 세션 유지 | live old 이름·primary·inflight·path·nonce는 불변. 자연 재생성의 guarded 등록 transaction에서만 새 primary로 승격하고 old key를 alias로 보존. 부록 B의 26 경계 전수. |
| 한글·충돌·경로 실물 | 부록 D의 d1 격리 실측과 r2 pane-target 정정. 운영 조건의 장문·옵션 read-back은 **미확인**이며 S8c 활성화 시험으로 남김. |
| 외부 세션 무조작 | S1 platform 마지막 관문, 명시 관리 표식, 원자적 새 이름 예약·생성, foreign 후보는 자동 대체. |
| 외부 의존 이행 | 부록 C의 16파일 preflight와 imessage-send 3파일 계약. 외부 스킬 수정은 코디네이터의 별도 승인 작업이며 이 레인은 수정하지 않음. |
| 독립 리뷰·결정 | 채널명은 확정. 표시·충돌 기본값 3건은 아래 확인 요청에 명시. r2 정식 리뷰 이후 추가 설계 라운드를 만들지 않음. |

main에 이미 있는 canonical identity/alias·channel ID 기반 inflight 파일명·immutable binding context·명시 host locator·owner/channel sidecar를 재사용한다. #7583의 전체 host 추상화, 새 운영자 스위치, 새 registry DB, 일괄 live rename은 범위 밖이다.

## 이름 규칙

입력 `SessionNameInput`은 kind, display label, provider/token namespace, channel ID, optional parent channel ID/thread ID를 별도로 받는다. builder는 문자열에서 host나 thread를 추정하지 않는다.

1. label을 NFC로 정규화하고 Unicode category `L`/`N` 및 `-`, `_`를 보존한다. 그 밖의 문자는 `-`로 치환한다. 빈/구분자뿐인 label, 또는 **`AgentDesk-`로 시작하는 label은 `channel-<channel_id>`**로 대체한다. 신규 namespace에 legacy prefix가 들어오지 않게 한다. Rust `is_alphanumeric`과 L/N 집합이 같다고 가정하지 않으며 U+0345·NFC 뒤 잔여 combining mark를 경계 시험에 둔다. 기존 이름·키·경로에는 정규화를 소급하지 않는다.
2. 실제 길드 채널 기본 이름은 label 전체이며 `^[\p{L}\p{N}_-]{1,100}$`, 100 scalar/최대 400 UTF-8 byte다. 새 이름의 44byte 절단은 없앤다. `.`, `:`, `/`, `#`, 공백·제어문자는 출력에 없다. `claude-sales`나 숫자로 끝나는 일반 채널도 provider/thread 의미로 읽지 않는다.
3. thread는 부모 채널 label + `-` + 명시 `thread_id`다. ID는 1..=u64::MAX의 10진수(1~20자리), 전체 최대 121 scalar/421byte다. thread 제목을 대신 쓰거나 끝 숫자를 역파싱하지 않는다.
4. 같은/다른 길드·카테고리의 동명 채널은 channel ID가 다르면 다른 identity다. **충돌 대체 형식의 정본**은 순서대로 `<agent>-<channel_id>`, `<agent>-<provider>-<channel_id>-<scope12>`, 마지막으로 `-<spawn_nonce32>`를 더한 후보다. thread는 이 agent 대체 이름 뒤에 `-<thread_id>`를 붙인다. scope12는 runtime-root/host namespace SHA256 앞 12hex이고, 실제 owner tuple 대조를 생략하는 유일성 증거가 아니다. 최대 521byte다. 본문과 부록 모두 이 순서만 사용한다.
5. 정상 foreign owner가 후보를 선점하면 그 세션·파일·표식에 손대지 않고 다음 후보를 선택한다. 자기 identity면 저장 actual locator 재획득으로 수렴한다. 모두 점유됐으면 retryable 생성 실패다. 전송 불명·조회 실패·owner 충돌·alias divergence를 absence로 읽어 후보를 연속 생성하지 않는다. 기존 세션을 선호 이름으로 rename하지 않는다.

비채널 hint만 provider 구분자를 유지한다. DM은 `dm-<provider>-<DMchannelID>`, scheduled는 `scheduled-<provider>-<definitionID>`, recovery는 `recovery-<provider>-<channelID>`다. DM 라우팅 hint `dm-<userID>`는 유지한다. **routine은 main의 읽기 쉬운 이름+agent 순서를 보존한 `routine-<provider>-<routine_name>-<agent_id>`를 기본값**으로 하며 routineID를 표시 이름에 새로 넣지 않는다. main의 compact/90자 basis와 fresh/persistent 재사용, DM+tmux routine의 canonical DM 공유, process attempt 분리, scheduled 정의별 재사용/native continuity reset을 유지한다. 같은 provider·agent의 동명 routineID 충돌은 기존 결함 후보로 남기고 이번에 ID 이름 정책으로 넓혀 고치지 않는다. 부록 F는 producer/재구성 58호출·43파일을 담는다.

## 관리 표식·마지막 관문·생성 실패

tmux에는 compact JSON **한 개 `AGENTDESK_IDENTITY` 세션 환경 변수**를 `new-session -e AGENTDESK_IDENTITY=<json>`으로 생성과 동시에 저장한다. owner_scope 해시·채널 ID는 비밀이 아니다. 예시는 fixture이며 운영 값이 아니다. layout 선택은 이 JSON에 의존하지 않는다.

```json
{"v":1,"owner_scope":"<sha256(runtime-root|execution-node)>","provider":"claude","token_namespace":"<existing token_hash>","channel_id":"1479671298497183835","parent_channel_id":null,"thread_id":null,"identity_kind":"discord_channel","name_kind":"channel","session_record_id":null,"logical_key":"<current primary key>","spawn_nonce":"<32hex>","display_name":"스마트홈-cc"}
```

DB `identity_kind`는 현 `discord_channel`/`scheduled_snapshot`을 유지한다(`db/dispatched_sessions/canonical_identity.rs:4-27`). `name_kind`는 이름 입력/metadata의 분류이며 DB enum 확장이 아니다. row ID는 기존 launch/hook 순서상 optional이고, 기존 launch admission·binding context/nonce로 최초 소유를 입증한다.

- 관리 판정은 현재 owner scope/provider/token/channel과 실행 nonce/실제 host 증거가 일치해야 한다. legacy는 기존 `.owner`·`.channel`·row/inflight/binding의 일치 증거로 읽고, prefix만으로 새 권한을 주지 않는다. legacy 호환을 빌미로 cleanup/TTL/lease/dispatch 보호를 넓히지 않는다.
- S1 `owner_scope`는 length-framed domain tag·canonical runtime root 문자열·기존 `platform::shell::hostname_short()`를 SHA256으로 해시한다. 설정 snapshot 설치 전후에 범위가 달라지지 않으며 기존 runtime-root/host namespace를 따른다. 이 해시는 권한 증거의 범위 식별자이며 비밀이 아니다.
- **S1 마지막 관문**: `services/platform/tmux.rs`의 kill 내부 공통 경로(`:370,390`, 공개 wrapper `:420-442`)와 send/paste(`:462,470,484,511`)는 이름이 `AgentDesk-`로 시작하지 않으면 `AGENTDESK_IDENTITY.owner_scope == current_runtime_scope`일 때만 명령을 보낸다. marker 없음/손상/조회 실패/다른 scope는 Refused이고 tmux mutation 0이다. 상위의 더 강한 nonce/host guard도 유지한다. legacy prefix를 통과시키는 부분은 main 범위를 유지하는 호환 예외이며 foreign 관리 권한의 새 근거가 아니다.
- 생성과 표식 부착은 `new-session -e` 한 명령이다. 중복 이름은 tmux가 원자적으로 거부하며 기존 세션에 표식을 쓰지 않는다. create의 실패·전송 불명은 기존 session presence/Unknown 계약을 유지한다. 별도 set-option/read-back 실패 상태나 자기 생성 kill 우회 helper는 만들지 않는다.
- 표식은 `show-environment -t '=name' AGENTDESK_IDENTITY`의 성공 출력에서 정확한 `AGENTDESK_IDENTITY=값`을 한 번 분리해 JSON으로 읽는다. 값 안의 `=`는 보존한다. 누락·손상·미지원 schema·조회 실패 및 `set-environment -u` 뒤 `-AGENTDESK_IDENTITY` 출력은 Unknown이며 조작을 거부한다. pane-target은 기존 `=name:`을 유지한다.
- 로그인 전용 `adk-login-*`도 같은 생성 시점 표식을 사용한다. 직접 tmux를 호출하는 busy-inject 및 input actor Pane은 owner_scope 확인 checked 생성자(Result)를 통과해야 한다. legacy prefix 호환과 기존 nonce/host 확인을 유지한다.
- Herdr는 현 HostedOwner/nonce/ServerWitness/pane ID를 권위로 쓴다. 기존 `logical_key#nonce` workspace marker와 ACK-gap 복구를 유지하며 읽기 쉬운 표시 이름으로 rename하지 않는다. 이는 사용자 확인 요청 기본값 (a)이다. pane/API display와 physical pane target을 구별한다.

## 권고안 A: 자연 재생성에서 primary 승격

**main 불변식 “row primary key의 tmux 꼬리 = 현재 물리 이름”을 유지한다.** live old가 있으면 그 이름·키·파일·inflight·nonce를 그대로 쓴다. 새 이름 요청은 live rename/승격 권한이 아니며 old가 부재로 확인되고 기존 turn/dispatch/pending이 정리된 자연 재생성에만 새 이름을 할당한다.

등록은 `canonical_identity.rs::upsert_hook_session_pg`의 update_target/preserve_alias 경계에서 같은 transaction으로 수행한다. 구조는 다음과 같다.

1. 기존 primary를 비권한 snapshot으로 확인한다. **old/new locator lock을 같은 정렬 순서로 먼저 잡고 identity lock, row FOR UPDATE**를 잡는다. 현 순서(`:216-219` locator→identity)를 뒤집어 identity 보유 중 old locator를 추가 획득하면 옛 hook와 deadlock할 수 있으므로 그렇게 하지 않는다. snapshot이 바뀌면 transaction을 다시 시작해 lock set을 재계산한다.
2. old host 부재 witness를 재검증하고 provider/token/channel/row ID·기존 actor pin/상태를 CAS로 확인한다. probe 실패, live old, 다른 nonce의 active turn/held inflight는 승격 거부다. row 실행 nonce는 **비었거나 새 실행 nonce**여야 한다. main이 terminal 뒤 old nonce를 남기는 경우에는, 검증된 fresh-registration만 기존 actor pin/old-absence witness 아래 새 nonce를 설치한 뒤 같은 transaction에서 이 조건을 만족시킨다. 일반 alias hook가 nonce를 먼저 덮어써 승격 조건을 만드는 것은 금지한다.
3. 새 key의 primary/alias 소유 충돌을 확인하고 `sessions.session_key`를 새 key로 승격한다. 그 **뒤에** old key를 alias로 보존한다. 반대 순서는 현 preserve_alias의 “같은 row의 primary면 return” 조기 반환(`:806` 부근) 때문에 old alias가 빠질 수 있다. 기존 새-key alias는 같은 row일 때만 소비/정리하며 foreign 충돌은 transaction 전체 rollback이다. 이 정리는 명시 key 승격 범위뿐이며 별도 세션 삭제 권한이 아니다.
4. `HookSessionUpsertOutcome.session_key`도 새 primary를 반환한다(현 `:272`는 target.session_key 반환). old key alias로 뒤늦게 오는 hook는 같은 row를 찾되 **update 전에 nonce/actor CAS로 거절**하며 primary를 되돌리거나 새 실행 metadata를 덮지 못한다.

PG RED는 primary=new / old alias=same row / outcome.session_key=new의 세 동시 단언, late old-nonce hook 거부, old live·probe error·alias foreign·row nonce 경합에서 승격 0이다. 승격 제거·old alias 보존 순서 역전·nonce CAS 제거 뮤턴트는 각각 RED여야 한다. queue 취소·dashboard 중지·policy kill은 **명령 key 전달이 아니라 실제 새 이름 세션이 사라지는 결과**를 시험한다. hosted thread GC는 새 primary의 live probe가 보호하는지 시험한다.

부록 B의 26파일 전수에서 DB primary를 소비하는 경로는 A로 자동 해결된다. 요청으로 받은 **old alias를 row 해석 전에 직접 파싱**하는 force_kill/tmux_cleanup/session_resume 등은 S3에서 기존 alias resolver로 current primary를 먼저 정규화한다. 26곳 전체를 effective descriptor API로 바꾸거나 새 session_descriptor JSON merge/CAS 기계를 두지 않는다. 기존 generation·inflight equality/hash·binding nonce·watcher fence는 느슨하게 하지 않는다.

## BindingContext·scheduled 후보·외부 보호 계약

S6이 `tui_prompt_dedupe/binding_context.rs:21-37`의 schema에 다음 optional 필드를 추가한다. old JSON은 None, 새 None은 생략하므로 기존 reader와 호환된다.

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub parent_channel_id: Option<u64>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub thread_id: Option<u64>,
```

thread context의 `channel_id`는 actual thread ID, `parent_channel_id`는 실제 부모 채널 ID, `thread_id`는 thread ID로 producer에서 전달한다. 이름 꼬리로 채우지 않는다. 일반 채널은 parent/thread None이다.

### #8118 경계

**부모 채널 필드는 #8119 S6이 정본이고 #8118은 소비자**다. #8118은 이를 host/endpoint/출력 경로 상속에 사용하며 이 레인은 그 배치 정책을 구현하지 않는다.

`imsg_send.py::caller_target()`는 새/Herdr context에서도 provider와 `parent_channel_id.or(channel_id)`로 부모 binding을 찾게 해야 한다. thread인데 parent가 없으면 숫자 꼬리를 새로 추측하지 않고 기존 legacy 폴백 또는 명시 실패로 간다. 스킬 시험은 새 이름+thread context→부모 채널, parent 읽기 제거→`{}`/잘못된 target으로 RED다. skill 수정/시험 실행은 별도 승인된 코디네이터 작업이다. 이 레인에서는 script만 읽고 iMessage나 운영 config를 실행하지 않는다.

`canonical_identity.rs:609,646`의 Rust 승격 후보 SQL은 구조화 `session_info`의 optional metadata `name_kind <> 'channel'` row를 제외한다. 이 분류 필드는 typed producer가 기록하며 parse 불가도 승격 후보에서 제외한다. 신규 nonlegacy row에서 name_kind/identity가 빠졌으면 ordinary로 추정하지 않는다. metadata 없는 옛 row에만 기존 legacy ordinary classifier를 제한적으로 허용한다. migration 0101은 수정하지 않는다. identity 없이 들어온 scheduled hook가 canonical 채널로 승격되지 않는 PG RED를 S6에 둔다. `name_kind`는 분류용 기존 metadata에 싣되 primary/current locator를 대체하는 descriptor 저장 기계를 만들지 않는다.

외부 보호 판정식의 정본은 **`AGENTDESK_BINDING_CONTEXT` 설정됨 ∨ `AGENTDESK_IDENTITY` 설정됨 ∨ 이름이 `AgentDesk-`로 시작**이다. 이는 보수적 “작업 세션이니 보호” 판정이며 I/O/kill 권한 판정이 아니다. caller env 또는 조사 대상 session의 표식에 적용한다. 조회 실패면 보호 집합이 불완전하므로 파괴적 작업을 보류한다. 별도 새 운영자 CLI는 필요 없고 기존 env/tmux 조회를 사용한다. S8c PR에는 부록 C **16파일**의 잔여 prefix 판정과 이행/legacy 보존/무관 처분 preflight를 첨부한다. restart self-protection·live cwd 보호·imsg parent context의 이행 확인이 S8c 선행이다.

## 파일 경로: class의 순수 함수

### 대안

**채택: pure formatter + allocation-only reservation.** old `AgentDesk-*` 입력은 기존 공식을 byte-identical하게 유지하고 새 namespace만 raw-byte SHA256 basename을 쓴다. reader가 manifest를 읽지 않아 String API에 새로운 Unknown이 생기지 않는다.
**비채택: d1 manifest-selected layout + 51 gateway checked handles.** 파일 판독 오류가 모든 read/cleanup 의미로 전파되어 수정 표면과 누락 위험이 크다.
legacy 길이 cap은 두지 않는다. 정상 supported/u64 출력의 66byte(옛 -dev 포함 70)는 시험 경계일 뿐이다. 기존 builder는 긴 숫자 -t 꼬리를 통째 보존하고 Unsupported provider 길이도 무제한이므로 cap을 분류에 쓰면 live 경로가 바뀐다.
신규 builder가 `AgentDesk-` label을 `channel-<id>`로 회피하므로 prefix-only legacy class가 분리된다. 공식 변경 때는 class/version의 하위 호환을 별도 설계하며 이번 두 공식을 고정한다. 코드 스파이크·추가 대안 라운드는 하지 않는다.

`ArtifactPrefix(runtime,host,exact_name)`은 **외부 I/O 없는 total 함수**다.

```text
exact_name.starts_with("AgentDesk-") => existing session_temp_prefix formula
otherwise => "adk-v2-" + SHA256(length-framed "adk-artifact-v2", runtime, host, exact UTF-8 bytes)
```

name의 NFC/NFD를 hash 단계에서 합치지 않는다. `session_temp_path`, `legacy_tmp_session_path`, RuntimeHintFence.paths와 source authority가 **같은 함수**를 쓴다. v2 canonical/tmp basename은 모두 hash이므로 v2 cleanup이 raw Unicode/legacy basename으로 빠지지 않는다. legacy persistent→tmp read 순서는 그대로다. longest extension 포함 NAME_MAX≤255와 ASCII만임을 시험한다. 임의 사용자가 가져온 nonprefix raw artifact의 소급 호환을 약속하지 않으며, main 제공 managed 생성이 쓰는 `AgentDesk-*`와 foreign 무조작이 호환 범위다.

manifest는 **새 할당 예약만** 담당한다. artifact 준비 전에 create-new로 runtime/host/name/owner를 예약하고 read-back한다. 정상 다른 owner면 CandidateOccupied→다음 후보, 손상/미지원/읽기 실패면 allocation만 보류한다. formatter/read/restore/cleanup은 manifest를 보지 않는다. 기존 live에는 enrollment나 layout manifest를 새로 요구하지 않는다. 동명 다른 채널의 선점 경합·manifest만 남은 foreign 예약은 overwrite 없이 다음 후보로 간다. scope/owner가 같은 재시도는 기존 admission/nonce를 재검증한다.

P2-6의 `codex_tui/execution_end/capture.rs:40`, `confirm.rs:102`, `discord/health/o_transition/codex_context.rs:236`은 기존 tmux_owner_path가 이 total formatter를 사용하므로 새 layout-Unknown 적응 대상에서 제외한다. 기존 marker의 NotFound/auth/owner 의미는 변경하지 않는다. d1의 G32/R19 개별 layout gateway와 S7a~d의 4조각은 폐기한다. source authority·fence가 서로 다른 prefix를 만들지 않는 시험은 유지한다. 부록 E에 정확한 수정 표면과 삭제 사유를 적는다.

## 전환 순서와 rollback

1. S1 마지막 관문/marker, S2 atomic primary 승격·old alias/12 candidate resolver를 먼저 넣고 writer는 old를 유지한다. S3~S6·S8b의 의미 판독·raw alias 입력 경계를 옮긴다.
2. S7 pure formatter를 legacy 입력에서 같은 bytes로 운영 호출하고 allocation 예약을 준비한다. 새 names의 formatter 시험은 격리 fixture로 고정한다. #7583의 ProcessBackend 제거·Windows Herdr·Core/trait 확장은 하지 않는다.
3. S6 BindingContext 부모 필드와 Rust scheduled 후보 guard를 넣고, 코디네이터가 외부 보호/iMessage skill 계약을 별도 수정·시험한다. policy/UI reader S9와 E2E 보호를 writer 이전에 준비한다.
4. S8c에서 **신규 생성만** 신형으로 전환한다. live old는 계속 old, old가 끝난 자연 재생성은 A transaction으로 new primary+old alias다. old/new를 map에 중복 등록하거나 live 파일/바인딩을 rename하지 않는다.
5. binary rollback 대상은 **reader 조각(S1~S9)을 포함한 호환 하한 이상**이어야 한다. S8c 착지 기록에 그 실제 하한 SHA를 남긴다. writer만 old로 되돌릴 수 있어도 d1 이전 바이너리로 내려가면 stored new name을 다시 AgentDesk-*로 생성하는 버그가 있으므로 허용하지 않는다. old reader 제거·일괄 DB backfill·live kill은 이번에 하지 않는다.

## 실측 범위와 활성화 시험

| 항목 | 현재 증거 | 남은 활성화 계약 |
|---|---|---|
| 한글·44byte 절단 충돌·NFC/NFD 파일 충돌 | d1 격리 tmux/temp directory 원문(부록 D) | 새 Rust builder/formatter 대상 RED→GREEN |
| `=name:` option read-back / session_created | r2 격리 tmux에서 ASCII·한글 모두 성공. `=name`은 둘 다 실패(`logs/r2-tmux-target.log`) | 운영 launch argv/환경과 같은 조건 재확인 |
| 44byte 초과 이름(최대521) | `-f /dev/null`, 격리 소켓, /bin/cat에서 성공 | **운영 조건 미확인** |
| @agentdesk_identity 옵션 read-back | 격리 fixture JSON에서 성공 | **운영 조건 미확인**, marker 실패 주입과 자기 생성 정리 |
| UI/API UTF-8 표시 | 실제 helper를 로드한 browser/HTTP fixture | 전체 app/production API 미실시, 구현 수용 시험 |

S8c 활성화 시험은 **`env -i` + 승인된 launchd.env 로더 + 실제 provider launch argv + 기본 tmux 소켓/사용자 tmux.conf** 조건에서 실행한다. 이 레인에서는 `launchd.env`를 읽지 않고 기본 소켓을 만들거나 변경하지 않는다. 환경 값/비밀은 출력하지 않으며 외부 운영 수용 권한이 있는 코디네이터가 수행한다. `list-sessions -F`는 **탭 구분자**로 session ID/session_created/name/표식을 읽는다. d1 원시 로그의 `|`는 역사 기록으로 보존하며 신규 계약/명령에 재사용하지 않는다.

## 상태 모델

기존 `TmuxSession.tla`·`HerdrSession.tla`의 NamingInit/NamingNext와 SessionNaming projection은 설계 ref에서 작성했고 S2에서 저장소에 이식했다. 기존 Init/Next·cfg는 그대로 유지한다. 새 fixed cfg는 기존 lifecycle 변수를 고정한 **등록 transaction의 bounded projection**이며 기존 세션/턴/입력 전이를 전부 함께 검증한 것은 아니다. tmux는 primary 꼬리=현재 physical name, Herdr는 primary=current logical name + host target=pane ID 불변식을 구별한다. 결과·mutant 반례·명령·한계는 부록 G 및 `logs/r2-statechart/`에 있다. PG/Rust/실제 재시작 결합 검증은 S2 수용 시험으로 남는다.

## 구현 조각과 크기

각 조각은 prod 30파일/순증 +1500 이내, **PLAN +1200에서 정지**한다. 추정은 아래 범위이며 실제 `pr_cap_prod.py <base> <head>`로 보정한다. 모델·문서·시험은 prod 추정에서 제외한다. giant에는 기존 코드 추출/작은 delegate만 넣고 새 책임은 child로 둔다. baseline/allowlist/#[allow]를 늘리지 않는다.

| 조각 / 선행 | prod 파일·순증 추정 | 파일·책임 | 수정 전 RED / 뮤턴트 |
|---|---:|---|---|
| S1 metadata·platform gate | 10~14 / +650~950 | 새 `domain/session_identity/managed.rs`, `services/session_identity_metadata.rs`, `services/platform/tmux/session_identity.rs`; platform/tmux.rs·tmux_common/session_markers·binding_context·session_host 배선. 마지막 send/paste/kill guard·원자적 -e 표식·로그인 producer·Pane checked 생성 | 미표식 adk-cc send/kill 거부·pane/PID 동일; 로그인 재시도 성공; 파싱/Unknown 거부 / guard·Pane 확인·로그인 표식 제거 |
| S2 primary 승격·resolver / S1 | 18~22 / +500~800 | 새 `db/dispatched_sessions/key_promotion.rs`, `discord/session_locator.rs`; canonical_identity·host_key_derivation·adk_session·12 candidate 소비자와 cluster 매칭. primary/outcome/old alias atomic 등록 | PG primary=new/oldalias=same/outcome=new, late oldnonce 거부·lock 경합 / 승격 제거·alias순서 역전·nonce CAS 제거 |
| S3 provider·raw alias / S1,S2 | 23~27 / +300~500 | 부록 A provider 소비자와 부록 B raw-alias 경계. force_kill/tmux_cleanup/session_resume·정책/reconcile은 current primary resolve 후 tail 사용 | queue 취소·policy kill·dashboard 중지 후 새 세션 없음, hosted GC live 보호 / oldalias를 직접 parse·provider default |
| S4 watcher·복원 / S2,S3 | 13~17 / +350~550 | recovery_engine·tmux_watcher·watchers/lifecycle·claude_idle_runtime 기존 표. state_extractors 저장name 우선 | live old/path/nonce/offset 불변·newprimary 복원 / 저장actual 대신 builder·name CAS 완화 |
| S5 cleanup·inventory / S1~S4 | 13~17 / +450~700 | cli utils/doctor, reaper/host_guard·kill_policy·census·unbound·worktree_orphan_sweep, 새 wrapper_identity child. 기존 수명 모델 증거 연결 | foreign/fakelegacy/unknown에서 추가kill0·불완전cwd집합 삭제0 / prefix만 승인·wrapper filename 역추정 |
| S6 parent/thread/kind / S2~S5 | 21~25 / +400~650 | binding_context.rs + producer/시험 fixture, channel_routing·thread_gc·hook/listing·turn_bridge·routine/session_control·canonical_identity SQL 두 후보 지점 | thread context 부모/ID 왕복·None 생략·identity없는 scheduled 승격0 / parent 읽기 제거·name_kind 제외 제거·숫자suffix 확장 |
| S7a pure paths / S1,S2 | 3~5 / +120~220 | 새 `services/tmux_common/artifact_paths.rs`; tmux_common prefix 추출/delegate, runtime_hint_fence 공통 prefix 배선·Codex 후보 dedup. G/R51 개별 이관 없음 | legacy short/long/unsupported raw bytes 동일, v2 canonical/tmp/fence 같은hash·NAME_MAX·NFC/NFD분리 / raw name 삽입·legacy length cap·fence만 old |
| S7b allocation reservation / S7a | 5~8 / +250~450 | 새 reservation child + shared launch admission·process/Herdr/tmux 할당 배선. manifest reader는 allocator만 | 동명 channel 경합·foreign manifest만 남음·read failure nooverwrite / 예약을 artifact 뒤로 이동·foreign overwrite |
| S8a typed producer / S1~S6 | 13~17 / +300~500 | provider/session_name·adk_session·headless/routine_metadata·channel_routing·routine launch/execution·scheduled/recovery. routine 이름+agent 유지 | DM crossprovider 분리·routine provider보존·DM tmux 공유·process attempt·scheduled scope 유지 / kind=channel·provider 제거 |
| S8b1 제어 lookup / S2,S8a | 18~22 / +250~450 | 부록 F admin_host_guard/commands/health/host gate 재구성 caller. S2/S3 중복은 재구현하지 않음 | old live 재사용·newprimary stop 실제 효과 / generator 우선 |
| S8b2 복원 lookup / S2,S4,S8a | 19~24 / +250~450 | 부록 F recovery/session_runtime/reaper/watcher/routine 소비자 | old/new 자연재생성·receipt·dispatch 보호 / old primary 고정 |
| S9 policy/UI/docs reader / S2,S6 | 7~11 / +150~300 | discord-routing·SessionPanelModel·agents serialization·timeouts helpers/idle-kill·CLI/API inventory·공식 생성문서 | 새 plain/한글/provider형 label, 명시thread, **중지 뒤 새 물리 세션 없음** / parser우선·oldkeytail직접stop |
| S8c 최종 writer / S1~S9,S8a,b1,b2,외부16파일+iMessage이행 | 17~22 / +400~650 | naming/adk_session·provider launch·Herdr/process·E2E writer+midturnkill. 실제 reader 호환 하한 SHA 기록 | env-i 운영 launch 조건·장문/option readback·foreign경합·marker실패복구·AgentDesk-label대체 / 44절단복원·marker실패미정리·namespace회피제거 |

조각의 exact 소비 파일은 부록 A/B/F, S7 표면은 부록 E가 정본이다. source 추가 발견으로 +1200/30파일을 넘길 전망이면 분할한다. r2 리뷰 이후 남는 반례는 해당 조각의 RED·PG·운영 수용 조건으로 고정하고 설계 라운드를 늘리지 않는다. PG·cargo·clippy·Rust mutant는 이 설계 레인에서 실행하지 않는다.

## 사용자 결정 필요

1. 에이전트 이름 — **확정**: A 채널 이름(사용자 선택), B 역할 ID(미채택). 추가 답변 불필요.

다음 3건은 **기본값으로 진행, 확인 요청**이다. 문서/시험 준비를 막지 않으며 S8c 전 제품 선택 확인에 포함한다.

2. Herdr workspace 표시 이름. A `logical_key#nonce` 유지 / B 순수 채널명으로 변경. **추천·기본 A**: 기존 ACK-gap 소유 증거를 보존한다. Herdr workspace는 이번에 단순화되지 않는다는 한계를 명시한다.
3. routine 표시 이름. A main의 routine 이름+agent에 provider만 유지 / B routineID를 표시 이름에 새로 넣음. **추천·기본 A**: 읽기 쉬운 이름과 기존 재사용 scope를 유지한다. 동명 routineID 충돌은 별도 이슈 후보이며 이 레인은 이슈를 만들지 않는다.
4. 충돌 대체 이름. A 본문 순서 `<agent>-<channel_id>` → provider/channel/scope → nonce 후보 / B 기본부터 모든 이름에 provider/scope를 붙임. **추천·기본 A**: 정상 채널은 단순 이름, 충돌자만 자동 구분한다. 부록과 같은 단일 규칙이다.

## 변경 이력·리뷰 지적 반영 위치

| 항목 | r2 반영 위치 |
|---|---|
| P1-1 | §권고안 A·부록 B 26파일·S2/S3/S9: primary/outcome 승격·old alias·late nonce PG, effective descriptor 기계 폐기 |
| P1-2 | §BindingContext·§#8118 경계(위 같은 절)·부록 C: parent/thread optional·imsg 3파일·외부 skill RED 선행 |
| 새 방향 | §대안·§파일 경로·부록 E·S7a/b: pure prefix class, manifest allocation-only, 긴 legacy 보존 |
| P2-1 | §관리 표식 S1 platform send/paste/kill 마지막 관문·거부/guard mutant |
| P2-2 | §생성 실패 S1 new-session -e 원자적 표식; 별도 marker 실패 정리·관문 우회 없음 |
| P2-3 | §사용자 결정 2~4 기본값/확인 요청·routine ID 확장 제외·충돌 규칙 통일 |
| P2-4 | §외부 보호 판정식·부록 C 16파일 preflight·S8c 선행 |
| P2-5 | §관리 표식 정확 target·§실측/활성화 표: =name:·탭·운영 조건 미확인/추후 시험 |
| P2-6 | §파일 경로·부록 E: pure total formatter로 3 reader에 새 Unknown이 없어 개별 이관 삭제 |
| P2-7 | §BindingContext/scheduled·S6 Rust 후보 SQL 두 지점·identity없는 scheduled RED |
| P2-8 | §전환/rollback·S8c 호환 reader 하한 SHA |
| P2-9 | §이름 규칙1·S8c AgentDesk-label→channel-id 시험/뮤턴트 |

## 리뷰 집중 항목과 남은 검증

- 경계 시각: live old 보존, old host absence와 fresh nonce CAS가 있는 자연 등록만 A 승격. PG/실제 provider 전환은 구현 수용 시험으로 남음.
- 동시성: 두 locator lock→identity→row 순서, snapshot 재검증, atomic 예약/create와 자기 created token cleanup. lock순서/latehook 경합은 S2 PG RED.
- 옛 형식 복원: 부록 B 26 primary-tail 경계·12 candidate·부록 F58 caller·imsg parent, old alias 입력은 current primary로 정규화. 승격 제거 mutant에서 stop/GC 실물 결과가 실패해야 함.
- 문서로만 선언한 guard: S1 공용 platform 관문과 외부16파일 보호식·외부skill 시험은 S8c 선행. 이 문서는 Rust/skill 구현 완료를 주장하지 않음.
- 잘못된 0/PASS: 정적 시험 후보 수와 실행 수 구별, 격리 tmux/HTTP fixture와 운영 조건 구별, 모델은 미구현 A 계약의 bounded 검증이며 cargo/PG 통과가 아님.

<!-- doc-path-refs: proposed domain/provider/session_name.rs services/tmux_common/artifact_paths.rs services/session_name_reservations.rs -->


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
| `src/services/message_outbox/identity.rs` | DM 이름인지 확인하여 delivery bot provider 선택 | S/B/M |
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

0단계의 '상태 전이 해당 없음' 판정을 뒤집는다. 근거: tmux_reaper.rs:504의 thread 분류가 delete/idle 전이를 고르고, :210/:395의 provider 판독은 정리 후보를 결정하며 watchers/lifecycle/restore.rs:45/:128은 재획득 대상과 channel을 고른다. 새 관리 표식 미확정·충돌 시 보존하는 분기가 들어간다. `docs/statecharts/session-lifecycle/` 기존 모델에 old/new managed locator·foreign collision·marker unknown·alias ambiguity를 입력 조건으로 추가하고, 신규 cleanup 권한이 없는지 TLC+목표 시험으로 검증하는 계획이 필요하다. 이 전수조사의 d1 시점에는 모델/코드 실행이 없었다. r2 모델 변경·검증은 본문과 부록 G를 따른다.

### r2 추가 외부 소비자와 마지막 관문

| 위치 | 지금 판독/결정 | r2 대체 |
|---|---|---|
| 외부 `imsg_send.py:227-241` | prefix와 -t 꼬리에서 부모 binding 추정 | S6 BindingContext parent/thread + 외부 skill 계약(부록 C) |
| `src/services/platform/tmux.rs:370,390,462,470,484,511` | kill/send/paste 진입점 | S1 마지막 관문 owner_scope 확인, 생성과 동시에 -e 표식 부착 |


# 부록 B. 세션 키·inflight·바인딩

## 세션 키·inflight·바인딩 경로 (기준 5bcbb82297f5cc3ab8897b81ddb9891459e0321b)

원시 검색: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/identity-grep.log` (명령 `git grep -n -E 'session_name|tmux_session|inflight|binding' -- 'src/**/*.rs' ':!agentdesk.yaml' ':!launchd.env' ':!settings.json'`; 36,609행). 아래는 단순 인자 전달/로그/시험 fixture를 묶고, 이름이 키·증거·경로가 되는 각 소유 seam을 열거한 표다. 코드·운영 저장소는 변경하거나 실행하지 않았다.

| 소유 seam / 좌표 | 이름을 포함하는 값·판정 | 이름 변경시 끊기는 대조 | 이중 판독·live 보존 방법 |
|---|---|---|---|
| `src/domain/session_identity.rs:25`, `:32`, `:80`; `src/services/discord/session_identity.rs:8` | legacy `host:<actual_name>` 및 namespaced `provider/hash/host:<actual_name>`의 tail; legacy 역직렬화 | 옛 primary를 남기면 parser는 죽은 이름을 정확히 읽음 | parser 문법은 이미 양쪽 이름을 읽지만, 그 안전성은 S2 primary 승격 불변식에 달림. wire 문법은 유지하고 actual_name을 불투명 locator로 취급. 새 이름에서 provider·thread 역추정 금지 |
| `src/services/discord/adk_session.rs:59`, `:84`, `:135`, `:150`, `:155` | 채널 basis→build_tmux_session_name→namespaced/legacy key 후보 2개 | generator를 먼저 바꾸면 follow-up에서 기존 행과 다른 key 생성 | canonical `(provider, token_hash, channel_id)`로 기존 actual key를 먼저 찾고 그 실제 이름을 반환. absence를 확인한 새 생성에만 새 이름. 후보 2개는 **동일 actual name의 key wire 2종**이며 old/new name 지원과 다름 |
| `src/services/discord/host_key_derivation.rs:105`, `:117`, `:141` | canonical row의 실제 session_key→name; 다중 행/실패 Conflict/Unknown | 생성한 이름으로 덮으면 host·live locator 상실 | 이미 존재하는 canonical row resolver를 재사용. live old는 그대로, 자연 재생성 후 A로 갱신된 primary key/name을 이후 launch/reattach/stop까지 보존 |
| `src/db/dispatched_sessions/canonical_identity.rs:33`, `:265`, `:287`, `:297`, `:320`, `:396`, `:469`, `:492`, `:825` | canonical identity `(kind, provider, discord_token_hash, channel_id)`, exact primary·alias 및 ID pin | exact direct SQL만 쓰는 소비자는 alias를 못 봄; name collision을 동일 채널로 잘못 합칠 위험 | main에 identity/alias 해법 이미 있음. exact→alias→canonical 증거가 **한 sessions.id**로 수렴할 때만 사용. divergence/foreign owner는 fail closed. 기존 live 행 primary key/name 변경·무조건 alias 새 생성 금지. 자연 재생성 등록 때 S2에서 primary=new / old alias=same row / outcome=new를 같은 transaction으로 승격 |
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
3. 새 선호 이름을 사용자가 선점했으면 사용자 세션은 그대로 두고 자동 충돌 이름(예: 채널ID·runtime digest suffix; 본문의 channelID→provider/channel/scope→nonce 순서가 정본)으로 **우리 세션을 새로 생성**한다. suffix가 어느 채널인지 읽는 권위는 표식이며 문자열 parser가 아니다. 충돌 candidate도 선점됐으면 다음 deterministic candidate를 시도하되 임의 세션 kill/rename 없음.
4. legacy `AgentDesk-*`도 prefix만으로 ownership을 인정하지 않는다. 기존 runtime-root `.owner`와 `.channel`, canonical row/provider/nonce가 일치할 때만 old-live 재사용/관리. 미표식·손상·다른 runtime 소유는 다른 사람 세션과 같이 처리한다. dual-read는 legacy 이름 **구문 판독**을 허용하는 것과 **mutation 권한**을 주는 것을 구별한다.
5. identity/hash/equality/fence를 alias 허용으로 느슨하게 하지 않는다. 이름을 canonical identity로 부르는 것은 탐색만, 실제 I/O·sidecar·turn CAS는 선택된 실제 locator에 고정한다. 기존 cleanup 범위/status/lease/nonce 검사는 유지한다.

### #8118 / #7583 경계

- 이번에 함: `<agent>-<threadID>` 생성에 explicit thread_id를 넣기, parent/provider/token/channel metadata 전달, old `-tID` compatibility reads, 새 이름에 thread suffix parser 의존 제거.
- #8118에 남김: 스레드가 부모 채널의 tmux/Herdr/process host·execution node/endpoint 선택을 상속하는 **배치 정책**. 이름 builder는 host를 결정하지 않는다. 상속된 parent descriptor를 입력으로 받는 계약만 공유한다.
- #7583에 남김: 전체 interactive session host abstraction과 모든 provider/ops API의 host-neutral routing 이관. 현재 `session_host` witness/locator를 읽어서 이름 추정을 줄이되 hosted execution schema·consent/termination gate를 재설계하지 않는다.

### 상태차트 판정

초기 "이름 문자열 변경만"이라면 비대상이다. 그러나 이번 수용조건은 prefix 판정 제거 후 외부 세션 비간섭·충돌 생성·옛 세션 재획득 조건을 바꾼다. 따라서 브리프의 예외가 적용된다: `docs/statecharts/session-lifecycle/` 기존 모델을 r2 모델 projection 및 구현 조각에서 `existing owned legacy → retain`, `existing foreign → choose another name`, `Unknown/Conflict → no mutation` admission 조건을 갱신해야 한다. r2 projection은 전체 턴/입력 모델 결합 검증을 대신하지 않으며 S2 PG/재시작 수용 시험에 남긴다.

### RED 계약·뮤턴트 제안

- old live key + old inflight/hash/nonce/FIFO/output + new preferred name을 넣어 restart discovery/queued follow-up/dispatch protection/stop/clear가 **같은 actual name**을 유지함. 뮤턴트: existing lookup보다 new builder 우선 → RED.
- same preferred name, 다른 provider/token/channel 또는 외부 미표식 session이 있을 때 0 mutation + own collision name 생성. 뮤턴트: name equality를 ownership으로 간주 → RED.
- 새 `<agent>-123...` **일반 채널명**인데 thread metadata absent: thread_gc/dispatch thread lookup 불발. 새 실제 thread는 explicit thread ID가 살아남음. 뮤턴트: 신규 이름 suffix 숫자를 thread로 추정 → RED.
- primary/alias/canonical이 다른 sessions.id면 Conflict, unreadable metadata는 Absent로 읽지 않음. 기존 main 계약을 재사용하고 새 resolver 소비자 경로만 추가.
- `InflightTurnIdentity`·cancel generation·binding nonce·panel instance 기존 비교에 actual name이 유지됨. 뮤턴트: alias 문자열 정규화로 equality 완화 → old/new 동시 pane fixture RED.

# r2 부록 B 추가: primary 꼬리 소비자와 권고안 A

기준 `8bcaeb4909225e2adef440fdc72050ccbda64d5f`(production source는 `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`와 동일). 코드·저장소 문서·운영 상태 변경 없음, cargo/PG 실행 없음.

## 계수 방법

`git grep -n -E 'tmux_name_from_session_key|derive_channel_session_(key|name|identity|keys|locator)' -- src` 원문을 `logs/r2-primary-grep.log`에 보존했다. 시험 파일과 `claude/tui_session_launch.rs:285` 아래 `#[cfg(test)]` 호출을 제외한 **심볼 경계는 25파일**이다. 이 수에는 정의·re-export·parent import·routine의 동명 local helper가 들어 있다. 여기에 리뷰가 명시한 **exact-primary SQL loader** `db/dispatched_sessions/force_kill.rs` 1파일을 합쳐 아래 **26파일 전수표**로 고정한다. 26개 모두가 central helper의 직접 함수 호출이라는 뜻은 아니다. lock/upsert 근거 grep은 `logs/r2-primary-lock-grep.log`에 별도 보존했다.

경로는 `src/` 기준. **자동(A)**는 새 목록/DB row primary를 받은 경로의 이름 추출을 고칠 필요가 없다는 뜻이다. **S2/S3 수정**은 HTTP·CLI 등이 저장된 **옛 alias를 직접 입력**할 때 먼저 alias→현재 primary를 resolve해야 하는 작은 경계 적응이다. 이 구분 없이 “A이면 모든 임의 old-key 입력도 자동 해결”이라고 쓰면 안 된다. routine의 실행 ownership token과 historical 출력 key는 의도한 과거 실행 pin이므로 무조건 current로 바꾸지 않는다.

| ID | file:line | 현재 무엇을 읽고 무엇을 결정하는가 | r2 처분 |
|---|---|---|---|
| K01 | `domain/session_identity.rs:81` | legacy/namespaced key의 마지막 `:` 꼬리를 tmux name으로 추출 | **자동(A)**. 문법 parser는 유지. 새 primary tail이 현재 물리 이름이므로 opaque tail로 해석해도 맞음. |
| K02 | `engine/ops/timeouts_ops/host_repair.rs:74` | 정책이 받은 key의 tmux name으로 host target/evidence를 만듦 | 일반 policy SELECT primary는 **자동(A)**. explicit alias 입력 경계는 **S3**에서 기존 locator resolver 결과 primary로 정규화한 뒤 probe/kill. |
| K03 | `server/routes/agents_crud.rs:1032` | dashboard listing의 row/session key→name→restart gate local tmux 판정 | **자동(A)**. S9에서 출력 primary가 신형이고 실제 중지로 신형 세션이 사라지는 회귀를 추가. |
| K04 | `server/routes/dispatched_sessions.rs:192` | stale-turn reconcile 후 요청 key 꼬리로 mailbox foreground release | 현재 listing primary 요청은 **자동(A)**. **S3**에서 old-alias 요청을 reconcile이 선택한 current primary로 변환한 뒤 release, 이름을 raw 요청에서 다시 만들지 않음. |
| K05 | `services/agents/turn.rs:399` | `extract_tmux_name` facade: turn/event 표시·조회에 key tail 제공 | **자동(A)**. 신규 current primary는 새 이름. 역사 event key는 원문 보존, current로 일괄 rewrite하지 않음. |
| K06 | `services/auto_queue/runtime.rs:124,565` | provider fallback과 thread row의 name을 얻어 slot reset host guard 검사 | current row name은 **자동(A)**. provider 역파싱 제거는 기존 **S3**, structured provider 우선. |
| K07 | `services/cluster/session_discovery.rs:390` | 저장 key의 tmux name→provider/channel segment directory 구성 | 이름 위치는 **자동(A)**. 의미 역파싱은 기존 **S2** metadata directory 전환으로 수정. |
| K08 | `services/discord/admin_host_guard.rs:132,203` | explicit key→approved name, managed reset에서 channel 이름 재생성 우선 후 key fallback | **S2/S8b1**: 저장 current row/actual locator 우선으로 lookup 순서 수정. explicit old alias는 current primary를 resolve. pure tail parser 자체는 유지. |
| K09 | `services/discord/health/recovery.rs:265` | health recovery에서 생성한 이름 없을 때 저장 key의 name 사용 | stored primary 부분은 **자동(A)**. 생성명 우선 문제는 기존 **S8b1**에서 row 우선 전환. |
| K10 | `services/discord/host_defer_gate.rs:106,131` | canonical row에서 derive한 key/name으로 hold/defer/recovery 판단 | **자동(A)**. query가 반환하는 primary가 신형이며 row ambiguity/Unknown 정책은 그대로. |
| K11 | `services/discord/host_key_derivation.rs:105,111,117,164` | canonical row resolver가 돌려준 primary를 그대로 반환하고 name 추출 | **자동(A)**. r1 effective descriptor 대체기계를 삭제해도 `primary tail=current physical name` 불변식으로 정합. |
| K12 | `services/discord/session_identity.rs:3` | central `SessionIdentity`/tail helper의 re-export | **자동(A)**. re-export 유지, 실행 효과/별도 parse 없음. |
| K13 | `services/discord/tmux_reaper/host_guard.rs:89` | owned key가 구조적으로 parse되는지 확인한 뒤 keyed teardown guard | current stored primary는 **자동(A)**. old execution pin은 current 이름으로 느슨하게 alias 합치지 않고 기존 nonce/host guard 유지(S5 계약). |
| K14 | `services/dispatched_sessions.rs:10` | 자식 `force_kill` 등에서 사용하는 central tail helper parent import | **자동(A)** / parent 배선 유지. 실제 old-alias 입력 적응은 K15/K19에서 수행. |
| K15 | `services/dispatched_sessions/force_kill.rs:73` | HTTP/CLI raw 요청 key를 DB load 전에 parse해 kill 물리 이름으로 보관 | 새 listing primary는 **자동(A)**. **S3**에서 alias load가 찾은 current primary로 물리 name 결정 후 Prepared 생성. raw old tail로 kill하지 않음. provider도 row/marker에서 읽음. |
| K16 | `services/dispatched_sessions/hosted_cleanup/thread_gc.rs:15` | GC row.session_key의 name 하나에 owner marker와 session_presence 보호 probe | **자동(A)**. 새 primary→새 실제 name probe. old 이름 부재가 새 live session 부재로 오인되지 않는 회귀 시험. |
| K17 | `services/dispatched_sessions/listing.rs:236` | API listing의 key→tmux_session_name 표시 helper | **자동(A)**. primary가 새 name을 표시. S9는 단순 “동일 key 전달” 대신 실제 stop 효과 단언. |
| K18 | `services/dispatched_sessions/output.rs:153` | row id로 읽은 primary→capture/output name 및 host guard | **자동(A)**. row primary에서 새 name을 얻음. 과거 explicit transcript pin은 별도 원문 보존. |
| K19 | `services/dispatched_sessions/tmux_cleanup.rs:27` | raw 요청 key→name을 먼저 만들고 DB row/provider/host를 읽어 kill-tmux | 새 primary 요청은 **자동(A)**. old-alias HTTP 입력은 **S3**에서 current primary resolve 후 name/marker/probe/kill. |
| K20 | `services/queue.rs:137,453,735` | active dispatch/channel cancel이 `SELECT s.session_key`(primary)→tmux name→lifecycle stop | **자동(A)**. queue cancel 대상이 새 name. PG+host fake 또는 격리 tmux 회귀에서 새 세션 중지·queue 보존 효과를 확인. |
| K21 | `services/queue/http_force.rs:152` | selected row의 primary에서 응답 `tmux_session` 표시(Herdr termination 응답 포함) | **자동(A)**. 표시/현재 row key 일치. Herdr pane 제어는 원래 pane ID여서 key tail로 바꾸지 않음. |
| K22 | `services/routines/session_control.rs:251,431,476,853` | local helper가 ownership token 또는 latest thread row key의 tail을 추출 | latest current primary는 **자동(A)**. persisted fresh-run ownership token은 **실행 pin**이므로 그대로 유지, alias로 successor 물리 이름을 강제로 선택하지 않음. synthetic routine row를 canonical channel 승격에 섞지 않는 S6 계약과 함께 검증. |
| K23 | `services/session_host/resolve.rs:162` | supplied matched key 꼬리의 host_kind marker로 host evidence 구성 | current primary는 **자동(A)**. **S2**에서 임의 HTTP alias가 SessionKey 증거로 들어오는 경계는 current primary로 정규화. pinned inflight host locator/nonce는 유지. |
| K24 | `services/session_resume.rs:226` | raw resume key의 name으로 provider/owner load와 local rebind 대상 결정 | 새 primary는 **자동(A)**. **S3**에서 old alias→current primary를 먼저 resolve; old name으로 rebind하지 않음. |
| K25 | `services/tmux_turn_liveness/cleanup_host.rs:22` | cleanup row locator key마다 host marker가 absent/tmux인지 확인 | current primary는 **자동(A)**. `locator_keys()`가 갖는 historical/source locator를 무조건 current로 치환하면 host proof를 지우므로 기존 all-locators 검사는 유지. alias 증거가 충돌하면 기존 보존. |
| K26 | `db/dispatched_sessions/force_kill.rs:68,75` | `resolve_session_locator_pg`가 돌려준 current key로 `WHERE session_key=$1` exact-primary SELECT | **자동(A)**. DB loader는 이미 alias→primary resolve. **S3**의 service층 K15/K19/K24는 이 load 결과 current key를 실제 name 결정에도 사용하도록 전달 적응만 필요. |

## primary 승격의 최소 트랜잭션 계약

1. r1의 “primary=옛 이름 + current descriptor로 모든 곳에서 우회”를 삭제한다. 신규 등록에서만 **primary tail = 현재 물리 이름** 불변식을 유지한다. live old가 있으면 새 요청 이름과 상관없이 기존 primary/실행을 유지한다.
2. 적용 지점은 `canonical_identity.rs:206 upsert_hook_session_pg`, `:256 update_target`, `:265 preserve_alias`, `:269 HookSessionUpsertOutcome.session_key`다. 자연 재생성의 **명시적인 등록 witness**(old row/old primary, old host absence, 새 실행 nonce/새 actual key)가 있을 때만 이름 교체를 허용한다. 표시 이름이 달라졌다는 사실이나 old alias hook 자체는 승격 권한이 아니다.
3. main의 현재 lock 순서는 `:215` incoming locator → `:217` canonical identity다. 따라서 identity를 잡은 뒤 옛 locator lock을 뒤늦게 얻는 안은 old hook(old locator 보유→identity 대기)과 deadlock을 만들 수 있다. 옛 primary를 비권한 discovery snapshot으로 읽은 뒤 `{old primary,new requested key}`를 정렬해 locator lock → identity lock → row/alias `FOR UPDATE`를 얻고 snapshot/row ID/primary/nonce를 재검증한다. snapshot이 바뀌면 롤백 후 다시 탐색한다. 기존 단일-locator 경로의 locator→identity 순서도 보존한다.
4. old locator의 absence는 명시 host observation이어야 한다. Live/ProbeFailed/Unknown/다른 server witness/hosted pending은 Missing이 아니며 승격하지 않는다. old inflight/actor가 아직 live면 새 primary로 바꾸거나 old key를 alias로 내리지 않는다. host는 old actual locator의 host이며 default tmux 소켓을 추측하지 않는다.
5. `:893 active_turn_nonce=COALESCE($17,active_turn_nonce)` 때문에 main은 terminal 뒤에도 old nonce를 남길 수 있다. row의 nonce가 새 실행 것인지 확인하기 전에 평범한 old-alias hook가 nonce를 먼저 덮어쓰게 해서는 안 된다. 검증된 fresh-registration witness와 기존 actor pin CAS(`:236-245`, `:899-902`, `:930`)로 old row snapshot을 승인한 **등록**만 새 nonce를 설치한다. 같은 트랜잭션의 primary 승격 시점에 nonce는 비었거나 이 새 실행 것임을 재검증한다. old-alias의 지연 hook는 alias로 같은 row를 찾되 업데이트 **전에** expected old nonce CAS로 거절한다.
6. 같은 tx에서 새 key를 sessions.primary로 UPDATE → **옛 key를** `preserve_alias`로 보존 → 새 primary를 outcome으로 반환 → commit. `preserve_alias`를 UPDATE 전에 호출하면 `:809-821`의 “이미 같은 row primary” 조기 반환으로 옛 alias가 생기지 않으므로 순서가 중요하다. 새 key나 옛 alias가 다른 row를 가리키면 tx 전체 rollback. 기존 alias를 임의 삭제하거나 row를 합치지 않는다. 현재 `:269`의 `target.session_key` 반환을 그대로 두면 SQL만 승격해도 caller는 old key를 받으므로 함께 수정해야 한다.
7. 이 변경은 현재 canonical Discord row의 자연 재생성 등록에 한정한다. synthetic routine/process/scheduled의 기존 격리 범위나 historical ownership token을 current execution으로 합치지 않는다. 새 `name_kind <> 'channel'` candidate 제외(S6/P2-7)와 explicit scheduled identity 계약을 유지한다. 새로운 DB enum·effective descriptor CAS 저장기계는 필요 없다.

## 수정 전 RED / GREEN / 제거 뮤턴트 계약

- PG 전환: old primary `host:AgentDesk-claude-adk-cc`, 새 등록 `host:adk-cc`, old host absent + 허용된 actor witness. `sessions.session_key==new`, old alias→동일 row ID, `HookSessionUpsertOutcome.session_key==new`를 동시에 단언. 현재 main은 primary/outcome이 old라서 RED. 승격 제거 또는 outcome old 반환 뮤턴트도 RED.
- PG 지연 hook: 새 등록 후 old nonce의 hook가 old alias로 같은 row를 찾음. nonce CAS refusal, primary/new nonce/status 보존. actor fence 제거 뮤턴트 RED. 새로운 일반 hook가 old key를 primary로 되돌릴 수 없어야 함.
- PG 충돌: new key/old alias가 다른 row 소유, source snapshot 변경, old nonce mismatch면 전체 rollback. alias만 저장되거나 primary만 바뀐 부분 성공 0. alias 보존을 UPDATE 앞으로 옮긴 뮤턴트 RED.
- host 차이: old live·probe error·다른 server witness·Herdr Pending이면 old primary 유지 및 기존 live 대상 무조작. absence 검사를 제거한 뮤턴트 RED.
- 동시성: old-alias hook와 new registration을 의도적으로 겹쳐 deadlock 없음/한 canonical primary/old hook CAS 거절. 정렬 locator lock을 identity 뒤로 이동한 뮤턴트는 timeout/deadlock 또는 계약 실패 RED.
- 효과 회귀: queue cancel(K20), force_kill(K15), policy host_repair(K02), hosted thread GC(K16)를 현재 primary와 명시 old alias 양쪽에서 호출. 취소/중지 후 **새 이름 세션이 없음**, 보호해야 할 새 live thread는 GC가 삭제하지 않음, queue 유지. 단순 JSON key 동일 전달을 성공 단언으로 쓰지 않음.
- S2 실행 범위 델타: 효과 회귀는 current primary 입력으로 검증한다. explicit old-alias force_kill/host_repair 효과 회귀는 S3가 검증하며 raw alias 정규화도 S3에 둔다.
- 위 항목은 **구현 레그의 수용 시험**이다. 이번 읽기 전용 설계 조사에서는 PG/cargo/운영 tmux를 실행하지 않았다.



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
| `dashboard/src/components/cluster/ClusterNodesPanel.tsx:81,84-86,96` | name/key 표시, 중지 호출에는 전체 session_key 그대로 사용. | S9에서 legacy→신규 primary 승격 뒤 중지를 호출해 **새 이름 세션이 없음**을 단언. 단순 key 전달 동일성은 성공 기준이 아님. |
| `src/server/routes/docs/inventory/endpoints/part_01.rs:357`, `part_05.rs:458,465`, `part_06.rs:131` | API 예제의 AgentDesk-* 이름. | 새 예제와 legacy 호환 예제 구분; 생성 문서 경로로 반영. |

API·UI는 코드 조사만 수행했다. 실제 빌드/운영 API 접근을 했다고 주장하지 않는다. 부모의 격리 fixture 렌더와 함께 증거 수준을 구분해야 한다.

## herdr, Unicode, shell

- `src/domain/provider/naming.rs:16-50`: 현재 `char::is_alphanumeric()` 또는 `-`/`_` 유지, 나머지를 `-`로 치환. 문서의 `[A-Za-z0-9_-]` 설명은 실제와 다르며 한글은 유지된다. `safe_prefix(...,44)`는 **byte** 예산이고 `src/utils/format.rs:4-12,38-43`이 UTF-8 char boundary로 내리므로 panic은 피하나 앞 14음절(42byte)이 같은 두 15음절 이름은 충돌한다.
- `Cargo.toml:87`에 `unicode-normalization` 의존성이 이미 있다. 현 naming 모듈에 `.nfc()`/`.nfd()` 적용 없음. 신형 writer에는 NFC 정규화 후 허용문자 치환/길이 계산을 넣고, 옛 경로 lookup은 저장된 원문을 사용해야 한다. APFS가 정규화 등가 파일을 같은 inode로 처리하는지 부모 실측 결과에 맞춰 이름 기반 파일 충돌을 방지한다.
- `src/services/tmux_common.rs:1113-1140`: 경로 basename에 `session_name` 원문 삽입. NFC/NFD가 tmux에서 별개여도 파일시스템에서는 같은 경로일 수 있다. 신형 locator 전환과 함께 NFC 정규화 + canonical ownership 충돌 확인 필요. 살아 있는 옛 파일명은 rewrite/rename하지 않는다.
- `src/services/herdr_launch.rs:342-345`: workspace label은 `logical_key#nonce`. `prepared.rs:37,58`이 create 요청에 전달. `src/services/herdr_launch/unplaced.rs:100-112`는 정확한 logical_key 또는 `logical_key#32hex`만 자기 후보로 읽는다. **표시용 label을 단순 에이전트명으로 바꾸면 orphan/adopt 판정이 깨진다.** 새 display label을 identity marker와 분리하거나 이 naming 이슈에서는 workspace ownership label을 보존해야 한다.
- `src/services/session_host/herdr/model.rs:238-243,268-293`: label/cwd는 JSON String, pane 제어는 별도 `pane_id`/`workspace_id` 기반. 이 프로토콜에는 pane name이나 rename 요청이 없다. 한글 label 실물 생성은 부모의 격리 herdr에서 검증하며 사용자 herdr는 조작하지 않는다.
- `src/services/herdr_launch.rs:350-358`: launch command는 절대 cwd/한 줄 non-control 조건. shell line의 Unicode 자체를 거부하지 않음.
- `src/services/process.rs:596-600`: POSIX `shell_escape`는 단따옴표로 감싸고 내부 `'`를 `'\''`로 치환, Unicode 바이트 보존. `src/services/claude/tui_session_launch/prepared.rs:82`가 script 경로에 적용. 외부 명령은 argv 사용, session-target과 pane-target 문법을 구분하며 pane-target에는 `=name:`를 사용. r2 정확 target 실측은 부록 D.
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
3. UI의 이름→provider/channel 역파싱 제거. 구조 필드·legacy fallback·표시를 분리하고 listing/new primary 및 old alias 요청 양쪽의 실제 중지 효과(새 이름 세션 없음)를 검증.
4. `docs/session-binding.md`, 보호 규칙/실행 문서, CLI 소스 설명·API inventory 예제 및 공식 생성 문서 갱신. historical plans에는 legacy 표기만 보강.
5. 코디네이터가 현행 스킬 15파일의 내용을 별도 승인된 스킬 작업에서 점검·수정·동기화. d1은 목록만 제공했고 r2는 아래 세 imsg script의 관련 함수까지 읽었다. 실행/수정은 하지 않았다. 대형 운영 수동 절차 없이 CLI/metadata query를 쓰게 전환. 외부 소비자 이행 전에도 살아 있는 옛 세션은 계속 지원.

## 계획 수리/시험 제안

- 대시보드와 E2E 조각: prod 5~8파일, 순증 약 +180~300, 시험 4~7파일/+200~350. 기존 giant `tmux_common.rs`에 새 Unicode/ownership 책임 추가하지 말고 naming 도메인과 작은 registry/metadata 모듈로 분리.
- RED: plain `adk-cc`, `claude-sales`, 한국어 `개발-담당`, NFC/NFD equivalent, `<agent>-<threadid>` fixture의 target label/provider/channel 복원; legacy key도 통과해야 함. old names-only midturn guard는 새 이름을 거부하는 RED를 먼저 확보.
- mutant: UI metadata 우선순위를 legacy parser 앞에서 뒤로 되돌림; new thread를 `-t`로 생성; E2E ownership guard 제거/AgentDesk-prefix만 확인. 모두 위 fixture 또는 foreign-collision 시험이 실패해야 한다.

## r2 외부 iMessage 영향과 BindingContext 계약

루트는 `/Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/`이며 아래 script는 **읽기만** 했다. 운영 agentdesk.yaml이나 state ledger를 실행/읽지 않았다.

| 파일:줄 | 현재 효과 | 이름 변경 영향·수정 계약 |
|---|---|---|
| `imessage-send/scripts/imsg_send.py:227-241` | caller_target이 context의 channel_id/provider 우선, 이름의 AgentDesk-와 -t 폴백으로 부모 binding을 보충 | **영향 있음**. S6 parent_channel_id/thread_id optional을 먼저 게시하고 스킬이 parent.or(channel)을 우선. 새 이름+thread context→부모 binding, parent 읽기 제거 mutant RED. S8c 선행 |
| `imessage-send/scripts/imsg_inbound.py:404-406` | imsg_fork.tmux_alive(target)를 호출해 원래 세션 생존/포크 분기 | 저장된 정확한 이름을 전달하며 이름 의미를 파싱하지 않음. 새 이름 영향 없음. current primary 승격 뒤 새 target이 저장되는 계약은 송신기 쪽 |
| `imessage-send/scripts/imsg_fork.py:62-69` | 저장 tmux_session을 shell quote하고 `has-session -t '=name'` | session-target exact 조회라 새 이름에도 동작. pane-target의 콜론 이슈와 구분. Herdr 수신기 정책 확장은 이번 범위 밖 |

필드는 #8119 S6이 정본, #8118이 같은 parent_channel_id를 host·출력 경로 상속에 소비한다. 스킬 수정·시험 실행은 코디네이터의 별도 승인 작업이며 이 레인은 계약만 고정한다.

## r2 잔여 prefix 16파일 preflight

외부 보호 판정식은 본문과 같은 **binding context env 설정됨 OR 비어 있지 않은 identity option OR legacy AgentDesk-prefix**다. 보호 여부와 mutation 권한을 혼동하지 않는다. 아래는 현재 read-only grep 원문 `logs/r2-external-prefix-preflight.log`의 16파일이다. S8c PR에는 이행/legacy 보존/무관의 최종 처분과 해당 skill 시험 결과를 첨부한다.

| 파일 (99_Skills 상대) | AgentDesk- 줄 | 활성화 전 처분 |
|---|---|---|
| `adk-release/SKILL.md` | 48 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `agentdesk-issue-pipeline/SKILL.md` | 52 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `agentdesk-issue-pipeline/references/execution.md` | 194 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `agentdesk-issue-pipeline/scripts/launch-headless-leg.sh` | 117 | live TUI cwd 보호를 후보별 표식/legacy 조건으로 전환. 조회 실패는 보호 집합 불완전 |
| `agentdesk-issue-pipeline/scripts/reclaim_tmp.py` | 35 | 정리 대상 provenance 확인과 보호식 대조. prefix-only 삭제/kill 권한 확대 금지 |
| `agentdesk-issue-pipeline/scripts/stop_orphan_test_procs.py` | 13,168 | 보호/프로세스 정리 fixture에 old/new 사례; prefix 문자열만 새 이름으로 일괄 교체하지 않음 |
| `agentdesk-issue-pipeline/scripts/test_stop_orphan_test_procs.py` | 165 | 보호/프로세스 정리 fixture에 old/new 사례; prefix 문자열만 새 이름으로 일괄 교체하지 않음 |
| `agentdesk-relay-audit/SKILL.md` | 51 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `agentdesk-relay-e2e/SKILL.md` | 15 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `agentdesk-restart/SKILL.md` | 19,35,42,63,64,71 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `agentdesk-restart/scripts/restart_agentdesk.sh` | 40 | 자기 작업 세션 재시작 거부를 보호식으로 전환. 새 이름에서 거부 RED/guard mutant |
| `agentdesk-runtime-ops/SKILL.md` | 12,31 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `agentdesk-runtime-ops/references/recovery-playbook.md` | 9 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `agentdesk-runtime-ops/references/topology.md` | 18 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `watch-agent-turn/SKILL.md` | 26,30,31,32,52,75,76,94,113,127 | 보호 문구/조회 예제를 보호식 기준으로 갱신; legacy 사례와 인프라 session 이름은 구별 |
| `imessage-send/scripts/imsg_send.py` | 231,237 | parent/thread context reader 및 위 부모 binding 시험. legacy 이름 폴백은 보존 |

위 16파일은 d1 현행15파일+imsg_send 1파일이다. imsg_inbound/fork는 prefix 판독이 없어 16에는 세지 않고 영향 표에 별도 포함했다. backup11파일은 계속 변경 대상에서 제외한다.


# 부록 D. 실물 점검

## d1 격리 실측 원문(운영 조건 검증 아님)

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

## r2 pane-target 정정 실측

ASCII/한글 모두 `set-option -t =name`은 실패, `=name:`는 성공했다. 따라서 한글 원인 주장을 철회한다. 실제 탭 구분자와 session_created를 사용했다. 기본 socket·launchd.env·운영 launch argv는 실행하지 않았으므로 44byte 초과와 옵션 read-back의 운영 조건 검증은 본문 활성화 계약으로 남긴다.

```text
$ tmux -L i8119-probe -f /dev/null new-session -d -P -F '#{session_id}	#{session_created}' -s adk-cc /bin/cat
rc=0
stdout=$0	1794469113
stderr=
$ tmux -L i8119-probe -f /dev/null set-option -t =adk-cc @agentdesk_identity fixture
rc=1
stdout=
stderr=no such session: =adk-cc
$ tmux -L i8119-probe -f /dev/null set-option -t =adk-cc: @agentdesk_identity '{"v": 1, "owner_scope": "isolated-fixture"}'
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null show-options -qv -t =adk-cc: @agentdesk_identity
rc=0
stdout={"v": 1, "owner_scope": "isolated-fixture"}
stderr=
$ tmux -L i8119-probe -f /dev/null display-message -p -t =adk-cc: '#{session_id}	#{session_created}'
rc=0
stdout=$0	1794469113
stderr=
$ tmux -L i8119-probe -f /dev/null new-session -d -P -F '#{session_id}	#{session_created}' -s '스마트홈-cc' /bin/cat
rc=0
stdout=$1	1794469113
stderr=
$ tmux -L i8119-probe -f /dev/null set-option -t '=스마트홈-cc' @agentdesk_identity fixture
rc=1
stdout=
stderr=no such session: =스마트홈-cc
$ tmux -L i8119-probe -f /dev/null set-option -t '=스마트홈-cc:' @agentdesk_identity '{"v": 1, "owner_scope": "isolated-fixture"}'
rc=0
stdout=
stderr=
$ tmux -L i8119-probe -f /dev/null show-options -qv -t '=스마트홈-cc:' @agentdesk_identity
rc=0
stdout={"v": 1, "owner_scope": "isolated-fixture"}
stderr=
$ tmux -L i8119-probe -f /dev/null display-message -p -t '=스마트홈-cc:' '#{session_id}	#{session_created}'
rc=0
stdout=$1	1794469113
stderr=
$ tmux -L i8119-probe -f /dev/null list-sessions -F '#{session_id}	#{session_created}	#{session_name}	#{@agentdesk_identity}'
rc=0
stdout=$0	1794469113	adk-cc	{"v": 1, "owner_scope": "isolated-fixture"}
$1	1794469113	스마트홈-cc	{"v": 1, "owner_scope": "isolated-fixture"}
stderr=
$ tmux -L i8119-probe -f /dev/null kill-server
rc=0
stdout=
stderr=
```


# 부록 E. r2 순수 artifact 함수·예약 대안

# #8119 r2 artifact 대안 검토

기준 head `8bcaeb4909225e2adef440fdc72050ccbda64d5f`의 코드(조사 정본 `5bcbb82297`)와 리뷰 원문을 읽었다. 코드·저장소 문서·스킬을 수정하지 않았고 cargo/운영 설정/DB/store/기본 tmux/Herdr를 실행하거나 조회하지 않았다. 원시 검색: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/r2-artifact-grep.log`.

## 대안 — 4줄

- r1의 manifest 결정형은 live layout을 명시하지만, reader마다 Unknown을 추가하고 G32/R19(+P2-6 3개)의 누락을 막아야 하는 비용이 크다.
- **이름 class 순수 함수형을 채택한다.** 기존 `AgentDesk-` namespace는 v1 공식 그대로, 그 밖의 신규 이름은 v2 raw-byte digest이며 reader는 manifest를 읽지 않는다.
- 신규 builder가 `AgentDesk-` 이름을 `channel-<id>`로 돌리는 P2-9를 먼저 고정하면 old/new class는 겹치지 않는다. manifest는 fresh allocation의 이름 예약만 맡는다.
- 단 리뷰 예시의 **legacy 최대 길이 cutoff는 쓰지 않는다.** 현재 builder의 긴 `-t숫자` 보존과 Unsupported provider 때문에 상한을 분류에 쓰면 살아 있는 옛 파일 경로가 바뀐다. prefix-only class가 정본이다.

## class 정본과 길이

```text
class(exact_name) = LegacyV1  if exact_name.starts_with("AgentDesk-")
                    HashedV2 otherwise
LegacyV1 basename = current session_temp_prefix formula, byte-identical
HashedV2 basename = "adk-v2-" + lowercase_hex(SHA256(domain, runtime, host, exact_name_utf8))
```

- 대소문자·Unicode·옛 이름 바이트는 layout 단계에서 정규화하지 않는다. digest에는 길이 구분 또는 NUL domain separation을 사용한다. runtime root와 host의 기존 실제 값이 입력이며 plaintext host/name은 v2 basename에서 빠진다.
- `src/domain/provider/naming.rs:15-49`의 일반 출력은 `10 + provider.len() + 1 + <=44 byte`; 지원 provider 최대는 `antigravity` 11byte(`src/domain/provider.rs:31-40`), 따라서 정상 최대 **66byte**. Claude 61, Codex60, Gemini61, OpenCode63, Qwen/Grok59다. 과거 `-dev` suffix를 읽는 경우 70byte는 회귀 fixture 경계다. **66/70은 분류 cap이 아니다.**
- `naming.rs:28-37`은 `-t` 뒤 숫자의 길이를 u64로 제한하지 않고 suffix 전체를 보존한다. 예를 들어 `-t`+60 ASCII 숫자는 tail62byte이므로 Antigravity 이름84byte가 된다. `ProviderKind::Unsupported(String)`도 임의 길이 id를 반환한다(:40). 이 이름을 짧은 v2 class로 재분류하면 live `.owner`/FIFO/JSONL을 잃는다.
- 현재 suffix 함수는 빈 문자열(`naming.rs:10`)이다. 지원 provider·정상 u64 thread 입력의 최대66과 역사적 suffix fixture를 보존하되, 앞으로 class 규칙을 바꿀 때 기존 class 자체를 줄이지 않는다.
- v2 basename은 ASCII **71byte**다. 현재 확인한 최장 확장자는 `codex-tui-launch-options.sha256` 31byte(`codex_tui/session.rs:11`), 합계103byte로 NAME_MAX255 이하. 구현 시험은 전체 알려진 확장자 집합을 순회해 255 이하를 단언한다. legacy의 기존 HOSTNAME/긴 이름 문제까지 고쳤다고 주장하지 않는다.

## 입력 소유 범위 — token logical key 함정과 non-prefix 이름

- Herdr `HostedOwner.logical_key`는 namespaced key 전체가 아니다. `discord/router/message_handler/provider_dispatch.rs:652-668`은 SessionIdentity로 parse한 뒤 **tmux_name tail**을 logical_key에 넣는다. `herdr_launch.rs:393,742,772`의 fence/context/host marker는 그 tail을 사용하므로 현재 운영 producer가 만든 legacy Herdr artifact도 `AgentDesk-` class다.
- tmux·process wrapper의 실제 production 생성은 기존 provider builder를 경유한다. `claude/process_session_launch.rs:119`, `codex/process_session_launch.rs:117`, `qwen/session_lifecycle.rs:396`의 SessionConfig는 이미 선택된 session_name을 전달한다. process 모드라는 이유로 namespaced `provider/token/host:` 전체를 artifact key로 바꾸면 안 된다.
- generic `session_temp_path(&str, ext)` 자체는 임의 문자열을 허용한다. 따라서 **모든 임의 non-prefix 옛 파일 경로까지 byte-identical라고 쓰면 틀린다.** 기존 main이 제공한 managed `AgentDesk-*` 이름의 live compatibility를 보장하고, 사용자/외부 non-prefix 세션은 S1 소유 관문에서 조작하지 않는 범위다. 외부에서 임의 non-prefix raw artifact를 이미 만들었다는 운영 가정은 이번 금지 범위상 조회하지 않았다.
- 소문자 `agentdesk-claude-channel-*`는 `domain/session_identity.rs:125-139`의 key parsing 호환 fixture와 API inventory 예제에서 확인했다. 현재 production managed builder로 생성하는 경로는 확인되지 않았다. lower-case parser fixture가 있다는 사실만으로 과거 운영 artifact가 있었다고 단정하거나 old files를 지우지 않는다. 구현에서 새 legacy writer가 발견되면 추가 frozen class 또는 별도 호환 계약을 잡고 활성화 전에 해결한다.

## formatter만 함께 바꾸면 되는 이유

`session_temp_prefix` 직접 사용은 `tmux_common.rs:130,140,1113,1138,1157`와 `tmux_common/runtime_hint_fence.rs:382`다. 따라서 하나의 작은 child pure formatter를 모두 공유하게 하면 source authority의 lock key, canonical path, tmp path, RuntimeHintFence의 두 inode 경로가 동일 class를 사용한다. 별도 manifest load/Result/Unknown 경로가 필요 없다.

`legacy_tmp_session_path`는 **위치가 /tmp라는 뜻**으로 유지한다. v2 이름에 이 API를 부를 때도 basename은 동일 v2 hash다. v2 canonical/tmp 후보가 모두 hash namespace이므로 raw `agentdesk-<runtime>-<host>-adk-cc` 경로를 건드리지 않는다. Codex의 기존 `legacy_*_path` PathBuf 필드는 이 규칙을 받으면 그대로 안전하게 유지할 수 있고, Option 전환/모든 caller 서명 이사는 필수가 아니다. 후보 dedup과 raw path 미포함 시험만 추가하면 된다.

| P2-6 파일:줄 | 현 판정 | 새 방향의 대체/시험 |
|---|---|---|
| `src/services/codex_tui/execution_end/capture.rs:40` | `tmux_owner_path` 읽은 owner와 execution context owner 일치 | `tmux_owner_path→session_temp_path→class formatter` 경유이므로 자동 올바른 v1/v2 위치. 새 v2 owner+nonce/context fixture에서 capture 성공, raw legacy 경로만 둔 fixture는 불일치로 거절 |
| `src/services/codex_tui/execution_end/confirm.rs:102` | vacancy/nonce 확인 후 owner NotFound는 현 정책상 허용 | formatter가 total이므로 manifest unknown을 NotFound로 압축하는 새 실패가 사라짐. 현 NotFound 의미를 넓히지 않고 v2 correct owner/changed owner fixture로 기존 조건 유지 확인 |
| `src/services/discord/health/o_transition/codex_context.rs:236` | context/nonce/owner/watcher binding 일치 | 같은 중앙 formatter로 자동 해결. v2 owner marker fixture와 원인 제거 hash formatter mutant RED |

세 reader가 source authority를 잃는 문제가 해결됐다는 것이 아니다. **manifest layout Unknown 때문에 독립 적응해야 한다는 요구가 사라진 것**이며 기존 owner/nonce/vacancy guard는 그대로다. P2-6의 별도 prod 수정 항목은 이 사유로 삭제하고 위 contract fixture에 포함한다.

## 반드시 남는 접점

- `src/services/discord/tmux_reaper.rs:829-835`의 wrapper output basename에서 `AgentDesk-`를 찾는 코드는 hash에서 실제 이름을 복원할 수 없다. 기존 S5의 explicit binding/argv 이름 전달 수리는 그대로 writer 선행이다. 순수 경로안이 이 이름 역추정을 자동으로 고치지는 않는다.
- 신규 이름 예약은 **artifact 하나를 만들기 전**에 완료한다. reservation의 owner는 provider/token/channel/kind 등 실제 identity이고, 정상 다른 owner면 자동 다음 후보, 미지원/손상이면 신규 할당만 거부한다. 이 reservation은 reader·warm follow-up·cleanup의 path 선택 원천이 아니다.
- 동일 canonical/temp hash 경로들을 cleanup할 때 기존 source authority/nonce/fence 조건을 유지한다. manifest가 없거나 손상됐다는 이유로 새 cleanup/kill 권한을 만들지 않는다.

## 줄어든 조각 제안

| 조각 | prod 파일·순증 추정 | 정확한 책임과 시험 |
|---|---:|---|
| S7a pure artifact formatter | 3~5파일 / +120~220 | 새 `src/services/tmux_common/artifact_paths.rs`(제안), `tmux_common.rs`의 기존 prefix 코드 추출/delegate, 필요시 `runtime_hint_fence.rs` 공통 helper 배선, `codex_tui/session.rs` 후보 dedup. Sha256/Digest import는 helper로 함께 옮겨 dead import를 남기지 않음. legacy bytes·v2 NAME_MAX·NFC/NFD exact-byte hash·source authority/fence 동일 namespace·raw v1 경로 미삭제 RED. |
| S7b allocation reservation | 5~8파일 / +250~450 | 작은 reservation module + 기존 fresh launch admission/allocator 배선. manifest는 create-new 이름 예약만. 같은 owner/layout이라는 모호한 판독 대신 owner/name claim을 검증하고 known other owner 자동 후보를 고름. old-live reuse는 예약/재생성에 진입하지 않음. 신규 v2 발행은 마지막 S8c에서만 연결. |

두 조각 모두 prod30/+1500, PLAN+1200 이내다. 구체 cold-launch entry가 8파일을 넘으면 S8c 소유 launch wrapper별로 나누되 r1 G32/R19 reader를 일괄 signature 이사하지 않는다. 기존 51파일+P2-6은 formatter 소비 확인표로만 남기거나 요약으로 대체하고 수정 소유 목록에서 빼도 된다. 별도 checked layout registry/loader와 session_descriptor용 CAS 기계는 이번 선택에 필요 없다.

## 수정 전 RED·원인 제거 뮤턴트

1. legacy old-live AgentDesk name의 모든 ext canonical/tmp path가 원래 함수와 byte-identical. 긴 `-t`84byte, Unsupported provider 이름도 포함. mutant: 길이66 cutoff 추가 → RED.
2. 새 일반/한글/장문/NFC-NFD exact input은 v2 ASCII hash basename, extension 포함255 이하. mutant: raw 이름 삽입 또는 NFC로 hash 입력 합치기 → RED.
3. RuntimeHintFence와 source authority가 canonical/tmp writer와 같은 class prefix. mutant: fence만 legacy 원공식 유지 → RED.
4. Codex cleanup이 new-name raw v1 path를 지우지 않고 hash canonical/tmp 후보만 사용. mutant: legacy_tmp만 raw old공식으로 남김 → RED.
5. P2-6 세 reader는 new-name owner/nonce/context marker를 읽고 기존 same-execution 조건을 지킴. mutant: owner helper만 old raw공식 → RED.
6. reservation 손상/삭제는 이미 살아 있는 v2 reader의 path 출력에 영향을 주지 않는다. fresh allocation의 reserve 실패는 0 artifact write/0 host create. mutant: path formatter가 reservation JSON을 조회함 → RED.
7. 동명 다른 identity의 두 fresh allocation이 artifact 준비 전에 한 이름을 공유하지 않음. mutant: 예약을 output/FIFO 생성 뒤로 옮김 → RED.

이 보고는 설계 계약이며 위 Rust/PG/실물 시험을 실행했다고 주장하지 않는다. 실제 수행은 읽기 전용 소스 조사와 문자열 길이 정적 계산뿐이다.


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
| Fresh/Persistent routine(tmux) | `routines/agent_executor.rs:1198` routine_agent_session_name = `routine <routine.name> - <agentID>` compact+90문자. `agent_executor/execution.rs:50-59` Tmux label. launch.rs:151 및 `headless_turn/routine_metadata.rs:83` 사용 | 이름만 다른 routine이 44byte prefix에서 충돌 가능. 같은 routine/agent의 provider 변경은 현 provider prefix가 분리. 서로 다른 routineID가 같은 display name도 가능(이 함수에 ID 입력조차 없음). fresh라도 per-attempt tmux 이름은 아님 | **이 경로만** main의 routine 이름+agent basis에 provider를 유지. 예 `routine-<provider>-<routine_name>-<agent_id>`. routineID를 표시 이름에 새로 넣지 않는다. 같은 이름·다른 routineID 충돌은 기존 결함 후보로 별도 수용 판단하며 이번 확장에서 제외. fresh/persistent의 재사용·정리 타이밍 유지 |
| Routine(process) | `agent_executor/execution.rs:60-65` `routine-process <turnID 마지막 : segment> <routine label>` | 현재 turn/attempt를 앞에 넣어 44byte 절단에도 서로 다른 retry가 분리됨. provider prefix가 없으면 동일 문자열에서 provider 충돌 가능 | tmux를 새로 만들지 않음. 논리 key/binding 이름에 kind/provider/attemptID를 유지. `routine-process-<provider>-<attemptID>`+필요하면 scope digest; tmux 이름 규칙을 process identity 전체에 일괄 적용하지 않음 |
| Scheduled snapshot | `scheduled_messages/context_snapshot.rs:227-228` `scheduled:<definitionID>` → :290 label → `scheduled_messages.rs:386` metadata → `headless_turn/routine_metadata.rs:26,39` key override | definition ID가 label 기준이며 같은 정의의 매 fire는 같은 label을 재사용하고 headless_turn.rs:550 이하가 native session continuity를 끊음. provider prefix는 현재 분리. 새 이름을 단순 실제 채널 이름으로 돌리면 live channel row를 덮을 수 있음 | **이 경로만** `scheduled-<provider>-<definitionID>` 유지; kind=scheduled_snapshot 명시. definition scope를 그대로 유지하고 per-fire pane 수명 정책은 바꾸지 않음. 실제 channel 이름과 절대 같은 key로 생성하지 않음 |
| Recovery fallback | `agent_recovery/handoff.rs:39` fallback_provider + binding.channel_id(숫자)를 builder에 전달 | 원래 owner/fallback provider가 달라도 같은 raw channel ID. 현재 provider prefix 분리. role 이름은 기반이 아님 | **이 경로만** `recovery-<provider>-<channelID>` 또는 명시 fallback identity scope; original channel routing은 그대로. 저장된 actual locator는 우선하여 live rename/추가 kill 없음 |
| Name unavailable/numeric binding fallback | `cluster/session_matcher.rs:156,170` tmux_segment 없으면 channel_id; `:426` helper도 param 이름은 channel_id이나 raw name/id 모두 수용. 등록 fallback hint도 raw string | ID/alias를 실제 이름인 것처럼 처리하면 실제 숫자형 채널명과 혼동. 같은 raw ID를 provider가 함께 쓸 수 있음 | 실제 채널 name을 기존 directory/Discord projection에서 먼저 얻어 Channel typed input으로 전달. 얻지 못하면 existing locator만 탐색. 새 synthetic 필요 경로는 kind=fallback/provider/channelID 구분자를 유지하며 실제 채널명 함수에 넘기지 않음 |

`channel_routing.rs:51`은 Guild만 이름을 반환하고 비Guild는 None, :264도 Guild만 Some이다. 따라서 DM의 `dm-*`는 실제 Discord 채널 이름으로 분류할 근거가 없다. “channel_name” 필드가 string이라는 이유로 모든 caller를 새 규칙으로 돌리면 위 synthetic ID가 섞인다.

## 2. 종류를 받는 생성 계약

하나의 `build_tmux_session_name(&str)`를 prefix만 제거하도록 바꾸지 않는다. 새 입력은 `Channel{display_name,channel_id}`, `Thread{parent_display_name,thread_id}`, `Dm{provider,dm_channel_id}`, `Routine{provider,routine_name,agent_id}`, `RoutineProcess{provider,attempt_id,...}`, `ScheduledSnapshot{provider,definition_id}`, `RecoveryFallback{provider,channel_id}`처럼 scope를 명시한다. 실제 길드 채널만 채널명 자체를 사용하고, 나머지는 해당 kind에서만 provider/ID 구분자를 유지한다. synthetic label 이름으로 kind를 다시 추측하지 않으며 marker/session_info에 같은 kind와 원 ID를 저장한다.

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
| `src/services/routines/agent_executor/reliability.rs:245` | R: routine_name/agentID로 fresh provider liveness probe 대상 재구성 | 저장 actual locator 우선; routine name/provider/agentID 경로 |
| `src/services/routines/fresh_session_reaper.rs:69` | R: routine_name/agentID로 fresh routine 정리 대상 후보 재구성 | 저장 actual locator 우선; routine name/provider/agentID 경로 |
| `src/services/routines/session_control.rs:433` | C: 저장 session_key 없을 때 primary-name 또는 parent-tID fallback | 저장 locator 우선; typed Channel/Thread/Dm 경로 |
| `src/services/routines/session_control.rs:904` | R: fresh teardown의 routine_name/agentID fallback locator | 저장 actual locator 우선; routine name/provider/agentID 경로 |

중앙 정의는 `src/domain/provider/naming.rs:15`. 간접 소비자 `src/cli/run.rs:772`, `cluster/session_matcher.rs:156,170`은 expected_session_name_for→`:426`을 호출한다. parent cfg(test) 아래 `relay_e2e/registered_bootstrap_e2e.rs:108,335`, `relay_e2e/thread_guard_host_e2e.rs:195`, `watchers/lifecycle/watcher_install_fixture.rs:55`는 prod 호출 수에서 뺐다.

## 5. RED 계약·뮤턴트 보강

1. DM user가 같고 provider가 다른 두 typed 입력을 동시에 생성하면 서로 다른 locator·binding·sessions row. provider를 제거한 뮤턴트는 RED. dm workspace hint는 `dm-<userID>`로 그대로 전달되는 별도 단언.
2. main의 routine 이름+agent를 보존하고 provider가 다른 경우 분리. provider 제거 뮤턴트 RED. 같은 이름·다른 routineID 충돌 수리는 별도 후보로 남기며 ID 표시 정책을 새로 넣지 않음. DM+tmux routine의 synthetic label None은 그대로라 main과 같은 canonical DM 세션을 쓴다는 회귀 계약.
3. routine process retry turnID가 다르면 논리 key 분리; tmux 생성 호출 0. attempt 분리 제거 뮤턴트 RED.
4. scheduled snapshot은 live channel과 다른 key, 같은 definition의 현재 reuse scope와 native fresh reset 유지. kind를 Channel로 넘긴 뮤턴트 RED.
5. 같은 길드/다른 category/다른 guild의 동명 channel ID 각각 신규 충돌 fallback. bare 사용자 세션 PID·pane·option 변화 0, existing managed old/new locator 이름 변경 0. metadata provider/channel 불일치에도 attach하지 않음.
6. 동일 이름 동시 생성 두 채널 경합에서 하나의 세션을 공유하지 않음. atomic create 실패를 자기 managed 세션으로 간주한 뮤턴트 RED.
7. thread `is_synthetic_thread_channel_name`는 `session_runtime/channel_routing.rs:11`에서 `ends_with(-t<id>)`를 읽고 :22에서 restore 보존을 결정한다. 명시 thread ID 우선+legacy suffix read, parent 이름 미조회/숫자 fallback 회귀를 별도로 고정.



# 부록 G. 상태 모델 실행 결과



# 이름 이전 등록 트랜잭션 모델 — r2

이 모델은 **아직 구현하지 않은 A 승격 계약의 별도 유한 projection**이다. 기존 수명 모델 전체와 naming을 합성 탐색한 결과가 아니다. TmuxSession은 `msg`, HerdrSession은 `handoff`에 Naming record를 넣고 나머지 기존 lifecycle 변수는 고정한다. 기존 `Init`/`Next` 본문과 기존 cfg는 바이트 동일하게 보존했다. 따라서 기존 model의 알려진 수명 위반을 고쳤거나 재검증했다고 주장하지 않는다.

## 범위와 계약

- 한 채널의 옛 실행 하나와 새 successor 하나, old/empty 실행 nonce 2개 initial state, probe failure 최대 1회, 등록 전 nonce 교체 1회, 늦은 old hook 1회와 중지 재요청 1회를 탐색한다.
- 새 이름 요청 자체는 old live 이름/primary/물리 대상을 바꾸지 않는다. old host의 실제 종료와 부재 관측 뒤에만 successor를 준비한다. 등록은 old-host absence와 row nonce empty/new를 확인해 **primary 새 key + old alias**를 한 행동으로 커밋한다.
- 등록 직전 nonce가 old로 바뀌면 등록을 거부하고 자기 fresh successor만 정리한다. 자기 생성·서버 witness가 유효하다는 조건과 DB identity/두 locator lock의 원자성은 이 projection의 가정이다. 소유 증거 교체·기본 tmux 서버·실제 SQL 격리는 구현 시험 책임이다.
- tmux는 `PrimaryTailIsCurrentPhysical`을 검사한다. Herdr primary는 logical artifact 이름이므로 pane ID와 literal 동등성은 검사하지 않고 `PrimaryNamesCurrentLogical`과 `HostTargetMatchesCurrent`(old/new logical ↔ old/new pane)를 검사한다. pane의 실제 생성/복구 프로토콜은 기존 Herdr 모델에 그대로 남아 있다.
- `NoDuplicateKill`은 같은 owned successor에 대한 **종료 효과가 1회를 넘지 않음**이다. 호출 횟수나 모든 외부 프로세스에 대한 kill 권한을 증명하지 않는다. retry stop은 효과 0이다.

모델 작업은 2026-11-12 16:34:58 KST에 시작했고 16:50 KST 무렵 모든 실행을 종료했다. 30분 제한 안이며 진행 중 프로세스는 없다.

## 실행 결과

공식 TLC 1.7.4 배포 jar SHA256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`, 실행 `/opt/homebrew/opt/openjdk@25/bin/java`(Homebrew OpenJDK 25.0.5.0.3). JVM `-XX:+UseParallelGC`, TLC `-workers 1 -fp 0 -lncheck final`. scratch `/tmp/i8119-r2-models` 복사본에서만 실행했다. cargo/PG/Rust/운영 tmux/Herdr 실행은 없다.

정식 acceptance 구성은 **fixed 2개 + mutant 10개 = cfg 12개**다. fixed 각 8개 불변식은 primary 동등성(호스트별 위 차이), HostTargetMatchesCurrent, LiveOldNotPromoted, OldAliasRetained, PrimaryNotAlias, NoDuplicateKill, LateOldHookRejected, PromotionNonceWasCurrent이다. fixed는 각 52 distinct/78 generated/depth 12, queue 0으로 완주했다. model mutant의 exit 12는 로그에서 invariant violation을 대조한 RED이며 도구 오류가 아니다.

| Host | cfg mode | exit | generated | distinct | depth | 결과 |
|---|---|---:|---:|---:|---:|---|
| Tmux | Fixed | 0 | 78 | 52 | 12 | 모든 8개 불변식 유지 |
| Tmux | WithoutPromotion | 12 | 32 | 23 | 6 | PrimaryTailIsCurrentPhysical |
| Tmux | WithoutAbsenceGuard | 12 | 11 | 11 | 3 | LiveOldNotPromoted |
| Tmux | WithoutNonceCAS | 12 | 38 | 27 | 7 | PrimaryTailIsCurrentPhysical |
| Tmux | WithoutPromotionNonceGuard | 12 | 37 | 26 | 7 | PromotionNonceWasCurrent |
| Tmux | DuplicateKill | 12 | 46 | 33 | 8 | NoDuplicateKill |
| Herdr | Fixed | 0 | 78 | 52 | 12 | 모든 8개 불변식 유지 |
| Herdr | WithoutPromotion | 12 | 32 | 23 | 6 | PrimaryNamesCurrentLogical |
| Herdr | WithoutAbsenceGuard | 12 | 11 | 11 | 3 | LiveOldNotPromoted |
| Herdr | WithoutNonceCAS | 12 | 38 | 27 | 7 | PrimaryNamesCurrentLogical |
| Herdr | WithoutPromotionNonceGuard | 12 | 37 | 26 | 7 | PromotionNonceWasCurrent |
| Herdr | DuplicateKill | 12 | 46 | 33 | 8 | NoDuplicateKill |

Skill pipeline으로 차트도 추출했다. fixed 각각 52 states/76 steps, 도식은 8 states/7 arrows이며 같은 도식 상태 안의 48 steps를 숨긴다. 별도 view는 probe·nonce 재확인·late hook/retry의 같은 상태 전이를 접는다. 승격 제거 도식은 반례를 표시한다. wrapper의 janitor shell에 금지된 `rm -rf`가 있어 wrapper를 실행하지 않고 같은 skill의 `worker.ts` pipeline을 직접 실행했다. skill 파일은 변경하지 않았다.

- [tmux 도식](https://github.com/itismyfield/AgentDesk/blob/01053c2be0/docs/statecharts/session-lifecycle/tmux-naming.chart.mmd)
- [Herdr 도식](https://github.com/itismyfield/AgentDesk/blob/01053c2be0/docs/statecharts/session-lifecycle/herdr-naming.chart.mmd)
- [승격 제거 반례 도식](https://github.com/itismyfield/AgentDesk/blob/01053c2be0/docs/statecharts/session-lifecycle/tmux-naming-without-promotion.chart.mmd)

## 반례와 남은 실제 재생

승격 제거 mutant의 최단 반례(도식의 0~5): 옛 실행 → 새 이름 요청 → 옛 host 종료 → 부재 확인 → successor 준비 → **새 실행 등록인데 primary는 old**. PrimaryTailIsCurrentPhysical·PrimaryNotAlias 위반이다. 그 뒤 old hook까지 0~6에서 중지가 잘못된 primary를 보고 진행하지 못하는 NoDeadlock 위반도 나타난다.

나머지 모델 반례는 raw `*-withoutabsenceguard.out`의 LiveOldNotPromoted, `*-withoutnoncecas.out`의 primary 동등성, `*-withoutpromotionnonceguard.out`의 PromotionNonceWasCurrent, `*-duplicatekill.out`의 NoDuplicateKill을 확인한다. 이들은 **제안 계약을 바꾼 모델 반례**이며 현 Rust 버그 재현이 아니다. 사용자 cargo 금지에 따라 실제 PG/Rust 재생은 미실시이며 S2의 PG 승격/늦은 nonce CAS/queue 취소·중지·thread GC 회귀와 restart E2E 수용 시험으로 넘긴다. 여러 채널의 동시 할당·host witness 교체·동시 stop·inflight 복구/alias resolver의 SQL interleaving은 여기서 제외했다.

## 재현

S2에서 이식한 저장소 모델·관련 cfg·SessionNaming 모듈·view를 새 scratch로 복사한 뒤 직접 TLC를 실행한다. 전체 cfg 실행 명령은 외부 로그의 `run_all.py`에 있고 cfg마다 stdout에 명령 원문이 남아 있다. 차트는 다음과 같이 같은 skill worker로 새 stage에 만든다(stage는 먼저 빈 디렉터리로 생성):

```sh
TLA2TOOLS=/tmp/i8119-r2-models/tla2tools.jar JAVA=/opt/homebrew/opt/openjdk@25/bin/java node --disable-warning=ExperimentalWarning /Users/itismyfield/.codex/skills/statechart/scripts/worker.ts /tmp/i8119-new-chart-stage /tmp/i8119-r2-models/TmuxSession.tla /tmp/i8119-r2-models/TmuxSessionNamingFixed.cfg /tmp/i8119-r2-models/session-naming.view.mjs
```

원시 증거: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8119/logs/r2-statechart/`의 `results.json`, `model-sha256.json`, cfg별 `.out`, `tmux-chart/`, `herdr-chart/`, `mutant-chart/`. 초기 36-state 시도와 view 라벨 수정 전 산출물은 scratch에 남았지만 최종 acceptance 증거로 쓰지 않았다. chart extraction은 최종 모델의 같은 cfg를 도식화하기 위한 추가 실행이며 전체 Rust 시험 수에 합산하지 않는다.

## 리뷰 집중 항목

- 경계 시각: request와 host 종료/부재 확인을 다른 행동으로 분리해 old live primary 보존을 검사했다. witness freshness의 실제 관측은 구현 E2E로 남는다.
- 경합: successor 준비 뒤 nonce 교체를 탐색하고 nonce-guard 제거 RED를 얻었다. DB lock 원자성 자체는 PG 시험 책임이다.
- 옛 형식 복원: old alias 보존·old hook 거절을 검사했다. 실제 저장 key 역직렬화·26 소비자는 S2/S3 수용 시험 책임이다.
- 문서만의 guard: fixed 불변식과 guard 제거 모델 5종을 연결했다. 실제 runtime guard가 연결됐다고 주장하지 않는다.
- 잘못된 0/PASS: cfg 12개 고유 실행 결과를 표로 고정했고 cargo/PG 0을 PASS로 세지 않는다. 기존 full lifecycle의 알려진 위반·기존 cfg는 손대지 않았다.

<!-- doc-path-refs: external imessage-send/ imsg_send.py imsg_inbound.py imsg_fork.py adk-release/ agentdesk-issue-pipeline/ agentdesk-relay-audit/ agentdesk-relay-e2e/ agentdesk-restart/ agentdesk-runtime-ops/ watch-agent-turn/ -->
<!-- doc-path-refs: proposed src/services/tmux_common/artifact_paths.rs services/session_name_reservations.rs -->
