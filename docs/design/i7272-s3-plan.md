PLAN_READY 05d89aaa6a2d047c41f6ba44a45a85991206e52d S3=3a/3b/3c/3d canonical=r2 S2b_P2-1..4=before_S4 no_build

# #7272 S3 계획 — 공통 payload·첫 입력·history·epoch custody

## 0. 판정과 기준

지정 정본 `design-r2-webgpt.out.md`의 **답변 본문 204~814행**을 기준으로 초안을 대조·보정했다. 앞부분의 전달 프롬프트/r1 첨부나 사이트 UI는 r2 결정으로 취급하지 않았다. `design-r2-carryover.md`, 승인된 S2 PLAN, S1·S2b 이월과 S2b Opus 리뷰 원문까지 대조했다. S2b P2-1~P2-4의 수리 위치·증명은 §5에 확정했다. 이번 산출물은 독립 설계 리뷰용 계획이며 구현 완료/운영 활성화 판정이 아니다. 코드·시험·등록 변경, 빌드·시험 실행, 커밋은 없다.

- 계획 코드 기준 M: fetch한 최신 `origin/main` = `05d89aaa6a2d047c41f6ba44a45a85991206e52d`.
- 이미 승인된 foundation 기준 L: S2b `2fca26599817c3c9134780d536b14d4fc5baa07b` / `refs/review/i7272-s2b-r1`.
- M과 L의 merge-base: `22278dd57feda64bf2888ac1fbd36f21ad7e3324`. S1/S2는 M에 아직 포함되지 않았으므로 아래 **M 좌표와 L 좌표를 구분**한다. checkout/merge/rebase/commit은 하지 않았다. 구현 전에는 코디네이터가 M과 foundation을 merge하여 새 실제 base를 고정하고 공식 생성물을 동기화해야 한다. 오래된 r2의 rebase 권고는 사용하지 않는다.
- 정본 r2: `design-r2-webgpt.out.md`, SHA256 `ab833e9e456c766b77c4f37b43a3565f6b12fcb05f9e28e4bc204ef3ec456b43`, 설계 소스 기준 `794e7123e1749db0dea10b638a2af84d7cc8c191`. 기존 `design-r2.md`의 인용은 정본 답변·이월 계약에 재대조했으며 정본을 대체하지 않는다.
- A1 r1 참고: `35f7c972ff9c679290692b6bd1fc92cd6ca77053` / `refs/review/i7314-a1-r1`. r2 수리 완료본은 아직 확인하지 않았다. r1의 동작이나 파일 줄 수를 r2 계약으로 승격하지 않는다.
- 이전 native PID 186471은 종료된 상태임을 확인했다. `logs/s2b-native-postreview.rc`=0, 같은 L의 `POSTREVIEW_FINISHED ... rc=0`와 `PREPUSH_OK`를 확인했다. 이번 계획에서 빌드·시험·PG·clippy·prepush를 새로 실행하지 않았다.

### 정본과 계획의 차이·보정표

| 정본/승인 이월의 기준 | 초안과 다른 점·최종 처분 | 종류·증명/완료 위치 |
|---|---|---|
| 정본 §7 조각3 = payload 공통화+PreSource+네 cold 첫 전달, 18~26파일/+700~1100 추정 | 이 범위 유지. S2에서 이월된 history와 #7314 C·S2b 수리를 더하면 한 단위로 수용할 수 없어 **3a~3d 네 리뷰 단위**로 세분화. 논리 6조각은 유지 | 크기 변경. §3 파일별 합산 및 merge 뒤 재측정. 옛 +700~1100이나 전체 +1600~3400 안에 들어간다고 주장하지 않음 |
| 정본 §3/§7은 history lifecycle을 조각2에 둠 | 승인된 `report-impl-s2-plan.md`의 측정/이월에 따라 lifecycle **3a**, S2b에는 호환 reader만 존재 | 승인 이월. 실제 V2 writer golden→L read-only reader A24 및 A12~A14 |
| 정본 §2.2는 MayHaveSent에서 deadline 결정, 이월 P2-1은 cold 부팅·준비 제외 | S2 arm 즉시30초를 그대로 소비하지 않음. **bootstrap/payload 단계와 payload 직전 deadline CAS**를 제안. no-deadline은 재시작 후 NotSent 증거가 아님 | 설계 보완, 독립 리뷰 대상. 3d 느린 boot/stash·절단점 복원, warm publish3 목표/추가 준비 commit 구분 |
| 정본 §4.3의 PreSource→검증된 Source 결합 | 기존 AttemptRef를 바꾸는 enum 치환 대신 **별도 optional resolved-source/target 증거**를 bind-once로 추가. history에는 file-bound Entry 외 `planned` 필드 | 구현 계약 명료화. 3a/3d immutable key·A24·foreign execution 거절 |
| 정본 §3 A13 = 한 source에서 5000회 이상, rotation/GC 없이 진행 | 초안의 4096 passive fixture만으로 완료되지 않음을 보정. **실제 history 등록·receipt·slot 반환 루프 5001회**를 추가하고 과거 row replay도 검증 | 증명 보강. 3a H1, 더 높은 층 실제 ingress trace는 S4 |
| 정본 §2.3의 draft 보호, 이월 Veto 분류의 Draft 대기 | 사용자 원문 스펙과 #7314 A1 r2를 적용해 **safe readable Person은 stash 후 submit**, unsafe/OwnPending만 Held. 새 transaction은 만들지 않음 | 요청된 보완. 3d A1 공통 transaction 재사용, S4 wake. capability 미증명 조합은 S5 제거 보류 |
| 정본은 session/binding 변경 시 과거 attempt 보존, PaneEpoch wire/hold 수명은 미정 | #7314 C를 흡수해 **PaneIdentity+Known spawn nonce**, epoch-qualified hold·old custody 분리. 세대 변경만으로 guard/lease 제거 금지 | 요청된 보완. 3d, 실제 resolver/boot 활성화 S4. 동일 증거를 #7314 종결에 연결 |
| 정본 §2.6/§9 I1·I2·I8과 A20~A22 | S2b 구현의 정상 sync 누락·arm 시각 정산·identity 일괄 삭제·오류 시 item 소유권 문제를 번호별로 확정 | 새 수리 이월. P2-1/3 및 API는3a, P2-4 접수 증명3b, P2-2는3d. **모두 S4 유입 연결 전** |
| 정본 §10 A01/A07/A16, 조각5 제거 전 실제 ingress 수용 | #6325 E-50 콜드 부트 첫 입력1/응답1을 별도 명시. S3 helper/mock 성공을 종결로 부르지 않음 | 요청된 보완. S4 PG 실제 ingress, S5 fallback 제거 전 격리 native 수용 |
| 정본 §7 좌표 갱신에 rebase 언급 | 이 캠페인 지시가 우선. **코디네이터가 foundation L과 최신 main을 merge**, 새 base에서 재계수; rebase 없음 | 작업 절차 보정. M/L 좌표는 합성 base라고 표시하지 않음 |

정본의 (b) 비실행 원문 보관, 30초 확인 상한, 미확정 자동 재전송 금지, 한 physical writer, 네 provider/host의 첫 전달 방식, Source≠receipt, 기존 non-TUI/remote 계약, 새 journal/flag 없음은 유지한다. 세부 모델/시각 보완을 정본에 이미 있던 구현 사실로 서술하지 않는다.

### 먼저 확인한 main 드리프트

실행한 핵심 조사 명령은 `git log --oneline 794e7123e1749db0dea10b638a2af84d7cc8c191..origin/main -G 'PaneIdentity|spawn_nonce|composer_admission|inject_report_any_turn|BootstrapAndFirstInput|plain_inject|register_planned|submitted|InputHold|first_prompt|retained_inputs|busy_inject|paste_footer' -- src`다. 명령과 전체 결과는 `logs/s3-drift-log.txt`에 보존했다.

| 커밋 | 현재 근거·드리프트 | S3에서 반영할 내용 |
|---|---|---|
| `05d89aaa6` #7318 / #7314 A0 | M `claude_tui/busy_inject/screen/paste_footer.rs:2`, exact `  paste again to expand` 인식 | **이미 main에 충족된 parser 수리는 재구현하지 않는다.** footer 단독·scrollback·타 attempt chip은 여전히 쓰기 권한이 아니다. |
| `1fcc476a5` #7313 | M `herdr_launch.rs:324,388,398,412,434`; `claude/tui_session_launch.rs:51`; `codex/tui_session_launch/launch_policy.rs:70` | 실제 launch 파일은 `services/herdr_launch.rs`다. UserDefault CreatePermit·durable grant·Pending CAS·host 선택을 보존한다. 입력 EffectPermit으로 host 정책 permit을 대체하지 않는다. |
| `3302ae0c8` #7316 | M `tui_o/writer/rotation/codex_end.rs:143,198,222` | 증명된 End+Seal의 reader 퇴역은 입력 부재/미송신 증거가 아니다. history/retained의 삭제 경계로 쓰지 않는다. 해당 파일은 S3 수정 0. |
| `29cbb3be7` #7297 | native `/clear`와 `tui_prompt_dedupe/native_clear.rs` 등 퇴역 | 삭제된 helper/CLI/경계를 되살리지 않는다. clear/종료 검증은 현재 `!clear`·End/Seal 계약을 소비한다. |
| `88cf918ad` #7300 | M `codex/absence_launch.rs:373,403,408`; warm submit 후속 봉인 | create 전 `end_observation.prepare`, 검증된 spawn 뒤 `spawned`의 순서를 추출 후에도 유지한다. End/Seal은 exact input receipt를 대신하지 않는다. |
| `6ad292e10` #7304 | relay shadow·writer_protocol·park S1·감사 코드 제거 | 새 journal/ACK/shadow ledger/park 저장소로 복구하지 않는다. epoch hold는 기존 owner snapshot과 기존 guard의 projection이다. |

위 표는 `-G`에 실제 걸린 커밋 전부다. 정책/host 변경은 운영 설정을 읽어 확인한 것이 아니다. UserDefault 활성 운영값과 provider 설정값은 [U]이며 이번 계획에서 필요하지 않다.

## 1. S3 범위와 변경 계약

논리적인 6조각은 유지한다. S3 안의 리뷰 단위를 3a→3b→3c→3d로 나눈다. 기존 S3 추정 +700~1100에 S2에서 이월된 history lifecycle, #7314 C의 epoch·hold·custody, 호환·실제 진입점 비용을 합치면 한 리뷰 단위가 될 수 없다. 아래 보수적 재계수는 고유 prod 55파일, 합산 순증 약 +2492이며 **각 리뷰 단위**에서 상한을 따로 지킨다. 동일 파일의 순차 수정은 각 단위에 다시 센다. 이 숫자는 구현 diff 실측이 아니다.

| 단위 | 완성할 계약 | prod / 순증 추정 | 생성·등록 여유 포함 | 독립 안전성 |
|---|---|---:|---:|---|
| 3a | history V2·PreSource·strict sync/한 head 제거/borrow API | 16 / +804 | 19 / +924 | 기존 V1 writer와 운영 admission 유지, 신규 V2 writer는 시험 hook만 사용 |
| 3b | raw/선택 prelude/voice/첨부/session 준비·fresh source·접수 실패 custody | 12 / +448 | 15 / +568 | 기존 caller가 추출 helper로 같은 bytes/부수 효과를 유지; owner production 연결 없음 |
| 3c | Claude/Codex × tmux/Herdr prepare/create/first-submit/observer 분리 | 18 / +516 | 21 / +636 | 기존 legacy wrapper는 같은 순서로 조합, 신규 owner cold effect는 시험 hook |
| 3d | PaneEpoch·hold·payload commit·A1 stash·완료 시각·시험 child | 20 / +724 | 23 / +844 | V2 owner 경계의 시험 연결만 완성, live/boot/worker/wake production 배선은 S4 |

각 단위에서 +1200에 닿기 **전에** PLAN_READY로 멈추고 더 나눈다. 공식 등록의 여유 3파일/+120을 소진하거나 공통 정책 파일 추가가 필요하면 예산/소유를 먼저 다시 고정한다. 전체 캠페인의 옛 총량 추정도 이 증가만큼 재산정 대상이며, 옛 총량 안에 억지로 맞췄다고 주장하지 않는다.

S3에 포함: S2b P2-1~P2-4 수리와 증명, 공통 input 준비, history V2 lifecycle, PreSource/ExistingSource와 bind-once, owner 소유 첫 사용자 입력 effect, physical writer와 observer의 분리, epoch/hold metadata 및 A1 transaction 재사용 계약. S4에 남김: Gateway/catch-up/boot/worker/completion-drain 전 유입 연결, production wake, Codex queued gate 해제, live Unconfirmed custody 활성화, Herdr 실제 hold release/ReadyWithoutSource pump. S5에는 실제 ingress 수용을 통과한 조합만 legacy fallback 제거. S6에는 불용 writer 정리.

