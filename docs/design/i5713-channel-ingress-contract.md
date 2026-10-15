# 채널 소유 ingress 판정 계약 (관측 shadow)

기준: `itismyfield/AgentDesk@070157faf`. §1–§8의 좌표는 이 SHA 기준이다. §9의 관측 연결은 `b0062777a` 위에 얹었고 좌표 대신 심볼로 적는다.

이 문서는 채널 home 권위를 읽어 "이 노드가 채널 ingress의 실행 후보인가"를 판정하는 계약을 고정한다.
구현은 `src/services/cluster/channel_ingress.rs`의 순수 판정 함수 `observe`다.
런타임 호출자는 §9의 ingress shadow 하나다. 기본값은 꺼짐이고, 켜도 판정을 구조화 로그로만 남기며 실행·forwarding·ack를 바꾸지 않는다.
채널 ingress 전환, 자동 takeover, forwarding 제거는 이 문서의 착지 범위가 아니다.

## 1. 착지 형태

| 항목 | 내용 |
|---|---|
| 운영 코드 | `src/services/cluster/mod.rs`의 `pub(crate) mod channel_ingress;`. 호출자는 §9 shadow worker뿐이다 |
| 판정 모듈 | `src/services/cluster/channel_ingress.rs`. DB·gate·registry·queue·HTTP 핸들을 받지 않는다 |
| 시험 | `src/services/cluster/channel_ingress/tests.rs` |
| 관측 연결 | §9. 시간(PG `statement_timestamp`)과 로컬 gate 표본(`channel_home::ingress_snapshot`) 공급자를 함께 붙였다 |

운영 호출자가 만들지 않는 `SourceEpoch::Routed`와 그 두 보류 사유(`StaleSourceEpoch`, `RouteWithoutHomeEpoch`)는 `#[cfg(test)]`로 둔다.
새 `#[allow]`와 baseline 인상은 금지라서, 전달 입력을 관측하는 호출자가 생길 때 그 조각이 이 표시를 푼다.

## 2. 판정 계약

### 2.1 불변식

| 번호 | 불변식 | 구현 |
|---|---|---|
| I1 | home 조회 결과로 실행 권한을 만들지 않는다 | 반환형은 관측값뿐이다. `HeldHome`·`CommandPermit`·admission으로 바꾸는 API가 없다 |
| I2 | 소유 identity는 (provider, 실제 channel_id)다 | `IngressKey`는 `HomeProvider`와 0이 아닌 채널 ID만 받는다. policy 채널·agent·user·선호 노드 입력이 없다 |
| I3 | 물리 PK가 channel 하나라는 한계를 감추지 않는다 | 다른 provider의 row는 `Hold(ProviderMismatch)` |
| I4 | row 부재·조회 실패·전이 상태는 local fallback을 만들지 않는다 | `Missing`→`NoHomeAuthority`, `Unreadable`→`AuthorityUnreadable`, Worker 외 상태→`HomeNotTakingIngress` |
| I5 | `home_epoch`와 `gate_epoch`를 비교하지 않는다 | 로컬 표본에서는 `home_epoch`만 row epoch와 비교한다 |
| I6 | 자기 노드 후보에는 row와 로컬 renewal 표본의 일치가 필요하다 | holder·key·home epoch·intake open·로컬 만료시각을 모두 검사한다 |
| I7 | 시간 불명·만료는 후보를 만들지 않는다 | 노드 wall clock과 PG `renewed_at`을 직접 비교하지 않는다(2.4) |
| I8 | 관측은 기존 상태를 바꾸지 않는다 | 입력은 값과 빌린 row뿐이다. `HomeGate::ownership()`(만료 처리)·registry 조회(prune)를 호출하지 않는다 |

### 2.2 입력

| 타입 | 의미 |
|---|---|
| `IngressKey::parse(provider, channel)` | `HomeProvider::parse` 정규화. 모르는 provider와 채널 0은 `None` |
| `SourceEpoch::UnstampedSource` | 새로 관측한 원천 이벤트에만 쓴다 |
| `SourceEpoch::Routed(Option<i64>)` | 전달된 입력. `intake_outbox.home_epoch`를 그대로 담는다. `None`은 gateway 규칙으로 이미 전달된 입력이다. 운영 호출자가 없어 test 빌드 전용이다 |
| `HomeRead::{Missing, Unreadable, Row(&ChannelHome, ReadClock)}` | 조회 실패를 부재로 접지 않는다 |
| `ReadClock { pg_now, started }` | PG의 읽기 시각과 조회 시작 로컬 monotonic 시각. 하나라도 없으면 시간 불명 |
| `LocalLeaseSample` | key, holder, 기존 `HomeOwnership`, 표본 시각, 마지막 renewal이 정한 만료시각. gate 핸들·callback이 없다 |

`Routed(None)` 계약: home row가 있는 채널에서 epoch 없는 전달 입력은 `Hold(RouteWithoutHomeEpoch)`다.
현재 claim SQL(`src/db/intake_outbox.rs:674–683`)은 이런 행을 row가 없는 채널에서만 고르고, `channel_home::intake_hold`(`src/services/cluster/channel_home.rs:819–837`)도 등록된 home에서 실행하지 않는다.
어댑터는 `None`을 `UnstampedSource`로 바꾸거나 현재 row epoch·0으로 채우지 않는다. row가 없으면 기존대로 `Hold(NoHomeAuthority)`이며, 기존 gateway 실행 경로는 그대로 남는다.

