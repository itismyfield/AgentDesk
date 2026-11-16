# Native membership receipt 생산자 — #8148 설계 델타

기준: 보존 커밋 `c307ea4d7846d3df09ae472dfdf69ed1d46f8290`(착지 후보 아님).
상위 계약은 [native attribution 설계](native-attribution.design.md)와
`report-plan-b2b3-1.md`의 단일 O 공급 경로다. 이 문서는 구현 승인이 아니라
**membership 관측을 기존 v3 Event receipt로 확정하는 새 권한**의 리뷰 대상이다.
Herdr Idle native 정산/활성화를 끄는 안은 채택하지 않는다(mac-1408).
새 O native port만 사용하며 legacy tmux 전용 sender/receipt polling 경로는 바꾸지 않는다.

## 반례와 바꾸지 않는 경계

일반 Herdr Idle 입력은 실제 paste/Enter 뒤 Submitted가 된다.
O projection은 같은 source의 own membership + strict closer로 Executed 후보를 만들지만,
`history/v2/native.rs:633-641`은 그 UUID의 확인 receipt가 없으면 결과 저장을 거절한다.
`landprep4-traced-pinless-{0,1}.txt`에서 `native executed receipt unconfirmed`가 반복됐다.
source/nonce/actor/row 검사를 제거하지 않았으며 fixture-only receipt도 넣지 않았다.

**receipt는 전달 증거, strict closer는 실행 종료 증거**다. 두 단계를 합치지 않는다.
새 생산자가 receipt를 저장해도 결과·lease·row·counter·admission·wake를 바꾸지 않는다.
결과 CAS의 기존 membership UUID receipt 검사, native actor/row CAS와 cutoff는 그대로 둔다.
receipt 부재를 foreign-prompt barrier로 간주하거나 일반 Idle abstain으로 우회하지 않는다.

## 생산자와 진입점 전수표

좌표는 보존 커밋 기준이며, “추가”는 구현 계획이다.

| 현재 진입점(file:line) | 현재 증거/역할 | 추가할 연결 |
|---|---|---|
| `src/services/tui_o/shadow/capture.rs:245` `reopen_with_prefix` | 같은 FD의 기존 0..cursor hash pass, hash/identity 검증 | 읽기 추가 0. 검증 전 receipt 외부 공개 0 |
| `src/services/tui_o/writer/input_facts/native.rs:260,277` `Projection::reopen` | hash가 맞고 complete coverage인 projection만 반환 | 기존 prefix decoder에서 receipt용 소형 hash/identity fact도 만들고, 검증 성공 뒤에만 공개 |
| 같은 파일 `:302,319` `append/read` | 연속된 durable CapturedRecord와 이미 파싱한 JSON | 동일 decode에서 UUID/kind/range/정확한 frame digest 추출. 원문 body/JSON 보존 0 |
| 같은 파일 `:238,482` `Fact/reduce` | compact facts와 등록/receipt snapshot으로 B1 의미 재평가 | 실행 후보와 별개인 membership receipt 후보. strict closer·Executed 후보가 없어도 확인 가능 |
| `src/services/tui_o/writer/rotation/owe.rs:319,453` | live append와 boot seed가 같은 source owner에 합류; ticket별 origin 선택 | 같은 facts → receipt offer/ack → snapshot 재평가 → 기존 barrier/result offer 순서 |
| `src/services/tui_o/writer/actor/native_input.rs:15-43` `EvidencePort` | 기존 snapshot/offer/abstain/retry 포트 | receipt 전용 typed method 하나; 기존 O actor/host 경로 사용 |
| `src/services/discord/turn_finalizer/native_supply.rs:105,319,453` | registry snapshot, 실제 supply, generation/binding 검증 | receipt offer의 domain/full-ticket/AnyKey/current stamp 검증. terminal actor에 receipt 쓰기 메시지를 넣지 않음 |
| `src/services/tui_input/human_frame/history/v2/native.rs:522` | 분류 snapshot은 outcome/cleanup과 별개로 알려진 등록 유지 | 기존 class hash 옆에 **등록의 exact Frame.hash** 노출. source=None도 불확실한 경쟁 후보로 유지 |
| `src/services/tui_input/human_frame/history/v2.rs:578-640` `Registry::receipt` | full registration/Entry, event owner, collision, Event 기록의 기존 규칙 | 동일 보호를 새 native receipt transaction에서 지킴. 범용 receipt의 의미·서명 변경 0 |
| `src/services/tui_input/human_frame/history/v2/native_control.rs:246-319` | try-mutex/try-file-lock, unconfirmed publication의 durability 재시도 | 같은 보호를 parent 범위에서 재사용. 다른 registry transaction으로의 범용 try 모드 추가 0 |
| `src/services/tui_input/human_frame/history/v2/native.rs:587-659` | 결과 CAS의 fingerprint/UUID receipt/first-result 검증 | **변경하지 않음** |
| `src/services/discord/turn_finalizer/native_restore.rs:30,72,94` | old-generation full registration adoption 및 saved result 소비 | 새 receipt를 생성한 뒤에도 기존 old-ticket 생성자/정산 경로를 그대로 사용 |

