판정 요약

P0 0건, P1 4건, P2 3건입니다.

새 abort-release의 Released·Releasing DELETE CAS 자체에서는 이중 소유나 이중 writer를 만드는 차단 결함을 찾지 못했습니다. 차단 사유는 주로 런북이 결정된 운영 계약과 아직 합쳐지지 않은 점, epoch 판정이 실제 코드와 다른 점, Worker 이후 실패 복귀가 닫히지 않은 점, 실측 SHA가 구 버전으로 고정된 점입니다.

기준 SHA 차이

2239d6c4755는 설계 좌표 de739c72d78의 바로 다음 커밋이며, 추가분은 #7021 transcript 추종 변경입니다. runtime_bootstrap.rs, o_channel_homes.rs, channel_home_boot.rs, channel_home_drain.rs, o_writer_host.rs는 두 SHA에서 blob SHA가 같으므로 이번 리뷰의 home 상태 전이·Runner adopt·writer join 판정은 좌표 차이의 영향을 받지 않습니다. 반면 transcript 교체 추종, !clear, 재부착·복구 증거는 #7021 및 오늘 밤 배포분의 영향을 받으므로 실제 배포 SHA에서 다시 고정해야 합니다.

런북 쪽 지적
P1-1 — 공식 실패 경로가 아직 금지된 수동 PG 복구로 남아 있다

근거: i5340-twomac-r1-runbook.md:29-34, 87-128, 587-591, 631-634, 1120-1138

런북은 결론과 R2에서 “정확 epoch 기반 긴급 PG 복구”를 승인 대상으로 두고, S3·S4 롤백과 §7 승인표에도 exact Released emergency repair를 남겨 두었습니다. 반면 CLI 설계 §5는 이 손 SQL을 abort-release로 대체한다고 명시합니다. 두 문서가 서로 다른 공식 실패 경로를 가리킵니다.

수용 기준 위반:

“운영 PG를 손으로 고치는 긴급 복구는 쓰지 않는다. 대신 탈출 CLI를 실측 전에 착지시켜 공식 실패 경로로 쓴다.”

실패 장면: S3가 Released(E1)에 도달한 뒤 H 부팅이 실패한다. 현재 런북을 따르면 운영자는 승인된 exact-CAS PG 수리를 수행하거나, 그 문구를 따르지 않을 경우 공식 다음 단계 없이 C를 Released로 남기게 된다.

고치지 않으면 운영에서 생기는 일: 수동 DB 변경 금지 원칙을 어기거나, 시험 채널 입력이 Released에서 계속 차단된다.

개선 방향

docs/design/review/i5340-twomac-r1-runbook.md의 다음 부분을 한 번에 고쳐야 합니다.

결론의 “긴급 PG 복구 승인”을 “CLI 착지 SHA와 abort-release 사용 승인”으로 교체.
R2의 손 SQL 계약을 삭제하고 CLI 계약을 본문으로 편입.
S3 Releasing 실패는 G 재기동 후에도 정해진 blocker가 남을 때 abort-release C --expected-epoch E0.
S3·S4 Released 실패는 endpoint를 G로 되돌리고 H writer 부재를 확인한 뒤 G에서 abort-release C --expected-epoch E1.
성공 뒤 status에서 row 없음 확인 → G fenced restart → G writer·단일 응답 확인.
§7의 exact Released emergency repair 행을 제거하고, Released abort CLI 추가=후속을 실측 진입 선행조건으로 변경.
승인 증거에 CLI 구현 커밋 SHA와 T1~T6 결과를 넣기.

증명 시험·뮤턴트

문서 ratchet: twomac 런북에 긴급 PG, exact Released emergency repair, 운영 DELETE FROM o_channel_homes가 남으면 실패.
상태별 dry-run fixture: Releasing·Released만 CLI를 가리키고 Worker·Reclaiming·Reclaimed·Orphaned는 각 정상 경로 또는 명시적 중단을 가리키는지 검사.
뮤턴트: --expected-epoch 제거, Released에서 H writer 확인 생략, CLI 대신 손 SQL 문구 복원.

권고: CLI 구현 전에 런북을 먼저 또는 같은 변경 묶음에서 고치십시오. 별도 후속으로 두면 사용자 승인 시점의 공식 문서가 금지된 절차를 계속 지시합니다.

