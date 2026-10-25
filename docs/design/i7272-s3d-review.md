## 요약

S3d는 P0·P1 없이 **CLEAN**입니다. 다만 S4에서 owner scope를 운영 경로에 연결하기 전에 닫아야 할 P2가 5건 있습니다. 판정은 소스 정독에 의한 정적 판정이고 cargo는 돌리지 않았습니다. 106/0/0과 12/12는 보고서 수치이고, 원시 로그(WSL)는 보지 못했습니다. 크기는 `pr_cap_prod.py` 실측으로 prod 29파일 / 순 +1168이라 CAP PASS이고, giant 파일의 raw 증가도 0입니다.

1. **bootstrap ARM·deadline: 충족.**
   - `arm_bootstrap`은 deadline을 None으로, window를 Bootstrap으로 둡니다(`v2/attempt.rs:112-113`).
   - deadline은 `payload_started`가 Bootstrap일 때만 한 번 정합니다(`:123-145`). 이미 Started면 Ok만 돌려주고 deadline을 다시 쓰지 않습니다.
   - callback이 붙은 곳은 content 직전 네 곳뿐입니다.
     - `SubmissionTmux::send_literal/paste_buffer`(`submission_tmux.rs:79,105`)
     - GuardedPane의 Literal 직전, 그리고 load-buffer 뒤·paste 직전(`guarded.rs:294-309`)
     - Herdr의 `send_text/send_paste`(`host_input.rs:266,279`)
     - Codex argv create(`tui_session_launch/first_input.rs:40-47`)
   - callback이 붙지 않은 것도 확인했습니다: `load_buffer`, `send_keys`(C-s·Enter·Escape), capture, 그리고 `stash()`의 C-s effect(`guarded.rs:272`).
   - 재시작 처리도 맞습니다. adopt가 Bootstrap을 Unknown으로 낮추고(`v2.rs:51-60`), tick은 Unknown이면서 deadline이 없는 head만 보관합니다(`retained.rs:8-12`).
2. **PaneEpoch·lease: 대체로 맞습니다.**
   - `LiveEpoch::observe`(`target_epoch.rs:19-47`)는 다섯 가지를 검사합니다: spawn nonce marker가 Known인지, `execution_context`의 nonce·root·session·host가 맞는지, 그리고 실제 pin의 다섯 값(server pid, session, window, pane, pane_pid)입니다.
   - `condition`(`:51-67`)은 effect마다 marker를 다시 확인하고 tmux 포맷 조건을 덧붙입니다.
   - lease 획득은 모두 기다리지 않는 compare-exchange 시도라서 교착은 없습니다(`attempt.rs:58-64`). physical lease는 logical lease 위에 덧붙일 뿐 교체하지 않습니다(`:191-197`).
   - effect 도중 epoch나 physical 값이 바뀌면 거절하고 아무것도 지우지 않습니다(`owner_input.rs:83-84`, `first_input.rs:131-133`).
   - 운영 `DRAFT_GUARDS`의 동작은 같습니다. scope가 없으면 epoch가 항상 None이라 키가 `(session, None)` 하나뿐입니다(`composer_lock.rs:145-197`).
3. **callback 범위: 충족.**
   - commit, pin, hold callback은 모두 mailbox `v2_request` 한 번입니다: actor 안의 CAS와 disk commit(`inject/first_input.rs:116-157`). UI, composer, Source를 기다리지 않습니다.
   - actor는 composer lock을 잡지 않으므로, lock을 쥔 채 호출해도 다시 잡는 경로가 없습니다.
   - legacy 결과도 같습니다. scope가 없으면 `before_payload`/`payload`/`condition`/`observe_pin`/`request`/`argv_payload`가 모두 그대로 통과합니다(`owner_input.rs:47-133,250-267`).
   - `claude/first_input.rs`의 `submit_using` 추출도 sleep 주입 외에는 본문이 같습니다.