### 2.3 판정 순서

순서를 바꾸면 오류가 foreign·candidate 판정 뒤에 가려진다. 아래 순서로 고정한다.

1. 로컬 identity: 빈 `local_instance_id` → `NoLocalIdentity`
2. 조회: `Unreadable` → `AuthorityUnreadable`, `Missing` → `NoHomeAuthority`
3. row identity: 채널 불일치 → `ChannelMismatch`, provider 불일치 → `ProviderMismatch`
4. row 구조(테이블 제약과 같다): 비정규 provider, epoch < 1, holding 상태와 holder 유무 불일치, 공백 holder, holder가 있는데 `renewed_at` 없음, Worker의 target → `InvalidHome`
5. 원래 epoch: `Routed(Some(e))`이고 e ≠ row epoch → `StaleSourceEpoch`, `Routed(None)` → `RouteWithoutHomeEpoch`
6. 상태: Worker가 아니면 `HomeNotTakingIngress(state)`. `Released.target == local`도 여기서 보류한다. target은 예정 수신자이지 holder가 아니다
7. 시간: `TimeUnknown` 또는 `RowExpired`. foreign row에도 적용한다
8. holder가 다른 노드 → `IgnoreForeign { holder, home_epoch }`. "holder가 실행 가능하다"는 보증이 아니다
9. 로컬 표본: 없음 → `NoLocalLease`, key·holder 불일치 → `LocalSampleMismatch`, `Lost` → `LocalLost`, home epoch 불일치 → `LocalEpochMismatch`, intake 닫힘 → `LocalIntakeClosed`, 만료시각 없음·표본이 판정보다 늦음·표본 시각 ≥ 만료 → `TimeUnknown`, `now >= expires_at` → `LocalExpired`
10. 모두 통과 → `LocalCandidate { home_epoch }`

### 2.4 시간

새 TTL은 없다. `channel_home::HOLD_FOR`(20초)를 쓴다. 갱신 주기 5초, force 창 200초와 서로 대체하지 않는다.

```
age_upper = (pg_now - row.renewed_at) + (now - started)
row_fresh := 0 <= age_upper < HOLD_FOR
```

조회 전체 경과를 더하므로 응답 지연으로 유효기간이 늘지 않는다. 음수, 누락, overflow는 `TimeUnknown`, `age_upper == HOLD_FOR`는 `RowExpired`다.
자기 노드에는 `now < expires_at`이 추가로 필요하다. 이 계산은 row의 관측상 신선도 필터일 뿐, 원격 노드의 실행 permit 만료시각이 아니다.

### 2.5 두 노드 중 최대 하나

같은 현재 row를 서로 다른 instance_id의 두 노드가 판정하면 `LocalCandidate`는 많아야 하나다. row 하나는 holder 하나만 지칭하기 때문이다.
이 명제는 서로 다른 시점의 관측을 실행 허가로 써도 된다는 뜻이 아니다. handoff 뒤 예전 `LocalCandidate`는 역사적 관측이며, 실행은 그 시점의 gate admission을 다시 통과해야 한다.
같은 instance_id를 쓰는 두 프로세스는 row holder로 구분하지 못한다(U5). durable source dedup도 이 분류기의 성과가 아니다. claim·accept SQL의 home 조건 재검사를 이 관측으로 대체하지 않는다.

## 3. 후속 전환이 따를 결정

| 결정 | 내용 |
|---|---|
| D1 Failover | 읽기 결과로 takeover하지 않는다. 만료 조건과 이전 epoch를 함께 검사하는 단일 조건부 DB 전이로 새 holder·새 epoch를 만든다. `force_orphan`→`adopt`를 자동 takeover로 조합하지 않는다(현재 `adopt`는 Released의 지정 target만 받는다). 새 노드는 새 세션으로 시작한다 |
| D2 Split-brain·복귀 | 이전 노드는 새 epoch를 확인하거나 로컬 hold가 끝나면 신규 admission과 출력 제출을 할 수 없어야 한다. 복귀나 gateway 선호만으로 채널을 되찾지 않는다. 이미 제출된 HTTP 결과 불명은 재조정 대상이며 무조건 재전송하지 않는다 |
| D3 Catch-up cursor | cursor identity도 (provider, actual channel)이며 epoch가 바뀌어도 이어진다. source dedup key에 epoch를 넣지 않는다. 관측·foreign 무시·실패한 보관으로 cursor를 전진시키지 않는다. 수신 cursor와 처리 완료 frontier를 구분한다. REST와 live 입력은 같은 source 판정에 합류한다 |
| D4 Interaction ack | 실행 소유와 initial ack 소유를 분리한다. (application_id, interaction_id) 단위 단일 claimant만 initial callback을 보낸다. ack 권한은 실행 권한이 아니다. callback 종류별로 응답하며 일괄 defer하지 않는다. 결과 불명은 성공도 미전송도 아니다 |
| D5 DM | DM도 (provider, actual DM channel_id)를 쓴다. user→node 선호는 최초 배정 입력일 뿐 기존 home을 덮어쓰지 않는다. pending DM 답변 소비도 활성화 때 이 경계 안으로 들어온다 |

## 4. 진입점 전수표

### 4.1 신규 관측값

신규 상태는 메모리에 반환되는 `IngressObservation` 하나다. 저장·갱신·재수화되는 durable 상태는 없다.
생산자는 `observe` 하나, 운영 소비자는 §9 shadow worker 하나이며 그 결과는 로그로만 나간다.