P1-2 — 런북의 epoch 전이 기대값이 실제 상태 기계와 정면으로 다르다

근거: i5340-twomac-r1-runbook.md:52-65, 605-621, 636-675, 790-1030; src/db/o_channel_homes.rs:280-365

실제 코드는 다음 두 전이에서 epoch를 올리지 않습니다.

Released(E1) → Worker(E1): adopt는 state·holder·target만 바꾸고 epoch는 유지.
Worker(E1) → Reclaiming(E1): begin_reclaim도 epoch를 유지.
epoch가 새로 생기는 시점은 holder가 떠나는 finish_release와 finish_reclaim뿐입니다.

CLI 설계의 W3·W5 표는 이를 정확히 적었지만, 런북은 S4에서 Worker(E2), S14에서 Reclaiming(E3), Reclaimed(E4)를 요구합니다.

실패 장면: 올바른 생산 코드가 다음 정상 순서를 만든다.

일반 텍스트
Releasing E0
→ Released E1
→ Worker E1
→ Reclaiming E1
→ Reclaimed E2
→ row 없음

하지만 현재 판정기는 S4에서 Worker E2를 기다리고, S14에서 Reclaiming E3을 기다린다. 따라서 정상 구현이 S4 또는 S14에서 반드시 FAIL한다. S5~S12의 Prepared(epoch=E2) 기대도 정상 E1 출력을 잘못 거절한다.

고치지 않으면 운영에서 생기는 일: 정상 위임·회수 실측이 거짓 실패로 중단되고, 목표 1·2의 수용 증거를 만들 수 없다.

개선 방향

런북과 offline judge의 epoch 표기를 다음으로 통일해야 합니다.

일반 텍스트
E0: Releasing
E1: Released → Worker → Reclaiming
E2: Reclaimed
이후 row 제거

구체적으로 S4~S14의 모든 Worker E2, Prepared E2, Reclaiming E3, Reclaimed E4와 S15의 row epoch를 함께 수정해야 합니다. evidence manifest에는 이름표 release_epoch, worker_epoch, reclaimed_epoch를 쓰고 숫자 별칭 E0/E1/E2는 판정기가 계산하게 하는 편이 안전합니다.

증명 시험·뮤턴트

실제 DB 함수로 delegate → finish_release → adopt → begin_reclaim → finish_reclaim을 실행해 [E0,E1,E1,E1,E2]를 고정하는 PG 시험.
offline judge fixture에서 정상 시퀀스가 PASS하고, adopt 또는 begin_reclaim에서 epoch 증가를 기대하는 fixture가 FAIL해야 함.
뮤턴트: adopt나 begin_reclaim에 nextval을 넣으면 상태기계 시험과 judge 시험 모두 죽어야 함.

권고: 코드가 아니라 런북·판정기를 코드 계약에 맞추십시오. epoch를 상태마다 새로 만드는 방향으로 코드를 바꾸면 기존 intake stamp와 CAS 계약을 불필요하게 넓힙니다.

P1-3 — Worker 이후 holder 재기동 실패에는 공식 복귀 수단이 없다

근거: i5340-twomac-r1-runbook.md:815-851, 890-951, 990-1035; i5340-home-escape-cli-r1.md:225-246; channel_home_boot.rs:604-687; channel_home_drain.rs:133-208

S10은 H 재기동이 실패하면 동일 SHA/config로 한 번 더 재기동한 뒤 입력 금지 상태로 남깁니다. S12도 한 번 더 재기동한 뒤 force를 금지합니다. S14의 Reclaiming 역시 “H를 살려 drain을 끝내는 것”만 허용합니다.

하지만 Worker·Reclaiming에서:

abort-release는 의도적으로 거절합니다.
reclaim은 Worker를 Reclaiming으로만 바꿉니다.
finish_reclaim은 holder H의 watch/drain만 실행할 수 있습니다.
force를 쓰면 Orphaned가 되지만 그 상태는 이번 CLI가 지원하지 않습니다.

즉 H가 다시 뜨지 않으면 G가 안전하게 채널을 회수할 공식 경로가 없습니다.

재현 조건