운영 Codex queued gate와 대상 밖 provider/headless/Remote의 기존 계약은 S3에서 유지한다. 새 flag·ACK·delivery journal·DB schema·cleanup/kill 권한을 추가하지 않는다. 기존 main의 M `inject.rs:665` live Delivered 정산과 M `claude.rs:1959` legacy 실패 teardown은 새 owner 호출자가 아직 없으므로 S3에서 범위를 넓히지 않는다. #7314의 typed Held 수리를 먼저 merge하고, 신규 owner는 이 일반 완료/실패 finalizer로 떨어지지 않는다. #7314 C의 운영 유실 수리는 S4의 실제 ingress 증거까지 있어야 종결이다.

### 1.1 history/PreSource 최소 계약

1. 저장소는 기존 `runtime/injected_frames.json` 하나다. envelope는 `version:2, live, passive`를 유지하며 PreSource 등록은 별도 `planned` 확장 필드에 넣는다. live에는 실제 파일에 결합된 Entry만 둔다. S2a의 L `history/compat.rs:5`가 `live:Vec<Entry>`를 요구하므로 PreSource에 가짜 path/session 또는 `(0,0)` 파일 ID를 넣지 않는다.
2. `planned`와 추가 필드는 새 reader에서 `serde(default)`로 읽는다. **live + planned의 실제 active 등록**에만 4096 제한을 적용한다. retained/settled는 passive로 옮겨 슬롯을 반환하지만 충돌·분류·native row 소유 증거는 남긴다. passive를 count cap/TTL/End+Seal만으로 물리 삭제하지 않는다.
3. stable AttemptRef와 준비된 frame bytes/digest는 변하지 않는다. 늦은 Source는 InputAttempt의 별도 optional resolved evidence에 CAS로 한 번 연결한다. `AttemptIdentity.source`를 PreSource→Existing으로 덮어써 이미 발급된 permit/completion의 key를 바꾸지 않는다.
4. Source 증거는 provider/host/root/execution nonce/expected SID·path/binding watermark·정확한 runtime 관계를 모두 검증한다. resume은 실제 기존 offset, 새 source로 증명된 파일에만 offset 0. 다른 execution의 동명 path·Source/Resolved는 거절한다. Source 존재·SessionStart·pane ready만으로 Delivered를 만들지 않는다.
5. row claim은 전체 live/planned/passive 후보와 이미 확정된 owner를 함께 본다. 동일 row를 먼저 조회한 A에게 임의 배정하지 않는다. 모호한 A/B/C는 어느 질의 순서에서도 Pending이며, 각 입력은 효과 이후 확인 상한 뒤 원문 보관으로 진행할 수 있다. command/enqueue/user의 기존 소유권도 보존한다.
6. history identity는 stable full AttemptRef/namespace로 비교한다. Entry의 기존 nonce alias는 호환용으로 남길 수 있으나 nonce 단독 조회로 V2 owner를 확정하지 않는다. Claude transcript scanner를 자식으로 추출하고 Codex submitted parser에 full-key receipt context를 받는 seam을 둔다. 기존 API는 V1 wrapper로 유지하고 신규 owner는 강한 key API만 호출한다. 동일 nonce·동일 body·동일 Source라도 다른 original/source identity의 receipt가 서로 정산되지 않아야 한다.
7. queue V2가 원문·보관·정산의 정본이다. history는 exact attempt 기준 멱등 projection이다. commit→projection 사이 중단을 복구하고, history 부재를 NotSent로 해석하지 않는다. 기존 receipt reader `rollout::submitted`와 Claude exact user/enqueue 판독기를 이 projection 위에서 재사용한다.
8. V1 history는 구조적으로 읽고 보존한다. writable V2 전환은 S4의 명시 migration에서만 한다. 3a의 새 V2 writer가 만든 planned-only/live/passive golden을 **S2b 호환 reader**로도 읽기 전용 인식시키고 원시 bytes·write refusal을 증명한다. 새 version만 V1 `entries` 구조에 덧붙이는 방식은 금지다.

### 1.2 raw payload·fresh source

- original Intervention/PendingQueueItem, source IDs·segments, author/role/provider/CWD/worktree, raw reply, uploads, voice accepted claim을 먼저 소유한다. #7314 B의 RawV1 carrier와 선택 prelude 소유/반환 계약을 소비하며 새 carrier나 WIP stash를 만들지 않는다.
- `render_input_chunks`의 업로드→context→user 순서와 provider author frame을 정확히 한 번 적용한다. 사용자 literal `_`, WIP와 같은 문자열도 strip/replace하지 않는다. 표시 이름 조회 실패는 ID 표시로 fallback하되 **author/source 동일성 검증 실패를 이름 fallback으로 우회하지 않는다**.
- role/profile/model/options/session strategy/CWD/worktree/native instruction 준비는 기존 helper를 추출·재사용한다. 실제 launch에 전달한 native instruction에만 manifest를 기록하고 warm 입력에 보내지 않은 system prompt의 manifest를 만들지 않는다. async asset/voice/worktree 준비 전에는 원입력을 먼저 durable 접수하며 publish 횟수를 줄이려고 접수를 미루지 않는다. voice PG claim과 accepted snapshot commit 사이의 crash도 같은 source/owner의 기존 claim 증거로 복구해야 한다. 그것이 기존 API로 불가능하면 원문을 보존하고 3b에서 해당 seam을 먼저 재계획한다.
- 기존 `prepare_busy_inject_uploads`→`retain_busy_uploads`/`MaterializedUploads::copy_into`를 재사용한다. owner 반환 뒤에도 파일이 있고, NotSent/restart 때 원 upload reference가 남아야 한다. 별도 cluster 보관 정책이나 삭제 권한을 추가하지 않는다.
- 기존 queue payload의 M `router/intake_queue_transaction/queue_payload.rs:5`는 voice를 제거한다. 새 owner는 이 legacy 변환 **전**의 승인/accepted payload를 받아야 한다. S3는 공통 constructor와 시험을 만들며 실제 유입 치환은 S4다. 이미 provenance가 사라진 옛 row를 새 원문으로 복원했다고 주장하지 않는다.
- 신규 admission은 dedupe 후 fresh source 단위로 만들고 그 뒤에만 안전한 병합을 한다. `[known A, fresh B]`에서 B를 버리는 L `v2/sources.rs:32`의 envelope 전체 veto를 일반 admission으로 쓰지 않는다. 완전한 per-source provenance가 있으면 B만 접수·effect 1회. 분리 근거가 없는 legacy envelope는 전체 bytes를 비실행 보관+notice로 남기며 임의 substring 제거/자동 재전송은 하지 않는다.

### 1.3 첫 입력·확인 창·host 정책

네 조합의 첫 content-bearing 경계는 Claude tmux paste/Enter, Codex tmux argv prompt, Claude Herdr gated paste/Enter, Codex Herdr argv prompt 없음+gated paste/Enter다. Codex tmux의 launch prompt 뒤 첫 paste를 추가하지 않는다. Herdr first input 뒤 Source를 관찰하며 Source를 먼저 기다리는 교착을 만들지 않는다. warm도 같은 prepared payload와 exact attempt로 Submit/Steer intent를 선택한다.

L `v2/attempt.rs:76`의 arm은 즉시 30초 deadline을 정한다. cold에 그대로 쓰면 부팅 시간을 확인 창에 넣으므로 보완한다. 권고 최소 확장은 **같은 V2 snapshot 안의 bootstrap 단계와 payload 확인 시작을 구분**하는 것이다.

- 준비·host policy/consent·payload 없는 runtime create는 확인 30초를 소비하지 않는다. BootstrapAndFirstInput 자체는 한 owner·한 permit의 단일 effect다. create 전 원문/PreSource와 MayHaveSent를 보존하되, payload 없는 bootstrap에는 deadline을 아직 두지 않는다.
- argv에 실제 payload를 싣는 Codex tmux는 create 직전 ARM commit에서 deadline을 정한다. Claude/Herdr는 준비된 composer에서 **첫 payload 전 callback**이 같은 key의 durable deadline을 한 번 정한 뒤에만 paste한다. callback 재호출은 같은 key/같은 저장 결과에 멱등이며 새 permit을 발급하지 않는다. 이 멱등성은 commit 재확인일 뿐 payload/Enter의 반복 허가가 아니며 기존 single-use transaction/typed effect를 재실행하지 않는다. tmux 크기/초안/UI 관측 자체는 receipt가 아니다.
- bootstrap 단계는 새 optional tagged field로 식별하고 누락 값은 LegacyUnknown으로 읽는다. deadline=None 하나만으로 미송신/준비 중을 추론하지 않는다. 기존 `claude_tui/submission_fence.rs:20,65,76` callback을 owner의 single-use effect에 결합한다. 기존 idle-stash의 claim은 C-s 전일 수 있으므로 그것만으로 확인 deadline을 시작하지 않는다(M `idle_stash.rs:19,388`). owner의 custody/claim과 **실제 typed payload I/O 직전**의 deadline commit을 구분한다. 후자는 `GuardedPane::payload:270`, `SubmissionTmux::paste_buffer:91`, Claude `Attempt::paste:754`, Codex `Attempt::paste:318`, Herdr의 payload action 직전에 같은 owner scope의 idempotent callback을 둔다. C-s/observer/Enter를 command 문자열 파싱으로 payload라고 추정하지 않으며 stash sequence 자체를 복제하지 않는다. MayHaveSent인데 deadline이 없는 상태의 재시작은 준비 중 미송신으로 추측하지 않고 보관/대조한다. 같은 프로세스가 payload 미진입을 확실히 아는 경우만 KnownNotSent로 구분한다.
- 즉시 준비된 warm 입력은 기존 A26의 queue publish 3회 계약을 유지한다. bootstrap/준비 대기가 필요한 입력의 추가 commit은 별도 계측한다. history·outbox 포함 전체 fsync 3회라고 말하지 않는다.
- physical writer lease는 launch/실제 submit만 소유한다. Source 대기·native reply 완료 대기는 별도 observer가 맡는다. Source가 없던 A를 retained로 옮긴 뒤 B는 같은 proven execution에 입력하며 create 총수는 1이어야 한다.
- UserDefault의 host CreatePermit/grant/정책 검증과 queue EffectPermit은 서로 다른 권한이다. 양쪽을 모두 충족한다. owner를 이유로 복원 정책을 바꾸거나 local tmux로 우회하지 않는다. legacy dedicated와 UserDefault 준비 분기를 현재 #7313 계약 그대로 유지한다.
- adapter 반환은 `(EffectOutcome, AttemptRef)`뿐이다. settle/retain/notice 투영은 owner가 한다. Error 문자열/Enter 성공/초기화 이벤트를 긍정 receipt로 사용하지 않는다.

### 1.4 #7314 C — PaneEpoch·hold·stash

정본은 새 별도 원장이 아닌 #7272 owner다. `PaneEpoch`는 기존 `PaneIdentity`의 server/session/window/pane/pane_pid와 host/endpoint/owner/root 및 **Known** spawn nonce·검증된 binding을 결합한다. M `busy_inject.rs:490`의 attach count/last-attached Generation은 사람 개입 검사용으로 계속 쓰되 pane epoch로 사용하지 않는다. Known/Absent/Unreadable 관측은 M `binding_context.rs:95`를 재사용한다.

cold create 전에는 실제 PaneIdentity가 아직 없을 수 있다. 이때도 host/owner/root/prepared execution nonce의 기존 logical writer 보호를 먼저 얻고 실제 absence/create 정책을 지킨다. create 뒤 검증된 pane pin/physical 보호를 추가하되 old logical lease를 놓고 다른 key로 갈아타지 않는다. 늦게 알게 된 pane은 InputAttempt의 별도 optional resolved-target 증거에 CAS로 연결한다. **AttemptIdentity.target/AttemptRef를 뒤늦게 바꾸지 않는다.** Existing runtime의 최초 pin은 예약 때 고정할 수 있다. Source 바인딩과 마찬가지로 old/restart에서 field가 없다는 사실은 새 쓰기/새 epoch의 권한이 아니다.

wire 형태의 epoch metadata는 새 공통 `tui_input/target_epoch.rs`에 두어 Windows도 inspection/복원이 가능하게 한다. Unix 전용 PaneIdentity 관측/atomic pin과 **직렬화된 record 자체가 쓰기 권한이 아닌** 검증된 live proof를 분리한다. 새 snapshot field의 기본값은 unknown이며 저장된 epoch를 역직렬화했다고 효과 permit을 만들지 않는다. 실제 변환은 기존 guarded child에서 한다. 기존 guarded geometry 조건의 `session_attached=0` 전체를 idle/cold의 공통 pin으로 강제하지 않는다. identity 조건과 busy/idle의 기존 attach 정책을 분리하고 legacy guarded caller의 조건은 그대로 둔다.