### 4.2 입력 권위의 생성·변경 경계 (이번에 바꾸지 않음)

| 경계 | 좌표 | 보존할 계약 |
|---|---|---|
| row 구조 | `migrations/postgres/0133_o_channel_homes.sql:1–29` | channel PK, 여섯 상태, holder/target 제약 |
| durable epoch | `migrations/postgres/0134_channel_home_epochs.sql:1–12` | 삭제·재생성에도 epoch를 반복하지 않는 sequence |
| 최초 위임 | `src/db/o_channel_homes.rs:234–257` `delegate` | 부재 조건으로 Releasing 생성 |
| 해제·회수·채택 | `src/db/o_channel_homes.rs:259–377` | 상태·holder/target·epoch 조건부 전이 |
| renewal·force | `src/db/o_channel_homes.rs:373–437` `renew`, `force_orphan` | write 성공만 `HeldHome` 생성 |
| 로컬 gate | `src/services/cluster/channel_home.rs:246–496` | 조회로 gate를 열지 않음. 성공한 renewal과 그 전송 시각만 연다 |
| registry | `src/services/cluster/channel_home.rs:671–760` | 교체 시 이전 gate 철회 |
| boot·lease | `src/services/cluster/channel_home_boot.rs:97–254`, `src/services/cluster/channel_home.rs:867–940` | switch-on 준비, renewal loop, row 상실 처리 |
| drain·복귀 | `src/services/cluster/channel_home_drain.rs:133–260` | intake 폐쇄 → 의무 확인 → 조건부 leave/remove |

### 4.3 live ingress 앞단 (home을 직접 읽지 않지만 활성화 때 빠뜨리면 안 되는 경계)

| 경계 | 좌표 | 부수효과 |
|---|---|---|
| component interaction | `src/services/discord/router/intake_gate.rs:298–355` | model picker, idle recap, node picker, sidecar |
| slash command | `src/services/discord/runtime_bootstrap/gateway_runtime.rs:56–83` | Poise `command_check`와 event handler가 별개 |
| live 메시지 | `src/services/discord/router/intake_gate.rs:356–650` | 로컬 message-ID dedup, thread 문맥, raw/effective 채널 |
| DM·첨부 | `src/services/discord/router/intake_gate.rs:650–930` | pending DM 회신 소비, checkpoint, 첨부 admission |
| REST 복구 | `src/services/discord/catch_up.rs:61–188`, `src/services/discord/catch_up/phase2.rs:23–180` | retry checkpoint, 미해결 frontier |
| singleton gateway | `src/services/discord/runtime_bootstrap/gateway_lease.rs:267–443` | lease 뒤 framework·client 시작 |

### 4.4 home 권위 직접 소비 전수표

`src/` 아래 시험 파일이 아닌 파일 중 `o_channel_homes`, `channel_home::`, `channel_home_boot`, `channel_home_drain`, `home_fence`, `home_availability`를 직접 참조하는 파일 전부다.
`home_authority_inventory_lists_every_direct_consumer` 시험이 이 표와 소스를 대조한다. 새 직접 소비자를 추가하면 여기에 분류해야 시험이 통과한다.

구분: 생성(권위·epoch·gate 개방) / 변경(조건부 전이·철회) / admission(실행·명령·출력 직전 거절) / 라우팅(배치·전달 결정) / readiness(위임 가능 여부, 소유 발급 아님) / 진단 / 배선 / 시험 / 관측(읽어서 로그만 남김).