## typed 값과 확정 조건

제안하는 `MembershipReceipt`는 **struct**이며 새 enum/disposition이 아니다.
producer leaf의 private factory만 만들고 외부 JSON에서 Deserialize하지 않는다.
read-only accessor를 제외하고 임의 생성자를 공개하지 않는다.

| 묶는 값 | 실제 출처 | transaction의 확인 |
|---|---|---|
| namespace + full Ticket | 실제 v3 registration snapshot | key 전체, extra, owning_episode, channel, attempt nonce/provider/target/source 전체 일치 |
| 계정 domain | 기존 `queue_persistence_context` → `queued_head::v2_queue` (`v2/actor.rs:81-94`) | 수신 Port의 provider/token namespace와 일치. 채널 번호만으로 계정 소유 추론 0 |
| SourceId와 ticket.source | 인증된 source owner + bound registration | provider/session/path/dev/ino, target owner/root/execution_nonce, pre-write offset 일치. source=None은 확인 불가 |
| binding seq + generation | ticket별 `native_origin`의 Bound/Resolved와 기존 Observation | N1/N2 각자의 실제 binding. 항상 최초/최신 binding 선택 금지. observer generation으로 old episode generation을 덮지 않음 |
| UUID + kind + range | 실제 user/queued-command 행과 CapturedRecord 경계 | UUID 비어 있지 않음; `source.offset <= start < end <= through`; newline까지 완전한 행. queue remove/dequeue/closer의 UUID를 user receipt로 사용하지 않음. B1과 같은 outer row UUID만 사용 |
| frame hash | **관측한 텍스트**의 SHA256 | 등록 `Frame.hash`와 일치. 등록 hash를 관측값인 것처럼 복사하지 않음. class_hash 일치만으로 승인 0 |
| row digest | 동일 capture line(마지막 LF 포함)의 digest | 같은 observed/current receipt fact의 UUID/range/digest 일치. raw JSON 추가 보존 0 |
| capture stamp | SourceId, binding seq, revision, 실제 through, registrations_fingerprint | observed/current 전체 일치 후, registry lock 안에서 registration fingerprint 재검증 |
| AnyKey | 기존 durable native settlement | true만 승인. 실제 key와 ACK 사이에는 확인 보류 후 기존 poll에서 재평가; 생산자가 AnyKey를 쓰지 않음 |

텍스트는 문자열 또는 **모든 블록이 text인 배열**만 허용한다. non-text를 버려 일치시키지 않는다.
정확한 원문과 `busy_inject::pasted_inner`가 허용하는 한 개의 온전한 paste wrapper 안쪽을
각각 hash한다(`busy_inject.rs:54`, `busy_inject/receipt.rs:14-42`). 임의 trim/부분문자열/새 정규화는 없다.
기존 native `frame_hashes`의 class 정규화는 분류용으로 남고 receipt 권한과 구분한다.
`isMeta/isCompactSummary/isSidechain` 및 다른 session/parent source는 거절한다.
queued-command는 확인 가능한 prompt/human 행만 새 receipt 대상으로 한다. 모호한 구형 행은 추측하지 않는다.

