# #7308 RCA 및 수리 계획 r1

- 조사 기준: `71577b574c91e3eec7e576a123dbf01dba4ee9eb`, `fix/i7308-reader-growth`.
- 조사 시각: 2026-10-24 04:49~05:04 KST / 10-23 19:49~20:04 UTC. 아래 코드 좌표는 조사 기준 SHA다.
- 단계: **계획만 작성. 구현·독립 설계 리뷰·빌드·운영 변경 없음. 다음은 코디네이터의 WebGPT pro 설계 리뷰다.**
- 이슈 정본: https://github.com/itismyfield/AgentDesk/issues/7308 . 원문은 `logs/issue-7308.json`.

## 결론

새 reader가 생기는 실제 경로는 **E2E `!clear` → 다음 fresh Codex 실행 → 서로 다른 native rollout의 Startup binding → writer attach**다. 같은 source의 정상 dcserver 재시작 복원이 reader를 하나 더 만드는 현상은 관측되지 않았다. 로그의 재시작 직후 경보는 누적된 reader를 다시 센 결과다.

근본 원인은 Codex 실행 종료 증거의 단계별 구현 중 **writer 소비 단계(A2b)가 아직 없음**이다. main에는 A1 End 생산(#7258/#7287), A2a Seal 생산(#7300)이 있지만, writer는 Codex의 provider `SessionStart/Clear`만 proof로 인정한다. 실제 managed clear 후 binding은 `Startup`, `hook_event=null`이다. #7284는 Claude 전용이므로 이 Codex 채널을 고치지 않는다.

조사 중 외부에서 #7300 배포·E2E 실행이 진행되어 더 강한 직접 증거도 얻었다. UTC 19:58:43에 **Seal(seq142)은 durable 저장됐는데, 그 정확한 old→new rotation의 proof는 여전히 없고 cursor는 미은퇴**다. End 생산 실패만 고쳐서는 이 이슈가 끝나지 않는다.

권고: (A) End+Seal을 기존 writer 은퇴 게이트에 연결해 앞으로의 증가를 막는다. (B) 증거가 없는 과거 backlog는 #7268의 기존 **비최신·마지막 대화 후 7일·idle·정산 완료** 자동 정책을 Codex에 적용해 정리한다. 두 경로는 별도 증거와 audit를 사용한다. 과거 End에 Seal을 소급 생성하거나 Startup을 Clear로 취급하지 않는다.

## 수용 기준 대조

| 이슈 원문 기준 | main의 현 상태 / 이번 계획 |
|---|---|
| “재시작마다 reader가 늘어나는 경로를 file:line으로 특정한다.” | 이번 RCA로 특정. 재시작 자체와 재시작 사이 E2E fresh 교체를 구분했다. |
| “정상 재시작과 E2E 셀 재실행에서 reader 수가 늘지 않는다.” | 동일 source 복원은 이미 main에서 중복 추가하지 않는다(`rotation.rs:658-668`, `binding_events.rs:810-817`). 재구현하지 않는다. 남은 것은 fresh 교체 후 Codex proof 소비다. |
| “늘어난 옛 source는 근거가 있을 때만 은퇴한다. 데이터 손실 방지 가드는 완화하지 않는다.” | quiet만으로 퇴역 금지는 이미 충족(`rotation/readers.rs:593-614`). 이를 유지하고 정확한 sealed execution-end 또는 별도 retention 계약을 적용한다. |
| “이미 쌓인 20개를 어떻게 정리할지 정한다. 운영자 명령이나 자동 은퇴 중 하나이며, 근거와 함께 둔다.” | 자동 은퇴 권고. 7일 조건을 유지한다. 현재 증거 없는 source를 당장 모두 retire시키는 계획은 아니다. |
| “재시작 반복 시험에서 reader 수가 일정함을 보인다.” | 현재 Codex 실제 형태를 쓰는 반복 시험은 없다. 아래 RED→GREEN·뮤턴트 계획으로 추가한다. |

## 관측과 시간순 RCA

운영 DB 조회는 `/api/docs` 확인 후 GET `/api/health`, `/api/channels/<id>/watcher-state`, `/api/discord/bindings`, `/api/sessions`, `/api/session-termination-events`, `/api/agents/<id>/session-evidence`만 사용했다. PG 직접 접속은 하지 않았다. 파일 기반 `binding_events`, `o_store`, `o_codex_ends`는 읽고 레인 logs에 복사했으며 원본을 열어 쓰거나 OStore API를 실행하지 않았다. 설정 파일은 읽지 않았다.

### 재시작 경보와 앞선 fresh 교체

아래 stdout 줄 번호는 `/Users/itismyfield/.adk/release/logs/dcserver.stdout.log`다. 원본 채널 추출본에 원본 줄 번호를 보존했다(`logs/runtime-channel-timeline-final.clean.log`). 시간은 UTC, rollout 이름의 시간은 KST다.

| 서버 경보 시각 / stdout 줄 | count | 복원 source (UUID 앞부분) | 직전 fresh source가 생긴 시각 / binding seq |
|---|---:|---|---|
| 04:55:39 / 25870 | 14 | `01a166cc` / 12-27-06 | 03:27:08 / 134 |
| 06:36:51 / 117857 | 15 | `01a16720` / 13-58-54 | 04:58:56 / 135 |
| 08:39:46 / 122057 | 16 | `01a1677c` / 15-40-01 | 06:40:03 / 136 |
| 09:41:57 / 124068 | 16 | 동일 source | 새 binding 없음 |
| 10:34:28 / 126049 | 16 | 동일 source | 새 binding 없음 |
| 12:26:24 / 129942 | 17 | `01a16856` / 19-37-48 | 10:37:50 / 137 |
| 14:29:39 / 134024 | 18 | `01a168bc` / 21-29-29 | 12:29:33 / 138 |
| 16:45:54 / 138310 | 19 | `01a1692d` / 23-33-03 | 14:33:05 / 139 |
| 18:21:47 / 142226 | 20 | `01a169aa` / 10/24 01-49-08 | 16:49:10 / 140 |
| 19:55:27 / 145395 | 21 | `01a16a02` / 10/24 03-25-03 | 18:25:05 / 141 |

fresh 실행 직전 `session-strategy reason=session_cleared_by_user resumed=false tmux_alive=false`가 있다. 예: stdout 31189(04:58:51), 118554(06:39:58), 126684(10:37:45), 130510(12:29:27), 134661(14:33:01), 138880(16:49:06), 142742(18:25:01). P5 seq127~142는 모두 `old=Some(source)`, `cause=startup`, `hook_event=null`이다. **old 관계가 빠져서 이번 증가가 생겼다는 후보는 이 데이터에서 배제된다.**

초기 세 clear는 stdout 31062(04:58:44), 118530(06:39:51), 126660(10:37:38)에 `Codex clear end not captured`, `reason="pane source unsettled"`가 있다. 이 과거 결함은 #7287 수리와 구분한다. 후반 clear는 실제 End를 저장했다. 후반에 경고가 없다는 이유로 Herdr 분기를 탔다고 추정하면 틀린다.

### 초기·후속 현물 대조

| 관측 | 초기(19:50~19:53 UTC) | 후속(19:59~20:00 UTC) |
|---|---|---|
| GET health가 보고한 deployed SHA | `6ad292e109e2ed3b5ee9aec0a2591408fba85e0a` | `88cf918add4765ce992a92a2a14118d5e0cb2fd7` |
| P5 마지막 seq | 141 | 142 |
| cursor / retired | 21 / 0 | 22 / 0 |
| rotation links / successors / proofs | 21 / 20 / 0 | 22 / 21 / 0 |
| Codex End / Seal | 4 / 0 | 5 / 1 |

초기 cursor 21개는 정확한 dev/ino, EOF, prefix SHA256, 마지막 newline, JSON 전체 파싱을 모두 확인했다. 모두 일치한다(`source-observation.json`). 이는 **drain 가능성의 근거**이지 실행 종료 증거가 아니다. 초기에는 #7300 미배포이므로 Seal 0은 정상이다. 후속에서 #7300 배포를 GET health로 확인했다.

후속 End `2be933787ccf4f429099172164611ce8`: old=`01a16a02…`, `binding_seq=141`, `explicit_clear`, 종료 19:58:32.144677Z. Seal: new=`01a16a58…`, nonce=`135eba2d51694673abc032c07061e95e`, seq142. stdout **146001 / 19:58:43.628643Z `Codex successor Seal durable`**. `boundary-final.json`의 old key `797109aef31df171f663c917a62079bc`는 successor seq142와 일치하지만 proof가 없다. `cursor-census-final.json`은 여전히 22개 미은퇴다. 이 레인은 배포·clear·입력을 실행하지 않았다.

관측은 원자적 DB snapshot이 아니다. 조사 중 외부 배포/입력으로 원본이 바뀌어 `snapshot-readback.json`의 초기→후속 해시는 달라졌다. 초기·후속 자료를 별도로 보존했으며, 시점이 다른 숫자를 하나의 snapshot으로 합치지 않는다. stderr 로그 파일은 없다.

비교 채널 `1559670200419352696`은 stdout 134087~134101에서 historical Claude clear-end proof 15건, 134126(14:30:00)에 일시 count16, 이후 seq60/61/62 proof가 있다. Codex와 같은 미지원 provider 소비 경로로 분류할 근거는 없다. 이번 수정 대상에 넣지 않는다.

## 원인에서 증상까지의 코드 경로

| 구간 | file:line / 계약 |
|---|---|
| fresh 실행 뒤 native binding 등록 | `src/services/codex/absence_launch.rs:422-453` → `codex/tui_session_launch.rs:607` → `codex.rs:571-579` → `codex_tui/session.rs:304-379` |
| durable P5 공개 | `tui_prompt_dedupe/runtime_binding/codex_hook.rs:15-26`; `binding_events.rs:781` old 보존, `:810-817` 동일 source no-op, `:863-888` fresh Startup·hook 없음 |
| writer의 신규 reader | `tui_o/writer/rotation.rs:455-478` bind/prove → `:633-654` old→new successor(proof=None) → `:658-664` 처음 보는 source만 attach/push |
| 복원 | `rotation/readers.rs:52-146` 저장 cursor마다 하나씩 복원. `historical_hops.rs:55-88` 관계만 복원하고 proof는 만들지 않음 |
| 현재 Codex proof | `rotation.rs:212-241`: Codex는 provider SessionStart/Clear만. `rotation/scoped.rs:105-125` pane/seq/old session을 추가 제한 |
| Claude 전용 clear-end 보완 | `rotation/clear_end.rs:37-38`, `:146`, `:195-217`. #7284의 빈 session ID 보완은 Codex에 적용되지 않음 |
| Codex End 생산 | `commands/control/clear_teardown.rs:91-109` → `codex_end_record.rs:8-35` → `codex_tui/execution_end/capture.rs:23-101`, `confirm.rs:25-40,43-133`. pane/PID/PGID 종료와 source/context 확인 후 저장 |
| Seal 생산(이미 main) | `execution_end/producer.rs:221-344`: exact old/source, 첫 후속 binding, fresh/shadow context, nonce/host/owner/root 검증. #7300 원문은 소비를 명시적으로 A2b에 남김 |
| 소비 누락 | `rg codex_end src/services/tui_o/writer` 결과 없음. `store/codex_end.rs:1`도 End 단독은 reader 은퇴 증거가 아니라고 명시 |
| 실제 은퇴 가드 | `rotation/readers.rs:592-614`: proof + successor capture + drain + 10초 quiet. configured reader는 `:290-347,641-648`의 추가 prefix validation 유지 |
| 경보 반복/회복 | `rotation.rs:311` 재시작마다 alarm latch 초기화. `rotation/readers.rs:621-626` count>3 재경보 및 reconciliation. `alarm/sink.rs:12-15` 성공 tend에서 count<=3일 때 해제 |

E2E 하네스에는 별도 직접 kill 경로도 있다(`scripts/e2e/run_tui_relay.py:1170-1206`, `:3315-3335`, `:4182-4202`). 이는 End observer를 지나지 않는다. hard-reset 기본값은 false이며, **위 운영 증가가 hard-reset 플래그 때문이라는 증거는 없다.** 단순 서버 restart 단계(`:4634-4687`)도 provider를 kill하지 않는다. 파괴·비정상 종료 시험은 End가 없으면 보존하는 음성 사례로 유지한다.

## 수리 A: 앞으로의 managed clear / fresh 반복 증가 차단

범위는 #7300의 A2b 중 reader retirement 소비다. dormant matcher 활성화나 Codex native source adoption 권한 확대를 묶지 않는다.

1. `rotation/codex_end.rs`에 전용 matcher/settler를 둔다. store의 strict read/validation을 재사용하되, **End와 Seal 모두 있는 레코드**만 후보로 삼는다. channel/provider/pane/host/owner root, old/new immutable context, old SourceId와 Names의 저장 identity, successor SourceId·nonce·seq, 첫 exact Bound 또는 Pending→Resolved adjacency가 실제 strict binding log와 일치해야 한다. End-only, 다른 실행, resume, 잘못된 context, 생략된 binding, 다른 inode는 proof를 만들지 않는다.
2. `execution_end::identity`의 native session/file/root·first-prompt 검증을 재사용한다. transient/unwritten은 재시도, 확정 mismatch는 보존하고 진단한다. dev 재번호는 기존 Names/reopen/prefix 정책으로만 처리한다. basename/빈 UUID/설정값을 증거로 사용하지 않는다.
3. producer의 `retry_bound_candidate`를 actor follow의 idle 경로에서도 호출해 generic publisher의 늦은 Pending→Resolved를 회수한다(#7300 P2-4). **이미 존재하는 typed candidate만** 진행시키며 End에서 ticket/candidate를 복원하지 않는다. 같은 source authority를 재진입하지 않도록 actor 쪽 adapter를 분리한다.
4. restore 직후·binding 변화·End/Seal publish·transient retry 때 matcher를 실행한다. 성공하면 exact hop의 `proof=Some(seal.binding_seq)`를 **rotation에 durable 저장한 다음** 기존 `wake_proven`/tend에 넘긴다. 새 은퇴 타이머나 별도 FD cap을 만들지 않는다. configured/dormant reader가 잠들어 있으면 기존 검증 경로로 wake한다. proof는 source consumption permission을 대신하지 않는다.
5. 기존 `RETIRE_QUIET=10s`, successor captured, old drain, prefix validation, regrowth watch, ownership/suspension 가드를 그대로 사용한다. health 회복은 기존 성공 tend의 실제 reading count에만 맡긴다.

새 불변식/조건: **“정확한 durable End+Seal과 해당 hop의 재검증이 provider Clear와 동등한 ‘그 old 실행이 끝남’의 증거가 된다.”** 이는 Codex의 새 proof 종류다. quiet/Startup를 proof로 승격하는 변경이 아니다. 설계 리뷰가 이 계약을 승인해야 구현한다.

재시작 계약: 같은 source 복원은 count 그대로다. durable Seal 이후 proof 저장 전 재시작은 재매칭하고, proof 저장 후 재시작은 rotation을 복원한다. End와 다음 dispatch 사이의 volatile ticket 소실은 #7300의 기존 fail-closed 계약을 유지한다(`attempt.rs:1,102`, `attempt_tests.rs:108,179-213`). 이 경우 proof를 꾸며내지 않으며 B의 대상이 될 수 있다. 정상 E2E 반복은 clear→fresh first bind→Seal/settle→server restart 순서로 검증한다. crash-gap을 정상 성공으로 표시하지 않는다.

bounded evidence: `codex_end.rs:16-17,283-290,328-336`은 16건/16KiB를 넘어 오래된 End를 제거할 수 있다. rotation에 저장된 proof는 record eviction 이후에도 유지돼야 한다. >16회 정상 반복과 byte-cap eviction을 시험한다. actor가 장시간 전혀 읽지 못하는 동안 증거가 밀린 경우는 보존·보류로 판정하고 B로 회수한다. cap을 넘었다고 missing proof를 합성하거나 count를 0으로 보고하지 않는다. 이 장시간 stall은 일반 반복 시험의 성공 근거로 쓰지 않는다.

## 수리 B: 기존 backlog 정리 — 자동 7일 정책 권고

### 현재 main이 실제로 할 수 있는 것

#7268의 사용자 결정은 “비최신이며 마지막 대화 후 7일 이상이면 자동 정리, 운영자 Inspect/Apply 삭제”다(`logs/issue-7268.json`). 따라서 새 운영자 명령을 되살리는 안보다 이 정책의 Codex 적용을 권고한다. #7308도 자동 은퇴를 허용한다.

현재는 그대로 실행할 수 없다. sweep는 Claude 전용(`discord/health/o_transition/automatic.rs:118-120,153,192-200`), context는 Bound Herdr row/Claude 전용(`o_transition.rs:35-82,114`), actor와 judge는 CodexDeferred(`writer/actor/transition.rs:245-246`, `transition_cleanup.rs:295-296`)다. age parser도 Claude user/assistant 전용(`actor/automatic_age.rs:40-53`)이다. **기존 명령으로 지금 20개를 정리할 수 있다고 보고하지 않는다.**

### Codex용 최소 계약

- 같은 actor의 Inspect→scope digest→AutomaticApply 및 durable `TransitionCleanup` audit를 재사용한다. A의 provider proof와 B의 retention audit를 혼합하지 않는다.
- provider를 Context/Target에 명시하고, ready actor의 provider와 맞춘다. Codex의 Bound Herdr는 기존 row/endpoint 검증을 provider에 맞게 사용한다. **운영의 legacy tmux에는 별도 context adapter가 필요하다.** 같은 channel transition lock, source authority, local host/runtime owner, exact current spawn nonce와 immutable context, 실제 current binding을 서로 대조한다. 이 adapter를 완성하기 전에는 CodexDeferred를 제거하지 않는다.
- 현재/과거 pane 목록은 strict P5 log에 나온 pane 전체를 출발점으로 읽는다. 각 pane의 실제 존재/nonce/owner/runtime binding을 확인하고, current execution이 쓰거나 resume/fork로 보유하는 source 전체를 보호한다. 같은 transition/source authority 아래 적용 직전에도 재검증한다. historical pane의 종료가 불명확하거나 새 binding/Pending이 있으면 해당 scope를 보존한다. 다른 host는 기존 `historical_execution_ended`의 긍정 확인 없이는 정리하지 않는다.
- **TTL cache에서 안 보인다는 사실을 ended/empty history로 해석하지 않는다.** `runtime_bindings_for_kind`는 24시간 TTL cache(`binding_access.rs:83-105`), `codex_adoption::fold`는 caller가 준 목록을 믿는 dormant 평가기(`codex_adoption.rs:1-2`, `fold.rs:51,129-134`)다. 둘 중 하나만으로 completeness를 선언할 수 없다. Codex용 complete/error 결과를 명시적으로 만들고, source/pane/nonce 누락은 Unknown으로 처리한다.
- legacy Codex history는 provider별 adapter에서 만든다. current fresh 실행이 이전 source를 보유하지 않는다는 것을 strict log·current launch mode·native source와 함께 검증한다. resume는 expected native session과 이전 context가 보유한 source를 모두 보호한다. verified의 pending/conflicted/legacy-unverified가 해소되지 않았으면 hold한다. “현재 source 하나”를 전체 held set으로 대체하지 않는다.
- 종료 근거는 기존 retention의 per-source 마지막 binder→현재 Bound 실행 nonce 대조 또는 확인된 historical host 종료다(`automatic/termination.rs:91-145`). Codex에서는 위 complete execution/held-source 검증을 함께 요구한다. 이를 provider native Clear proof로 기록하지 않는다. **이 alternate retirement 조건의 Codex 확대와 legacy completeness는 이번 설계 리뷰의 핵심 변경 계약이다.** 단순 CodexDeferred 삭제는 금지한다.
- age는 Codex native JSONL의 실제 user/assistant dialogue timestamp를 해석한다. `response_item/message` 및 대응 `event_msg`를 provider parser와 맞춘다. mtime·파일명 시간·마지막 턴 완료를 age 대체값으로 쓰지 않는다. 새 dialogue를 숨길 수 있는 malformed/판별 불가 row, 불완전 tail, missing timestamp는 Unknown이다. 기존 32MiB budget/identity/hash/EOF 고정과 **7일**을 유지한다.
- 모든 기존 안전 판정을 유지한다: home/ownership, current/pinned/held 보호, successor captured, pending batch/drain/capture-ahead, owed/prepared/unsealed/POST/verified work/protected source, 두 probe의 identity/hash/10초 quiet/EOF/newline, 적용 직전 scope/token/path CAS, runtime idle, atomic audit+retirement, regrowth watch. 관련 main 좌표: `transition_cleanup.rs:310-335,365-517`, `transition_snapshot.rs:222-234`, `actor/transition.rs:477-544`, `store/spool/transition.rs:22-48`.

### 정리 시점과 결과

초기 21개 중 현재 source를 제외한 old 20개가 B의 census다. 원본 자체를 삭제하지 않고 reader/cursor만 audit를 남겨 은퇴시킨다. End-only 4개와 End가 없는 과거 source에는 소급 Seal을 만들지 않는다.

초기 자료에서 가장 오래된 source의 마지막 대화는 **10-17 23:30:43.584 UTC**, 7일 도달은 **10-24 23:30:43.584 UTC**다. 기존 old 20개의 가장 늦은 대화는 **10-23 18:24:45.403 UTC**, 7일 도달은 **10-30 18:24:45.403 UTC**다(`source-observation.json`). 실제 sweep는 이 시각 이후 idle·증거·가드를 다시 만족할 때 처리한다. 날짜 도달을 성공 보장으로 표시하지 않는다. 새 대화/미정산/unknown이 있으면 계속 보존한다.

후속 seq142에 붙은 Seal 1개는 A에서 해당 old 1개를 더 빨리 은퇴시킬 수 있다. 따라서 마지막 snapshot의 22개는 A 후 `old backlog20 + current1`로 돌아가고, B가 증거 있는 old를 순차 정리한다. 정리 전에는 health degraded가 남을 수 있다. 이 지연을 감추려고 MAX_READERS를 높이거나 경보를 지우지 않는다.

수용상 자동 정리 완료를 즉시 요구한다면 7일 policy를 낮추지 않고 코디네이터에 범위 결정을 돌려야 한다. 이번 권고는 7일 조건을 유지하는 자동 경로이며, 새 운영자 명령/수동 종료 확인 기능은 포함하지 않는다.

## 시험 계획 — 구현 전 RED, 후 GREEN, 원인 제거 뮤턴트 RED

이번 조사에서는 cargo나 새 시험 코드를 실행/작성하지 않았다. 아래는 구현 단계의 필수 실행 계획이다. 테스트 결과를 미리 PASS로 쓰지 않는다.

| ID | 실제 검증 경로 / 기대값 | RED와 뮤턴트 |
|---|---|---|
| A1 | Codex, Startup+hook 없음, actual clear→fresh producer End+Seal→actor follow/capture/tend. 10초 전 old 유지, 이후 proof=seq·old retired. legacy/configured 양쪽 | 현 main은 proof가 없어 RED. 새 matcher 제거 뮤턴트 RED |
| A2 | 20회 clear→fresh→settle와 각 회 2회 actor/store 재생성. settle 후 reading=1, 미은퇴 current=1, 초기 backlog가 있으면 `backlog+1` 일정. delivered ID/body exactly once | consumer/restore/wake 각 제거 시 고유 단언 RED. 원인과 무관한 timeout/skip는 인정 안 함 |
| A3 | 같은 source로 5회 정상 restart: cursor key 집합·P5 seq·reading count 일정. proof 저장 전후 crash point 각각 replay | same-source 중복 attach/restore mutant RED. 이 main 충족 조건을 회귀 시험으로만 고정 |
| A4 | Seal 늦게 도착, generic Pending→Resolved, 초기 transient/native first record 미완성 후 동일 binding에서 완성. 새 bind 없이 eventual proof | idle retry/notice 제거 RED. End-only/volatile ticket restart는 proof 없음 |
| A5 | wrong channel/provider/host/pane/nonce/root/old/new seq, resume, 다른 첫 bind, inode 교체, prefix mismatch, missing/oversize/corrupt store 각각 보존 | identity/adjacency/fresh 검증 제거별 해당 거부 시험 RED |
| A6 | >16 normal cycles·16KiB eviction 뒤 persisted proof 복원. actor stall 동안 증거 eviction은 Unknown·reader 보존. retire 후 regrowth는 unretire·정확한 offset 배달 | proof persistence/eviction fail-closed/regrowth watch 제거 RED |
| B1 | 실제 Codex actor+sweep(legacy tmux/Bound Herdr 각각), noncurrent·7일 이상·idle·complete evidence만 audit+retire. current와 7일 미만은 유지 | 현 main CodexDeferred로 양성 RED. provider-only gate 제거로 PASS를 꾸미지 않음 |
| B2 | 7일 경계 ±1ns, Codex 실제 JSONL 형태, mirrored message, newer dialogue, invalid timestamp/row/tail, hash/dev/ino 변경 | age/current/unknown/identity 뮤턴트 각각 고유 RED |
| B3 | 현재 execution이 과거 source를 resume/hold, Pending/conflicted, TTL-expired cache지만 live pane, 다른 host 상태 unknown, 도중 nonce 교체 | held-set/completeness/last-binder/context 재검증 제거 RED |
| B4 | queue·prepared·unsealed·POST·verified work·protected source 각각 양수, unread POST count, pending capture/drain. Inspect 후 input/ownership 변화 | 각 obligation/CAS/idle 생략 뮤턴트 RED. fake snapshot만이 아닌 actor에서 확인 |
| B5 | sanitized original backlog20 + current1. 7일 전 보존, 각 source별 시간 경과·증거 충족 후 20→0, current 유지, 재시작 후 count1. unknown 대상은 명시적 hold | Retire 없이 count만 낮추는 뮤턴트 RED. audit 및 cursor receipt와 함께 검증 |
| H1 | count>3 경보→실제 retire/성공 tend→count<=3에서 동일 프로세스 health reason 해제, 다른 channel alarm 유지 | health early-clear/always-clear 뮤턴트 RED |

빌드 단계 전 `wait_headroom.sh`, 새 target이면 `seed_target.sh`를 실행한다. cargo-shim PATH와 `unset TMUX TMUX_PANE`, `CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none`을 사용한다. lib 전체 시험은 금지, PG가 필요한 context/row 시험은 `pg_throwaway.sh`만 사용한다. 대상 시험·뮤턴트·check/clippy, required test-lane 선택 확인, static prepush를 각 구현 조각에서 수행한다. 원본/뮤턴트 파일 바이트 동일 복원과 바이너리 SHA256·실제 실행 수/skip/환경을 보고한다.

운영 E2E·재시작은 이 레인이 실행하지 않는다. 코디네이터가 후속 수용 검증으로 수행한다. default managed clear/fresh 반복을 먼저 검증하고, 직접 kill 테스트는 증거 없는 source 보존을 확인하는 별도 음성 사례로 둔다.

## 대상 파일·크기·소유권

예상은 리뷰 전 예산이며 실측 cap을 대체하지 않는다. 전체 **prod 20파일 이내 / 순 +1,230 목표, 최대 +1,500**. 각 리뷰 조각은 공통 +800 기준도 만족하도록 A/B로 나눈다. 1,000줄 넘는 prod 파일은 raw 증가 0이며, 아래 기존 대상은 모두 1,000줄 미만이다. giant `control.rs`, `binding_events.rs`, `tui_prompt_dedupe.rs`, `warm_followup.rs`, `scripts/e2e/run_tui_relay.py`는 수정하지 않는다.

| 조각 | 대상(모두 src/services/ 아래, 새 파일 포함) | prod 파일수 / 예상 순증 |
|---|---|---|
| A | `tui_o/writer/rotation.rs`(현재893), `rotation/codex_end.rs`(신규), `actor.rs`(832), `actor/codex_end.rs`(신규), `tui_o/store/codex_end.rs`(357), `codex_tui/execution_end/identity.rs` | 6 / +450 |
| B context/sweep | `discord/health/o_transition.rs`(120), `o_transition/automatic.rs`(325), `o_transition/automatic/termination.rs`(147), `o_transition/codex_context.rs`(신규) | 4 / +280 |
| B actor/history | `tui_o/writer/transition_port.rs`, `actor/transition_snapshot.rs`(317), `actor/codex_history.rs`(신규), `actor/transition.rs`(697), `host/transition.rs`(151), `transition_cleanup.rs`(522) | 6 / +360 |
| B age/wiring | `actor/automatic_age.rs`(64), `actor/codex_age.rs`(신규), `actor/automatic.rs`, `discord/health/recovery/stall_watchdog_task.rs`(26) | 최대4 / +140 |
| 합계 | A +450, B +780 | 최대20 / +1,230 |

시험은 `writer/codex_end_proof_tests.rs`, `writer/actor/*_tests.rs`, `discord/health/o_transition/*_tests.rs`와 기존 Codex End producer fixture 재사용을 예상한다. 생성 inventory는 공식 생성 스크립트로만 동기화한다. 실제 transcript 본문은 시험 fixture로 복사하지 않는다.

**FILES/겹침:** `src/services/tui_o/writer/**`는 브리프 공통 목록의 i5340_s3act A 소유 범위와 겹친다. 계획 단계는 읽기만 했으며 구현 전 코디네이터가 A2b/retention 변경 소유권을 확정해야 한다. 다른 명시된 활성 레인 소유(`cluster/**`, intake/queue/input/provider_dispatch/herdr/http/task_notification/message_outbox/session_relay_sink 등)는 수정 대상에서 제외했다. `o_transition.rs`의 기존 channel-home API를 호출할 수는 있으나 `cluster/**`를 바꾸지 않는다. 소유권 확정 또는 검토 후 예상이 20파일/+1,230을 넘으면 구현을 시작하기 전에 delta 계획을 제출한다.

## 리뷰 집중 항목과 미확인 사항

- 경계 시각: 10초 quiet, 7일 age 모두 정확한 경계 시험. 미래 timestamp/역행은 Unknown/hold.
- 경합: source authority 재진입 금지, Inspect→Apply의 scope/token/path 재검증, 새 input/Pending/nonce 변화는 hold. 최근 count를 내려 health를 선행 해제하지 않음.
- 옛 형식 복원: End-only/optional Seal 없음은 정상 보존, legacy rotation의 seq/pane은 유일 strict history match만. 손상은 empty/0으로 읽지 않음.
- 문서 전용 가드 금지: Codex complete execution snapshot을 코드가 반환해야 한다. 현재 main의 TTL cache/dormant evaluator가 이 계약을 충족한다고 간주하지 않음.
- 잘못된 0/PASS: count뿐 아니라 cursor, proof 또는 audit, obligations·skip/unknown을 동시에 검사. original old20이 남으면 해결 완료로 보고하지 않음.
- [U1, 설계 핵심] B의 legacy tmux complete/held-source adapter는 새 계약이며 현 main에 구현돼 있지 않다. 위 범위와 테스트로 보존 가드의 동등성을 증명할 수 있는지 WebGPT pro가 우선 검토해야 한다. 증명에 필요한 범위가 cap/소유권을 넘으면 B를 구현하지 않고 설계 delta로 멈춘다. 부재를 empty로 대체하지 않는다.
- [U2, 수용 일정] backlog를 7일 정책으로 정리하는 동안 degraded가 지속된다. 정책을 낮추지 않는 권고를 명시했다. 즉시 전체 정리를 요구하면 별도 범위 결정이 필요하다.
- [U3, 운영] production End/Seal·cursor 수는 계속 변할 수 있다. 다음 구현/검증 레그 시작 때 동일 GET 및 정확한 store 파일의 읽기 전용 census를 새로 받아야 한다. 금지 설정값은 필요하지 않았고 조회하지 않았다.

이 계획을 고치지 않으면 운영에서는 E2E clear마다 unproven reader가 하나씩 추가되고 health 경보가 상시 남는다. A는 현재 실제 경로의 추가 누적을 막고, B는 기존 데이터와 미정산 output을 보존하며 과거 누적을 정책에 따라 회수한다. WebGPT pro 설계 리뷰 전에는 어느 계약도 구현·활성화하지 않는다.

## 증거 목록

- `logs/issue-{7308,7300,7268,7258,7284,6577}.json`: 원문/단계별 의도.
- `logs/health-initial.json`, `health-second.json`, `health-final.json`, `watcher-state*.json`, `session-evidence.json`, `discord-bindings.json`: GET 결과. 중간 health의 reconcile_in_progress는 별도 transient startup 이유이며 원인 판단에 섞지 않음.
- `logs/runtime-channel-timeline.raw.log`, `runtime-channel-timeline-final.raw.log`, `runtime-channel-timeline-final.clean.log`, `runtime-log-observation.json`: stdout 원본 줄 번호/시각과 추출본.
- `logs/snapshot-binding_events-1509350778043895902.log`, `bindings-final.log`: P5 seq/old/new/cause/nonce.
- `logs/snapshot-boundary.json`, `boundary-final.json`, `snapshot-binding_checkpoint.json`, `snapshot-o_codex_ends-1509350778043895902.json`, `codex-ends-final.json`: relation/proof/End/Seal 현물.
- `logs/cursor-*.json`, `final-cursor-*.json`, `cursor-census*.json`, `source-observation.json`, `rca-summary.json`, `snapshot-readback.json`: cursor·identity·EOF/hash/newline·dialogue age 시점별 대조.
- cargo/test binary: 없음. 이번 단계는 재현 입력·관측과 계획 산출 단계이며 Rust RED/GREEN 실행 증거는 아직 없음.