기존 DRAFT_GUARDS를 epoch-qualified로 투영하되 legacy session-only hold는 보수적인 Unknown scope로 남긴다. 새 known pane은 새 화면으로 재평가할 수 있지만 **세대 변경만으로 guard를 remove하지 않는다**. old attempted input은 old epoch의 retained 원문/attempt에 독립 보존한다. old raw/receipt identity/실행 중 writer를 지우거나 새 epoch의 성공으로 정산하지 않는다. identity/nonce가 없거나 읽히지 않으면 Held이며, 이것으로 새 runtime을 부재라고 추정하지 않는다.

writer는 현재 논리 owner 보호를 유지하고 known physical target 보호를 보완한다. epoch가 바뀌어도 old WriterLease를 강제 해제하지 않는다. 같은 물리 pane을 다른 logical label로 가리켜도 content I/O가 겹치지 않아야 한다. legacy unknown writer에서 known key로 옮기며 진행 중 lease를 우회하지 않는다. 동일 owner의 old writer가 끝나기 전 새 epoch B를 병렬로 쓰게 하는 최적화는 이번 권고에 넣지 않는다.

실제 호출 경계는 `inject/first_input`→`claude_tui/owner_input`의 준비된 scope→기존 public input/host submit→A1 `busy_inject/stash.rs::transact`다. busy steer는 기존 inject_report 계열이 같은 transaction을 호출한다. managed idle은 `input/submission.rs:14`→`host_input/guarded_submission/idle_stash.rs:405`를 거친다. **새 C-s/paste/Enter transaction을 복제하지 않는다.** A1의 folded proof/restore/last pre-Enter 검사도 함께 소비한다.

준비 결과는 Empty→ReadyDirect, readable·safe Person→ReadyStash, unreadable/unsafe/modal/OwnPending→Held로 명시한다. 단순 nonempty를 영구 대기 사유로 만들지 않는다. A1이 지원하지 않는 provider/host의 stash capability는 실측·계약을 확인하기 전 ReadyStash로 꾸미지 않는다. Codex/Herdr의 미증명 capability가 남으면 S5의 해당 fallback 제거를 승인하지 않는다.

같은 live transaction이 아직 Enter를 쓰지 않았다는 증거를 가지고 own 확인을 마무리하는 경우와 old/restart attempt를 분리한다. chip/LF count/256자 prefix는 exact payload identity나 새 Enter 권한이 아니다. 후자에는 자동 Enter/C-s/paste/새 nonce retry/cleanup 0회다. input delivery와 draft restore의 결과도 분리하여 draft restore 불명 때문에 이미 전달된 입력을 다시 보내지 않는다.

composer lock 안의 pre-payload callback은 mailbox commit을 기다릴 수 있다. 따라서 그 actor callback은 exact metadata CAS·disk commit만 수행하고 composer/session/physical writer lock을 다시 얻거나 provider/Source를 기다리지 않는다. epoch projection도 UI capture/키 입력을 하지 않는다. 대략 `session transition→writer→composer`, effect 전후의 짧은 actor commit을 유지하며 actor 안에서 effect를 await하지 않는다. 이 순서를 깨는 변경은 실제 동시성 진입점 시험으로 막는다.

## 2. 불변식과 진입점 전수표

경로 약어: S=`src/services/`, D=`S/discord/`, T=`S/turn_orchestrator/`, C=`S/claude_tui/`. M은 위 main, L은 위 S2b다. new 파일의 함수 이름은 계획 API이며 존재하는 코드라고 주장하지 않는다.