exact Frame.hash를 snapshot에 추가할 때 `#[serde(skip)]`로 **기존 registrations_fingerprint의 직렬화
형식은 유지**한다(`native.rs:13-28`). receipt 확인 stamp는 `(기존 fingerprint, 관측 exact hash)`이고
후자는 current capsule 및 registry lock 안의 등록 Frame.hash와 직접 비교한다. 저장된 barrier/saved
proof의 구 fingerprint를 새 알고리즘으로 바꾸지 않는다. receipt Event 추가는 기존 receipts 배열을
바꾸므로 기존 fingerprint가 정상적으로 변경된다. 옛 fingerprint byte 일치 시험을 필수로 둔다.

### 포트 모양(구현 계획)

기존 `Observation`에 `receipts: Vec<MembershipReceipt>`를 추가한다. capsule은 namespace/full Ticket,
행 UUID/kind/range, 관측한 exact frame hash와 row digest를 갖고, source/binding/generation/through/stamp는
Observation과 함께 전달한다. 진단 Serialize는 가능하되 Deserialize/외부 생성은 허용하지 않는다.
`EvidencePort::receipt(observed, current, index, origin) -> Future<Result<(), String>>` 하나를 추가한다.
`Ok(())`는 같은 Event가 이미 있던 경우까지 포함한 **durability 확인 ACK**이며 종결 ACK가 아니다.
거절/경합/flush 불명확은 Err다. current 안에 full capsule이 동일한 후보가 있어야 하며 UUID만 비교하지 않는다.
이미 정확한 receipt가 있는 행은 다음 snapshot에서 새 receipt 후보로 내보내지 않아 무한 ACK 루프를 막는다.
new method의 기본 구현은 fail-closed이고 실제 Discord Port만 수신을 구현한다.

현재 `EvidenceRegistration`에는 AnyKey가 없으므로 factory는 관측 후보만 만든다. AnyKey의 최종 권한은
Port/registry CAS가 **durable settlement를 새로 읽어** 확인한다. 임시 key 관측이나 mailbox busy를
그 대신 사용하지 않는다. 이 확인 실패는 원문 facts를 버리는 일이 아니며 ACK 뒤 같은 facts로 다시 시도한다.

### 경쟁 후보와 durable CAS

1. 후보 생성은 실행 결과/closer/barrier의 성립 여부를 기다리지 않는다. 등록·AnyKey·실제 membership의
   일치가 근거다. receipt가 먼저 있어야 foreign/self 분류가 확정되는 follower의 순환 대기를 막는다.
2. **후보 집합을 outcome/cleanup 또는 r.candidate로 필터하지 않는다.** 같은 source/execution의
   알려진 receipt는 결과 뒤에도 분류 근거다. source=None 미확정 등록과 ACK를 기다리는 같은 hash의
   등록도 가능한 rival로 남긴다. 명시적인 abandoned/known-not-sent 증거 없이 rival을 지우지 않는다.
3. 정확히 하나의 full namespace/key가 행을 소유할 때만 확인한다. 같은 파일의 UUID가 다른 key의
   user/command receipt로 이미 점유됐거나 twin 소유가 불명확하면 새 Event 0이다.
   같은 key의 동일 receipt는 idempotent다. 같은 UUID가 다른 실제 range에 반복되면 확인하지 않는다(동일 range 재관측과 구분). 기존 `collision::contested` 보호를 약화하지 않는다.
4. 수신 Port는 registry I/O 전에 binding/domain/capture 및 full registration을 확인한다. receipt
   확정에는 **역사적 등록**의 권한만 필요하며 현재 mailbox가 busy라는 이유로 primary 권한을 만들지 않는다.
   outcome/cleanup이 끝난 등록에도 진짜 늦은 receipt는 추가할 수 있으나 terminal 결과는 불변이다.
5. registry worker는 try-mutex/try-file-lock 안에서 full Ticket/source/hash/AnyKey/fingerprint와 event owner를
   다시 검사한다. abandoned/known-not-sent에는 새 receipt를 쓰지 않는다. 기존 durability 경로로
   flush가 확인된 **기존 Event**만 ACK한다.
   lock busy/corrupt/unknown schema/unconfirmed flush는 Err이며 Event 성공으로 세지 않는다.