S4~S9를 통과해 Worker(E1, holder=H)가 된다.
계획된 S10 또는 S12에서 H dcserver를 종료한다.
다음 H 프로세스가 start_channel_homes 전에 종료되거나 supervisor가 잘못된 SHA·role을 시작한다.
row는 Worker H에 남고 G의 C 입력은 로컬로 실행되지 않는다.
G에서 reclaim을 실행해도 Reclaiming H가 될 뿐 H가 없어서 완료되지 않는다.

이는 계획된 holder 정지 경로 하나와 다음 부팅 실패 하나로 재현되며, C는 무기한 중단됩니다.

고치지 않으면 운영에서 생기는 일: 의도적으로 holder를 재시작한 뒤 H가 복귀하지 않으면 시험 채널을 gateway로 되돌릴 수 없다.

개선 방향

Worker/Reclaiming을 PG에서 강제로 지우는 기능을 abort-release에 추가하는 것은 권고하지 않습니다. 노드 로컬 pane·ledger·미배달 출력을 모른 채 지우면 실제 유실과 이중 실행 위험이 생깁니다.

최소 계약은 holder rescue boot입니다.

S0에서 H의 이미 검증된 <RUN_SHA> 바이너리, Runner config, runtime root, launch 환경과 독립 수동 기동 명령을 보존.
launch supervisor 경로와 별도로 같은 바이너리를 다시 띄울 수 있는 복구 명령을 확정.
S10·S12·S14 실패 시 endpoint는 H-local로 유지하고 신규 입력을 금지.
rescue H가 동일 Worker/Reclaiming epoch를 mount하고 renewed_at을 두 번 갱신하며 writer 상태를 증명한 뒤 정상 reclaim.
이전 바이너리를 rescue로 쓸 경우 현재 DB schema와 queue/transcript 형식 호환성을 사전에 확인.
이 rescue 경로를 시험할 수 없다면 S10·S12를 이번 실측에서 제거해야 하며, 그 경우 목표 3을 닫았다고 판정해서는 안 됩니다.

증명 시험·뮤턴트

실제 또는 두 프로세스 하네스에서 첫 H restart를 의도적으로 실패시키고, 독립 rescue start로 동일 epoch를 재개한 뒤 terminal 한 번·reclaim 완료를 증명.
rescue 후 같은 nonce의 duplicate terminal이 없는지 검사.
뮤턴트: rescue를 Standby로 시작, 잘못된 instance ID 사용, 다른 SHA 시작, epoch 갱신 없이 health만 정상으로 표시.

권고: 지금 런북에 rescue 계약을 넣으십시오. Orphaned 지원을 넓히는 것보다 단순하고, 기존 안전한 정상 reclaim 경로를 그대로 사용합니다.

P1-4 — 실측 바이너리와 증거 manifest가 폐기된 ccd737074에 고정돼 있다

근거: i5340-twomac-r1-runbook.md:4-7, 430-447, 1148-1180

런북 결론은 두 노드가 ccd737074를 유지한다고 하고, S0도 그 SHA를 진입 조건으로 두며, evidence 경로와 manifest도 ccd-to-de739-critical-path.diff, gateway/holder=ccd737074로 고정합니다. 이는 “오늘 밤 배포 SHA로 다시 고정한다”는 결정과 반대입니다.

수용 기준 위반:

“실측 바이너리는 ccd737074로 고정하지 않는다. 오늘 밤 배포가 두 노드에 나가고, 그 SHA로 다시 고정한다.”

실패 장면: 실제 두 노드는 새 SHA로 실행하지만 manifest와 judge는 ccd를 기대한다. S0에서 거짓 실패하거나, 더 나쁘게는 새 코드의 결과를 ccd 결과로 기록한다. S8의 stop capability, S7의 !clear, S12의 transcript 재부착 전제도 실제 바이너리와 어긋날 수 있습니다.

고치지 않으면 운영에서 생기는 일: 실측 결과가 어느 코드에서 나온 것인지 증명할 수 없어 전체 증거를 수용할 수 없다.

개선 방향