4. **production 비노출: 충족.**
   - `CURRENT`를 설정하는 곳은 `capture_scope` 하나이고 `cfg(all(test, unix))`입니다(`owner_input.rs:180-236`).
   - cold hook은 `inject/first_input`이고, 이 모듈 자체가 `cfg(all(test, unix))`입니다.
   - V2 owner는 여전히 `cfg(test)`이고, 새 wire 필드는 모두 `serde(default)`에 skip 처리입니다.
   - `NotSentProof`는 위조·복제할 수 없습니다. `cfg(test)`이고, 필드가 private이며, Clone·Serialize가 없습니다. 또 `into_reason`이 attempt 일치를 요구하고 `retain_not_sent`가 이를 소비합니다.
5. **이월: 보고대로 닫혔습니다.**
   - S3c P2-1: 네 실제 helper가 `v2_cold_bootstrap` 안에서 돕니다(claude, codex, 두 Herdr 시험).
   - S3c P2-2: Held·Guarded·소진 시험이 추가됐습니다.
   - S3c P2-3: `fn enqueue`가 private이 됐고, 가드가 "pub 없음"을 단언합니다.
   - S3c P2-4: 주석 위치를 옮겼습니다.
   - S3b P2-1: 접수 receipt로 확인합니다(`source_admission.rs:55-60`).
   - S3b P2-3: `OpaqueUnsent`와 `retained_retry_required` 알림으로 처리합니다.
   - S3b P2-4(b): HTTP fixture로 restore 성공 분기를 시험합니다.
   - S2b P2-2: 정산 시각을 `clock()`으로 다시 읽습니다(`queued_owner.rs:416`).
   - S2b P2-8(b): Codex 회귀 시험은 ID 하위 경로만 `v2_tests::`로 바뀌었고, 옛 ID 참조는 남아 있지 않습니다. lib/PG manifest와 CI에 새 ID로 등록됐습니다.
6. **잘못된 PASS: 소스로 확인할 수 있는 범위에서는 없습니다.** 뮤턴트 대상마다 고유 단언 문자열이 있습니다. 다만 실제 RED 여부는 WSL 로그 없이는 확인하지 못했습니다(미실행).

## 지적

### P0
없음.

### P1
없음.

### P2

**P2-1. callback이 붙지 않은 content 전송 경로가 남아 있습니다.**
- **근거:**
  - Claude `TmuxInput`의 `send_literal/paste_buffer`(`host_input.rs:176-192`)에 callback이 없습니다. 이 transport는 `run_legacy`(`:569-576`)가 쓰고, `run_legacy`는 다음 경로에서 도달합니다.
    - `inject_steering_prompt`(`input.rs:593-607`): owner `request`가 `steering: false`(`owner_input.rs:130`)라서 `steering_selected`가 None이 되고 legacy로 떨어집니다.
    - `send_compact_while_busy`(`input.rs:458,481`)
    - `legacy_write`(`tui_relay.rs:117,124`)
  - Codex `TmuxCodexInput`(`codex_tui/host_input.rs:75-90`)에도 callback이 없습니다.
  - owner scope 안에서 이 경로로 content를 보내면 commit이 없습니다.
    - deadline이 생기지 않습니다. Bootstrap에 deadline이 없는 head는 tick이 보관하지 않습니다(`retained.rs:8-12`).
    - `allowed`도 false로 남습니다. 같은 scope에서 뒤이어 BeforePayload hold가 나면, 실제로 타이핑된 입력에 `NotSentProof`가 만들어질 수 있습니다(`owner_input.rs:219-234`).