6. actor/Watch/O mutex를 잡은 채 registry lock이나 PG를 기다리지 않는다. synchronous filesystem 작업은
   Port의 `spawn_blocking` 작업에서 수행하고 async ACK로 회수한다. finalizer/mailbox actor에는 blocking 작업을
   넣지 않는다. 새 observer/전역 task/retry loop 없이 다음 기존 O poll이 보유 facts로 재시도한다.

저장은 기존 Registration/Source/Frame/Entry/Event뿐이다. Event의 `key/kind/row` 형식을 바꾸지 않는다.
full identity/source/frame hash는 registration/entry에, UUID/kind는 Event에 묶인다.
range/row digest/capture stamp는 확인 transaction의 전제이며 별도 journal에 쓰지 않는다.
receipt-only crash 뒤에는 같은 O prefix를 다시 인증하여 range와 digest를 재구성한다.
이미 저장된 Event가 있다는 이유로 prefix/source 검증을 건너뛰지 않는다.

## 순서와 fingerprint

`기존 O capture → authenticated receipt 후보 → receipt transaction/flush ACK → 새 snapshot으로 같은 facts 재평가 → NativePrepared::admit → 결과 durable CAS → actor CAS → exact row CAS → cleanup admission → QueueEligible → owner`

receipt가 추가되면 registrations_fingerprint가 바뀐다. **observed/current를 모두 폐기하고** 기존
projection의 같은 facts를 최신 snapshot으로 재평가한다. 단지 fingerprint 문자열만 덮어쓰지 않는다.
그 poll의 나머지 낡은 result/barrier offer도 사용하지 않는다. source open/read/cursor reset은 0이다.
동일 offer의 ACK 유실/중복은 Event dedupe로 흡수하고 기존 facts 재평가로 계속한다. 새 lost-wake 감시·재생은 없다.
receipt ACK만으로 `CompletionAdmissionPlan`, global_active 또는 다음 head를 바꾸지 않는다.

### ACK 불명확과 기존 pending result의 재시도

`Registry::inspect`(`v2.rs:533`)는 파일에 보이는 Event를 읽을 뿐 publication의 pending flush를
확인하지 않는다. rename 뒤 flush 실패로 Event가 보일 수 있으므로 **존재만으로 확인 ACK로 취급하지 않는다.**
새 receipt worker의 성공 ACK 외에도, receipt에 의존하는 결과 제출과 새 barrier 저장은 actor 밖 worker에서
기존 `try_native_settlement(namespace, full_ticket)`을 먼저 통과해야 한다.
이 메서드의 `try_control_transaction`은 pending candidate 및 **변경 없는 snapshot도**
`retry_published_status == Flushed`를 확인한다(`native_control.rs:270,300`).

연결 지점은 `native_supply.rs::Port::submit`(`:159`, live/boot 모두)이며 **기존 pending job의 매 재시도**도
포함한다. 새 barrier의 `record_native_barrier` 앞(`native_supply.rs:258`)에도 같은 preflight를 둔다.
따라서 receipt ACK가 불명확해 다음 snapshot에서 후보가 사라져도, Event의 durability가 확인된 뒤에만
그 Event에 의존하는 result/barrier CAS로 간다. preflight는 receipt Event를 새로 쓰거나 ACK 문자열을
위조하는 경로가 아니다. 이미 durably 저장된 outcome/barrier의 기존 정리 권한에는 unrelated receipt
때문에 새로운 전역 차단을 얹지 않는다.

새 pending map/enum/journal이나 범용 V2 transaction 모드는 필요 없다. exact native mutation의 기존
try-settlement 포트를 재사용한다. flush 실패/lock busy면 후보와 기존 job을 보존하고 기존 poll에서 재시도한다.
결과/barrier CAS가 guard 사이에 바뀐 fingerprint/first-result를 다시 검사하는 보호도 그대로다.

```mermaid
flowchart LR
    O[기존 O의 인증된 prefix] --> M[user/command membership]
    M --> R[receipt CAS와 flush ACK]
    R --> S[새 snapshot으로 같은 facts 재평가]
    O --> C[own run의 strict closer]
    C --> S
    S --> X[receipt와 closer를 확인하는 결과 CAS]
    X --> A[기존 actor·row 정리와 owner 진행]
```

## live·boot·재시작·incarnation

