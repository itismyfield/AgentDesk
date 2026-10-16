i5340_twomac 설계안
결론

권고 토폴로지는 현재 gateway를 그대로 유지하고, 맥미니를 일시적으로 Runner/RestWorker로 전환한 뒤 같은 bulk1 시험 채널 1509350490461180105의 Herdr endpoint를 단계적으로 왕복시키는 안이다.

실측 바이너리는 두 노드에 이미 배포되어 있고 s3fence 수용 증거와 정확히 대응하는 ccd737074를 유지한다. 설계 좌표는 요청대로 de739c72d7856a34dbbbde6a0b54f867b698005b를 사용한다. ccd737074..de739c72d의 관련 diff는 증거 묶음에 저장하되, 이번 실측만을 위해 네 커밋을 추가 배포하지 않는다.

핵심 순서는 다음과 같다.

일반 텍스트
Gateway-local / row 없음
  → 기존 gateway Herdr 실행을 정확한 nonce로 terminate·Retired
  → delegate row 생성
  → endpoint는 아직 gateway에 둔 채 gateway 재시작
  → Releasing → Released
  → endpoint를 mini로 변경
  → gateway 먼저 재시작하여 기존 gateway writer join
  → mini를 Runner로 재시작
  → Released → Worker(holder=mini)
  → 두 Mac 실측

  → mini의 Herdr 실행을 terminate·Retired
  → reclaim
  → Reclaiming → Reclaimed
  → endpoint를 gateway로 되돌림
  → mini 먼저 재시작하여 delegated writer join
  → gateway 재시작
  → gateway의 home-bound writer join → row 제거 → gateway writer 시작
  → row 없음 / gateway-local 복귀

이 순서라면 생산 코드 변경 없이 목표 1~5를 실측할 수 있다. 다만 아래 네 항목은 실행 전 승인 게이트다.

맥미니를 Full Standby에서 Runner로 일시 전환.
같은 채널의 Herdr 실행을 endpoint 왕복 전에 두 번 terminate·Retired.
양 노드 endpoint 설정과 프로세스를 정해진 순서로 재시작.
Released에 고립될 경우에만 사용할 정확 epoch 기반 긴급 PG 복구 계약을 사전 승인.
1. R1~R6 판정
R1 — 현재 Full Standby인 맥미니는 Released를 adopt하지 못한다

판정: 사실. 현재 설정 그대로 실행하면 P1 경로다.

Standby의 turns_restored는 false로 시작한다. Released target은 port.restored()==true일 때만 adopt하지만, Standby의 채널 복구인 restore_settled는 이미 holder가 되어 recovery permit을 얻은 뒤에만 실행된다. 즉 다음 순환이 생긴다.

일반 텍스트
adopt하려면 restored=true 필요
restored=true가 되려면 restore_settled 필요
restore_settled하려면 이미 holder여야 함

관련 좌표는 runtime_bootstrap.rs:500-522,580-624, channel_home_boot.rs:72-131,645-676이다.

고치지 않으면 운영에서 생기는 일: row가 Released(holder=NULL,target=mini)에 머물고 intake가 계속 닫혀 목표 1의 “다른 노드가 adopt·lease로 holder가 된다”를 위반한다.

이번 실측 해소안

맥미니를 runtime_profile=runner, cluster.role=runner로 부팅한다.
RestWorker는 reconcile_done을 restored 표식으로 쓰므로 Released target을 정상 adopt할 수 있다.
endpoint와 row가 준비된 뒤 맥미니를 부팅해야 한다.

증명

Released(E1,target=mini)가 30초 안에 Worker(E2,holder=mini,target=NULL)로 바뀐다.
맥미니 health가 runtime_profile=runner, cluster_standby=false, 해당 endpoint local=true를 보인다.
channel_homes[C].home=intake_open, epoch=E2가 된다.
renewed_at이 5초 주기로 진전한다.
Standby 코드 수정안 — 이번 노드에는 비권고

선행 조각 i5340_standby_target_restore로 분리한다. 맥 승인 대상이다.

최소 계약은 다음과 같다.

channel_home_boot.rs: Released target이 Standby일 때 CAS adopt 자체는 허용.
channel_home.rs: adopt 직후에는 lease와 recovery permit만 열고 일반 intake·명령·POST는 계속 닫음.
Standby 채널 복구가 성공한 뒤에만 일반 intake를 연다.
복구 실패·프로세스 종료·stale epoch에서는 일반 intake가 절대 열리지 않는다.
runtime_bootstrap.rs에는 줄을 추가하지 않고 자식 모듈에서 배선한다.

예상 생산 변경은 2~4파일, 순증 약 120~180줄이다. 시험은 최소한 다음이 필요하다.

restored=false Standby가 Released를 adopt하되 일반 입력은 계속 거절.
복구 성공 후에만 intake와 POST 허용.
복구 실패, adopt 직후 crash, stale epoch, 다른 holder 경쟁.
실제 PG 두 프로세스에서 Released → closed Worker → restored/open Worker.
뮤턴트: restore 전에 intake 열기, 복구 실패를 성공으로 읽기, stale epoch 허용.

권고: 이번에는 Runner로 실측하고, Standby 지원은 별도 후속으로 둔다. 현재 목적에 비해 코드 선행 변경의 위험이 크다.

R2 — Released·Orphaned에는 운영 CLI 탈출 경로가 없다

판정: 사실. Releasing에도 abort가 없다. 실행 전 복구 승인 없이는 실측 착수 금지다.

reclaim은 Worker만 받으며, force는 holder가 있는 stale row만 Orphaned로 바꾼다. Released는 holder가 NULL이므로 force 대상도 아니다. Orphaned를 삭제하거나 gateway로 복귀시키는 CLI도 없다. src/cli/channel_home.rs:23-39,173-201, src/db/o_channel_homes.rs:347-455에 해당한다.

고치지 않으면 운영에서 생기는 일: gateway가 release를 끝낸 뒤 target 부팅에 실패하면 시험 채널이 입력 차단 상태로 무기한 고립된다.

이번 실측 계약

정상 복귀는 항상 상태별 기존 경로를 쓴다.

row 상태	정상 조치
없음	설정 원복 후 재시작
Releasing	endpoint를 gateway-local로 유지하고 기존 holder gateway를 재기동하여 drain을 계속시킴
Released	target 부팅을 복구하여 adopt시키는 것이 우선
Worker	channel-home reclaim
Reclaiming	holder mini를 유지하여 drain 완료
Reclaimed	mini writer join 후 gateway를 부팅하여 기존 remove_reclaimed 경로 사용
Orphaned	자동 복구 금지. 실측 중단 후 사용자·맥의 별도 수리 결정
긴급 Released 복구 계약

사용자 사전 승인을 받은 경우에만 사용한다.

대상이 정확한 시험 채널 하나일 것.
상태가 Released.
holder IS NULL.
target과 epoch가 저장된 스냅샷과 정확히 일치할 것.
open_intake.current_epoch, other_epoch, unrouted가 모두 0일 것.
양 노드 writer가 종료되었음을 확인할 것.
endpoint를 gateway로 원복한 뒤 exact state/epoch 조건으로 row 하나만 제거.
영향 행 수가 1이 아니면 즉시 중단.
이후 gateway를 재시작하여 정상 writer를 다시 연다.

Releasing, Reclaiming, Orphaned에는 이 복구를 일반화하지 않는다.

후속 코드안

o_channel_homes.rs와 cli/channel_home.rs에 abort-release <channel> --expected-epoch를 추가한다.