| 파일 | 구분 | 직접 소비 지점 |
|---|---|---|
| `src/db/o_channel_homes.rs` | 생성·변경·관측 | `delegate` :234, `adopt` :307, `renew` :373(유일한 `HeldHome` 생성자), `force_orphan` :403, 진단 `list_homes` :439·`open_intake_by_epoch` :460, 관측 `read_home_at`(row와 PG 시각을 한 SELECT로) |
| `src/db/intake_outbox.rs` | 라우팅 | claim SQL :674–683. epoch 없는 행은 row 없는 채널에서만, epoch 행은 같은 epoch row에서만 |
| `src/db/intake_outbox/worker_status.rs` | 라우팅 | worker status 쓰기 펜스 `HOME_CLAIM_FENCE` :87. claim SQL과 같은 조건: epoch 없는 행은 row 없는 채널에서만, epoch 행은 같은 epoch worker row에서만 |
| `src/db/mod.rs` | 배선 | 모듈 선언 :32 |
| `src/cli/channel_home.rs` | 변경·진단 | 운영자 `delegate` :168, `begin_reclaim` :182, `force_orphan` :197, 진단 `list_homes` :120, `read_home` :138 |
| `src/cli/args.rs` | 배선 | CLI 인자 :571 |
| `src/cli/run.rs` | 배선 | CLI 실행 :712 |
| `src/lib.rs` | readiness | `home_availability::{Availability, Unavailable, state}` 재노출 :291 |
| `src/services/cluster/mod.rs` | 배선 | 모듈 선언 |
| `src/services/cluster/channel_home.rs` | 생성·변경·admission | 관측 표본 child 모듈 선언, gate `confirm` :340, `admit_unattributed` :182, `admit_command` :195, `HomeGate::admit*` :498–590, `refusal` :592·:759, `register`·`unregister`·`registered` :671–749, `intake_hold` :819, `boot_home` :858, `run_lease` :916 |
| `src/services/cluster/channel_home_boot.rs` | 생성·변경 | `start` :97(registry 등록 :133, lease :188), `listed` :77, `delegated_boot_ownership` :288 |
| `src/services/cluster/channel_home_drain.rs` | 변경 | `drain_round` :133(renew :117, read :138), `run_drain` :212, `finish_return` :235 |
| `src/services/cluster/channel_home_port.rs` | 변경 | drain 의무 포트 :12 |
| `src/services/cluster/home_availability.rs` | readiness | `preflight` :56, `install_enabled` :94, `refusal` :173. 소유권을 발급하지 않는 별도 가드 |
| `src/services/cluster/channel_home/ingress_snapshot.rs` | 관측 | `try_sample`·`sample_gate`: registry·local·inner gate를 `try_lock` 한 번씩 읽어 복사만 한다. expire·prune·close·초기화 없음 |
| `src/services/cluster/channel_ingress.rs` | 관측 | 이 계약. 호출자는 shadow worker뿐 |
| `src/services/cluster/intake_router_hook.rs` | readiness·라우팅 | `home_availability::refusal` :130, `read_home` :135 → `route_to_home` :162 |
| `src/services/cluster/intake_worker.rs` | admission | accept 전 `intake_hold` :558 |
| `src/services/discord/ingress_shadow.rs` | 관측 | 설정·descriptor·bounded 제출. `HomeProvider::parse`로 provider만 정규화 |
| `src/services/discord/ingress_shadow/worker.rs` | 관측 | `read_home_at` → `ingress_snapshot::try_sample` → `observe` → 로그. 쓰기 없음 |
| `src/services/discord/commands/control.rs` | admission | `home_fence::admit` :354·:641, `home_fence::refused` :582·:637 |
| `src/services/discord/commands/control/home_fence.rs` | admission | `admit` :27 → `channel_home::admit_command`, `check` :36, `refused` :41(거절 시 사용자 응답) |
| `src/services/discord/commands/control/provider_reset.rs` | admission | `home_fence::admit` :27 |
| `src/services/discord/commands/restart.rs` | admission | `admit_command` :196·:257 |
| `src/services/discord/commands/skill.rs` | admission | `admit_command` :183 |
| `src/services/discord/commands/stop.rs` | admission | `home_fence::admit` :122 |
| `src/services/discord/commands/text_commands.rs` | admission | `current_command` :37, `admit_command` :389·:1300 |
| `src/services/discord/meeting_orchestrator/lifecycle.rs` | admission | `admit_command` :399 |
| `src/services/discord/router/message_handler/goal_lifecycle.rs` | admission | `admit_command` :165 |
| `src/services/discord/recovery_engine/restore_inflight/scoped_restore.rs` | admission | `read_home` :462·:528, `registered_channel` :469 |
| `src/services/discord/router/intake_dispatch.rs` | 라우팅 | `registered` :201. PG가 있고 등록된 채널은 home 경로가 먼저 결정 |
| `src/services/discord/runtime_bootstrap.rs` | 생성·readiness | 역할별 boot 공급 :503–624(`HomeRole`, `turns_restored`, `HomeSettings`, `install_home_availability`, `start_channel_homes`) |
| `src/services/discord/runtime_bootstrap/o_writer_host.rs` | admission | `delegated_boot_ownership` :336 |
| `src/services/discord/runtime_bootstrap/shared_data.rs` | readiness | availability 등록 보관 :7 |
| `src/services/discord/shared_state.rs` | readiness | availability 등록 보관 :657 |
| `src/services/discord/mod.rs` | 시험 | 시험 fixture의 `_home_availability: None` :1331 |
| `src/services/discord/voice_barge_in.rs` | 시험 | 시험 fixture의 `_home_availability: None` :1353 |
| `src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs` | admission | `registered_channel` :665, `HomeRefusal::NotHeld` 판정 :734 |
| `src/services/discord/turn_lease.rs` | admission | `registered_channel` → `admit_recovery` :155 |
| `src/services/discord/turn_presence/entrypoints.rs` | admission | `HomeGate` 재노출 :7, `withdraw_unmounted` :96: Home mount가 바뀌면 이전 owner에 기댄 presence 승인만 거둔다. row·gate를 쓰지 않는다 |
| `src/services/tui_o/writer/actor/transition_snapshot.rs` | 관측 | `home_now` :155: `registered_channel`로 요청 epoch의 home이 열려 있는지만 읽는다. row·gate를 쓰지 않는다 |
| `src/services/tui_o/writer/transition_port.rs` | admission | 전환 요청이 `CommandPermit` :40을 운반한다. 발급은 `admit_command` |
| `src/services/discord/health/o_transition.rs` | admission | 운영자 전환 요청의 Context 조달 `home_admission` :109: 같은 home lock 아래 `registered_channel`·`ownership`으로 epoch를 확인하고 `admit_at_epoch(…, recovery=false)`로 `CommandPermit`을 받는다. row·gate를 쓰지 않는다 |
| `src/services/queue.rs` | admission | `admit_command`·`admit_unattributed` :243, `admit_command` :501 |
| `src/services/session_forwarding/home_stop.rs` | 라우팅·admission | sender `read_home` :176, receiver `read_home` :335 → `registered` :368 |
| `src/services/turn_host.rs` | admission·readiness | `home_refusal` :118, `for_turn` :130, `refusal_before_turn` :155 |
| `src/services/tui_o/cutover.rs` | admission | `delegated` → `registered` :121 |
| `src/services/tui_o/cutover/intake_route.rs` | readiness·라우팅 | `home_availability::refusal` :39, `held_channels`·`intake_held_channels` :58–59, `any_registered` :88 |
| `src/services/tui_o/repost/runner.rs` | admission·readiness | `registered_channel`·`home_availability::refusal` :37–38 |
| `src/services/tui_o/writer/deliver.rs` | admission | `admit` :157–166(availability 거절 뒤 home `admit_post`) |
| `src/services/tui_o/writer/actor/transition_snapshot.rs` | admission | `home_now` :155. 등록 home의 기대 epoch·Owned·Open을 확인하고, home 없는 채널은 writer gate를 확인한다 |
| `src/services/tui_o/writer/host.rs` | admission·readiness | `owned_now` :353, `home_availability::refusal` :499, `registered_channel` :505 |
| `src/services/tui_o/writer/transition_port.rs` | admission 보관 | `Context::permit` :40. 요청자가 취득한 `CommandPermit`를 작업 종료까지 보관하며 새 permit을 발급하지 않는다 |
| `src/services/tui_o/alarm.rs` | 시험 | 시험 모듈의 `registered_channel` :778 |
| `src/server/routes/agents_stop.rs` | admission | `admit_command`·`admit_unattributed` :66–70 |
| `src/server/routes/queue_api.rs` | 시험 | test 빌드 전용 뮤턴트 스위치 `command_mutant` :206·:240 |
| `src/server/routes/health_api/runtime_profile.rs` | 진단 | `channel_home::health` :10(메모리 읽기만) |
| `src/server/routes/docs/inventory/endpoints/part_04.rs` | 진단 | home stop 전달 endpoint 설명 :538 |