문서에는 우선 <RUN_SHA> 자리표시자를 쓰고, 배포 완료 뒤 실제 SHA로 생성 단계에서 치환.
G·H의 git SHA뿐 아니라 실행 파일 SHA-256까지 동일해야 S0 PASS.
manifest에 design_base=2239d6c..., run_sha, 두 노드 binary hash를 분리 저장.
ccd-to-de739-critical-path.diff를 2239d6c..<RUN_SHA>-critical-path.diff로 교체.
오늘 밤 diff에서 적어도 다음 전제를 다시 판정:
runtime_bootstrap, channel_home_boot, o_channel_homes, drain, claim fence.
/clear 실제 거절·commit 계약.
#7021 transcript 교체 추종과 S6·S12 재부착.
herdr_stop_settlement_available() 및 S8 예상 결과.

증명 시험·뮤턴트

judge는 G/H SHA 또는 binary hash가 다르면 즉시 실패.
manifest에 ccd737074, <RUN_SHA> 같은 미치환 값이 남으면 실패.
critical-path diff와 capability snapshot이 없으면 S0 실패.
뮤턴트: 한 노드 SHA 불일치를 허용하거나 manifest SHA만 검사하고 실제 binary hash를 무시.

권고: 실제 배포가 끝난 직후 문서를 재생성해 고정하십시오. 현재 상태로 사용자 승인 게이트에 올리면 안 됩니다.

P2-1 — preflight 실패의 C 외 영향이 destructive 전 단계에서 검증되지 않는다

근거: runtime_bootstrap.rs:526-578; home_availability.rs:94-147; 런북 S3·S13

preflight 실패는 C 하나만 막는 것이 아닙니다. 해당 프로세스에서 같은 provider의 selected_herdr_channels 전체를 held_channels로 기록하고, turn 시작·intake routing·O writer·delivery가 모두 이를 거절합니다. 다른 provider는 영향을 받지 않지만, 같은 Claude provider의 선택된 Herdr control 채널은 같이 보류될 수 있습니다.

현재 S13 체크포인트는 S1, S4, S9~S12, S16이고, delegation switch를 켠 G의 첫 재부팅 직후인 S3은 빠져 있습니다.

실패 장면: G가 switch=true로 부팅하지만 PG pool 또는 instance ID preflight가 실패한다. C뿐 아니라 같은 provider의 다른 선택된 Herdr 채널도 입력과 POST가 거절된다. S4까지 가기 전에는 control nonce로 이를 확인하지 않습니다.

고치지 않으면 운영에서 생기는 일: 시험 채널 밖의 선택된 Herdr 채널이 유지보수 창 동안 함께 멈출 수 있다.

개선 방향

S2 전에 row가 없는 상태에서 S0b — delegation preflight dry boot를 추가하는 것이 가장 단순합니다.

양 노드 switch=true로 재부팅하되 home row는 만들지 않음.
provider availability가 PreflightPassed인지 확인.
실제 selected_herdr_channels 집합을 evidence에 저장.
선택된 각 control 채널에 nonce를 보내 한 번 응답하는지 확인.
H Runner에서는 Discord REST 자격증명도 확인. RestWorker는 queue restore가 끝난 뒤에만 reconcile_done을 올리므로 자격증명이 없으면 adopt가 fail-closed로 계속 대기합니다.
이 dry boot가 실패하면 row가 없을 때 switch를 원복하고 종료.
S3 G restart 직후에도 control snapshot을 한 번 추가.

증명 시험·뮤턴트

두 선택 채널 C·X가 있는 fixture에서 MissingPool 또는 MissingInstanceId를 주면 delegate 이전에 런북 판정이 중단되고 X도 refusal 대상임을 보고해야 합니다. control 채널 검사를 생략하는 judge 뮤턴트가 살아남아서는 안 됩니다.

권고: P2이지만 실측 전에 런북에 반영하십시오. 설정 오류 한 번의 영향 범위를 C로 한정했다고 오판하는 것을 막는 저비용 절차입니다.

CLI·상태 전이 쪽 지적
먼저 확인된 안전성

제안된 DELETE는 Released 또는 Releasing, exact epoch, Releasing의 exact holder, open intake 0을 한 SQL 문에서 검사합니다. finish_release·adopt와 같은 row를 갱신하므로 PostgreSQL row lock 뒤 WHERE 재평가에 의해 한쪽만 이깁니다. 삭제 후 G writer도 기존 bundle을 stop_and_join한 뒤 새 writer를 build하므로 동일 프로세스의 이중 writer 경로는 찾지 못했습니다. H는 endpoint가 remote이면 delegated_boot_ownership(herdr_runs_here)에서 writer 대상에서 빠집니다.