Released, holder NULL, target=local gateway, exact epoch일 때만 제거.
open intake가 하나라도 있으면 거절.
Orphaned는 계속 거절.
새 테이블은 만들지 않는다.
예상 순증 100~160줄.
맥 승인 대상이다.
R3 — endpoint는 한 노드에 고정되며, 단순 이동 순서는 실제로 잘못된다

판정: 사실. 작성자가 제시한 두 선택지보다 더 엄격한 단계 전환이 필요하다.

특히 endpoint를 mini로 먼저 옮기고 그 다음 delegate하는 순서는 금지다.

gateway가 Releasing을 처리할 때 endpoint가 더 이상 local이 아니면 legacy_reset을 호출한다. 그러나 이 채널은 여전히 Herdr-configured이므로 기존 reset 경로가 configured_refusal을 반환한다. drain은 source_reset에서 멈춘다. 관련 좌표는 다음과 같다.

runtime_bootstrap.rs:626-667: herdr_runs_here와 release reset.
commands/control/provider_reset.rs:38-82: home drain용 reset.
admin_host_guard.rs:86-123,239-247: Herdr-configured reset 거절.

고치지 않으면 운영에서 생기는 일: row가 Releasing의 source_reset blocker에 고정되어 위임이 시작되지 않는다.

안전한 forward 전환
endpoint가 gateway-local인 상태에서 기존 Herdr 실행을 terminate·Retired.
delegate row 생성.
endpoint를 바꾸지 않은 채 gateway 재시작.
Releasing → Released 완료.
그제야 양 노드 endpoint 설정을 mini로 변경.
gateway를 먼저 재시작해 기존 gateway writer를 join.
mini를 Runner로 재시작해 writer를 시작하고 adopt.
안전한 return 전환
endpoint가 mini-local인 상태에서 mini의 Herdr 실행을 terminate·Retired.
reclaim.
Reclaiming → Reclaimed.
양 노드 endpoint 설정을 gateway로 되돌림.
mini를 먼저 재시작해 delegated writer를 join.
gateway를 재시작해 Reclaimed row를 제거하고 gateway writer를 연다.

Retired 실행은 다음 Herdr cold launch의 유효한 선행 상태다. herdr_launch.rs는 HostedState::Retired에서 새 Pending 실행 설치를 허용한다. 따라서 이것은 지원되지 않는 “Herdr → legacy 복귀”가 아니라 Herdr endpoint 간의 retire 후 재생성이다.

R4 — 기존 하네스는 mixed endpoint를 거절한다

판정: 사실. 단, 현재 관측처럼 configured endpoint가 하나뿐이면 코드 수정 없이 사용할 수 있다.

scripts/e2e/tui_relay/herdr.py:86-105는 선택 endpoint만이 아니라 모든 configured endpoint가 local이어야 통과한다.

고치지 않으면 운영에서 생기는 일: 다른 remote endpoint가 하나라도 추가되어 있으면 실제 시험 채널이 정상이어도 readiness 단계에서 실패한다.

권고
현재 endpoint가 정확히 하나인지 설정 파일과 두 노드 health로 확인한다.
한 개라면 위임 단계에서는 mini loopback에서, 복귀 단계에서는 gateway loopback에서 기존 하네스를 실행한다.
하네스 결과는 로컬 Herdr readiness 보조 증거일 뿐, row·writer·두 노드 위임의 최종 PASS로 세지 않는다.

추가 endpoint가 발견될 때만 시험 코드에 다음 최소 변경을 한다.

herdr.py에 --expected-local-channel C 추가.
선택 채널 endpoint가 local인지 엄격히 확인.
선택되지 않은 remote endpoint는 목록에 남기되 readiness 거절 사유에서는 제외.
선택 endpoint 누락·remote·중복은 여전히 실패.
예상 순증 20~40줄, 시험 전용이며 생산 변경은 없다.
R5 — home 등록과 delegated writer 시작은 boot 고정이다

판정: 사실. candidate watch가 보인다는 사실만으로 무재시작 전환은 완성되지 않는다.

channel_home_boot::start는 boot 때 row와 candidate를 읽는다. target candidate watch는 나중에 생긴 Released row를 볼 수 있지만, delegated runtime의 writer는 boot 시 등록된 home을 기준으로 한 번만 시작되며 watch가 동적으로 시작하지 않는다. gateway도 endpoint가 remote이고 row가 foreign인 상태로 부팅하면 반환 watch가 없다. channel_home_boot.rs:396-484,604-686과 ChannelWriters::start_boot·delegated가 이를 고정한다.

고치지 않으면 운영에서 생기는 일: mini가 holder를 adopt했지만 managed writer가 없거나, Reclaimed row가 gateway target으로 남은 채 제거되지 않는다.

해소 규칙
delegate row 생성 후 gateway를 재시작한다.
Released와 endpoint 변경 후 gateway를 먼저 재시작하고 mini를 재시작한다.
Reclaimed와 endpoint 원복 후 mini를 먼저 재시작하고 gateway를 재시작한다.
switch를 켰다는 이유만으로 기존 프로세스가 home을 설치했다고 간주하지 않는다.

증명: 각 재시작 뒤 새 PID·process start time, row mount, writer readiness, local O ledger를 함께 남긴다.

R6 — preflight 실패는 provider의 선택된 Herdr 채널 전체를 보류한다

판정: 사실.

home_availability::preflight 자체는 MissingPool과 MissingInstanceId만 생산하지만, registration 실패는 해당 provider의 선택된 delegated Herdr 채널 집합 전체에 적용된다.

고치지 않으면 운영에서 생기는 일: 시험 채널 외의 같은 provider Herdr 채널도 입력·writer·delivery가 보류될 수 있다.

delegate 전 읽기 전용 확인
양 노드의 비어 있지 않은 cluster.instance_id.
PostgreSQL pool 구성 및 실제 연결 health.
두 instance ID가 cluster node registry에 online으로 존재.
gateway/runner 역할과 gateway_preferred_instance_id.
해당 provider의 selected writer channels.
endpoint execution node·socket·home·session.
startup log에 다음 오류가 없음:
channel homes need a PG pool and cluster.instance_id; none started
delegation unavailable 관련 경고.
provider backlog와 target 채널 queue가 0.
startup doctor의 failed 항목이 PG·cluster·Herdr·O writer·recovery와 무관하다는 구체적 판정.

전용 public preflight endpoint는 없으므로 설정 스냅샷, loopback health, cluster registry, startup log를 조합한다.

2. 전제 고정표

이하에서:

G: 현재 실제 gateway인 100.71.169.10. MacBook인지는 아직 [U].
H: Mac-mini.local.
C: 1509350490461180105.
Phase A: gateway-local release.
Phase B: mini holder.
Phase C: gateway return.
전제	G에서 필요한 값	H에서 필요한 값	현재값·읽기 전용 확인	변경과 적용 시점
바이너리	ccd737074	ccd737074	/api/health, 프로세스 binary SHA, 배포 manifest	변경 없음. 양 노드 SHA 불일치면 중단
설계 소스	de739c72d… 좌표	동일	git show, ccd..de739 critical-path diff 저장	실측 바이너리와 설계 source 차이를 manifest에 명시
cluster.instance_id	실제 gateway ID, 예: mac-book-release	mini의 고정 ID	config 원문, cluster registry, health	서로 다르고 비어 있지 않아야 함. boot 적용
delegation switch	Some(true)	Some(true)	config 원문. health에 row가 없으면 값 확인 불가	CLI는 즉시 읽지만 home 설치는 boot 고정. 첫 delegate 전에 파일 설정
intake routing	enabled=true, mode=enforce	enabled=true, mode=enforce	현재 두 노드 live가 이 값	유지. env override가 null인지 재확인
owner authority allowlist	0	0	health detail	유지
cluster 역할	Gateway	Runner/RestWorker	G는 현재 non-standby, H는 Full Standby	H만 임시 변경. boot 필요
runtime profile	Full/Gateway	Runner	health runtime_profile, modules	H에서 gateway/admin 모듈이 뜨면 실패
gateway preference	G instance ID	같은 G instance ID	config 원문	이동하지 않음. boot 적용
turns restored	gateway reconcile	RestWorker reconcile	역할로부터 유도, reconcile_done log	Standby restored 경로는 이번 실측에서 사용하지 않음
endpoint Phase A	C endpoint execution_node=G	동일 canonical config, local=false	현재 mini health가 mac-book-release, local=false	기존 값 유지
endpoint Phase B	동일 endpoint가 H를 지칭, G local=false	H local=true	config hash와 /health/detail.herdr.endpoints	Released 뒤 파일 변경, G 먼저 restart, H 다음 restart
endpoint Phase C	G로 원복, G local=true	local=false	동일	Reclaimed 뒤 파일 변경, H 먼저 restart, G 다음 restart
endpoint key	기존 key 유지 권고	동일	현재 mac-book	이번 실측에서 key rename 금지. node/socket/home/session만 phase별 교체
Herdr turn switch	해당 provider true	해당 provider true	config live reload 값 [U]	turn switch는 live. 각 phase에서 health·실제 turn으로 확인
Herdr cancel switch	어떤 값이어도 실제 stop settlement는 false	동일	config 및 경고	이번 SHA에서는 실제 stop 성공 기준으로 쓰지 않음
terminate switch	terminate 직전에만 true	terminate 직전에만 true	config live 값 [U]	live 적용. 각 terminate 완료 직후 false로 되돌림
O writer selection	C가 선택됨	C가 선택됨	config 원문과 health. all_tui 사용 여부 확인	양 노드 같은 명시적 channel list 권고. boot 필요
O store	G의 C store readable	H의 C store readable	<runtime_root>/o_store/C/ledger.jsonl 사본	손상·unresolved·blocked이면 중단
PG pool	available	available	health, startup log, read-only query	변경 없음
home row 시작값	C row 없음	동일 PG	channel-home status	health의 channel_homes 키 부재로 row 부재를 판정하지 않음
health 접근	SSH 후 loopback detail 200	loopback detail 200	G의 Tailscale detail 403은 정상적인 원격 거절일 수 있음	모든 증거는 각 노드 loopback에서 수집
harness	Phase A/C에 local	Phase B에 local	herdr.py readiness	endpoint 이동 후 해당 노드에서만 실행
대조 채널	변경 없음	변경 없음	최소 한 개의 같은 provider 비위임 채널 선정	endpoint·writer list·home row가 달라지지 않아야 함
현재 degraded	backlog·released alarm 해소 또는 원인 특정	standby 관련 degraded는 Runner 전환 후 사라져야 함	18:30 health 스냅샷	blanket waiver 금지. 항목별 소명 없으면 중단
실측 바이너리 선택

ccd737074를 유지한다.

이유는 다음과 같다.

두 노드가 이미 같은 SHA다.
s3fence 수용 증거가 이 SHA에 직접 대응한다.
de739c72d까지의 네 커밋은 요청자가 제공한 범위상 channel-home 설계 변경이 아니다.
active restart 실측에서는 “최신”보다 “수용 증거와 같은 실행물”이 더 중요하다.

다만 #7027이 inflight/restart 관련 휴면 가드를 정리했으므로 다음 파일 목록을 증거로 남긴다.