## 5. 운영 전제

| 항목 | 코드 조건 | 이 계약의 처리 |
|---|---|---|
| delegation switch | `src/config/runtime_settings.rs:135–140`, `src/services/discord/runtime_bootstrap.rs:580–590` — `Some(true)`일 때만 boot | 새 ingress 플래그로 재사용하지 않는다 |
| 노드 역할별 복구 표본 | `src/services/discord/runtime_bootstrap.rs:503–522` — Gateway는 utility 여부(`agent.is_none()`)에 따라 `reconcile_done` 또는 미복구, RestWorker는 `reconcile_done`, Standby는 항상 미복구 | 판정 입력이 아니다. 활성화 조각이 역할별로 표본 공급을 검증한다 |
| boot 공급 | `src/services/discord/runtime_bootstrap.rs:526–624` — `HomeSettings`(switch·trim한 instance_id), availability 등록, 후보 채널(이 노드에서 Herdr 실행), `BootPort` | 판정은 이 공급을 대신하지 않는다 |
| 로컬 identity | `cluster.instance_id` 필수, hostname 보충 없음 | 빈 ID는 `NoLocalIdentity` |
| PG | 없거나 조회 불명이면 권위 불명 | `Unreadable`/`Missing` 보류. 기존 standalone 분기는 그대로 |
| 기존 routing mode | `src/services/cluster/intake_router_hook.rs:124–190` — 모든 mode에서 home row를 먼저 읽는다 | 기존 Observe와 다른 계약. switch off를 "home 무시"로 정의하지 않는다 |
| provider | claude·codex | 넓히지 않는다 |
| gateway 역할 | singleton lease | 이번 착지로 standby가 gateway를 열지 않는다 |
| 시간 | `HOLD_FOR` 20초, `RENEW_EVERY` 5초, `FORCE_AFTER` 200초 | 새 임계값 없음 |

코드가 강제하는 조건과 배포 설정(U4·U5)은 따로 확인한다.

## 6. 수용 기준과 시험

| AC | 지키는 계약 | 시험 | 뮤턴트 |
|---|---|---|---|
| AC1 | 자기/외국/불명 분리 | `own_foreign_and_unknown_homes_stay_apart` | missing_as_local, lookup_error_as_missing, foreign_as_local |
| AC2·AC8 | row만으로 후보 없음, 로컬 Lost·closed·expired 보류 | `own_worker_row_holds_without_an_open_unexpired_local_lease` | row_is_enough, ignore_local_intake_closed, ignore_local_expiry |
| AC3 | Worker 외 다섯 상태·Released target 보류 | `only_a_worker_holder_takes_ingress_never_a_released_target` | target_is_owner, draining_as_worker |
| AC4 | provider·실제 채널·row 구조 | `identity_is_the_provider_and_the_actual_channel` | skip_provider_match, skip_channel_match |
| AC5·AC6·P2-1 | 원래 epoch, epoch 없는 전달 입력, gate epoch 비교 금지 | `a_routed_input_keeps_its_original_epoch_and_never_compares_the_gate_epoch` | ignore_source_epoch, routed_none_as_source, ignore_local_home_epoch, compare_home_to_gate_epoch |
| AC7 | 신선도 경계·조회 지연·시간 불명 | `row_freshness_counts_the_whole_read_and_ends_at_hold_for` | expiry_inclusive, drop_read_elapsed, missing_clock_is_fresh |
| AC9 | 두 노드 중 최대 하나 | `two_nodes_never_both_observe_a_candidate_for_one_row` | holder_check_removed |
| AC10 | 예전 관측으로 현재 admission 통과 불가 | `an_earlier_candidate_admits_nothing_after_a_handoff` | 관측값에서 gate를 여는 API가 없어 행동 뮤턴트를 만들 수 없다(구조 경계) |
| AC11·AC12·P2-3 | 관측이 기존 home 진입점 결과를 바꾸지 않음, 진입점이 관측을 호출하지 않음 | `observing_leaves_every_existing_home_entry_point_unchanged` | observe_closes_intake, observe_unregisters, entry_point_observes |
| P2-2 | 4.4 표가 직접 소비자를 빠짐없이 분류 | `home_authority_inventory_lists_every_direct_consumer` | 표에서 `home_fence.rs`·`turn_host.rs` 행 삭제 |