| 경계 | 처리 |
|---|---|
| live 첫 key 전 | source/receipt 부재로 Executed 합성 0. 기존 준비 거절/NotSent unwind 그대로; receipt는 AnyKey를 만들지 않음 |
| key 뒤 receipt 앞 | O가 이미 읽은 user/command fact를 보유. 뒤늦은 등록/bind/AnyKey ACK 뒤 같은 facts 재평가 |
| receipt 저장 앞 crash | prepare-before-reap의 full registration custody 보존 → reaper → restore. 기존 reopen hash pass에서 complete prefix를 재구성하고 receipt 확인 |
| receipt 저장 뒤 결과 앞 crash | Event 포함 full registration/Entry가 기존 boot custody에 보존됨. `RestoredInput::refreshed`의 merge/adoption으로 Event를 잃지 않음. source proof 재인증 후 Event 중복 추가 0 |
| 결과 저장 뒤 actor/row 앞 crash | receipt를 다시 만들지 않고 기존 saved outcome/old-row CAS 소비. 결과 1, exact 옛 row만 정리 |
| old debt와 N2 resume 공존 | 같은 SourceId여도 full ticket의 execution별 Bound/Resolved를 선택. old episode generation 보존, N2의 lease/row/counter 변경 0 |
| 기존 source 파일 부재/손상/hash 불일치 | 새 receipt·빈 성공 합성 0. 기존 custody-only saved-result/control 경로만 소비 가능. 영구 증거 부재에는 liveness를 주장하지 않음 |
| legacy/future schema | 기존 migration/decode의 fail-closed 유지. 새 persistent field가 없어 reader-first/rollback floor 변경 없음 |

준비→custody→reap→restore 순서는 `runtime_bootstrap/native_input.rs:4-35` 그대로다.
새 source 재개방이나 다른 reader는 없다. boot와 live는 동일 factory/registry 수신 규칙을 쓴다.
이 설계의 liveness 전제는 정상적인 Idle 입력의 full registration/AnyKey, 인증 가능한 complete-prefix,
유일한 소유자, 결국 회복하는 저장소, 기존 O/owner scheduler의 공정성이다. 시간은 종료 증거가 아니다.

## 인접 경로와 결과의 우선순위

| 진입점 | receipt와의 경쟁 계약 |
|---|---|
| Cancel: `episode_identity.rs:300` → `policy_close.rs:72` | control 결과가 먼저면 늦은 receipt는 역사적 확인만 추가. Cancelled를 Executed로 바꾸지 않음 |
| Terminate: 같은 파일 `:442` | token/row 삭제의 기존 admission 유지. receipt가 새 cleanup 권한을 만들지 않음 |
| clear: `clear_channel.rs:141`와 V2 owner 위임 | Release의 native admission·read-only 거절/signal 보존, #8153 owner clear_all 의미를 함께 유지. Preserve는 receipt debt를 삭제하는 우회가 아님 |
| stop: `episode_identity.rs:729`, `discord/commands/stop.rs:52,98` | 기존 native stop/control 증거와 first-result CAS가 결정. receipt는 stop 또는 closer가 아님 |
| operator release: `discord/turn_lease.rs` → 기존 mailbox finish/control | 새 명령 없이 기존 경로만. successor가 정리 권한을 승계했으면 기존 debt 종결 규칙을 유지 |
| 공통 소비: `turn_finalizer/native_policy.rs:132,155,225` | 같은 registry lock/CAS 직렬화. 결과 먼저/receipt 먼저 둘 다 원본 결과 1. 비소유자의 lease/row/counter/wake 변경 0 |

모든 위 Orchestrator 좌표는 `src/services/turn_orchestrator/` 아래다. receipt write 자체는 row 삭제·GC를 하지 않는다.
정상 terminal/GC가 native outcome·cleanup·custody 확인 전에 boot debt를 지우지 못하는 조각 2 계약도 유지한다.
P2-1 최신 binding 검사 뒤 Idle 경합은 사용자 보류 그대로이며, 새 atomic binding 보장을 주장하지 않는다.

## 입력 출처