일반 텍스트
git diff --name-status ccd737074..de739c72d --
  src/services/cluster/channel_home*
  src/services/discord/runtime_bootstrap*
  src/services/discord/inflight/**
  src/services/tui_o/**
  src/services/session_host/**
  src/services/turn_finalizer/**
  src/config/session_hosts.rs
  src/services/turn_host.rs
  src/db/o_channel_homes.rs

예상 범위를 벗어난 실행 경로 변경이 있으면 ccd 유지 또는 de739 양 노드 동시 배포 중 하나를 다시 결정해야 한다. 한 노드만 SHA를 바꾸는 선택은 없다.

3. 토폴로지 선택
선택지	장점	문제	판정
A. G 유지, H=Runner, 같은 C, endpoint 왕복	gateway 전역 영향 최소, R1 회피, O-U-3를 같은 채널로 닫음, 생산 코드 변경 없음	endpoint 두 번 변경·재시작, 실행 retire 필요	권고
B. H=Full Standby 유지 + 코드 수정	최종 운영 토폴로지와 유사	R1 수정과 closed-restore 계약이 선행되어야 함	후속
C. gateway를 mini로 이동, MacBook을 holder로 사용	현재 endpoint 위치를 덜 바꿀 수 있음	모든 채널 gateway 역할과 tmux 배치에 전역 영향	기각
D. 새 전용 시험 채널	기존 채널 위험 최소	“같은 bulk1 시험 채널”과 O-U-3 범위를 닫지 못함	보조 시험만 가능
확정안
Gateway: 현재 non-standby 노드 G.
Holder: Mac mini H, Runner/RestWorker.
시험 채널: C 그대로.
Provider: 우선 Claude 하나로 고정. Claude와 Codex를 한 런에서 섞지 않는다.
endpoint key: 기존 key 유지.
endpoint의 execution node·socket·home·session만 phase별 전환.
gateway preference: G 유지.
다른 channel endpoint와 writer selection은 변경하지 않는다.
all_tui 범위를 넓히는 설정 변경은 금지한다.
4. 관측 신호 정의

시나리오에서는 다음 신호를 공통으로 쓴다.

R — PG row

<ADK> channel-home status

확인 항목:

state
holder
target
epoch
renewed_at
updated_at
open_intake.current_epoch
open_intake.other_epoch
open_intake.unrouted

구현 좌표는 src/cli/channel_home.rs:112-151이다.

M — 프로세스 메모리 상태

각 노드 loopback /api/health/detail:

runtime_profile
modules
herdr.configured_channels
endpoint node, local
herdr.admission
restart_required
channel_homes[C].home
epoch
drain_blocker
degraded reasons

중요하게, channel_homes 부재는 그 프로세스가 gate를 등록하지 않았다는 뜻이지 PG row가 없다는 뜻이 아니다. row 존재 여부는 항상 R로 판정한다.

H — Herdr 실행
<ADK> herdr status
exact session key, execution nonce
Hosted state
execution node
endpoint key
pane ID
provider running/exited
input hold 유무
O — holder의 managed O writer 증거

각 노드의 로컬 파일:

일반 텍스트
<runtime_root>/o_store/<C>/ledger.jsonl

O writer는 POST 전에 Prepared에 home epoch를 기록하고, Discord가 생성한 message ID를 Posted에 기록한다. 따라서 holder provenance는 다음 결합으로 증명한다.

H ledger: Prepared(epoch=E2)와 같은 serial의 Posted(msg_id=M).
G ledger: delegated phase 동안 msg_id=M 또는 같은 run nonce의 새 Posted가 없음.
Discord history: message ID M이 정확히 하나.
row는 그 시각 Worker(holder=H,epoch=E2).
G writer는 H 부팅 전에 join된 상태.

이 파일 구조와 Prepared/Posted 기록은 deliver.rs와 store/mod.rs에 정의되어 있다.

D — Discord

REST history에서 다음을 저장한다.

사용자 입력 message ID.
bot 응답 message ID.
author ID.
timestamp와 edited timestamp.
reply reference.
run nonce.
content SHA-256.
전체 응답 수.

응답은 의도적으로 한 메시지에 들어갈 짧은 문자열로 제한한다. 여러 chunk가 필요한 출력은 exact-once 시험에 쓰지 않는다.

L — 로그

공통 키:

PID와 process start time.
SIGTERM received — graceful shutdown
committed restart joins every home
a delegated home's writer end is unconfirmed
row unreadable·source reset·turn unreadable.
home availability refusal.
O writer alarm.
provider/Herdr restore 결과.

SIGTERM과 fenced restart의 writer 종료 경로는 shutdown.rs와 spawns.rs에 있다.

P — pane·transcript 최소 증거
H에서 해당 pane이 존재.
run nonce 입력이 pane에 정확히 한 번 들어감.
같은 nonce가 transcript 턴에 정확히 한 번 등장.
terminal record 위치와 execution nonce.
민감한 전체 transcript 대신 해당 record의 digest와 전후 최소 구간만 저장.
5. 공통 시간 상한과 중단 기준
작업	상한
health fully recovered·role 판정	재시작 후 120초
idle row transition	holder health 정상 후 60초
Released → Worker adopt	target health 정상 후 30초
짧은 실측 turn	180초
장시간·재시작 경계 turn	300초
POST 후 중복 부재 관측	30초
!clear 명시 응답	90초
row transition 전체 중단선	120초
force 가능 창	200초 이후지만, 정상 실측에서는 사용 금지

다음 중 하나면 즉시 중단한다.

동일 run nonce의 bot message ID가 둘 이상 생성.
row가 Worker가 아닌데 일반 입력이 pane에 도달.
holder·epoch와 O ledger Prepared.epoch가 불일치.
transition 완료 시 open intake 세 값 중 하나라도 0이 아님.
writer end is unconfirmed.
endpoint가 두 노드에서 모두 local이거나 모두 remote.
target 채널 외 channel에 home row가 새로 생김.
control 채널의 host/session/endpoint가 바뀜.
terminate 결과가 retired가 아닌 indeterminate, provider_survived_close, confirmation_required.
row unreadable, O store damage, ambiguous/unresolved POST.
120초가 지나도록 transition이 끝나지 않음.
force를 써야만 진행할 수 있는 상태.
6. 실측 시나리오
S0 — 노드 신원·설정·배경 부하 고정

진입 조건

어떤 설정·DB 변경도 하지 않은 상태.
C에 active turn이 없음.
두 노드가 현재 ccd737074.

조작

G·H에서 hostname, instance ID, PID, binary SHA, process start time 수집.
각 노드 config의 관련 절을 비밀값 제거 후 저장.
R, M, H, O 스냅샷.
cluster node registry.
ccd..de739 critical-path diff.
C 외 실제 대조 채널 두 개를 선정:
가능하면 같은 provider의 비위임 Herdr 하나.
비위임 tmux 하나.

PASS — 120초

100.71.169.10의 hostname과 instance ID가 확정.
instance ID가 양 노드에서 다름.
C row 없음.
PG·Herdr·O store readable.
provider queue와 deferred hooks가 0이거나 target과 무관하다는 항목별 증거가 있음.
tui_o:released:* 두 alarm이 C의 home row가 아님을 R로 확인.
startup doctor failed 항목이 시험 경로와 무관함을 기록.
endpoint가 현재 G에만 local.

즉시 중단

node identity 불명.
C row 존재.
O ledger damage/unresolved.
global provider backlog의 귀속을 확인할 수 없음.
추가 configured endpoint가 있는데 R4 대응을 하지 않음.

롤백

읽기 전용 단계이므로 없음.
S1 — gateway-local 기준선과 다른 채널 기준선

진입 조건

S0 PASS.
C endpoint G-local.
row 없음.

조작

C에 짧은 nonce prompt를 Discord 일반 입력으로 보낸다.
각 control channel에도 한 번씩 실제 입력한다.
모든 턴이 완전히 끝날 때까지 기다린다.

관측·PASS — 각 180초 + 30초 quiet

C 입력은 G pane에서 한 번 실행.
G O ledger에 한 Prepared/Posted.
Discord bot message ID 한 개.
control 채널도 각자 한 응답.
host/session/endpoint·writer가 시작 전과 동일.
모든 open queue와 POST count가 0으로 복귀.

즉시 중단

기준선부터 duplicate·missing.
control 채널 상태 변화.
rotation_unsettled, O alarm, inflight 미정리.

롤백

아직 row가 없으므로 설정 변경 없이 원인 분석 후 종료.
S2 — gateway Herdr 실행 retire

진입 조건

S1 완료.
active turn·queue·owed·POST가 모두 0.
endpoint G-local.
row 없음.

조작

G에서 runtime.herdr_terminate_enabled=true.
H에서 변경하지 않는다.
H/Discord가 아니라 G의 exact session key·execution nonce를 사용해 operator terminate.
완료 직후 terminate switch를 false로 되돌린다.

관측·PASS — 90초

결과가 정확히 retired.
H 상태가 아니라 G의 row가 Retired.
pane에서 provider가 사라짐.
해당 nonce의 input hold 없음.
Discord에 추가 응답 없음.

즉시 중단

exact nonce 불일치.
indeterminate, provider survived, confirmation required.
active turn이 존재.
retire 뒤 hold가 남음.

롤백

endpoint·home row는 아직 바꾸지 않는다.
불명확한 terminate 결과에서는 재시도하지 않고 수동 판정으로 전환한다.
S3 — delegation 활성화와 gateway release

진입 조건

S2 PASS.
C 실행 Retired.
endpoint는 여전히 G-local.
C row 없음.
G·H config에 delegation switch=true가 저장됨.
H의 Runner 설정은 저장하되 아직 이 phase에서 부팅할 필요 없음.

조작

G에서:

일반 텍스트
<ADK> channel-home delegate C --provider claude --to <H_INSTANCE_ID>

즉시 R을 저장한 뒤 G에서 fenced restart:

일반 텍스트
<ADK> restart-dcserver

관측·PASS

delegate 직후:
Releasing
holder=G
target=H
epoch=E0
G 재기동 후 120초 안에 health 정상.
추가 60초 안에:
Released
holder=NULL
target=H
epoch=E1, E1≠E0
open intake 세 값 모두 0.
G log에 writer 종료 실패 없음.
drain blocker가 잠시 나타날 수 있으나 최종적으로 없음.

즉시 중단

source_reset blocker.
Retired 실행이 다시 Bound/Pending으로 바뀜.
endpoint가 G-local이 아님.
row가 120초 넘게 Releasing.
open intake가 남음.

롤백

Releasing이면 G와 endpoint를 그대로 유지하고 G를 재기동해 drain을 계속한다.
Released이면 우선 S4 target 부팅을 복구한다.
target 부팅이 불가능할 때만 사전 승인된 Released exact-CAS 복구를 사용한다.
S4 — endpoint 이동, 기존 gateway writer join, mini adopt

진입 조건

C row가 정확히 Released(E1,target=H).
open intake 세 값 모두 0.
G의 기존 실행은 Retired.
S3의 gateway writer가 더 이상 POST 중이 아님.

조작

양 노드의 canonical endpoint 설정을 H로 변경.
G를 먼저 fenced restart.
G health가 endpoint local=false이고 C writer가 hosted가 아님을 확인.
H를 Runner/RestWorker로 restart.
H loopback health와 R을 반복 관측.

관측·PASS

G restart에서 이전 writer가 join됨.
새 G는 row를 foreign으로 읽고 C에 gateway writer를 시작하지 않음.
H:
runtime profile Runner.
endpoint local=true.
delegated writer hosted.
Released(E1) → Worker(E2,holder=H,target=NULL).
channel_homes[C].home=intake_open.
renewed_at이 적어도 두 번 진전.
30초 내 adopt, 전체 restart 120초 내 완료.
G와 H 중 endpoint local=true는 정확히 하나.

즉시 중단

H가 Standby로 뜸.
G writer가 계속 hosted.
H가 adopt했지만 writer가 없음.
row는 Worker인데 health gate가 lost/draining.
양 노드 모두 local 또는 모두 remote.

롤백

row가 아직 Released면 H를 더 이상 시작하지 않고 endpoint를 G로 원복, H를 재시작해 writer 부재를 확인한 뒤 승인된 Released 복구.
Worker가 됐으면 config를 직접 원복하지 않고 정상 reclaim 절차로만 되돌린다.
S5 — holder 첫 일반 입력과 정확히 한 번 POST

진입 조건

S4 PASS.
row=Worker(E2,holder=H).
H intake_open.
G writer 없음.
queue·owed·POST 0.

조작

Discord C에 다음 성격의 짧은 입력을 보낸다.

일반 텍스트
TWOMAC-S5-<run-id>에 대해 정확히 "ACK TWOMAC-S5-<run-id>" 한 줄만 답해.

관측·PASS — 180초 + 30초 quiet

D: 입력 message ID 한 개.
P: H pane에 nonce 입력 한 번.
H: 실행 node=H, state=Bound.
O/H:
Prepared(epoch=E2).
동일 serial에 Posted(msg_id=M5).
O/G: M5가 없음.
D: bot message ID=M5가 정확히 하나.
내용이 정확한 한 줄.
30초 동안 같은 nonce의 추가 message ID 없음.
row holder·epoch는 변하지 않음.

즉시 중단

G pane 또는 G ledger가 입력/POST를 관측.
duplicate message ID.
Posted의 epoch가 E2가 아님.
Worker가 아닌데 pane 입력.
content transform·ambiguous·unresolved alarm.

롤백

입력·POST가 불명확하면 추가 입력 금지.
row는 Worker로 유지하고 기존 증거로 판정한 뒤 정상 reclaim 또는 명시적 중단을 선택한다.
S6 — 후속 대화

진입 조건

S5 PASS.
같은 H execution nonce가 계속 Bound.
queue·POST 0.

조작

첫 nonce를 문맥으로 요구하는 두 번째 짧은 입력을 보낸다.

일반 텍스트
직전 답의 run-id만 사용해 "FOLLOWUP <run-id>"로 답해.

관측·PASS — 180초 + 30초 quiet

같은 execution nonce와 pane.
H pane에서 두 번째 input 한 번.
H ledger에 다음 serial 한 개.
Discord response ID 한 개.
첫 응답과 두 번째 응답의 순서가 보존.
G ledger 변화 없음.

즉시 중단

새 execution이 이유 없이 생성.
문맥이 사라짐.
duplicate·G writer 관여.

롤백

추가 입력을 중지하고 현재 Worker를 정상 reclaim한다.
S7 — !clear

진입 조건

S6 완료.
active turn 없음.
H O writer hosted.
row Worker E2.

조작

Discord에서 !clear를 한 번 전송한다.

PASS — 90초

실제 clear commit 응답이 한 번 도착.
/clear가 H pane에 한 번만 입력.
이전 execution/session 경계를 참조하지 않는 새 짧은 입력이 정상 처리.
새 입력도 H ledger·Discord에서 정확히 한 번.
hold와 inflight가 남지 않음.

FAIL 조건

rotation_unsettled(n).
rotation_unread.
input_held.
turn_in_progress.
무응답.
/clear 중복.

rotation_unsettled는 알려진 gap으로 PASS 처리하지 않는다. clear 계획은 unsettled가 0이 아닐 때 mutation 전에 명시적으로 거절한다.

중단·롤백

명시적 refusal이고 상태가 전혀 바뀌지 않았음이 확인되면 core 위임 시나리오는 계속할 수 있지만, S7은 FAIL로 남긴다.
indeterminate clear 또는 hold 생성이면 전체 destructive 시나리오를 중단하고 정상 reclaim을 시도한다.
S8 — !stop

진입 조건

Worker E2.
H에서 30초 이상 수행할 제어 가능한 turn.
terminate switch는 off.

조작

장시간 turn을 시작.
pane과 active turn을 확인한 뒤 !stop 한 번.

고정 SHA에서의 예상 결과

생산 코드의 herdr_stop_settlement_available()은 false다. herdr_cancel_enabled=true여도 Escape delivery는 활성화되지 않는다. 따라서 실제 provider interrupt 성공을 이번 SHA의 PASS로 설계하면 안 된다.

판정

stop 기능 PASS는 불가능하다.
다음이면 fail-closed 계약 PASS로 별도 기록한다.
명시적 refusal 또는 host-refused 결과.
Escape가 pane에 들어가지 않음.
mailbox token이 잘못 cancel되지 않음.
원래 turn이 정상 terminal에 도달.
terminal 답은 정확히 한 번.
결과 보고에는 STOP_CAPABILITY: FAIL_UNAVAILABLE를 남긴다. known_gap PASS로 바꾸지 않는다.

고치지 않으면 운영에서 생기는 일: 사용자가 !stop을 눌러도 Herdr provider가 중단되지 않는다. 다만 이번 노드 목표 1~5에는 성공적인 stop이 수용 조건으로 명시되어 있지 않다.

개선 방향

provider-terminal settlement를 먼저 완성.
그 다음 herdr_cancel_enabled를 실제 delivery와 연결.
시험: Escape exactly once, terminal settlement, late stop, restart 중 stop, 다른 turn token 불변.
별도 선행 이슈로 분리하며 이번 실측에 섞지 않는다.
S9 — gateway fenced restart

진입 조건

Worker E2 holder=H.
idle 상태.
S5·S6에서 holder POST 증명 완료.

조작

G에서 <ADK> restart-dcserver.

관측·PASS — 120초

G PID·start time 변경.
committed restart joins every home 로그.
row는 Worker, holder=H, epoch=E2로 유지.
H renewed_at 계속 진전.
H writer와 endpoint local 유지.
G 복귀 후 C에 짧은 후속 입력 한 번, H에서 정확히 한 번 처리.
G O ledger에는 해당 응답 없음.

즉시 중단

gateway 재시작이 row를 gateway로 되돌림.
H intake가 닫힘.
duplicate.
writer 종료 불확정.

롤백

G만 원래 binary/config로 재기동.
row는 H에 유지하고 추가 destructive 조작을 하지 않는다.
S10 — holder fenced restart

진입 조건

Worker E2.
idle·queue·POST 0.
H endpoint local.

조작

H에서 <ADK> restart-dcserver.

관측·PASS — 120초

committed restart joins every home.
기존 delegated writer가 join.
새 H process가 같은 row를 mount.
holder=H 유지.
epoch는 그대로이거나, 코드가 새 CAS를 하지 않는 정상 renew만 발생.
intake가 다시 open.
restart 후 짧은 입력이 H에서 정확히 한 번 처리.
새 O serial에 Worker epoch가 기록.

즉시 중단

row가 Released/Orphaned로 이동.
writer join 불확정.
새 process가 Standby.
health 정상인데 writer가 없음.

롤백

H를 같은 Runner config와 SHA로 다시 재기동.
120초 내 복구하지 않으면 입력 금지 상태로 유지하고 사용자 승인 전 force 금지.
S11 — gateway OS SIGTERM과 [O-U-3] 단절 중 완료

이 시나리오는 gateway SIGTERM 실기기 증명과 [O-U-3]를 함께 닫는다.

진입 조건

Worker E2.
H에서 45~60초 걸리는 제어 가능한 turn.
gateway launch supervisor의 stop/start 명령을 S0에서 확정.
다른 채널 사용자가 없는 승인된 유지보수 창.

조작

C에서 장시간 turn 시작.
H pane에서 실행 시작을 확인.
G 서비스를 supervisor 수준에서 내린다. 이 동작은 SIGTERM을 전달하고 자동 즉시 재시작을 잠시 막아야 한다.
G process와 Discord gateway websocket이 사라진 것을 확인.
G가 없는 동안 H turn을 완료시킨다.
H managed writer가 Discord REST로 답을 POST하게 둔다.
답 확인 후 G 서비스를 다시 올린다.
G 복구 뒤 C에 후속 입력을 보낸다.

관측·PASS — 전체 300초, G 정지 최대 120초

G log: 실제 SIGTERM 수신과 graceful shutdown.
G down interval이 명확히 기록.
그 interval 안에 H transcript terminal과 H ledger Posted가 발생.
Discord message timestamp가 G down interval 안.
message ID 정확히 하나.
G 복구 뒤 row는 여전히 Worker E2 holder=H.
후속 입력도 H에서 정확히 한 번.
반환 시 duplicate 없음.
다른 channel의 기존 tmux/Herdr process는 살아 있고 G 복구 뒤 같은 session으로 계속됨.

즉시 중단

G가 실제로 내려가지 않음.
H POST가 G 복구 후에만 발생.
H lease·gate가 닫힘.
다른 채널 세션 종료.
duplicate.

롤백

G 서비스를 즉시 같은 SHA/config로 올린다.
H는 건드리지 않는다.
row Worker를 유지한 채 추가 시나리오를 중단하거나 정상 reclaim한다.

이 시나리오로 [O-U-3] 중 다음 범위를 닫는다.

실제 Mac 두 대.
gateway와 holder 분리.
holder에서 실제 실행.
gateway 연결 단절 중 완료와 Discord POST.
gateway 복귀 후 후속 대화.
이후 정상 reclaim.

holder 자체의 인터넷·Discord REST 단절은 다른 실패 유형이므로 포함하지 않는다.

S12 — holder OS SIGTERM 중 실행

진입 조건

G 정상 복구.
Worker E2.
H에서 45초 이상 장시간 turn.
launch supervisor가 H process를 같은 SHA/config로 자동 복귀시킴.

조작

장시간 turn 시작.
pane input과 active turn을 확인.
H dcserver PID에 실제 SIGTERM.
Herdr pane/process 자체는 종료하지 않는다.

관측·PASS — 300초

H log: SIGTERM received — graceful shutdown.
queue/inflight state 저장.
delegated writer join.
writer end is unconfirmed 없음.
새 H PID가 같은 Runner 역할로 부팅.
row holder=H 유지.
lease가 잠시 lapse했더라도 새 renew 뒤 intake_open 복귀.
기존 turn을 재부착·복구하여 terminal 한 번.
Discord reply 한 개.
inflight row가 최종적으로 정리됨.
다음 짧은 입력도 한 번 처리.

즉시 중단

SIGKILL 또는 supervisor가 다른 SHA를 시작.
pane가 사라짐.
duplicate terminal.
row가 Orphaned.
새 process가 writer 없이 intake만 open.

롤백

동일 H config/SHA로 한 번 더 재기동.
불명확한 terminal이면 새 입력을 금지하고 Discord history·O ledger settlement를 먼저 판정.
force 금지.
S13 — 다른 채널 무변화 대조

이 시나리오는 S1, S4, S9~S12, S16의 체크포인트에서 반복한다.

진입 조건

사용자와 코디네이터가 확정한 실제 control channel.
C와 다른 channel ID.
home row 없음.

조작

각 checkpoint에서:

host/session/endpoint·pane ID snapshot.
짧은 nonce 입력.
한 응답 확인.
C 외 row 전체 목록 비교.

PASS

control channel에 home row가 생기지 않음.
endpoint execution node·session이 변하지 않음.
기존 pane/session ID 유지. 정상적인 자체 rotation이 있었다면 그 인과를 별도 증명.
한 입력·한 응답.
C 전환 때문에 새 degraded reason이 생기지 않음.
gateway outage 구간에는 신규 입력을 요구하지 않지만 기존 process가 살아 있고 복구 후 같은 session이 이어짐.

즉시 중단

control channel의 writer가 중지.
home availability provider-wide refusal.
endpoint나 host 이동.
duplicate/missing.

롤백

C의 다음 시나리오를 중지하고 현재 row 상태에 맞는 정상 복귀 절차를 시작한다.
S14 — mini 실행 retire와 reclaim drain

진입 조건

S5·S6·restart 시나리오 완료.
active turn·queue·owed·POST 0.
endpoint H-local.
row Worker E2.

조작

H에서 terminate switch를 잠시 켠다.
exact session key·nonce로 H 실행 terminate.
결과 retired 확인 후 switch off.
G의 CLI config가 delegation enabled인지 확인.
G에서:
일반 텍스트
<ADK> channel-home reclaim C

관측·PASS

terminate: Retired, no hold.
reclaim 직후:
Reclaiming
holder=H
target=G
epoch=E3, E3≠E2
H health에 draining과 실제 blocker가 표시.
blocker가 모두 해소된 뒤 60초 안에:
Reclaimed
holder=NULL
target=G
epoch=E4, E4≠E3
open intake 세 값 0.
H ledger에 새 POST 없음.

즉시 중단

terminate indeterminate.
turn/owed/post blocker가 실제로 남아 있는데 CAS가 진행.
row가 120초 넘게 Reclaiming.
force가 필요해짐.

롤백

Reclaiming에서는 H를 살려 drain을 끝내는 것만 허용한다.
Reclaimed이면 row를 직접 지우지 말고 S15의 mini-first writer join과 gateway return을 수행한다.
S15 — delegated writer join, row 제거, gateway writer 복귀

진입 조건

row=Reclaimed(E4,target=G).
H 실행 Retired.
open intake 세 값 0.
G는 아직 endpoint remote 상태라 return watch가 없을 수 있음.

조작

양 노드 endpoint config를 G로 원복.
H를 먼저 fenced restart.
H health:
endpoint local=false.
C delegated writer 없음.
writer 종료 불확정 로그 없음.
G를 그 다음 fenced restart.
R·M·O를 반복 관측.

관측·PASS — 각 restart 120초, row 제거 60초

H restart가 기존 delegated writer를 join.
새 H는 C writer를 시작하지 않음.
G boot는 Reclaimed target row를 mount.
gateway의 home-bound writer가 먼저 stop·join.
그 뒤 기존 remove_reclaimed CAS가 row를 제거.
row가 없어진 뒤 gateway writer가 시작.
R에서 C row 없음.
G endpoint local=true, H local=false.
G writer hosted/readiness 정상.
H ledger는 더 이상 변화하지 않음.

이 순서는 ChannelWriters가 gateway watch에서 writer를 join한 뒤 row를 제거하고, row가 사라진 뒤 gateway writer를 보장하는 기존 계약을 그대로 사용한다. delegated runtime writer는 watch가 아니라 해당 runtime shutdown이 종료하므로 mini-first 재시작이 필요하다. finish_return은 row 제거 뒤 동일 gate unregister를 확인한다.

즉시 중단

G가 row 제거 전에 정상 gateway writer로 POST 가능해짐.
H writer가 여전히 hosted.
row가 사라졌는데 G writer 없음.
writer 종료 불확정.
endpoint local 충돌.

롤백

Reclaimed가 남아 있으면 H는 writer 없는 상태로 유지하고 G를 같은 config로 재기동.
row가 이미 제거됐다면 endpoint G-local을 유지하고 G writer 복구만 수행한다.
새 delegate를 만들지 않는다.
S16 — gateway 복귀 후 정확히 한 번 배달과 정리

진입 조건

S15 PASS.
row 없음.
endpoint G-local.
H writer 없음.
양 노드 health 안정.

조작

C에 짧은 nonce prompt.
control channel에도 마지막 nonce prompt.
결과 수집 후 시험용 설정 원복.

관측·PASS — 180초 + 30초 quiet

C는 G pane에서 실행.
G ledger에 새 Prepared/Posted.
H ledger에는 변화 없음.
Discord 응답 message ID 한 개.
control channel도 기준선과 같은 host/session에서 한 번 응답.
C row는 계속 없음.
추가 home row 없음.

정리 순서

terminate switch off 확인.
delegation switch를 끄려면 반드시 C row가 없음을 R로 재확인.
H runtime profile을 원래 Full/Standby로 되돌릴지 사용자 결정.
delegation switch off와 H role 원복은 양 노드 restart가 필요.
endpoint는 G-local 상태 유지.
config hash와 최종 health 저장.
마지막 30초 quiet에서 추가 Discord post가 없어야 함.
7. 운영 설정 변경과 승인 지점
변경	노드·시점	재시작	승인
runtime.channel_home_delegation_enabled=true	G·H, S3 전	필요	사용자 승인: provider-wide admission 영향
H runtime_profile=runner	H, S4	필요	사용자 승인: 역할 변경
H cluster.role=runner	H, S4	필요	사용자 승인
gateway_preferred_instance_id=G 확인	G·H	변경 시 필요	변경하지 않는 것이 원칙
endpoint G→H	G·H, Released 뒤	필요	사용자 승인: 같은 채널 실행 위치 변경
endpoint H→G	G·H, Reclaimed 뒤	필요	사용자 승인
tui_o.writer.channels에 C	G·H	필요	기존 값이면 변경 없음; 추가 시 사용자 승인
herdr_turn_enabled	해당 provider	live	기존 off라면 사용자 승인
herdr_terminate_enabled=true	실행 retire 직전	live	사용자 승인: destructive. 사용 직후 off
gateway supervisor stop	S11	프로세스 정지	사용자 승인: 다른 채널 일시 intake 중단
exact Released emergency repair	실패 시에만	이후 G restart	별도 사용자 승인
evidence nonce·파일명·관측 주기	전 구간	없음	코디네이터 재량
loopback health·read-only SQL·ledger 복사	전 구간	없음	코디네이터 재량
Standby adopt 코드 수정	후속	배포 필요	맥 승인 대상
Released abort CLI 추가	후속	배포 필요	맥 승인 대상
runtime_bootstrap*, channel_home*, tui_o/**, session_host/**, turn_finalizer/** 변경	모든 코드안	배포 필요	맥 승인 대상
금지되는 변경
gateway를 mini로 이동.
새 home·custody·retry 테이블.
새로운 durable ledger.
all_tui 범위를 넓혀 다른 채널을 우발적으로 writer 대상에 넣기.
row가 남은 상태에서 delegation switch off.
정상 진행 수단으로 force.
endpoint를 mini로 먼저 옮긴 뒤 delegate.
Reclaimed row 제거 전에 mini delegated writer를 살려 둔 채 gateway를 부팅.
8. 증거 형식
일반 텍스트
evidence/i5340_twomac/<run-id>/
  00-manifest.json
  01-config/
    gateway.redacted.yaml
    holder.redacted.yaml
    config-sha256.json
    ccd-to-de739-critical-path.diff
    node-identities.json
    approvals.json
  S00-preflight/
  S01-baseline/
  ...
  S16-final/
  controls/
  SHA256SUMS
  final-verdict.json
00-manifest.json

필수 필드:

JSON
{
  "run_id": "...",
  "started_at_kst": "...",
  "started_at_utc": "...",
  "design_sha": "de739c72d7856a34dbbbde6a0b54f867b698005b",
  "binary": {
    "gateway": "ccd737074",
    "holder": "ccd737074"
  },
  "channel_id": "1509350490461180105",
  "provider": "claude",
  "gateway": {
    "hostname": "...",
    "instance_id": "...",
    "pid": 0
  },
  "holder": {
    "hostname": "Mac-mini.local",
    "instance_id": "...",
    "pid": 0
  }
}
시나리오 디렉터리

각 Sxx에 다음을 둔다.

일반 텍스트
start.json
end.json
channel-home.before.json
channel-home.after.json
gateway.health.before.json
gateway.health.after.json
holder.health.before.json
holder.health.after.json
gateway.herdr-status.json
holder.herdr-status.json
gateway.o-ledger.before.jsonl
gateway.o-ledger.after.jsonl
holder.o-ledger.before.jsonl
holder.o-ledger.after.jsonl
discord.jsonl
gateway.log.jsonl
holder.log.jsonl
pane-proof.json
verdict.json

pane-proof.json에는 전체 transcript 대신 다음만 저장한다.

execution nonce.
pane ID.
input nonce.
input record offset·digest.
terminal record offset·digest.
transcript file identity.
최소 전후 record type.
offline 판정 스크립트

scripts/e2e/tui_relay/twomac_home_judge.py 정도의 시험 전용 파일 하나면 충분하다. 생산 코드 변경은 없다. 예상 150~250줄이다.

필수 검사:

row 상태·holder·target·epoch 전이 순서.
transition 완료 시 open intake 세 값 0.
Worker 시각과 Prepared.epoch 일치.
holder ledger의 Posted(msg_id)와 Discord message ID 일치.
비holder ledger에는 같은 message ID가 없음.
nonce당 bot-created message ID 정확히 하나.
restart 시 PID 또는 process start time 변경.
writer end is unconfirmed 부재.
control channel row 부재와 host/session 불변.
known_gap, not_applicable, 빈 증거를 PASS로 변환하지 않음.

스크립트 exit 0 자체는 PASS가 아니다. verdict.json에 기대값과 실제 관측값이 모두 있어야 하며 최종 판정자는 해당 artifact를 확인한다.

9. s3fence 증거 대응
실측	기존 증거와의 대응	실제로 닫는 한계
S3 release	H6-1, H6-2a의 실제 gateway·PG판	실제 gateway process가 Releasing을 drain하고 Released CAS
S4 adopt	H6-2a Released target boot watch	부모가 restored 신호를 직접 주던 시험 한계를 RestWorker 실제 boot로 대체
S5 first turn	H7-P*, H7-G*, H8-G, H9-D/W 보완	실제 Discord ingress → 다른 Mac pane → holder O POST
S6 follow-up	H7-U·H7-O 보완	같은 실제 pane·execution에서 후속 문맥
S9 gateway fenced restart	H6-6a·H6-6b 보완	실제 두 프로세스 세대와 PG holder 유지
S10 holder fenced restart	H6-6b	실제 delegated writer join·다음 boot 재개
S11 gateway SIGTERM/O-U-3	H6-6b와 런북 O-U-3	실제 OS signal, gateway 부재 중 holder 완료·POST·복귀
S12 holder SIGTERM	H6-6b	실제 OS SIGTERM, pane 생존, inflight 복구, writer 재개
S14 reclaim drain	H6-2a·H6-3a	기존 evidence scope에서 도달하지 못한 Reclaimed CAS 실측
S15 handback	H6-3b·H6-6b	mini writer 실제 join 후 gateway return watch·row 제거·writer 복귀
S13 controls	H7 계열 isolation 보완	비위임 채널의 실제 무변화
S16 gateway final turn	H6-3b 후속	row 제거 뒤 gateway가 정확히 한 번 다시 배달
§3.2 이월 항목 대응
이월	이번 실측
a1 cancel token identity	S12가 실제 복구 경계를 보완하지만 경쟁·identity 뮤턴트를 대체하지 않음
a2 전부 걸러진 queue·marker 처분	대체하지 않음
b1 PG 대기 판정	실제 시간축을 보완하지만 deterministic 시험을 대체하지 않음
b2 adopt 시험 sleep 대기	S4가 실제 Mac·실제 process boot 부분을 대체
c1 Standby finish_reclaim	대체하지 않음. 이번 holder는 Runner
c2 inflight row 복구 단언	S12에서 exact inflight row before/after를 저장하면 실기기 보완
c3 다른 채널 무변화	S13이 실기기 보완. restored_ignores_queue_or_marker 단위 속성은 대체하지 않음
c4 marker-only	대체하지 않음
c5 손상 3종·read-error-as-empty	대체하지 않음
c6 Held receipt	별도 receipt 상태를 의도적으로 만들지 않으므로 대체하지 않음
c7 복구 중 crash	SIGTERM은 graceful이므로 대체하지 않음
c8 relay·boot 복구 경합	S12가 실제 순서를 보완하지만 인위적 race 시험은 대체하지 않음
d1 반환 CAS Stale	정상 CAS만 실행하므로 대체하지 않음
d2 두 provider runtime 순차 boot	provider 하나만 사용하므로 대체하지 않음
d3 후보 없는 반환	대체하지 않음
e1 실제 PG lease 2세대 역할 결정	S9·S10·S12·S15가 실기기 부분을 닫음
e2 실제 OS SIGTERM	S11·S12로 대체
f HTTP health 경계	전 시나리오의 실제 health timeline으로 보완하되 단위 경계 시험은 유지

특히 다음은 과대 주장하지 않는다.

S4는 Standby 경로를 증명하지 않는다.
S15는 CAS-stale race를 증명하지 않는다.
SIGTERM은 crash-recovery 증명이 아니다.
exit 0이나 health 200만으로 writer join·exact-once를 증명하지 않는다.
10. 위험과 [U]
ID	미확인·위험	확인 방법	담당
U1	100.71.169.10이 실제 MacBook인지	SSH hostname, instance ID, cluster registry	코디네이터
U2	양 노드 실제 config 값	redacted config 원문과 hash	코디네이터
U3	bulk1 최종 판정	기존 evidence artifact·Discord IDs·ledger	코디네이터
U4	C에 자동 producer가 있는지	bot·scheduler·webhook 목록, 유지보수 창	사용자
U5	실제 control channel ID	같은 provider의 비위임 채널 선정	사용자
U6	H의 cwd·프로젝트·인증·Claude 실행 환경	H local Herdr smoke, read-only environment inventory	맥
U7	H O store의 기존 rotation·alarm	ledger 복사, writer health, !clear 전 상태	맥·코디네이터
U8	기존 G 실행 terminate 후 cold launch 가능성	exact Retired/no-hold, S4 첫 launch	맥
U9	실제 launch supervisor 명령	plist/label, stop/start dry inspection	맥·코디네이터
U10	G detail 403	SSH loopback GET	코디네이터
U11	Released 긴급 PG 복구 승인	exact 계약 서면 승인	사용자
U12	tui_o:released:<ch> alarm과 home row 관계	R과 local O alarm/store 비교	코디네이터
U13	endpoint가 정말 하나뿐인지	config와 양 노드 health endpoint 전체 목록	코디네이터
U14	현재 provider backlog 12/1의 귀속	queue read-only inspection, 0까지 대기	코디네이터
U15	실제 stop 지원	코드상 unavailable. 별도 선행 이슈	맥
U16	rotation_unsettled(59) 잔존 여부	H writer 안정 후 !clear, ledger/rotation inspection	맥·코디네이터
U17	ccd와 de739의 active restart 차이	critical-path diff artifact	맥
U18	O ledger가 양 노드에서 분리된 runtime root인지	realpath, device/inode, runtime root snapshot	코디네이터
U19	startup doctor failed 항목	상세 doctor report	맥
U20	G outage 동안 다른 채널 영향 허용	승인된 유지보수 창	사용자
11. 개선 후속과 새 방향 제안
후속 1 — Standby target restore

앞서 설명한 closed-Worker restore 계약을 구현한다. 이번 실측 뒤 별도 조각으로 권고한다.

후속 2 — Released abort CLI

exact epoch와 quiescence를 요구하는 제한된 abort-release를 추가한다. 직접 PG 수정 의존을 제거하므로 운영 안전성 이득이 크다.

후속 3 — 성공 POST 진단 노출

현재 성공한 POST는 로컬 ledger.jsonl에서 직접 증명할 수 있지만 operator CLI는 성공 serial 전체를 보기 어렵다. agentdesk o inspect --channel C --from-serial N 같은 읽기 전용 명령으로 다음만 출력하는 방안이 좋다.

serial.
Prepared epoch.
outcome 종류.
Posted message ID.
content hash.
source key hash.

새 원장이나 테이블은 필요 없다. 기존 ledger의 projection만 출력한다.

새 방향 제안 — prepare/commit형 위임

장기적으로는 delegate 직전에 target이 다음 준비 상태를 기존 health/capability로 광고하도록 할 수 있다.

endpoint local.
RestWorker 또는 fixed Standby restore 가능.
PG와 instance ID 정상.
delegated writer를 이번 boot에서 시작할 수 있음.
O store readable.
channel restore ready.

gateway는 이 준비 증거가 있을 때만 delegate CAS를 실행한다.

현재 대비 이득

Released 고립 가능성 감소.
row-before-boot 순서를 operator가 수동으로 기억할 필요 감소.
endpoint 전환 오류를 조기에 거절.
새 durable registry 없이 기존 node capability와 health로 가능.

비용·위험

동적 writer 또는 boot ticket 계약이 필요.
capability의 신선도와 boot identity를 연결해야 함.
현재 실측보다 코드 범위가 훨씬 커진다.

권고: 지금 갈아타지 않는다. 현재 Runner 기반 두 Mac 실측을 먼저 착지하고, 실제 artifact에서 가장 불편했던 단계만 후속 설계에 반영한다.

최종 판정

현재 Full Standby mini를 그대로 target으로 쓰는 안, endpoint를 먼저 mini로 옮기는 안, row 생성 전에 target을 부팅하고 writer가 자동으로 생긴다고 가정하는 안은 모두 실제 중단 경로다. 위 권고안은 세 경로를 각각 Runner 전환, release-before-endpoint-swing, row-before-boot 순서로 제거한다.

실행은 다음 게이트가 모두 충족된 뒤에만 시작한다.

node identity와 config 확정.
current backlog·doctor failure 정리.
같은 채널 terminate 승인.
endpoint 왕복·Runner 전환 승인.
Released 긴급 복구 계약 승인.
실제 control channel 선정.
G·H 모두 ccd737074.
C row 없음.

성공 보고는 CORE PASS와 !clear, !stop 결과를 분리해야 한다. 특히 실제 !stop 기능은 고정 SHA에서 사용할 수 없으므로 PASS로 포장하지 않는다.

VERDICT: DESIGN_READY