비간섭 시험이 실제로 부르는 진입점: `admit_unattributed`, `admit_command`, `refusal`, `turn_host::refusal_before_turn`, `intake_hold`(epoch 있음·없음), `HomeGate::{admit, admit_post, ownership, expiry, commands_in_flight}`, `intake_held_channels`, `health`.
열린 gate, drain 중 gate, 한 번도 잡지 않은 gate, 미등록 채널 네 경우를 모두 부르고, 기대값(열린 채널 permit, drain 거절, 미등록 Legacy)을 먼저 단언해 빈 기록끼리의 비교를 막는다.

Discord 앞단 `handle_event`와 Poise 명령 경로의 비간섭은 §9.4의 바이트 비교 하네스(`ingress_shadow::compat_tests`)가 증명한다.

## 7. 미확보 항목 [U]

| 번호 | 항목 | 필요 시점 |
|---|---|---|
| U4 | 두 노드의 실제 SHA, `cluster.instance_id`, delegation·routing·owner-authority 설정, PG 대상 | 운영 준비 전 |
| U5 | 같은 instance_id의 동시 프로세스가 없다는 운영 증거. 없으면 process incarnation 펜스 | 후보 배타성 활성화 전 |
| U6 | PG 시간 표본 공급, query 지연·재시도, clock 이상 시 보류 | live sampler 조각 |
| U7 | 단일 잠금 아래 로컬 gate 표본 공급, close·renew·교체와의 경쟁 | live sampler 조각 |
| U8 | 같은 bot의 다중 gateway session에서 guild·thread·DM 수신 양상 | gateway 활성화 전 |
| U9 | Discord intents·history 권한, REST rate limit·재접속·누락 구간 | catch-up 활성화 전 |
| U10 | 모든 실행·출력 sink의 최종 epoch 펜스, 이미 제출된 HTTP 결과 조정 | takeover 활성화 전 |
| U11 | ack claimant의 3초 응답, claim 직후 종료, 응답 유실, callback 종류별 동작 | ack 활성화 전 |
| U12 | catch-up의 첨부 전용 입력, live/REST 겹침, frontier 앞 미해결 항목 | 공유 cursor 전환 전 |
| U13 | 구버전/신버전 혼합 배포, OPEN outbox 잔여의 이전·롤백 | forwarding 제거 전 |
| U14 | 반대 노드의 이미지·일반 파일 실제 독해(원본 메시지 ID → 실행 노드 → 복원 digest → 응답 ID) | 전환 smoke |
| U15 | 노드 중단·복귀에서 누락 0, 중복 응답 0, 성공 initial ack 1 | 전환 smoke |

## 8. 활성화와 롤백

관측 연결은 §9에 착지했다. 롤백은 설정을 끄고 재시작하는 것이며 DB·cursor·queue 이전이 없다.

실제 ingress 전환 전 필요한 것:

| 게이트 | 통과 조건 |
|---|---|
| 권위 | 최초 배정, 자동 takeover(D1), 복귀 노드(D2), process identity(U5) 검증 |
| 실행·출력 | source admission과 최종 출력 경계에서 stale epoch 거절, 관측값 재사용 우회 없음 |
| 수신 보존 | live/REST 공통 source dedup, 공유 custody·cursor(D3) |
| interaction | 단일 ack claimant, 타입별 callback, 불명 결과 조정(D4) |
| 전환 순서 | 기존 OPEN forwarding과 새 ingress가 같은 source를 이중 실행하지 않음 |
| 운영 증거 | U14·U15 |
| 변경 충돌 | `router/intake_dispatch*`, `tui_o/**`를 당시 main 기준으로 재감사·직렬화 |

활성화 뒤 롤백은 routing env를 바꾸거나 singleton gateway를 다시 여는 것으로 하지 않는다.
새 admission 중지 → 진행 중 실행·ack·출력의 종료 또는 불명 상태 보존 → 채널 gate 철회 → 기존 경로가 보존된 source·cursor를 이해하는지 확인 → singleton 경로 재개 순서다.
이전 바이너리가 새 custody를 이해하지 못하면 자동 롤백하지 않고 보류한다. `intake_outbox` forwarding 제거는 잔여 OPEN 작업의 귀속을 확인한 뒤의 별도 단계다.

## 9. 관측 연결 (ingress shadow)

판정을 실제 gateway 이벤트에 붙이되, 결과는 구조화 로그로만 남긴다. 기존 실행·forwarding·ack의 결정, 효과 순서, payload bytes, 호출 횟수는 바꾸지 않는다.

### 9.1 켜는 조건과 수명