따라서 다음 전제가 유지될 때 Releasing abort의 exact epoch·open intake 0 조건은 이중 소유·이중 writer 방지에 충분합니다.

G가 실제 holder이자 원래 gateway.
endpoint가 G-local.
H endpoint가 remote이고 delegated writer가 없음.
runtime command가 병행되지 않는 승인된 quiet window.
성공 뒤 G fenced restart.
P2-2 — Worker→Reclaiming과 늦은 delegated intake INSERT가 직렬화되지 않는다

근거: intake_router_hook.rs:121-215, 700-840; intake_outbox.rs:105-164; o_channel_homes.rs:347-365; worker_status.rs:87-93; channel_home_drain.rs:100-111, 133-208

leader router는 처음에 Worker row와 epoch를 읽은 뒤 owner·policy·readiness를 여러 번 await하고 마지막에 평범한 insert_pending을 수행합니다. 그 사이 begin_reclaim이 같은 row를 Reclaiming으로 바꿔도 마지막 INSERT에는 home 상태 재검증이 없습니다. claim fence는 state가 정확히 Worker일 때만 그 row를 claim하며, drain은 모든 open intake가 0이 될 때까지 기다립니다.

실패 장면

Discord 입력이 Worker(E1)를 읽고 route_to_instance에 진입한다.
INSERT 직전에 멈춘다.
운영자가 channel-home reclaim C를 실행해 Reclaiming(E1)이 된다.
늦은 router가 home_epoch=E1 pending row를 INSERT한다.
row는 Reclaiming이라 worker가 claim할 수 없다.
drain은 그 open row 때문에 끝나지 않는다.
abort-release는 Reclaiming을 거절하고 force는 금지돼 있다.

사용자가 유지보수 quiet window를 어기거나 정확한 경합 시점에 입력해야 하므로 P2입니다.

고치지 않으면 운영에서 생기는 일: reclaim이 open_intake에서 영구 정지해 return 단계가 수동 판정으로 넘어간다.

개선 방향

새 테이블이나 원장 없이 기존 home row로 직렬화할 수 있습니다.

delegated INSERT와 begin_reclaim을 각각 transaction으로 변경.
delegated INSERT는 home row를 FOR SHARE로 잠그고 Worker·holder·provider·epoch를 다시 확인한 뒤 같은 transaction에서 INSERT.
begin_reclaim은 row를 FOR UPDATE로 잠근 뒤 open intake 0을 같은 transaction에서 확인하고 Reclaiming으로 갱신.
router가 먼저면 reclaim이 기다린 뒤 open intake를 보고 거절.
reclaim이 먼저면 router가 기다린 뒤 Reclaiming을 보고 INSERT를 거절.
새 durable 상태나 migration은 필요 없습니다.

현재 캠페인에는 코드 수정 전이라도 S14 직전 “신규 입력 금지 → 두 번의 5초 간격 open-intake 0 → reclaim” 절차를 넣어야 합니다. 다만 이 절차는 경합을 줄일 뿐 코드 증명은 아닙니다.

증명 시험·뮤턴트

barrier 시험 A: router가 Worker row lock을 잡은 뒤 멈춤 → reclaim 시도 → INSERT commit 뒤 reclaim이 open intake로 거절.
barrier 시험 B: reclaim이 row lock·상태 변경을 먼저 완료 → router가 깨어나 INSERT 없이 HomeChanged/InTransition.
어느 순서에서도 Reclaiming과 unclaimable pending row가 동시에 남지 않아야 함.
뮤턴트: router 최종 state 재검증 제거, reclaim open-intake 확인 제거, row lock 제거.

권고: 현재 CLI 착지를 막지는 않는 P2 후속으로 분리하되, twomac 런북에는 quiet barrier를 지금 넣으십시오.

P2-3 — 핵심 finish_release ↔ abort 경합 시험이 시험 계획에서 선택 사항으로 남아 있다

근거: i5340-home-escape-cli-r1.md:165-235, 250 전후