- **고치지 않으면:** S4에서 owner scope가 이 경로에 닿을 때 두 가지가 생깁니다. 하나는 head가 MayHaveSent로 남아 재시작 전까지 큐가 멈추는 것이고, 다른 하나는 KnownNotSent로 잘못 분류된 입력을 다시 보내 중복이 생길 수 있는 것입니다. 지금은 dormant입니다.
- **개선 방향:**
  - (1) 두 transport의 content 메서드를 `owner_input::payload`로 감쌉니다. 운영에서는 no-op입니다.
    - 대안은 `owner_input::active()`일 때 이 경로를 거절(fail-closed)하는 것입니다.
    - 권고는 감싸기입니다. 모든 content 전송이 한 지점을 지나므로 계약이 단순해집니다.
  - (2) source guard를 추가합니다: 모든 `impl InputTransport`/`impl CodexWrites`의 `send_literal`/`paste_buffer` 본문에 `owner_input::payload`가 있어야 한다.
  - (3) 시험: owner scope에서 `run_legacy(Literal)`를 부르면 Started와 commit 1회가 나와야 합니다. 뮤턴트: 감싸기를 제거하면 RED.

**P2-2. 경합으로 생긴 거절도 사람이 직접 재시도해야 하는 보관으로 끝납니다.**
- **근거:**
  - Herdr의 physical lease는 전역 이름 하나(`"herdr:conservative-writer"`)를 씁니다(`owner_input.rs:55`). `add_physical`은 기다리지 않고 바로 시도합니다(`attempt.rs:191-197`).
  - 그래서 두 채널의 Herdr cold 입력이 겹치면, 두 번째는 content 전에 `physical writer busy`로 거절됩니다.
  - 이 거절은 `known_before`가 되어 `NotSentProof`를 만들고, `retain_not_sent`가 그 입력을 큐에서 빼 retained와 `retained_retry_required`로 보냅니다(`retained.rs:85-112`).
  - tmux에서 effect 도중 epoch가 바뀐 경우도 똑같이 처리됩니다.
- **고치지 않으면:** S4 연결 뒤, 동시에 들어온 Herdr 첫 입력 중 하나가 "재시도 필요" 알림과 함께 멈춥니다. 확실히 보내지 않은 입력인데도 자동으로 전달되지 않습니다.
- **개선 방향:**
  - (1) 최소 계약: content 전의 경합 거절(lease busy, epoch changed)은 `retain_not_sent`가 아니라 head를 Reserved로 되돌려 같은 자리에서 다시 시도하게 합니다. 보관은 초안·hold처럼 사람이 판단해야 하는 사유에만 씁니다.
    - 대안은 lease 대기에 짧은 backoff를 두는 것입니다.
    - 권고는 Reserved 복귀입니다. lease 대기는 교착 분석을 다시 열어야 합니다.
  - (2) 시험: 두 Herdr cold effect를 동시에 돌리면 둘 다 Started가 되고 key가 각 1이어야 합니다. 뮤턴트: 경합을 `retain_not_sent`로 보내면 RED.
  - (3) S4 착수 조건에 넣습니다. Herdr namespace를 세밀하게 나누는 일도 같은 단위에서 합니다.

**P2-3. 접수 receipt가 계속 쌓입니다.**
- **근거:**
  - `admit_commit`은 접수마다 receipt를 `admission_receipts`에 영구히 추가합니다(`source_admission.rs:98-104`, `v2_types.rs:14-17`).
  - owner commit은 매번 snapshot 전체를 다시 쓰고 fsync합니다(`commit.rs`).
  - 하루 100건이면 1년에 약 3.6만 개, 약 1.8MB를 commit마다 다시 씁니다. 보고서도 선형 비용으로 공개했습니다.
- **고치지 않으면:** S4 연결 뒤 채널 snapshot 쓰기 비용이 시간에 비례해 커집니다. 지연이 늘 뿐 유실은 없습니다.
- **개선 방향:**
  - (1) receipt를 접수된 item이나 retained 항목에 붙입니다. 정산 뒤에는 terminal identity의 24시간 창 안에서만 유지하고 함께 지웁니다. 그러면 상한이 "큐 크기 + 24시간분"이 됩니다.
  - (2) 시험: 5000회 접수·정산 뒤 `admission_receipts.len()`이 `items + retained + 24h terminal` 이하인지 확인합니다. 이 상태에서도 Pending custody 확인은 계속 통과해야 합니다. 뮤턴트: 정리 로직을 제거하면 RED.
  - (3) 권고는 item에 붙이는 방식입니다. 별도 ring보다 수명이 정확합니다.