| ID·불변식 | 실제 현재 진입점/소비점 | S3 책임·남는 배선 |
|---|---|---|
| P1 원문/승인 보존·render 1회 | M `D/router/intake_gate/busy_inject.rs:174,229,241`; `D/router/message_handler/turn_context.rs:19,43`; `intake_turn.rs:192,896` | 3b 공통 준비. Gateway/worker의 전 경로 접수는 S4 |
| P2 reply/prelude/voice provenance | M `intake_turn/voice_intake.rs:9,42,51`; `router/intake_queue_transaction/queue_payload.rs:5`; `router/turn_start.rs`의 B 소유 접점 | 3b, B의 RawV1/선택 prelude 재사용. 실 claim/checkpoint 경계 S4 |
| P3 첨부 수명 | M `message_handler/attachments/busy_inject.rs:5,17`; `S/cluster/attachment_transfer/materialize.rs:19,25` | 3b owner 반환 뒤 retention 확인. cluster 수정 0 |
| P4 cold effect 전 PreSource | M `S/claude/tui_session_launch.rs:71,110`; `codex/.../launch_policy.rs:62`; `S/herdr_launch.rs:398,406,434` | 3a planned, 3c prepare/create seam, 3d permit callback |
| P5 planned bind-once·stable key | L `T/queued_head/v2_types.rs:27,44`; `v2/attempt.rs:9,34` | 3a optional resolved evidence. 기존 AttemptRef를 변형하지 않음 |
| P6 receipt query order 불변 | M `human_frame/history.rs:144,251,307,371,382`; `C/busy_inject.rs:206,239`; `codex_tui/busy_inject/rollout.rs:170,226` | 3a 전체 후보 판정·row owner 유지 |
| P7 모든 history 소비자 | M `human_frame.rs:159,207`; `history/screen.rs:35`; `C/busy_inject/ownership.rs:25,86`; `codex_tui/busy_inject.rs:270,287,374`; `D/.../inject/herdr.rs:80,128`; `tui_prompt_dedupe/injected_steer.rs:111`; M `queued_reconcile.rs:324` | API 호환 reader/legacy guard와 새 V2 API를 구분. 실제 relay/source context 연결은 S4의 전수표에도 이관. census 원시 `s3-census-0.txt` |
| P8 active history slot 반환 | M `history.rs:160,163`; L `v2/retained.rs`, `tui_prompt_dedupe/injected_steer.rs` | 3a queue 정본→history projection. passive 삭제/전체 count 증가로 해결하지 않음 |
| P9 cold create/payload 각각 1회 | M `S/claude.rs:2046,2058,2071`; `codex/absence_launch.rs:374,422`; `claude/herdr_turn.rs:624,652,680`; `codex/herdr_turn.rs:363,382,406,434` | 3c 기존 primitive와 완료 관찰 분리. V2 신규 호출은 시험 hook |
| P10 확인 30초는 payload 경계 | L `v2/attempt.rs:76,91`; M `C/submission_fence.rs:20,76`; `C/input/submission.rs:40` | 3d bootstrap no-deadline/mark-before-payload. 자율 wake는 S4 |
| P11 Source 부재≠runtime 부재 | M `S/session_host/herdr_gate.rs:233,264`; `codex/herdr_turn.rs:406`; L `v2_types.rs:51` | 3a/3c Pending/Bound evidence와 observer. 실제 AnyTurn resolver S4 |
| P12 host admission 보존 | M `S/herdr_launch.rs:330,388,412`; `S/herdr_launch/policy.rs:12,20`; `S/codex/.../launch_policy.rs:70` | 3c CreatePermit·grant·no-local-fallthrough. 정책/운영값 변경 0 |
| P13 epoch-qualified hold | M `C/composer_lock.rs:125,138,144`; `S/tui_input/submission_tmux/guarded.rs:89,115,247`; `binding_context.rs:95` | 3d Known identity 합성. LegacyUnknown은 보수적으로 유지 |
| P14 매 content key에 같은 target | M `C/busy_inject.rs:378,387,400`; `codex_tui/busy_inject.rs:13`; `S/tui_input/submission_tmux/guarded.rs:89` | 3d tmux atomic condition에 pin을 합성. Codex도 동일 Pane을 재사용. scope는 spawn_blocking 내부에 명시 설치하며 TLS가 자식 thread로 저절로 전달된다고 가정하지 않음 |
| P15 stash가 준비의 일부 | M `C/busy_inject/stash.rs:105`; `C/input/submission.rs:14`; `C/host_input/guarded_submission/idle_stash.rs:405` | 3d A1 r2 재사용, A2 input/** 수정 0, unsafe/OwnPending은 Held |
| P16 exact completion만 정산 | L `inject/queued_owner.rs:343,380,394`; `v2/attempt.rs:15`; `v2/retained.rs` | 3c/3d adapter 결과+immutable key, owner가 settle. 현재 B 정산 금지 |
| P17 새 epoch가 old custody를 지우지 않음 | L `v2_types.rs:66`; `v2/retained.rs`; M `C/input/draft_hold.rs:85`(A1 r2 치환 예정) | 3d old retained/old hold·receipt 보존. A1 session-aware 치환을 복원하지 않음 |
| P18 혼합 source에 새 B 유실 없음 | L `v2.rs:53`; `v2/sources.rs:13,32` | 3b fresh constructor와 legacy opaque retention. 재도착 identity와 attempt CAS는 별개 |
| P19 옛 V2 rollback의 write refusal | L `history/compat.rs:16,32`; `T/queued_head/compat.rs`; M `history.rs:105` | 3a 실제 writer golden, optional field default. downgrade/삭제 0 |
| P20 End/Seal과 input receipt 분리 | M `codex/absence_launch.rs:373,408`; `tui_o/writer/rotation/codex_end.rs:143` | 3c 기존 lifecycle hook 보존, 3a GC 근거로 사용하지 않음 |
| P21 no actor/effect lock cycle | L `inject/queued_owner.rs:358,375,380`; M `C/submission_fence.rs:76` | 3d callback actor는 composer lock을 취득하지 않음. 실제 두 입력/held callback 시험 |
| P22 non-test gate·fallback 유지 | M `D/.../inject.rs:119`; L 같은 파일의 provider support gate | S3 내 closed. 실제 전 ingress 통합은 S4, fallback 제거 S5 |
| P23 정상 publish도 sync 확인 | M=L `D/runtime_store/durable_publish.rs:61,109,267`; L `T/queued_head/v2/commit.rs:28,48,61`; `v2/attempt.rs:93` | 3a status 반환 variant→strict owner commit/recover. SyncFailed·UnflushedPlatform은 payload permit0; 기존 critical_publish caller 의미 보존 |
| P24 완료 시각의 terminal 수명 | L `inject/queued_owner.rs:351,393`; `v2/retained.rs:69` | 3d spawn_blocking 완료 뒤 clock 재관측. ARM deadline과 settlement clock 분리, retained late receipt도 관측 시각 사용 |
| P25 선택 head 한 건만 제거 | L `v2/retained.rs:16,63`; `v2/sources.rs:112` | 3a tick/active settle 각각 first identity 검증 후 remove(0). 불일치면 mutation/effect0. retained A 정산은 B items 수정0 |
| P26 오류 시 접수자 원문 유지 | L `T/queued_head/v2.rs:53`; `v2/sources.rs:132` | 3a enqueue/migrate_parked는 borrow, snapshot 안에서 clone. 3b 접수 실패 시 durable ack/checkpoint 없음·호출자 원문으로 retry |


새 provider/host primitive나 legacy callback이 추가되면 이 표와 veto census에 행을 추가한다. old wrapper 이름만 적고 content boundary를 빠뜨리면 전수표 완료로 세지 않는다.

## 3. 파일 계획·크기

각 표의 현재 raw는 **M/L** 순서다. 새 작은 모델/전이 함수는 이미 작은 v2_types/sources/retained/composer_lock 및 한 history V2 child 안에 묶어 불필요한 모듈 분산을 줄였다. history V2 child 약645줄과 공통 payload child 약310줄에는 별도 test 파일을 사용하며 1000줄 경계를 넘기지 않는다. `0`은 해당 ref에 파일이 없음, 새 경로는 제안 경로다. `G`는 현재 어느 기준에서든 1000줄 초과. 추정 순증은 code 기준이고 raw 증가 한도와 다르다. 생성물은 공식 생성기로만 갱신하며 매 단위에 3파일/+120 여유를 따로 두었다. 자세한 측정 원시는 `logs/s3-plan-file-budget.json`이다.

### 3a — history·PreSource·S2b 보존 수리

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 |
|---|---:|:---:|---:|
| `S/tui_input/human_frame/history.rs` | 694/693 | — | -180 |
| `S/tui_input/human_frame/history/v2.rs` | 0/0 | — | +645 |
| `S/tui_input/human_frame/history/screen.rs` | 70/70 | — | +0 |
| `S/tui_input/human_frame.rs` | 251/251 | — | +0 |
| `S/turn_orchestrator/queued_head/v2_types.rs` | 0/89 | — | +18 |
| `S/turn_orchestrator/queued_head/v2.rs` | 0/100 | — | +8 |
| `S/discord/health/turn_deliver/inject/queued_reconcile.rs` | 397/465 | — | +25 |
| `S/discord/health/turn_deliver/inject/queued_provider.rs` | 0/111 | — | +10 |
| `S/turn_orchestrator/queued_head/v2/retained.rs` | 0/107 | — | +28 |
| `S/tui_input/human_frame/history/compat.rs` | 0/46 | — | +10 |
| `S/claude_tui/busy_inject.rs` | 884/884 | — | -100 |
| `S/claude_tui/busy_inject/transcript.rs` | 0/0 | — | +140 |
| `S/codex_tui/busy_inject/rollout.rs` | 269/269 | — | +20 |
| `S/turn_orchestrator/queued_head/v2/sources.rs` | 0/161 | — | +105 |
| `S/turn_orchestrator/queued_head/v2/commit.rs` | 0/94 | — | +35 |
| `S/discord/runtime_store/durable_publish.rs` | 547/547 | — | +40 |

합계 **16파일 / +804**. 생성물 여유 포함 **19파일 / +924**.

### 3b — 공통 payload·source admission

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 |
|---|---:|:---:|---:|
| `S/discord/router/message_handler.rs` | 343/343 | — | +1 |
| `S/discord/router/mod.rs` | 57/55 | — | +1 |
| `S/discord/router/message_handler/input_payload.rs` | 0/0 | — | +310 |
| `S/discord/router/message_handler/turn_context.rs` | 118/118 | — | -15 |
| `S/discord/router/intake_gate/busy_inject.rs` | 398/398 | — | -25 |
| `S/discord/router/message_handler/intake_turn.rs` | 2556/2569 | G | -220 |
| `S/discord/router/message_handler/intake_turn/session_preparation.rs` | 0/0 | — | +300 |
| `S/discord/router/message_handler/intake_turn/voice_intake.rs` | 156/156 | — | -30 |
| `S/turn_orchestrator/queued_head/v2.rs` | 0/100 | — | +8 |
| `S/turn_orchestrator/queued_head/v2/sources.rs` | 0/161 | — | -12 |
| `S/turn_orchestrator/queued_head/v2/source_admission.rs` | 0/0 | — | +110 |
| `S/discord/health/turn_deliver/inject/queued_owner.rs` | 267/456 | — | +20 |

합계 **12파일 / +448**. 생성물 여유 포함 **15파일 / +568**.

### 3c — 첫 입력 effect 추출

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 |
|---|---:|:---:|---:|
| `S/claude.rs` | 4411/4411 | G | -140 |
| `S/claude/first_input.rs` | 0/0 | — | +210 |
| `S/claude/tui_session_launch.rs` | 657/626 | — | -40 |
| `S/claude/tui_session_launch/prepared.rs` | 0/0 | — | +95 |
| `S/codex.rs` | 3195/3209 | G | +0 |
| `S/codex/tui_session_launch.rs` | 1233/1280 | G | -90 |
| `S/codex/tui_session_launch/first_input.rs` | 0/0 | — | +170 |
| `S/codex/tui_session_launch/launch_policy.rs` | 263/107 | — | +5 |
| `S/codex/absence_launch.rs` | 454/446 | — | -45 |
| `S/herdr_launch.rs` | 718/627 | — | -30 |
| `S/herdr_launch/prepared.rs` | 0/0 | — | +150 |
| `S/claude/herdr_turn.rs` | 928/891 | — | -90 |
| `S/claude/herdr_turn/first_input.rs` | 0/0 | — | +135 |
| `S/codex/herdr_turn.rs` | 751/748 | — | -70 |
| `S/codex/herdr_turn/first_input.rs` | 0/0 | — | +130 |
| `S/discord/health/turn_deliver/inject/queued_provider.rs` | 0/111 | — | +10 |
| `S/discord/health/turn_deliver/inject/first_input.rs` | 0/0 | — | +115 |
| `S/discord/health/turn_deliver/inject.rs` | 893/899 | — | +1 |

합계 **18파일 / +516**. 생성물 여유 포함 **21파일 / +636**.

### 3d — epoch·hold·stash 결합·정산 시각

| prod 파일 | 현재 raw M/L | giant | 예상 code 순증 |
|---|---:|:---:|---:|
| `S/tui_input/submission_tmux/guarded.rs` | 396/396 | — | +16 |
| `S/turn_orchestrator/queued_head/v2_types.rs` | 0/89 | — | +75 |
| `S/turn_orchestrator/queued_head/v2/attempt.rs` | 0/99 | — | +65 |
| `S/turn_orchestrator/queued_head/v2/retained.rs` | 0/107 | — | +85 |
| `S/turn_orchestrator/queued_head/v2.rs` | 0/100 | — | +3 |
| `S/claude_tui/composer_lock.rs` | 344/344 | — | +80 |
| `S/claude_tui/busy_inject.rs` | 884/884 | — | +0 |
| `S/claude_tui/busy_inject/epoch.rs` | 0/0 | — | +80 |
| `S/claude_tui/mod.rs` | 30/30 | — | +1 |
| `S/claude_tui/owner_input.rs` | 0/0 | — | +110 |
| `S/claude_tui/submission_fence.rs` | 120/120 | — | +15 |
| `S/claude_tui/host_input/guarded_submission.rs` | 432/432 | — | +5 |
| `S/discord/health/turn_deliver/inject/first_input.rs` | 0/0 | — | +20 |
| `S/discord/health/turn_deliver/inject/queued_owner.rs` | 267/456 | — | +25 |
| `S/tui_input/mod.rs` | 21/21 | — | +1 |
| `S/tui_input/target_epoch.rs` | 0/0 | — | +130 |
| `S/turn_orchestrator/queued_head/v2/sources.rs` | 0/161 | — | +6 |
| `S/codex_tui/busy_inject.rs` | 396/396 | — | +3 |
| `S/tui_input/submission_tmux.rs` | 134/134 | — | +3 |
| `S/discord/health/turn_deliver/inject.rs` | 893/899 | — | +1 |

합계 **20파일 / +724**. 생성물 여유 포함 **23파일 / +844**.

### giant raw 경계·되돌리기

| 파일 | 계획상 raw 경계 |
|---|---|
| `D/router/message_handler/intake_turn.rs` M2556/L2569 | session/voice 준비 블록을 자식으로 옮겨 raw 순감. 새 로직은 자식에 둠. B 착지 뒤 원본/prelude hunk를 재확인 |
| `S/claude.rs` 4411 | 첫 입력/관찰 helper 실제 추출로 raw 순감, 기존 teardown 권한 확대 0 |
| `S/codex.rs` M3195/L3209 | 기존 모듈 가시성/위임 치환, raw 증가 0. 새 wrapper를 giant에 덧붙이지 않음 |
| `S/codex/tui_session_launch.rs` M1233/L1280 | 실제 launch preparation/publish 블록을 자식으로 추출하여 raw 순감 |
| `C/input.rs` 4218, `C/host_input.rs` M1640/L1654, `tui_prompt_dedupe/binding_context.rs` 1161 | 이번 수정 0. 기존 API/child로 소비. 부모 변경이 불가피하면 구현 전에 FILES/재계획 |

S3의 각 단위는 새 V2 production 유입·writer를 열지 않으므로 독립 revert 가능하다. 기존 caller의 helper 추출은 byte/effect 동등성으로 검증한다. S4 활성화 후 rollback은 queue/history V2와 retained/source fence를 읽는 호환 bundle을 유지해야 하며 S3 전체 revert·파일 삭제·배열 downgrade를 안전한 rollback으로 부르지 않는다. 실제 구현 base merge 뒤 각 파일 raw와 `pr_cap_prod.py <해당 리뷰 단위 base> HEAD`를 다시 잰다.

## 4. FILES 겹침표·착수 순서

이 표는 제공된 소유 정보와 fetch한 ref를 대조한 계획이다. 다른 레인의 실제 미커밋 상태나 소유 해제를 새로 확인한 것은 아니다. 이번에는 저장소 코드를 쓰지 않으므로 FILES 승인을 받은 것으로 간주하지 않는다. 실제 구현 착수 전 코디네이터가 아래 접점의 순서·소유를 고정한다.

| 레인·접점 | S3 직접 겹침 | 권고 seam/착지 순서 |
|---|---|---|
| #7314 A1 r2: busy_inject/{ownership,screen,submit,stash}·shared Pane | 3a scanner 추출, 3d `C/busy_inject.rs`의 epoch condition 1곳, 새 `busy_inject/epoch.rs`; 함수/필드가 바뀌면 overlap | A1 r2 CLEAN·착지 후 main merge. `stash::transact`와 `StashEffects`를 재사용한다. r1을 cherry-pick하거나 수정 중인 screen/submit을 복제하지 않는다. A1 boundary 변경 시 먼저 FILES |
| #7314 A1 r2: `C/input/draft_hold.rs:85`를 session-aware로 치환 | **0** | 해당 hunk를 그대로 소비. `composer_owner_in_session`의 OwnPending이 cleanup 가능한 AgentDesk로 변하지 않게 통합 시험. flat `composer_owner`로 되돌리는 patch 0 |
| #7314 A2: `C/input/**` hold 수명 | **0** | 새 `C/owner_input.rs`에서 기존 input API/submission_fence를 호출한다. timeout/held/cleanup 정책은 A2 소유. private API 때문에 input/** 수정이 필요하면 구현 전에 FILES/새 seam 요청 |
| #7314 B: `T/queued_head/prefix.rs:38` eligibility 한 곳·빈 RawV1 carrier | **prefix 수정 0** | B의 carrier/eligibility를 merge 후 소비. S3의 새 source 정규화는 `v2/source_admission.rs`; legacy prefix를 독자적으로 완화하지 않는다. 빈 carrier는 원문/준비 상태를 잃었다는 뜻이 아님을 시험 |
| #7314 B: raw reply/선택 prelude와 `intake_turn.rs`·turn_start/inflight 접점 | 3b의 session/voice 추출과 동일 파일일 수 있음 | B 착지 뒤 추출. B의 raw/selected prelude 타입/소유 API를 재사용하고 inflight 모델·retry stash는 수정 0. 실제 B 타입과 helper 경로는 [U]; 이를 임의 재정의하지 않음 |
| #7313 herdr 기본 서버: `session_host/herdr*`, `claude/herdr_turn*`, `codex/herdr_turn*`, `codex/tui_session_launch*`, provider_dispatch*, `codex.rs` | 3c provider/launch 파일에 직접 겹침. `session_host/herdr*`·provider_dispatch* 수정은 0 | 본체 `1fcc476a5` main 착지는 확인. 잔여 활성 hunk/소유 해제는 구현 전에 확인. policy/create/grant는 기존 trait/API 그대로 쓰고 first-input child로 분리. 필요하면 3c를 tmux/Herdr로 추가 분할 |
| 공통 durable publisher `D/runtime_store/durable_publish.rs` | **3a 신규 접점** 547/547줄, +40 추정 | 알려진 소유 목록과 직접 겹침 없음. 구현 전 코디네이터가 현 소유 확인. 기존 critical_publish 의미는 유지하는 status variant만 추가, giant runtime_store.rs 수정0. 다른 레인이 활성 소유하면 FILES/수리 단위 분리 |
| 위임 롤백: `runtime_bootstrap/queued_recovery.rs`, control.rs, cluster/**, intake_dispatch/intake_worker* | **0** | 기존 공개 materialize/claim API만 소비. production 복원/worker 연결은 S4의 별도 파일 계획. cluster materialize를 소유한다고 해석하지 않음 |
| 재시작 보관 정리: relay intake, intake_turn, health snapshot, queue_park* | intake_turn은 3b의 직접 접점; 나머지 0 | 전자가 아직 활성 소유이면 3b 추출을 착지 뒤로 미룸. session child를 새로 만든다고 giant 부모 수정 권한이 생기지 않음. boot/health/pump 배선 S4 |
| 알림 한 줄화: task_notification_delivery/**, message_outbox*, server/outbox_* | **0** | S2의 retained notice API/identity·non-turn provenance 재사용. 알려진 알림 결과만 참조하고 새 worker/schema를 만들지 않음 |
| Codex 종료 봉인/reader: execution_end/**, absence_launch.rs, warm_followup* | 3c absence_launch에 직접 겹침. execution_end/warm_followup/reader 수정 0 | #7300/#7316 착지는 확인. 원 소유자가 남은 hunk를 보유하면 먼저 해제/FILES. EndObservation의 prepare→validated spawn→spawned 순서를 자식 추출 시 보존 |

실행 순서는 정본/이월 입력 대조(완료)→S1/S2와 최신 main의 coordinator merge→A1 r2/A2/B 착지 hunk 반영→3a→3b→3c→3d다. 3a가 비중첩으로 먼저 가능해도 다른 레인의 부모를 건드릴 때는 해당 착지를 기다린다. 이후 S4에서 모든 production ingress와 wake를 연다. 구현 중 다른 소유 파일이 필요하면 쓰기 전에 FILES로 마감한다.

## 5. 이월 P2 처리 위치

### S1과 이미 확인한 설계 이월

| 원 이월 | 현재 근거·처리 위치 | 증명과 완료 경계 |
|---|---|---|
| S1 P2-1 Recorded::matches 고유 시험 | L의 Codex queued proof/mismatch PG 시험을 보존. Source 확장은 3a | provider/session/path/file/offset/execution/frame/nonce를 각각 바꿔 effect/settle 0. 기존 selector를 우회하는 mutant는 해당 고유 단언 RED |
| S1 P2-2 Unknown 재시작 자동 재전송 | L의 Unknown/restart 시험을 보존하고 3a/3d에서 PreSource·epoch 상태 추가 | 복원 후 positive receipt 전 추가 payload 0, receipt 뒤 A만 정산. 자동 재입력 mutant는 fixture 초기화가 아니라 **restart 뒤 key count**에서 RED |
| S1 P2-3 unsupported Herdr veto 시점 | L `queued_owner.rs:362`의 V2 pre-arm veto는 기반에 있음. 3c 준비 단계의 지원/host 정책 분류로 일반화 | unsupported를 effect/MayHaveSent 뒤로 옮기면 create/paste 0 또는 phase=Reserved 고유 단언 RED. S4 실제 조합 활성화 뒤 재검증 |
| S1 P2-4 adapter에 attempt identity | L `queued_owner.rs:343` typed EffectCompletion. 3c/3d cold 반환에도 유지 | late A 결과가 active B의 key/original/frame을 바꾸지 않음. adapter 직접 settle/current-head 정산 mutant RED |
| r2 P2-2 retained/history 비용 | S2 원문 보존 기반은 유지. terminal의 실제 완료 시각 결함은 별도 S2b P2-2로 3d에서 수리. 3a는 history projection 비용 추가 계측 | R/L/P=0/32/256/1024/4096 경계별 serialized bytes·읽기/전체 비교량, live 수와 passive 증가를 구분. passive 원문/row evidence 무제한 증가 비용은 P2로 공개 |
| r2 P2-3 동일 본문 retained 뒤 후속 | 3a의 live/passive+전체 후보 receipt, 3c/3d 실제 adapter | A 불명 보관 뒤 B/C 각 effect 1, 단일 row를 A/B 조회 순서로 소유시키지 않음. 결과가 모호해도 뒤 입력 진행 |
| r2 P2-6 Parked 의미 | S2의 positive-unattempted/Unconfirmed 분류를 유지. 실제 migration은 S4 | legacy Parked 증거별 fixture→실제 boot/admission PG. budget/marker reset 또는 Parked 영구 Rejected mutant RED. S3가 운영 migration을 완료했다고 쓰지 않음 |
| r2 P2-8 injected-steer projection | S2 slot 반환/late identity는 유지. 3a history projection, S4 실제 relay | 9개 Unknown 뒤 다음 입력, commit→projection crash 후 replay. slot을 withdraw/가짜 Handed로 비우는 mutant RED |
| S2 PLAN의 history V2 A24 이월 | 3a. S2b compat가 읽을 수 있는 planned/live/passive writer golden | live에 가짜 PreSource Entry를 넣거나 V1 entries envelope로 되돌리면 old-reader 거절/원시 보존 고유 단언 RED |
| S2b 구현 보고서의 혼합 envelope 후속(별도 항목, Opus P2-1~8과 구분) | 3b `source_admission` | 실제 PG `[known A,fresh B]`: A effect0/B1/B 원문 보존. 분리 불가 old envelope는 bytes 보관. any-source 전체 거절 복원 mutant RED |

### S2b Opus P2-1~P2-4 — 운영 연결 전 완료 계약

번호와 원인은 `carry-s2b-p2.md:3~7` 및 `review-impl-s2b-r1-claude.out.md`의 원문을 따른다. 이 항목들은 dormant 코드의 **P2**이며 P1으로 재분류하지 않는다. 구현 head의 CLEAN 판정과 별개로 코디네이터가 정한 **S4 production 유입 연결 전 완료 조건**이다.

| 이월·고치지 않을 때의 운영 결과(가정형) | 처리 조각·권고 수리 위치/최소 계약 | 실제 진입점 증명·고유 뮤턴트 |
|---|---|---|
| **P2-1 정상 경로 directory sync 미확인**. 활성화 후 fsync 실패와 전원 손실이 겹치면 Reserved로 rollback되어 A를 중복 입력할 수 있음 | **3a**. M=L `D/runtime_store/durable_publish.rs:267`에 `critical_publish_status -> Result<FlushStatus,String>` variant. 기존 `critical_publish`는 같은 log/사전 오류 처리 후 status를 버리는 wrapper로 유지. giant runtime_store.rs 변경0, pub child를 qualified 호출. L `v2/commit.rs:28`는 Flushed만 채택; SyncFailed면 old/candidate를 pending에 보존하고 **이번 ARM은 Err로 반환**한다. 후속 recover에서 readback+file/parent sync 회복 뒤 채택한다. 회복이 한 번 성공했다고 실패한 ARM 호출에 permit을 뒤늦게 반환하지 않는다. **UnflushedPlatform도 strict V2 effect는 fail-closed**(현재 지원 대상 Unix), read-only reader는 유지. status를 반환하는 권고안은 기존 caller 의미를 바꾸지 않음 | 실제 `durable_probe::Watch`의 **Step::ParentSync 지속 실패**를 owner ARM/실제 mailbox 경로에 주입. 최초 arm Err/permit0·payload0·full candidate 보존. watch 해제→recover→MayHaveSent, **새 permit 재발급0**. owner Fault::Sync만으로 대체하지 않음. SyncFailed를 Flushed처럼 채택하는 mutant는 `permit_count==0`에서 RED. post-rename/old/third/read 상태도 A20 회귀 |
| **P2-2 늦은 결과를 arm 시각으로 정산**. 24h 넘는 지연 후 terminal이 즉시 만료되어 source를 재접수할 수 있음 | **3d**. L `inject/queued_owner.rs:351,393`에서 ARM clock은 deadline에만 사용. blocking effect 완료 뒤 주입 가능한 clock을 다시 읽어 exact settle에 전달. 늦은 receipt reconcile도 실제 관측/정산 시각을 전달. `retained.rs:69`에 임의 arm 값을 보정하는 fallback을 넣지 않음 | 실제 mailbox `v2_effect` 경로에서 effect를 barrier로 막고 clock을 arm+25h로 전진. 완료 terminal.settled_at=completion, 완료+24h 미만 fence 유지·이후 만료. retained A와 active B 분리도 단언. **arm 시각 재사용** mutant는 settled_at 고유 단언 RED. reducer에 시각을 직접 주는 시험만으로 완료하지 않음 |
| **P2-3 같은 identity item 전부 삭제**. 같은 synthetic identity 두 원문이 있을 때 첫 head 정산/만료가 두 번째 원문도 지울 수 있음 | **3a**. L `v2/retained.rs:16,63` 두 곳 각각 `items.first()` identity==선택 head 검증 후 **remove(0)**. 다르면 Err로 원 snapshot 보존. late retained A settle은 active B/items를 건드리지 않음. synthetic까지 Discord message-ID-only fence로 바꾸는 대안은 채택하지 않음 | 서로 다른 body의 non-Discord 동일 id/generation 두 item을 실제 actor→disk에 접수. tick 만료와 active delivered settle **각각** 후 두 번째 raw 그대로1건. first/head mismatch는 Err·bytes 불변·effect0. **tick retain 복원 / settle retain 복원** mutant를 별도로 돌려 각 두 번째 raw 보존 단언 RED |
| **P2-4 enqueue/migrate_parked 오류 시 새 item 반환 없음**. 생산 호출자가 이동한 유일 사본을 잃으면 저장 오류 때 새 원문도 사라질 수 있음 | **3a API 수리, 3b 접수 경계 증명으로 닫음**. L `v2.rs:53`, `v2/sources.rs:132`를 **&PendingQueueItem**으로 받고 candidate에 clone. 호출자는 durable 성공 전 원문/asset/accepted claim을 소유. queue-full·Fault::Before·readback 불명·duplicate(false)를 구분; unknown publish 뒤 새 item/nonce로 재타이핑하지 않고 같은 candidate recovery. known duplicate만 명시 consumed | Fault::Before에서 caller 원문 bytes/source 보존, 회복 후 같은 사본 retry하여 정확1건. parked 확실미시도/불명 두 분류도 동일. 3b actual admission/mailbox+PG에서 publish Err이면 accepted ack/checkpoint0, 원문 유지 후 retry1. 참조를 값 인자로 되돌려 **compile 실패한 것은 kill로 세지 않음**. commit Err를 성공으로 삼키는 compiled mutant는 `accepted_ack==0`/disk1/retained raw 단언에서 RED |

P2-1의 지속 sync 실패 동안은 candidate를 채택하지 않고 pending+원문을 보존한다. 회복 후 MayHaveSent를 채택해도 사라진 EffectPermit을 복원/재발급하지 않는다. 정상 경로와 오류 경로 모두 durable 권한의 의미를 맞춘다. L `commit.rs:72`의 기존 `fsync_parent_and_ancestors`는 `runtime_store.rs:1021`의 직접 fsync를 거쳐 `flush_probe`를 우회한다. 따라서 같은 비giant `durable_publish.rs`에 **게시된 파일의 parent/ancestor sync 결과를 FlushStatus로 돌려주는 재시도 seam**을 두고 strict recover도 그 경계를 쓴다. 기존 fsync helper와 giant 부모는 수정하지 않는다. status variant는 기존 wrapper가 사용하므로 dead_code를 늘리지 않는다. strict recover 전용 seam은 V2 owner와 같은 test-only 경계로 두고 production 전환 시 함께 promote한다. ARM 실패뿐 아니라 watch 유지 중 recover도 Err/미채택, watch 해제 뒤에만 Flushed 채택을 실제 probe event로 증명한다. 정상 ARM 뒤 바로 payload를 못 보낸 결과는 확인/retained로 풀며 자동 재전송하지 않는다.

P2-4에서 `&item`은 caller 소유권을 보장하지만 오류 처리 전체를 타입만으로 증명하지는 않는다. 따라서 새 public admission의 durable 성공 표시·asset 해제·retry까지 별도 runtime 단언으로 닫는다. 기존 파일에 있던 원문과 아직 접수되지 않은 새 원문을 구분한다.

**S4 착수/활성화 체크리스트:** 위 4행의 수리 head·실제 full test ID·binary SHA·양수 실행 수·GREEN/고유 mutant RED·원복 GREEN을 evidence manifest로 확인한다. P2-4는 3a API 시험만으로 완료 표시하지 않는다. 현재 non-test gate/신규 caller 부재를 유지하며, 이 표 자체를 실행 시 가드로 주장하지 않는다.

### S2b P2-5~P2-8 — 원문 기준 후속 배정

| 항목·고치지 않을 때의 운영 결과(가정형) | 처리 위치·권고 | 증명/뮤턴트·완료 시점 |
|---|---|---|
| **P2-5 read-only clear가 다른 오류와 session reset을 가림** | **S4 제어/호환 연결 전** L `D/commands/control/clear_report.rs:49,68`. warning은 QUEUE_READ_ONLY를 기존 reasons 앞에 추가, reply 조기 반환 제거. queue 줄만 “비우지 않음”, session/execution/다른 실패 줄 유지. move-aside/삭제 권한 추가0 | read-only+ExecutionCleanup::Held+다른 failed 동시 fixture에서 모든 이유·세션 초기화 포함. 원문 replace/early-return 복원 mutant RED. control 소유 레인 인계 후 별도 S4 파일 예산에 포함 |
| **P2-6 test-only writer pin을 production 전환 뒤 남김** | **S3~S4에서 cfg 해제하는 바로 그 PR**에 `python3 scripts/check_legacy_supervision_census.py --sync-skips --promote <path>` 공식 절차. 기존 7개 pin 및 S3 신규 pin 전수 확인. production writer를 정식 census로 전환; allowlist/baseline 증가나 gate 개편으로 우회하지 않음 | production caller/해당 writer의 census 선택 양수와 pin 행 제거 대조. cfg 해제만 하고 promotion 생략 mutant는 공식 audit 실패. S3가 test-only라면 기존 pin 유지가 정상임을 기록; 활성화 체크리스트에 carry |
| **P2-7 legacy provider/target 고정값·terminal 무기한** | **S4 boot adoption 연결 전** `v2/sources.rs:57,87`에 **명시 Legacy source/target 상태**를 두고 fake Claude/tmux/namespace-target로 projection하지 않음. 실제 context를 얻은 경우에만 검증된 bind. legacy terminal은 **최초 adoption 시각을 snapshot에 한 번 기록**하여 그때부터24h(재시작마다 연장하지 않음). old attempted raw는 retained로 유지 | V1 Codex/Claude/불명 fixture가 정확 provider 또는 Legacy이고 bogus slot projection0, 입력 재전송0. terminal adoption+24h 경계. fabricated-target 복원 / u64::MAX 복원 mutant RED. S4 모델/reader/rollback 예산에 포함 |
| **P2-8(a) 변화 없는 tick full publish/fsync 비용** | **S4 wake 연결 전** `v2/retained.rs:29`, `commit.rs`에서 pending/recover를 먼저 처리한 뒤 exact serialized bytes가 같으면 publish 생략. **project_slots는 계속 호출**. 변화 있는 만료/notice/TTL은 정상 commit | R=1024에서 unchanged tick의 inode/mtime와 flush probe publish count0, slot은 복구됨. no-op에서도 publish / projection 생략 mutant 각각 RED |
| **P2-8(b) giant 시험 파일에 회귀 본문 추가** | **3d** L `inject_codex_tests.rs:470~518`의 S2 V2 회귀를 새 `inject_codex_v2_tests.rs`로 byte 보존 이동. 부모 `inject.rs`는 자식 선언만 +1 예산. 기존 giant test 파일 raw 감소, 새 회귀는 child | test ID/선택 양수와 본문 지문·회귀 GREEN 대조, 공식 inventory/PG/resolver 생성. 단순 이동은 동작 mutant 대상 아님; 원래 고유 runtime mutants는 보존 |
| **P2-8(c) A23 notice loopback이 predicate 시험뿐** | **S4 producer 연결** 실제 Gateway/intake 분류→owner 진입으로 operational notice 검증. 기존 source provenance/outbox identity 유지 | 실제 throwaway PG에서 허용 bot notice→새 turn/접수0; enqueue/Discord 실패에도 B 진행, marker 전후 restart dedupe. provenance 제거 mutant는 실제 ingress turn0 단언 RED |

위 후속은 원문의 P2로 남기며 새 P1 근거로 쓰지 않는다. P2-5/7/8의 S4 파일 예산은 이번 S3 표에 숨겨 합산하지 않고 S4 PLAN에 추가한다. 반복 상태 파일 선형 비용·미확정 보관량 증가는 별도 정책 변경 없이 공개한다.

### #7314 C 흡수와 종결 책임

| 흡수 항목 | 구현 위치 | #7314에도 이관할 동일 증거 |
|---|---|---|
| full raw custody/retained | S2 기반 + 3b constructor, S4 live ingress 활성화 | 실제 live 경로에서 payload 전 original/source/attempt/target durable; Unconfirmed+restart 후 raw 보존·추가 input 0 |
| generation 재평가·Known spawn nonce 정본 | 3d PaneEpoch/epoch guard, S4 target resolver | same logical name의 old/new pane, same attach count, different physical identity+known nonce. old guard/retained 보존, new screen 재평가 |
| epoch-qualified hold/old attempted input 분리 | 3d projector, S4 boot/wake/hold release | old retained A의 late receipt가 B를 바꾸지 않음. Absent/Unreadable/unknown marker에서는 key0·guard 삭제0 |
| 사람 초안 stash | A1 r2 구현 재사용 + 3d wrapper, S4 실제 ingress | readable Person+folded outgoing frame에서 C-s1/paste1/Enter≤1, draft bytes restore, receipt/response 각1. UnknownUi는 key0, overlay 사라진 뒤 safe stash wake |
| OwnPending 비파괴 수명 | A1 r2/A2 수리 소비 + 3a/3d custody | old chip/foreign prefix/복원 attempt에 cleanup0·추가 Enter0·새 nonce retry0. 살아 있는 단일 transaction의 남은 확인만 별도 허가 |

제안하는 공통 시험 이름은 `i7314_c_epoch_preserves_old_attempt_pg`, `i7314_c_live_unconfirmed_raw_survives_restart_pg`, `i7314_c_unknown_nonce_never_releases_hold_pg`, `i7314_c_overlay_then_stash_uses_existing_transaction_pg`다. 이름은 아직 등록/실행하지 않았다. 구현 때 actual full test ID·binary SHA·head·raw log·source/attempt/epoch/native row/count를 한 evidence manifest에 묶어 코디네이터가 #7314 종결 근거로 재사용한다. A1/A2/B만으로 C 완료를 선언하지 않는다. GitHub 쓰기는 이번 레인이 하지 않는다.

## 6. 증명 계획

### 6.1 RED/GREEN·실제 경로

새 API가 없다는 compile 실패는 RED가 아니다. 우선 L 또는 M+L 고정 base에서 **기존 함수·같은 공개 진입점으로 컴파일되는 회귀 시험**을 만들고 고유 단언의 런타임 실패를 기록한다. 새 tagged field/permit API만의 계약은 유효한 GREEN과 원인 제거 mutant를 별도로 증명한다. 이번 PLAN에서 실행 결과가 있다고 주장하지 않는다.

| 묶음 | RED를 만들 기존 경로·GREEN 진입점 | 고유 단언·PG/행위 경계 |
|---|---|---|
| H1 동일 frame/4096 수명 (A12~A14) | 현재 history::register/receipt를 실제 temp source로 호출하여 identical-unconfirmed/lifetime CAP의 고유 RED 확보 | 3a 실제 owner/adapter PG에서 A retained 뒤 동일 body B/C effect 각1. 별도로 **한 native source에 실제 register→receipt→slot 반환 5001회**, 종료/rotation/GC 없이 모두 진행. 과거 row replay에서 anchor/placeholder 증가0. 단순 passive5000 fixture는 비용 시험일 뿐 A13 완료로 세지 않음 |
| H2 PreSource | L의 기존 inject_human_input(QueuedHead) 시험 hook에 **이미 존재하는** PreSource snapshot과 proven runtime/Pending row를 넣음. Source 없음에 effect0인 현재 실패를 확인 | 3a/3c 실제 mailbox→snapshot→planned registry→permit→argv/paste/Herdr transport. 새 Origin variant 부재 compile 오류를 RED로 세지 않음 |
| H3 late bind/row ownership | 같은 native row를 A/B 반대 순서로 질의하고 부분 line/동명 다른 inode·다른 execution·다른 root/SID를 대조 | Source/target bind는 한 번, 최초 AttemptRef bytes 불변, 확정된 row owner 이동0. ambiguous row에 임의 receipt0, command/enqueue replay도 같은 결과 |
| H4 queue/history crash | snapshot retain/settle commit 뒤 history projection 전에 subprocess 중단·복원; 반대 방향의 dangling planned 등록도 복원 | 원문 한 벌, 새 input0, slot 재투영 멱등. 단순 reducer만 아닌 실제 file I/O/catalog→mailbox 경로 |
| P1 payload | 기존 render_input_chunks/normal/live 준비를 같은 raw fixture로 비교 | reply+병합 sources+첨부2+attachment-only+voice+빈 RawV1+사용자 literal WIP/_; live/복원 최종 frame bytes 동일, author frame1, 원문 불변 |
| P2 voice/첨부 PG | 실제 consumed/live durable voice row·동시 sibling claim, 기존 attachment materialize/retain API | accepted claim 유지·중복 효과0, NotSent/restart에도 original/accepted 보존. owner 반환 뒤 retained 파일 실제 open/hash 성공 |
| P3 혼합 source PG | L의 any-source fence가 새 B 전체를 거절하는 현재 admission을 기존 mailbox API로 재현 | `[known A,fresh B]` A0/B1, B full raw 보존. 불명 legacy provenance는 원문/notice 보관, 문자열 추정 분할0 |
| F1 네 cold 첫 입력 | 실제 provider first-input 함수/launch script와 fake tmux argv/실제 test-owned Unix socket host. Herdr row/grant는 실제 PG | Claude paste1/Enter≤1, Codex tmux argv prompt1/첫 paste0, Codex Herdr argv prompt0/first submit1. 실제 content-bearing 경계를 source+attempt별 기록 |
| F2 Source 지연·AI 미완료 | A의 Source/답변을 막고 physical submit만 끝냄. tick으로 A retained 뒤 B를 같은 runtime에 접수 | create 총1, B input1이 Source/AI 완료 **전** 발생. 물리 lease가 남아 있을 때 writer 중첩0. 실제 observer를 await하면 단언 RED |
| F3 bootstrap 느림·재시작 | boot/UI 준비 clock을 30초보다 길게 지연, payload 전후 단계별 저장/복원 | 준비 중 retained/거짓 경고0, payload 전 commit 실패 시 payload0. MayHaveSent/no-deadline 복원도 자동 create/Enter0, old raw 보존 |
| Matrix·능력 공백 | 네 provider×host 각각 fresh/resume/warm idle/warm busy의 16개 셀을 실제 primitive 경계에서 증명하고 S4 실제 ingress에서도 재증명 | 두 문단·1000자 초과/fold·attach 중 idle·reply/첨부/voice·뒤 입력 진행을 포함. 기존 미지원/Remote의 거절 의미는 별도 유지하며 미증명 조합을 지원 완료로 세지 않음 |
| F4 lifecycle | existing Claude resume `_`/Codex clear EndObservation/warm idle·busy→new turn fixtures를 그대로 소비 | 실제 payload receipt만 인정, `_` 전역삭제0. End prepare/spawned 순서 유지, source anchor/placeholder/completion 각1은 S4 실제 relay에서도 재증명 |
| E1 epoch/hold | same session name과 attach count, different pinned pane+known spawn; Known/Absent/Unreadable·same pane changed nonce 각각 | old guard/retained 삭제0, late A가 B 변경0. 새 known epoch는 새 화면으로 분류하되 physical writer가 살아 있으면 B key0 |
| E2 stash 통합 | A1 r2의 실제 transaction을 호출하는 owner 실제 PG 진입점 | readable draft+folded payload C-s1/paste1/Enter≤1, draft bytes exact restore. unknown/modal/old chip/foreign frame/restore 불명에는 파괴 키0 |
| E3 lock/restart | composer permit을 잡은 pre-payload callback과 두 번째 mailbox 명령을 barriers로 경합 | callback commit 완료·actor responsiveness, writer 중첩0, Source 대기/epoch projection이 composer를 재취득하지 않음 |
| A24 호환 | 실제 3a writer가 만든 golden을 L compat reader와 이전 V1 reader에 제공 | V1 unknown/V2 명시 거절, S2 read-only V2 인식, legacy update/clear/purge write0, planned/live/passive 원시 bytes 보존. 새 optional 필드 누락도 decode |
| Gate/기존 회귀 | 기존 Claude queued/Unknown reconcile·S1 receipt·cfg(not(test)) Codex origin gate 시험 | S3 운영 Codex queued=false 유지. 기존 live/fallback/Remote/headless·대상 밖 provider 결과 불변 |
| C1 strict publisher (S2b P2-1/A20) | 실제 mailbox arm→critical status publisher→flush_probe ParentSync 실패→recover | permit/payload0, disk full candidate 보존, sync 회복 뒤 MayHaveSent/재arm0. fake Fault::Sync만 실행한 PASS 제외 |
| C2 settlement clock (S2b P2-2/A22) | 실제 queued_owner blocking effect를 잡고 완료 clock=arm+25h | settled_at=실제 완료, terminal TTL24h. retained late A/active B 격리 |
| C3 exact head removal (S2b P2-3) | 동일 synthetic identity·다른 원문의 두 item, actor tick/active settle 각각 | 두 번째 raw1건·first mismatch bytes 불변·늦은 retained settle은 B 수정0 |
| C4 failed admission custody (S2b P2-4) | 3a borrow API Fault::Before/parked 두 분류, 3b 실제 admission/mailbox+PG | caller original/asset/voice 보존, accepted ack0/checkpoint0; 회복 후 같은 item retry1. 불명 publish는 readback이며 재타이핑0 |


frame history key는 nonce 단독이 아니라 namespace+source identity+AttemptRef를 포함한다. legacy nonce API에는 exact source를 함께 대조하고 collision은 Pending으로 남긴다. 새 B의 등록은 runtime 최초 `first_prompt_digest`를 덮어쓰지 않는다. 이 둘도 H2/H3의 변형 축에 넣는다.

### 6.2 필수 원인 제거 뮤턴트

각 변형은 compile 성공·선택 양수·고유 assertion 실패(rc101 등)를 확인하고 원복 bytes/HEAD SHA를 대조한다. P2-3의 tick/settle 두 제거 지점은 각각 독립 mutant로 기록한다. 0-test·compile 오류·setup 실패·프로세스 중단은 kill로 세지 않는다.

| mutant | 반드시 실패할 단언 |
|---|---|
| 정상 publisher의 SyncFailed를 성공으로 채택 | 실제 ParentSync 실패 ARM의 permit0·payload0 |
| completion clock 대신 arm 시각 재사용 | 완료+25h 시험의 settled_at과 완료 후24h fence |
| tick의 retain(identity) 복원 | 만료 후 두 번째 synthetic 원문 보존 |
| active settle의 retain(identity) 복원 | 성공 정산 후 두 번째 synthetic 원문 보존 |
| 접수 commit Err를 성공으로 삼킴 | actual admission의 accepted ack0·원문 custody·retry1 |
| passive까지 4096 cap / identical-unconfirmed veto 복원 | 완료·retained 과거 건 때문에 새 B 등록을 막지 않음 |
| 첫 조회 후보에 row 귀속 | A→B/B→A 조회 순서가 같은 Pending/owner 결과 |
| planned에 `(dev,ino)=(0,0)`/가짜 SID 주입, Source 검증 축 하나 제거 | foreign execution/root/path/file receipt0·old-reader 인식 |
| bind 때 AttemptIdentity.source 또는 target을 변경 | 이미 발급된 completion/permit의 stable key, late A only |
| queue commit 후 projection 재적용 제거 | crash/adoption 뒤 live slot 반환·late evidence 유지 |
| V2에 entries를 둠 / write guard 제거 | old writer 명시 거절과 원시 bytes 보존 |
| mixed envelope any-source 전체 veto 복원 | known A0/fresh B1·B 원문 보존 |
| raw reply 대신 rendered prelude 재큐 / voice accepted 제거 / materialize 조기 drop | byte 동등성·claim/파일 보존 고유 단언 |
| V2 receipt에서 full key 대신 nonce alias만 비교 | 같은 nonce/body/file의 B row를 A receipt로 정산하지 않음 |
| first payload를 argv와 paste 양쪽에 전달 / create를 owner 밖에서 호출 | provider별 content-bearing 횟수1 및 permit 없는 content0 |
| Source/AI 완료 await를 physical writer 안으로 이동 | Source 지연 중 B가 같은 runtime에 input1, create 총1 |
| bootstrap ARM에서 확인 30초를 시작 / restore 시 deadline reset | 느린 준비 중 retained0, payload deadline 불변 |
| pre-payload commit 실패에도 key 수행 / consumed permit 재사용 | commit 실패 payload0, 두 번째 content0 |
| same epoch에 physical lease 조기 release / mutation 중 identity pin 생략 | 같은 실제 target writer 중첩0·replacement pane stale key0 |
| generation만 바뀌면 guard.remove / nonce Absent를 fresh epoch로 승인 | old guard/raw 보존, unknown identity key0 |
| OwnPending을 Person 또는 cleanup 가능한 AgentDesk로 분류 | old/restart chip stash/Enter/clear0 |
| safe Person을 무조건 Held / A1 대신 별도 stash sequence | positive stash 경로 진행·transaction 1회·draft restore |
| pre-payload callback actor가 composer lock 재취득 | 실제 callback/두 입력 barrier 시험의 완료와 actor responsiveness |
| adapter가 current head settle / unsupported를 arm 뒤 veto | active B 불변·unsupported payload0/Reserved |
| Codex non-test gate 열기 / owner 밖 legacy 우회 허용 | S3의 cfg(not(test)) false·단일 owner trace·native E-50 exactly-once 경계 |

### 6.3 Veto/NotSent 전수 분류와 wake

형태만으로 NotSent를 판정하지 않고 **effect phase**를 함께 본다. payload 가능성이 생긴 뒤에는 아래 준비 불가 사유가 나타나도 Unknown/retained로 간다. 자동 재전송 근거가 아니다. 아래 출처는 M의 Claude Veto 11종(`busy_inject.rs:51`), Codex Veto 17종(`codex_tui/busy_inject.rs:27`), `host_input::InputRefusal:33`, `tui_input/submission::Refusal:10`, `HerdrGateRefusal:33`, L queued adapter의 static 사유다. 판정 코드에서는 enum을 exhaustively match하고 새 원인/문자열은 typed Held/Unknown으로 fail closed한다. 사유를 알 수 없다고 generic NotSent retry로 통과시키지 않는다.

| 현재 사유 전수 그룹 | 분류·목표 동작 | wake/사용자 의미·책임 |
|---|---|---|
| InvalidInput / InvalidPrompt | (다) 의미상 빈 원입력(본문·원 첨부·accepted voice 모두 없음), 렌더된 UTF-8 frame 64KiB 초과·영구 invalid는 Rejected+원문 보관. attachment-only/voice의 raw text empty와 구분 | 즉시 notice, 뒤 입력 진행. 3b/3d |
| LockContended / WriterContended / input_in_flight / MutationBusy / transition_busy | (가) 실제 writer/transition 준비 대기 | writer 종료·transition release+backstop. 새 key0. 3d 계약, S4 wake |
| HumanAttached | busy Steer에만 legacy attach 거절 유지. idle/cold는 기존 submit의 draft/identity 검증 적용 | busy는 detach/재관측. idle/cold를 attach만으로 영구 대기시키지 않음. 3c/3d |
| AttachUnknown / PaneUnavailable / CaptureUnavailable / GeometryChanged / NoComposer / Modal / NotReady | (가) identity/UI 준비 대기, 부재로 추정 금지 | 준비 상태별 관측+backstop; “화면/초안 안전 확인 대기”. 3d/S4 |
| Draft / OwnDraft | safe readable Person이면 ReadyStash, unsafe/OwnPending/unread는 (가) Held | Empty뿐 아니라 safe stash 가능 전환도 wake. A1/A2+3d/S4 |
| NotBusy | 새 owner 안에서 proven idle SubmitNewTurn로 재선택. 일반 턴 fallback 아님 | 별도 busy 턴 시작을 기다리지 않음. 3c/S4 |
| TranscriptUnavailable / session_unresolved | proven live runtime+파일 부재는 ReadyWithoutSource/PreSource. unreadable/identity 미확정은 (가) | Source/Resolved 또는 target 재관측. 파일 없음만으로 create 금지. 3a/3c/S4 |
| voice_claim_deferred / uploads_not_ready / upload_materialization_failed | (가) 실제 voice 승인/claim·attachment materialize 준비 | 기존 durable claim/accepted 및 upload API, 준비 완료+backstop. raw 삭제/일반 턴 우회0. 3b/S4 |
| episode_unidentified / imsg_reply::UNBOUND | 대상 밖 Obligated/external 응답 결합의 기존 거절 | 해당 origin 의미 유지, Discord owner가 무시하거나 local pane으로 우회하지 않음 |
| LoadFailed | payload 미진입을 입증하면 (가) transport/buffer 준비 실패 | bounded 재관측·backstop, 원문 유지. 3d |
| UnpredictableRender | (나) transport 능력 공백. 빈 줄/긴 본문/fold/control을 현재 adapter가 못 증명하는 경우 | A1 folded proof 또는 idle/cold legacy primitive로 지원. 단순 영구 NotSent 대기 금지. 미해결 조합 S5 제외 |
| NOT_TYPED_LEAD(!, /, <@, DISPATCH:) / queued_not_eligible | 실제 명령은 기존 intake command/dispatch 분류 유지. 일반 user payload 형태 제한은 (나) | source/role/RawV1 검증 후 owner 지원을 증명. eligibility 전역 삭제0. 3b/S4; S5 전 해소 |
| QueueShown | native queue 표시 자체를 busy 대기 근거로 삼지 않음. 실제 composer/intent 검증으로 재평가. 안전 primitive가 없으면 (나) | 해당 capability 시험 전 gate/fallback 제거 금지. 3c/3d→S4 |
| NotSteerable / TurnUnknown / TurnChanged | 미진입이면 (가) exact intent/target 재관측. proven idle면 SubmitNewTurn. 부재/실패로 치환 금지 | native UI/binding 변화+backstop. 3c/S4 |
| LedgerFull | (가) active history/steer slot 준비 대기. retained/settled 때문에 막히는 것은 lifecycle 수리 대상 | retain/settle projection 뒤 wake. cap 숫자 인상 금지. 3a/S4 |
| provider_unsupported / Unsupported(HostKind) | (나) 준비 단계에서 거절. provider/host 범위 밖 의미 유지 | 해당 조합은 ARM 전 중단, local fallthrough0. 3c |
| Unknown / Conflict / IdentityMismatch / holder_changed / queued_author_mismatch | (가) 권한/identity 확인 대기. 이름 fallback과 구분 | 정확 row/binding 재조회 또는 운영자 수동 확인 사유. 다른 attempt 정산0. 3a/3d/S4 |
| queued_name_unresolved | 표시 이름만 실패하면 ID fallback. source-author 검증 불가이면 위 identity Held | display lookup 완료를 무기한 필수 조건으로 두지 않음. 3b |
| queued_gate_off / v2_not_prepared / mailbox_unavailable | 기존 scope/gate 또는 내부 준비 불가. 무조건 새 provider fallback 금지 | S3 gate 유지. 준비 failure는 원문 보존/진단; S4 배선 완료 후 활성화 |
| queue_nonempty / source_owned / queued_head_reconciled | 앞서 소유한 source의 순서/멱등 처분. retained 자체는 backlog 아님 | active 순서 유지, known source effect0, notice를 새 turn으로 재섭취0. S2 보존+S4 |
| queued_head_unknown / queued_paste_unnamed | 이미 시도 가능성이 있는 기존 attempt. 준비 NotSent 아님 | receipt 재대조/기존 deadline·retained, 자동 입력0. 3a/3d/S4 |
| SubmissionFence | durable commit 실패/권한 거절 | 첫 payload0, 원문 유지, 저장소 회복 재관측. payload 뒤 오류면 Unknown. 3d |
| Confirmation | phase-aware. payload/Enter 뒤 확인 불명은 Unknown, receipt 성공으로 꾸미지 않음 | 30초→retained+notice; Source/턴 종료만으로 reset0. 3a/3d/S4 |
| AdmissionStopped / ExecutionClosing | 명시 중단/closing 상태의 준비 불가, closed input은 키0 | 기존 cancel/stop 권한만 소비, watcher가 취소 source를 재생성하지 않음. S4 제어 배선 |
| RestoreOn / RestoreUnverified / PolicyUnverified | (가)/(나) 현재 host mode의 정책 검증 불가 | LegacyDedicated/UserDefault의 기존 정책을 각각 소비. 설정 변경/host force0. 3c/S4 |
| NoStoredExecution / MarkerOtherHost / MarkerUnverified / ServerChanged / PaneUnverified / OtherNonce / RootReplaced / ProviderReplaced | (가) host/실행 identity 준비 불가 | exact evidence 재조회. absent proof와 다름; fallback0·새 launch0. 3c/3d/S4 |
| TerminateUnsupportedServer | 입력 전달과 다른 기존 제어 capability | owner가 kill/terminate로 해결하지 않음. 범위 밖 기존 거절 유지 |

busy Veto의 UnpredictableRender 중 geometry 미확정/resize는 (가), 유효 메시지의 render 능력 제한은 (나), 실제 영구 invalid는 먼저 validation에서 (다)로 구분한다. 빈 raw 본문에 원 첨부/accepted voice도 없으면 생성된 header/prelude만으로 유효 입력을 만들지 않는다. 빈 RawV1 carrier는 이 의미상 빈 입력과 별개다.

`InputRefusal::Composer(...)`와 `::Herdr(...)`는 해당 중첩 행을 소비한다. UnknownUi/LSP 등 미측정 화면에는 자동 선택키/Enter를 추가하지 않는다. 준비 대기 notice는 source/epoch/episode의 기존 non-turn dedupe를 사용하며 완료/성공으로 표시하지 않는다. 실제 wake와 모든 production caller의 매핑은 S4 전수표/시험에서 다시 확인한다.

### 6.4 E-50 콜드 부트 수용 E2E (#6325 흡수)

책임은 **S4에서 production 단일 경로를 실제 ingress로 연결해 결정적 E2E를 만들고, S5 fallback 제거 전 isolated native 수용을 완료**하는 것이다. S3의 provider helper 시험만으로 E-50 완료를 선언하지 않는다.

1. S3c/3d: 실제 mailbox·PG·snapshot·launch script/host transport의 deterministic 시험을 준비한다. clean runtime/없는 provider pane에서 새 input 1건, content effect1, owner 밖 fallback0, Source/receipt를 거친 정상 observer response1을 검증한다.
2. S4: temp root·throwaway PG·명시적인 test config로 AgentDesk 새 프로세스를 띄운다. boot restoration이 끝난 첫 Gateway/DM/허용 bot 입력을 실제 intake 분류→단일 owner→bootstrap+first input→native user row→assistant output→기존 relay/outbox delivery commit까지 통과시킨다. 새 코드의 높은 층 함수 호출 수만 세지 않는다.
3. 기본 matrix는 Claude/Codex × tmux/Herdr fresh. 각 조합에서 첫 payload1·create1·정확 native receipt1·logical answer delivery commit1·anchor/placeholder 완료1을 대조한다. argv에 payload가 든 Codex tmux에는 첫 paste0, Codex Herdr argv payload0을 추가한다. 답변은 고유 비어 있지 않은 ack/body hash를 포함한다. 대기 안내/empty final을 응답으로 세지 않는다. 실제 Discord 분할 청크는 logical answer 중복과 구분해 함께 기록한다.
4. 접수 직후/launch 직후/receipt 뒤 delivery commit 전 중단·재시작, 동일 Discord source catch-up 재도착을 추가한다. 새 generation에서도 추가 content effect0, 기존 원문/attempt 복원, 답변 중복 commit0을 검증한다. 실제 input 미확정이면 response 성공 대신 retained+notice로 남으며 자동 fallback0이다.
5. S5 최종 native 수용은 전용 temp AgentDesk 인스턴스·전용 test tmux session/socket·허용된 isolated Herdr 테스트 서버와 provider 실행으로 같은 trace를 확보한다. 운영 `AgentDesk-*`, user herdr, :8791/15432/운영 DB·운영 runtime을 쓰지 않는다. 운영 config/launchd.env 복사도 하지 않는다. Mac/WSL runner, 실제 CLI/Auth의 사용 가능 여부는 [U]; 코디네이터가 별도 안전한 시험 환경을 지정해야 한다. 실제 provider를 못 돌리면 deterministic PG만 PASS이고 **native E-50은 미실행**이다.

권장 등록 모듈은 `D/relay_e2e/input_owner_cold_boot_tests.rs`(제안), Native trace manifest에는 base/head, 환경, actual provider/host, source/attempt/epoch, argv/paste/Enter/create/native row/answer commit 횟수와 원시 로그를 넣는다. `owner admission을 우회한 legacy 호출`, `argv+paste 중복`, `response observer를 legacy fallback으로 재실행`, `cold receipt를 Source 존재로 대체` mutant가 각각 E-50 고유 단언에서 RED여야 한다. 이 동일 증거를 #6325·#7272 수용과 #7314 C의 live custody 종결에도 연결한다.

### 6.5 새로 보완한 계약의 검토 지점

이 계획의 bootstrap 단계·resolved-target·history planned 확장은 r2 보완 제안이며 구현 사실이 아니다. 독립 설계 리뷰는 다음을 먼저 확인해야 한다.

- `MayHaveSent + bootstrap tag + deadline=None`은 UI/부팅을 기다리는 단계이지 재시작 후 NotSent 증거가 아니다. payload 전 callback이 없었던 과거 row도 기본 LegacyUnknown으로 보존한다.
- 첫 create와 첫 payload에 같은 one-shot owner effect를 쓰되, Codex argv 경계와 Claude/Herdr payload 경계의 timestamp를 혼동하지 않는다. 기존 즉시 warm 3-publish 목표와 준비 대기 추가 commit을 구분한다.
- resolved Source/PaneEpoch는 immutable key 밖의 증거다. 늦은 A receipt는 저장된 A의 source/epoch를 검증하며 현재 B의 binding이 달라졌다는 이유로 A를 B에 귀속하거나 재전송하지 않는다.
- adapter가 호출하는 pre-payload callback은 actor가 현재 effect를 기다리고 있는 상태에서 실행되지 않는다. actor→effect await와 composer→actor commit→composer 재취득의 cycle을 둘 다 금지한다.
- Strict publish는 부모 sync 실패가 로그 후 Ok로 지워지지 않는 status API다. recover도 같은 관측 가능한 flush 경계를 사용하고 UnflushedPlatform을 성공으로 삼지 않는다. 기존 critical_publish 호출자는 로그/오류 의미를 유지한다.
- V2 full-key receipt seam은 기존 parser의 추출/매개화다. nonce alias만 받는 옛 wrapper를 새 owner의 exact settlement로 직접 사용하지 않는다.

### 6.6 실행 순서·잘못된 PASS 방지

구현 시 각 리뷰 단위에서 대상/회귀 시험→mutant→원복 GREEN→전체 clippy(변경 warning0)→공식 source readers→static-only 순서다. 실제 PG는 `pg_throwaway.sh`·AGENTDESK_REQUIRE_PG=1, cargo-shim PATH 선두, TMUX/TMUX_PANE unset, strip=none, 기본 병렬로 실행한다. full-lib 로컬 sweep과 test-threads=1은 사용하지 않는다. binary 경로/SHA256·head·시험 필터와 실제 이름·실행/실패/ignored·원시 로그를 보관한다.

required PR job의 path gate/run mode/platform에서 새 test ID가 실제 선택되는지 확인한다. lib/PG manifest·non-PG filter·문서·SQL 관측 inventory는 저장소 공식 생성기로만 갱신한다. 새 test-only helper는 resolver 등록 절차를 따르며 규칙/allowlist/debt baseline을 완화하거나 새 allow를 붙이지 않는다. source readers는 actual cargo_rc=0/failed=0/pg_not_run=0/selected>0를 대조한다. 각 새 head에서 영향받는 source test를 재실행하고, 별도 head 증거 재사용은 동일 source/의존성 지문과 범위를 명시한다.

## 7. 미확정 실측·설계 검토 항목

| 항목 | 현재 상태·결정에 필요한 측정 | 안전한 기본값/완료 위치 |
|---|---|---|
| 정본/이월 원문 | 지정 파일 수신·대조 완료. §0 차이표와 §5 P2-1~8 처분 확정 | 입력 누락 없음. 새 bootstrap/payload 단계의 독립 설계 리뷰와 구현 증명은 남음 |
| M+L+A1r2/A2/B 실제 base | M/L 각각만 측정, 합성 merge는 하지 않음 | 코디네이터 merge 후 줄 수/소유/전체 entry census 재측정; rebase 없음 |
| bootstrap no-deadline/actual-payload callback 의미 | S2b는 arm 즉시30초. 본 계획의 tagged bootstrap/payload 단계는 설계 보완 제안. 기존 stash claim이 C-s 전에 호출됨을 source로 확인했으므로 actual payload hook을 별도로 대조해야 함 | LegacyUnknown default·crash retention·warm 3회 publish 계측을 독립 리뷰. old missing field로 NotSent 추론 금지 |
| A1 r2 실제 API·fold proof | r1 ref만 읽음. r2의 draft_hold:85 session-aware 수리와 final pre-Enter 검사 확인 필요 | r2 CLEAN 뒤 shared transaction을 호출. API를 추정해 동시 수정하지 않음 |
| provider/host별 safe stash capability | Claude transaction/managed tmux entry는 source로 확인. Codex/Herdr의 동일 capability 실측은 없음 | safe로 입증된 조합만 ReadyStash. capability 공백을 일반 Draft 영구 대기로 숨기지 않고 S5 해당 조합 제거 보류 |
| attachment retention 기간/복원 reference | materialize/copy_into API 확인. 장시간 retained의 외부 파일/URL 보존 정책은 변경하지 않음 | owner 반환/재시작/NotSent에서 기존 보존 범위 시험. 외부 자산의 무한 수명 보장은 하지 않음 |
| history 비용 | current V1 전체 read/write, passive/planned compact는 아직 구현 전 | R/L/P별 JSON bytes·읽기 비교량·실제 등록 latency를 분리 계측. 모든 reader 안전 경계 전 물리 GC 없음 |
| physical target alias/lock 순서 | 같은 Pane을 Claude/Codex busy가 공유함을 확인. 새 epoch pin과 callback은 미구현 | 같은 실제 pane/다른 logical label 및 held pre-payload callback을 시험. old lease를 epoch로 우회하지 않음 |
| Source bind/reopen와 retired reader | End+Seal이 모든 retained/history 소비자 안전 경계를 보장하지 않음 | foreign Source/부분 line/기존 offset/reboot file 식별을 검증, unknown은 Pending/retained |
| E-50 native 환경 | Mac/WSL의 전용 provider/Auth/test host 지정 전 | S4 deterministic 실제 PG와 S5 native 증거를 구분. 미실행은 미실행으로 보고 |
| 새 source/host error 원인 | enum/현재 static 사유를 읽었으나 실제 r2/합성 base에서 추가될 수 있음 | 구현 전 exhaustive mapping을 갱신. phase 불명은 Unknown/Held, 자동 입력 없음 |
| 실제 크기 | 본문 표는 추정, 신규 구현 diff 0 | 단위 base 대비 pr_cap/raw 재계수. +1200 전에 추가 분할 |

### 리뷰 집중 항목

| 항목 | 이 계획의 차단 방법·남은 범위 |
|---|---|
| 경계 시각 | boot/준비와 payload 확인30초 분리, 29/30초·늦은 Source·restore deadline 고정. 실제 자율 wake는 S4 |
| 같은 owner 두 입력·경합 | 물리 lease 유지·stable exact key·actor callback의 composer 재취득 금지. 새로운 pane도 old lease를 삭제하는 근거가 아님 |
| 재시작 중간 상태 | Reserved/MayHaveSent/Unknown/retained/planned/Source-bind/projection 경계를 각각 복원. optional bootstrap field 없으면 LegacyUnknown으로 보수 처리 |
| 옛 형식 | S2 read-only HistoryV2의 Entry 제약을 지키는 planned 확장·writer golden. unknown version을 빈 store로 만들지 않음 |
| 문서로만 선언한 가드 | non-test gate/caller 부재, single-use permit, 원문 commit/fence, atomic target pin, 실제 entry mutant를 요구. 이 문서는 실행 보호 장치가 아님 |
| 잘못된 0/PASS | 이번 계획 새 시험/PG/빌드0. 구현 때 양수 선택·actual PG·binary SHA·고유 mutant assertion을 대조. E-50 fake transport와 실제 native 수용을 구분 |

## 증거 목록·진행 상태

이번 산출물은 계획뿐이다. 새 시험 바이너리/SHA, GREEN/RED, 실제 PG 실행 증거는 **없다**. 이전 S2b의 native 종료를 재개 시 확인한 것 외에는 cargo·시험·clippy·prepush를 실행하지 않았다.

- `logs/s3-plan-start.json`: 앞선 코드 조사 시작 시 head·이전 native rc(역사 기록). `logs/s3-plan-finalize-start.json`: 이번 정본 대조 30분 예산 시작 시각.
- `logs/s3-plan-source-bases.json`: 앞선 입력 누락 상태의 역사 기록. **최종 입력 SHA256·존재·대조 범위·현재 M/L은 `logs/s3-plan-finalize-facts.json`**을 기준으로 한다. 두 시점의 사실을 덮어 섞지 않는다.
- `logs/s3-drift-log.txt`: 지정 -G 명령과 실제 커밋 목록.
- `logs/s3-file-line-counts.json`, `logs/s3-plan-file-budget.json`: 기준별 raw·giant·파일별 예상 net·합계.
- `logs/s3-census-0.txt`: 현재 history 호출 소비점. `s3-census-1.txt`: render/upload 호출점. `s3-veto-census.json`: L의 기존 adapter reason 좌표. 본문 enum 표는 M에서 별도 읽어 보정했다.
- `logs/s2b-native-postreview.log/.rc`: 이전 L native 완료 rc0/PREPUSH_OK/Windows cross 성공. 새 S3 기능 증거로 세지 않음.

환경은 WSL2/bash, worktree `/home/adk/adk/wt/i7272`다. 운영 설정/서버/DB/runtime/user herdr를 읽거나 조작하지 않았다. git fetch/ls-remote는 읽기 용도로만 사용했으며 push·PR·이슈·DAG 쓰기는 없다. 저장소 소스/시험/등록 파일은 수정하지 않았다. 신규 실행 프로세스 없음. 정본 대조와 번호별 처분은 완료했으며 새로운 실측/독립 설계 리뷰 과제는 §7에 남겼다. 파일별 raw·예산·표 합계·필수 항목·남은 누락 문구에 대한 문서 자체 대조는 오류0이었다(`logs/s3-plan-finalize-validation.json`); cargo/기능 시험 결과로 세지 않는다.

최종 확인: 원격 main read-back도 `05d89aaa6a2d047c41f6ba44a45a85991206e52d`로 같았다. worktree HEAD는 L 그대로이며 clean이다. 이 계획은 **정본 대조를 완료한 PLAN_READY**이며 코드·시험 구현 또는 S4 활성화 완료를 뜻하지 않는다.