현재 T4는 Released의 adopt ↔ abort만 다룹니다. Releasing 갈래의 핵심 경쟁자인 finish_release ↔ abort는 기준 대응표에서 “필요하면 T4 방식으로 추가 가능”으로 남아 있습니다. 사용자 집중 항목 5의 바로 그 경합이므로 선택 사항이면 안 됩니다.

코드 논리상 양쪽 CAS는 안전해 보이지만, 상태·holder·epoch 조건이 구현 중 완화되거나 SQL이 두 문장으로 분리되는 회귀를 현재 계획이 직접 죽이지 못합니다.

고치지 않으면 운영에서 생기는 일: 구현 회귀 시 drain은 Released로 갔다고 보고 H 부팅을 진행하는데 G는 row를 삭제했다고 보고 gateway writer를 복귀시키는 상충 판정이 시험에서 검출되지 않을 수 있다.

개선 방향

T4b를 필수 시험으로 추가하십시오.

순서 A: finish_release(E0)가 먼저 이겨 Released(E1); abort(E0)는 Stale.
순서 B: abort(E0)가 먼저 이겨 row 없음; finish_release(E0)는 Stale.
두 순서에서 Applied 합계는 정확히 1.
A의 최종 row는 Released(holder NULL,target H,E1).
B의 최종 row는 없음.
실제 watch까지 붙인 시험에서는 B 후 G gate unregister, lease 종료, old writer join, gateway writer 한 개만 존재.
자동 재시도는 없어야 함.

추가 뮤턴트는 abort_releasing_ignores_epoch, abort_releasing_ignores_state, finish_release_after_missing_row_is_success가 적합합니다.

권고: 구현 크기가 작고 Releasing 지원의 중심 증명이므로 CLI PR의 필수 시험으로 승격하십시오.

집중 항목별 최종 결론

S1~S12 및 return 복귀:
S1~S2 row 없음은 닫혀 있습니다. S3 Releasing·Released는 CLI를 런북에 합치면 닫힙니다. S4 이후 Worker는 H가 살아 있는 동안 정상 reclaim으로 닫히지만, S10·S12의 H 재기동 실패는 현재 막다른 상태입니다. S14 Reclaiming도 같은 holder rescue가 필요합니다. S15 Reclaimed→row 없음은 mini-first writer join과 G remove_reclaimed로 닫혀 있습니다.

R1 Standby 순환과 Runner 우회:
근거가 맞습니다. Standby의 restored flag는 독립 false이고 holder가 된 뒤 채널 restore가 돌아 순환합니다. RestWorker는 queue restore 완료 후 reconcile_done을 올리므로 Runner가 순환을 피합니다. 단 REST 자격증명 누락 시 reconcile_done이 올라가지 않아 fail-closed Released로 남습니다.

R6 preflight 영향:
C 하나가 아니라 같은 provider의 선택된 Herdr 채널 전체입니다. 다른 provider와 선택되지 않은 tmux 채널까지 전역으로 막는 것은 아닙니다.

상태 전이 전수표:
생산 o_channel_homes 쓰기 W1~W8은 빠진 경로를 찾지 못했습니다. abort-release W9도 기존 상태만 삭제하며 새 상태·테이블을 만들지 않습니다. 읽기·소비 표도 직접 row 소비 경로를 실질적으로 덮습니다. CLI 문서의 [U-2]는 endpoint locality filter 코드로 해소됩니다.

Releasing 경합:
exact state·holder·epoch CAS와 writer stop-and-join 구조는 충분합니다. 다만 finish_release ↔ abort 실경합 시험을 필수로 올려야 합니다.

Orphaned 미지원과 force 금지:
생산 코드에서 Orphaned를 만드는 경로는 force_orphan뿐이므로 규칙을 지키면 twomac 중 Orphaned에는 도달하지 않습니다. 하지만 이 규칙만으로 Worker·Reclaiming의 holder 부팅 실패까지 닫히지는 않습니다.

금지된 설계 추가:
CLI 설계에는 새 off 플래그, 새 원장, 새 PG 테이블, migration이 없습니다. 기존 row에 조건부 DELETE 하나와 CLI·시험만 추가하므로 이 항목은 통과입니다.

VERDICT: ISSUES