**P2-4. scope가 thread-local이라, effect 안에서 스레드를 옮기면 commit 없이 content가 나갑니다(fail-open).**
- **근거:**
  - `before_payload`는 scope가 없으면 `Ok`를 돌려줍니다(`owner_input.rs:48-50`).
  - effect가 `std::thread::spawn` 같은 것으로 다른 스레드에서 전송하면 scope가 없으므로 commit 없이 content가 나갑니다.
  - 시험(`owner_scope_claim_..._not_inherited_by_threads`)은 이 동작을 고정할 뿐 막지는 않습니다.
- **고치지 않으면:** S4에서 스레드를 옮기는 adapter가 생기면 P2-1과 같은 증상(deadline 없음, 잘못된 NotSent)이 납니다.
- **개선 방향:**
  - (1) logical writer lease를 쥔 target이 있을 때, scope 없이 그 target에 들어오는 payload는 거절합니다. lease registry로 조회할 수 있고, 운영 legacy에는 lease가 없으므로 동작이 바뀌지 않습니다.
  - (2) 시험: lease를 보유한 채 다른 스레드에서 `SubmissionTmux::send_literal`을 부르면 Err, key 0이어야 합니다. 뮤턴트: 이 검사를 제거하면 RED.

**P2-5. 위생(묶음)**
- **(a) legacy guard가 owner를 계속 막습니다.** owner scope에서는 legacy `(session, None)` guard가 있으면 무조건 Held입니다(`composer_lock.rs:163-167`). 그런데 그 guard를 지우는 것은 legacy 호출자의 Settled 판정뿐입니다(`:176-178`). legacy와 owner 경로가 섞인 S4 과도기에는 owner 쓰기가 멈출 수 있습니다.
  - 개선: owner 경로에서도 현재 capture로 legacy guard를 재평가하고, Settled면 제거합니다.
  - 시험: legacy guard + 빈 composer + owner admission이면 Any가 되고 guard가 제거돼야 합니다.
- **(b) Codex busy paste에는 epoch 조건이 없습니다.** callback만 붙었고(`codex_tui/busy_inject.rs:319-321`), Claude busy와 달리 if-shell 조건에 epoch가 들어가지 않습니다. S4 target resolver 단위에서 맞추기를 권합니다.
- **(c) Bootstrap에 시간 상한이 없습니다.** effect가 매달리면 head가 계속 MayHaveSent로 남습니다(장시간 정지 범주). bootstrap 예산을 넘기면 Unknown으로 보내는 상한을 3d 후속이나 S4에 두기를 권합니다.
- **(d) 게이트가 아직 끝나지 않았습니다.** static이 `SYNTH_MERGE_CONFLICT`로 PASS가 아니고, native(pid 3338045)도 판정 전입니다.
  - native는 Windows-gnu cross를 포함하므로 `owner_input`의 `cfg(unix)` 분기가 cross 빌드를 통과하는지도 여기서 확인됩니다.
  - 운영 lib 빌드에서 `Scope`가 생성되지 않는 구조라 dead-code 경고가 나는지는 이 리뷰에서 실행해 보지 않았습니다.
  - 코디네이터가 통합하고 공식 생성기로 재생성한 뒤 다시 돌려 확인해야 합니다.

## 새 방향 제안

없음. 운영에서는 아무것도 하지 않고 scope가 있을 때만 commit하는 현재 구조가 계획(§1.4, 3d)과 맞습니다. S4 전에 P2-1과 P2-4의 "모든 content는 한 지점을 지난다"는 가드만 갖추면 됩니다.

VERDICT: CLEAN