| 출처 | ADK 소유 판단 | 새 receipt |
|---|---|---|
| Discord 사람 입력 | 준비된 full namespace/key + 물리 AnyKey + 실제 membership | 허용 |
| AgentDesk API/machine이 같은 owner 경로로 쓴 입력 | business 출처와 무관하게 자신의 준비 ticket/실제 TUI 행으로 판단 | 같은 계약으로 허용; dispatch 완료를 별도로 합성하지 않음 |
| AgentDesk routine가 지원하는 owner 경로로 쓴 정상 typed 입력 | routine provenance를 가진 full ticket/실제 TUI 행 | 같은 계약. routine 실행·분석/외부 job 성공은 따로 확정하지 않음 |
| 직접 TUI human typed | ADK 준비 ticket 없음 | 외부. native FIFO/barrier 분류에는 남지만 ADK receipt 0 |
| Claude scheduled task/background notification | ADK 준비 ticket 없음, meta/machine command | 외부. 본문이 닮았다는 이유로 승격 0 |
| ADK 입력의 mid-turn 가입 | 자기 full identity/UUID/hash/receipt | receipt만 확인. strict closer 전 Executed 0 |
| source=None 또는 twin/UUID 점유 불명확 | 소유 미확정 | 확인 보류. 첫 후보 선택·시간 성공 0 |

## 구현 파일과 크기 경계(계획)

| 파일 | 순 코드 추정 | 역할 |
|---|---:|---|
| `writer/input_facts/native/receipt.rs` **신설** | +85~120 | exact text/hash fact 및 opaque membership receipt 후보 factory |
| `writer/input_facts/native.rs` | +15~25 | 같은 decode의 fact 연결, parent 선언·receipt 재평가 |
| `writer/rotation/owe.rs` | +15~25 | ticket별 origin → receipt ACK → snapshot 폐기/재평가 |
| `writer/actor/native_input.rs` | +8~15 | 기존 EvidencePort의 typed receipt method |
| `human_frame/history/v2/native_receipt.rs` **신설** | +75~105 | full-ticket/owner/hash/stamp/Event dedupe/durability transaction |
| `human_frame/history/v2/native.rs` | +5~10 | exact Frame.hash snapshot, parent 선언 |
| `human_frame/history/v2/native_control.rs` | net 0 | try transaction의 parent 범위 재사용만(일반화 0) |
| `discord/turn_finalizer/native_supply.rs` | +45~60 | domain·live/old-ticket 검증, receipt ACK 및 result/barrier 재시도의 durability preflight |
| **계** | **8파일 / +248~360, 계획치는 +250~360** | 새 giant 본문 로직 없음. tests/constructor 적응 별도 |

별도 FILES 승인 후보는 신설 leaf 2개다. 기존 parent의 mod/use 선언은 FILES_AUTO 범위로 기록한다.
시험·컴파일·Windows 여부는 **구현 뒤 확인 필요**이며 이 설계 레그에서는 cargo를 실행하지 않는다.

크기 상수/기준선은 바꾸지 않는다. WIP까지 공식 원문 누계는 main 병합분을 포함한 **106파일/+5348**,
79aa 이후는 **3파일/+128**이다. 이전 조각 4만의 기록 +976에 후속 +128을 더한 **계획용 소계 +1104**는
main 병합을 제외해 보는 참고값이며 공식 CAP 측정을 대신하지 않는다. +1210까지 참고 여유 106보다
새 receipt +250~360이 크다(소계 +1354~1464). 따라서 **기존 +1210 경계 안에 수리 전체가 들어간다고 주장하지 않는다.**
구현 착수 전 코디네이터가 receipt를 새 리뷰 조각(base=WIP, 8파일/+250~360)으로 승인하거나 경계를
다시 정해야 한다. 새 조각 자체는 30파일/+1500 및 +1100 정지선 안이다. WIP는 계속 미완이며
혼자 착지하지 않는다. 기존·새 조각 전체의 회귀/정산/e2e를 통과한 뒤 함께 착지한다.

## 구현 레그 시험 계획(이 레그 실행 0)