| 항목 | 내용 |
|---|---|
| 설정 | `runtime.channel_ingress_shadow_enabled: true`와 0이 아닌 채널이 하나 이상 있는 `runtime.channel_ingress_shadow_channels`가 모두 있어야 한다. unset·false·빈 목록은 휴면이고 전체 채널 wildcard는 없다 |
| 부팅 | gateway runtime 시작(`run_bot_start_gateway_runtime`)에서 설정을 한 번 읽는다. `run_bot` 본문은 B1 boot census가 고정하므로 건드리지 않는다. 변경 적용은 프로세스 재시작이다. delegation switch와 별개이며 home boot·gateway 역할을 바꾸지 않는다 |
| 역할 | gateway 역할 분기 뒤 framework setup에서만 시작한다. standby·REST worker는 gateway 이벤트를 받지 않으므로 시작하지 않는다. provider는 claude·codex만 |
| 소유 | `Data.ingress_shadow`가 handle을 가진다. 꺼져 있으면 `None`이고 `begin`(pool·generation 읽기)도 부르지 않는다. handle drop은 queue를 닫아 worker를 끝내며, drop은 worker를 기다리지 않는다 |
| identity | 로컬 ID는 trim한 `cluster.instance_id`다. hostname으로 보충하지 않고, 비어 있으면 판정이 `NoLocalIdentity`로 보류된다 |

### 9.2 경로

1. hook: gateway framework의 event handler(`gateway_runtime::run_bot_framework_options`)가 `handle_event`를 부르기 직전에 `ingress_shadow::observe`를 한 번 부른다. 이 handler가 `handle_event`의 유일한 운영 호출자라 `handle_event` 첫 줄과 같은 지점이고 모든 early return 앞이다. giant 파일 순증 0 게이트 때문에 `router/intake_gate.rs`가 아닌 이 자리에 둔다. Poise `command_check`에는 붙이지 않는다(raw `InteractionCreate`에서 이미 제출한다).
2. 제출: 원본 이벤트를 빌려 종류·실제 channel·원본 ID·application ID만 복사한다. 채널은 thread parent·policy channel이 아니라 이벤트의 channel이다. allowlist 밖이면 아무것도 하지 않는다. `try_send` 한 번이며 DB·HTTP·lock·`await`·task spawn이 없다. 실행 ingress가 아닌 이벤트(edit·delete·ping·presence 등)는 `excluded`로 센다.
3. queue: 256개 bounded queue, worker 1개. full·closed면 그 descriptor를 버리고 센다. 버린 것도 sequence를 하나 받으므로 기록에서 빈 번호로 드러난다.
4. 조회: observer 전용 pool(최대 연결 1, 기존 runtime pool의 연결 옵션 복사, 첫 조회 전 연결 없음)로 `o_channel_homes::read_home_at`을 부른다. row와 PG `statement_timestamp()`를 한 SELECT로 읽는다. `started`는 pool acquire 전에 잡고 acquire+query 전체를 250ms로 자른다. timeout·DB 오류·decode 실패는 `Unreadable`, row 없음은 `Missing`이며 retry·cached row는 없다. worker가 끝나면 pool을 닫는다.
5. 표본: row가 있을 때만 `channel_home::ingress_snapshot::try_sample(channel, row.provider)`로 이 노드 gate를 읽는다. registry→local→inner를 각각 `try_lock` 한 번으로 읽고, expire·prune·close·registry 초기화를 하지 않는다. 표본 key는 row의 정규 provider와 gate의 실제 channel이다. contention·poison은 표본 없음이며 원인을 `sampler`에 남긴다.
6. 판정: `observe(key, instance_id, UnstampedSource, home, sample, now)`. key는 shadow의 provider와 이벤트 channel이다.

### 9.3 기록과 분모

| 기록 | 필드 |
|---|---|
| `start` | incarnation(UUID), instance_id, build(실행 파일 SHA-256), bot, provider, generation, allowlist |
| `observed` | incarnation, instance_id, generation, bot, provider, sequence, kind, channel, source_id, application_id, queued/lookup/sample ms, lookup·sampler 상태, row/local epoch, verdict·reason, foreign holder |
| `close` | incarnation, last_sequence(제출 수), completed, lost(full+closed), unknown, excluded, terminal. 60초마다, 그리고 queue가 닫힐 때 |

원문·첨부 bytes·URL·interaction token·DB 오류 문자열은 남기지 않는다. 카운터에는 source·channel·node ID가 없다.
`unknown`은 권위 조회 불명, 시간 불명, 로컬 identity 없음, 그리고 표본을 못 읽어 `NoLocalLease`가 된 판정이다.

증거 판정은 순수 함수 `ingress_shadow::telemetry::window::evaluate_window`(시험 빌드 전용, 런타임 reader 없음)로 고정한다.
PASS는 다음이 모두 참일 때뿐이다. 기대 incarnation마다 start가 하나이고 instance·build·bot·provider·allowlist가 기대와 같다. 마지막 close가 `completed == last_sequence`, `lost == 0`, `unknown == 0`이다. 1..=last_sequence가 빠짐·중복 없이 있고 close 뒤 기록이 없다. 모든 source를 모든 기대 incarnation이 같은 row epoch로 판정했다. paired 분모가 양수다.
기대 밖 incarnation, epoch가 갈린 source, 한쪽만 본 source는 INCOMPLETE다. 같은 source를 둘 이상의 incarnation이 자기 후보로 판정하면 instance ID·epoch가 같든 다르든 CONFLICT다.
singleton 운영에서는 paired 분모가 0이므로 결과는 INCOMPLETE이며 "두 노드 후보 0"으로 보고하지 않는다.