| 시험 이름/진입점 | 단언 | 원인 제거 뮤턴트 |
|---|---|---|
| 기존 pinless Idle 2건 / 실제 start_idle→O→Port | fixture receipt 삽입 없이 Event 1, Executed 1, lease/row 없음, counter 0, owner 진행 1 | receipt offer 제거; row 연결 제거 |
| `native_8148_receipt_before_closer` / Projection→Port | user membership receipt 1, closer 전 Executed 0 | receipt를 result로 직접 승격 |
| `native_8148_receipt_after_full_tail` / 실제 O poll | trailing metadata/closer가 있어도 실제 through 검증 유지, 결과 1 | 마지막 fact 끝으로 through 회귀 |
| `native_8148_receipt_full_identity_rejected` / registry ingress | namespace/full Ticket/source/UUID/range/hash/AnyKey/stamp 각 축 오염은 write 0 | guard를 한 개씩 제거 |
| `native_8148_receipt_hash_changes_without_fingerprint` / snapshot 뒤 registry CAS | 기존 fingerprint가 같아도 current exact Frame.hash가 다르면 Event 0 | current exact hash 비교 제거 |
| `native_8148_receipt_exact_frame` / 같은 decode | 원문/정확한 wrapper 긍정; class-only·부분문자열·mixed/nontext/meta 음성 | class hash 대체/블록 필터 완화 |
| `native_8148_receipt_twin_and_occupied_uuid` | None-source rival·다른 key UUID 점유는 write 0; 같은 key 재시도 write 1 | collision/owner dedupe 제거 |
| `native_8148_receipt_then_reobserve` / actual receipt→result CAS | receipt 추가 뒤 낡은 result/barrier stamp 사용 0, 재독 0, 새 stamp 정산 | stamp 갱신만 하거나 재평가 제거 |
| `native_8148_receipt_follower_result_order` | B 결과 CAS/cleanup이 먼저여도 A 분류 유지; A→B/B→A 모두 1, 부모 권한 불변 | r.candidate 필터 재사용 |
| `native_8148_receipt_boot_crash_windows` / prepare→reap→restore | receipt 앞/뒤·result 뒤 crash, Event/결과 각 1, old row만 정리, 후속 head 1 | Event custody 유실·old-gen 승격 |
| `native_8148_receipt_live_boot_n1_n2` | 같은 SourceId의 N1 debt/N2 입력을 각자 binding으로 확인 | 항상 최초/항상 최신 binding |
| `native_8148_receipt_control_races` / 각 control 실제 입구 | control/result 선후 모두 결과 1, historical receipt가 결과/권한을 바꾸지 않음; Clear 거절 signal/owner 의미 보존 | control 결과 덮기·release를 receipt에 연결 |
| 기존 Stop-only/agent-manager-cc isolated | receipt는 own user만. 외부 closer Executed 0, 원본 재주입 0, 유효 Idle 뒤 head 1 | barrier 제거·receipt를 closer로 사용 |
| `native_8148_receipt_on_resource_proof` | ON reader/open/prefix byte 증가 0, 등록/receipt 갱신 재독 0 | 재평가에서 source 재개방 |
| `native_8148_receipt_reader_compat` / old v2/v3·saved barrier | snapshot exact hash 추가 전후 기존 fingerprint bytes 동일, Event/old Ticket/old saved result 보존; unknown schema는 오류 | exact hash의 serde skip 제거·receipt 소실 |
| `native_8148_receipt_visible_unconfirmed` / Port submit·barrier | inspect에 Event가 보여도 flush 실패 중 result/barrier/cleanup 0; 새 Registry·기존 pending job 모두 guard 재시도 뒤 1회 | try-settlement durability preflight 제거 |
| `native_8148_receipt_busy_or_unflushed` | try-lock/flush 실패 시 성공 ACK 0, 같은 guard로 재시도, Event 중복 0 | unconfirmed 성공 처리 |

기존 isolated e2e의 leaf가 `history.receipt()`를 직접 호출하던 부분은 실제 생산자가 확인하게 바꾼다.
원문/UUID 일치 단언은 보존하고, receipt 확인 단언을 AnyKey ACK 뒤 O worker의 실제 확인 결과로 옮긴다.
leaf 안에서 receipt를 기다려 AnyKey ACK와 순환 대기하지 않는다. 같은 reducer를 두 번 호출한
일치만으로 증명하지 않고 B1 batch 및 실제 Event/actor/row/owner 진입점을 대조한다.