### 9.4 수용 기준과 시험

| AC | 계약 | 시험 | 뮤턴트 |
|---|---|---|---|
| AC1 | raw 이벤트마다 정확히 1회, 모든 early return 앞에서 제출 | `ingress_shadow_keeps_*_byte_identical`(case별 `submitted == 이벤트 수`), `an_event_is_described_by_its_own_channel_and_ids` | early_return_before_ingress |
| AC2 | off면 queue·pool·sampler·observe 0, `begin` 미호출 | `only_the_switch_with_listed_channels_starts_an_observer`, compat off 실행 | disabled_still_observes |
| AC3 | off/on(응답·timeout·full·closed)의 효과 순서·bytes·횟수·결과 동일, 응답 observer는 관측 완료 양수 | `ingress_shadow_keeps_{live_messages,attachment_messages,components,poise_commands}_byte_identical`, `ingress_shadow_keeps_a_pending_dm_reply_byte_identical_pg` | shadow_controls_dispatch, hook_removed |
| AC4 | 기대 payload가 비어 있지 않고 off/on이 정확히 같음 | 같은 시험 | drop_payload_byte |
| AC5 | Poise 허용·거절, `/help` initial callback 1·follow-up 1 | 같은 시험(`HelpAllowed`, `HelpRefused`) | skip_initial_callback |
| AC6 | 표본 key만 다른 `(codex, CH+1)`·`(claude, CH)`는 `LocalSampleMismatch` | `own_worker_row_holds_without_an_open_unexpired_local_lease` | sample_key_check_removed |
| AC7 | 한 SELECT의 PG 시각, acquire 전부터의 전체 지연, 20초 경계, 시간 불명 보류 | `freshness_counts_the_whole_lookup_on_the_database_clock`, `a_row_read_carries_pgs_clock_from_the_same_statement_pg` | drop_read_elapsed, expiry_inclusive, clock_error_as_fresh |
| AC8 | expire·prune·close·초기화 0, registry/local/inner contention·poison에서 표본 없음 | `sampling_expires_prunes_closes_and_initializes_nothing`, `a_taken_or_poisoned_lock_gives_no_sample`, `an_unreadable_gate_holds_and_counts_as_unknown` | sample_via_ownership, sample_via_registered, inner_read_blocks |
| AC9 | renew·close·lapse와 경쟁해도 한 상태의 epoch·intake·만료만 | `a_sample_never_mixes_two_states_of_one_gate` | split_epoch_and_expiry_reads |
| AC10 | 양수 분모에서 최대 한 후보, 같은 ID 다른 incarnation·다른 epoch 후보는 충돌 | `two_processes_judging_one_source_their_own_is_a_conflict_across_ids_and_epochs` | dedup_by_instance_only, join_partitioned_by_epoch |
| AC11 | 손실·불명·미종료·분모 0은 인증 실패 | `a_lost_unknown_or_unfinished_sequence_or_no_pair_never_certifies` | missing_samples_are_zero |
| P2-1 | 중간·마지막 기록 삭제, close 삭제, 기대 incarnation 누락, allowlist 불일치, 일부 source만 paired는 INCOMPLETE | `a_window_passes_only_when_every_observer_and_source_joined_to_a_finished_close` | ignore_unpaired_tail, ignore_terminal_manifest |
| AC12 | 새 직접 소비자가 §4.4에 분류됨 | `home_authority_inventory_lists_every_direct_consumer` | worker 행 삭제 |
| U3 | observer pool은 연결 1개, 첫 조회 전 연결 없음, worker 종료 때 닫힘 | `the_observer_pool_reads_rows_on_one_connection_and_closes_pg` | — |

호환 하네스(`ingress_shadow/compat_tests.rs`)는 case마다 off를 두 번, 응답·timeout·full·closed observer로 한 번씩 각각 별도 자식 프로세스에서 돌린다.
이벤트는 생산 `run_bot_framework_options`(command check·event handler)로 실제 Poise `dispatch_event`를 거친다. 기록은 Discord 요청의 method·path·raw body, runtime root 아래 모든 파일 bytes, mailbox queue, checkpoint, session, DM 회신 행, handler 오류다.
비교 전에 off 실행의 기대 효과(큐 경로의 reaction·checkpoint·durable queue 본문, DM 회신 소비, component callback 1, `/help` callback 1·follow-up 1, 거절 0)를 먼저 단언한다.
한계: 시계를 고정할 seam이 없어 durable queue의 `created_at_wall_time_ms`·`enqueued_at_epoch_us`와 mailbox의 도착 `Instant`만 값 대신 존재로 비교한다.
idle 채널 일반 메시지는 fixture가 provider 시작 placeholder를 거절해 본문이 효과에 나타나지 않으므로, 일반·thread 본문 bytes는 큐 경로(`Queued`, `ThreadQueued`)에서 비교한다.
live 첨부 다운로드는 Discord CDN host만 허용하고 test seam이 없어 다운로드 거절 경로만 비교한다. 첨부 bytes·digest 동일성은 U14로 남는다.

### 9.5 롤백과 남은 항목

롤백은 설정을 끄고 재시작하는 것이다. source·cursor·outbox 이전이 없다.
§7의 U4–U15는 그대로 남는다. 추가로 U16: 로그 수집 누락·worker 종료를 포함한 운영 완전성과, 양수 분모 두 노드 기록의 `evaluate_window` PASS(후보 충돌 0)를 운영에서 확보해야 한다.