기존 정본 828 ID 집합과 새 시험을 모두 실행해 누락/중복/PG_SKIP 0을 확인해야 한다.
13개 exact 및 TMPDIR=/tmp, 실제 PG test-threads=1, isolated herdr e2e 3갈래와 PID 0 정리,
원인 뮤턴트별 독립 patch/selector/바이트 원복, no-run·Windows·clippy·fmt·static도 구현 레그 몫이다.
WARN/lost-wake/replay는 사용자 축소 그대로 제외다. 새 native checkpoint/journal도 없다.

## 상태 차트와 증명의 한계

`NativeInputPreparation`의 기존 HostedBefore/Fixed를 metadata → created key → authenticated capture →
receipt 생산/flush → 새 snapshot → strict closer/result CAS → row/wake/head로 확장했다.
신규 Hosted receipt 26cfg와 기존 164cfg, 총 190cfg의 기대 집합을 검사한다.
Idle 입력의 의무는 `~>`로 검사하여 receipt-only crash 뒤 재생긴 의무도 덮는다. physical input/head 및 receipt/result write는
대입 상수가 아니라 실제 전이마다 카운터를 증가시킨다. receipt write 카운터는 하나의 물리 행(UUID/kind/range)별
중복 확인을 세며, 한 입력에 서로 다른 command/user 행이 생기는 것까지 금지하는 값이 아니다.
Fresh CAS 비교는 결과를 쓰는 순간의 current version을 ghost 관측값으로 잡아 검사한다. 결과 뒤 다른
정상 receipt가 추가되어 registry fingerprint가 바뀌어도 이미 저장한 결과는 무효화하지 않는다. boot는 durable AnyKey/Event를 보존하며
same-ticket의 old row debt를 다시 소비한다. Control은 기존 다섯 연산의 종결을 추상화한 값이며 새 runtime enum이 아니다.

모델 값은 위 전수표의 ticket N1/N2, snapshot stamp, 실제 complete-prefix 여부/identity guard,
Event 존재·durability, actor/row의 0/1, 물리 주입·다음 head·result/receipt write 관측 횟수에서만 취했다.
`hValid`는 full identity/hash/prefix/AnyKey 검증의 묶음이며 소스 코드의 guard를 대신하는 증거가 아니다.
invalid/twin 증거에는 성공 liveness를 주장하지 않는다. 정상·유일한 Idle 입력에는 receipt 없이
Pending으로 고착되지 않고 결국 정산/다음 head로 진행해야 한다. 영구 저장소 장애나 source 부재는 전제 밖이다.

수정 전의 실제 결함은 **live/boot receipt 생산자 없음 → Pending 고착** 2cfg다.
나머지 Before는 제안한 guard를 뺀 설계 뮤턴트이며 현재 Rust에 그 결함이 있다는 주장이 아니다.
결과 표와 runner 원문은 설계 보고서에 둔다. TLC의 GREEN은 설계 모델 증거이며 미구현 Rust 시험의 PASS가 아니다.


## 설계 리뷰 P2-1 구현 전 모델 보강

`hReceipt`는 파일에 보이는 Event publication, `hDurable`은 별도 flush 확인 ACK다.
HostedReceipt가 먼저 publication/version을 바꾸고 snapshot은 그 Event를 볼 수 있다.
HostedReceiptAck 전에 새 submit·기존 pending submit·새 barrier admission을 허용하지 않는다.
HostedReceiptDurable은 결과 또는 새 barrier가 durable 확인에 의존함을 검사한다.
기존 durable 결과의 정리에는 unrelated receipt의 미확인 publication을 새로운 전역 guard로 얹지 않는다.
이 구분은 runtime 저장 형식이나 상태를 추가하지 않는 모델 관측값이다.
기존 durability 쌍과 pending/barrier/saved 각 Before/Fixed로 검증한다.

P2-1 보강 뒤 Hosted 28cfg는 14 Before RED/14 Fixed GREEN, 관련 전체 190cfg는 expected 위반 집합 일치(rc0)다. pending 재시도와 새 barrier의 durable guard 제거는 각각 HostedReceiptDurable을 깨고, durable 결과 정리에 unrelated flush를 요구하면 HostedIdleEventuallySettles가 깨진다.
