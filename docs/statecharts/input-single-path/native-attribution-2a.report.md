# Native attribution 2a — independent reader and receipt stamp

기준 25d22a24a(B2b-1). 독립 reader로 Watch의 native prepare/snapshot/debug observe를 제거했다. activity rebuild는 native history를 열지 않으며 reader는 명시 ticket에만 존재한다. 기본 producer 비활성 경계를 유지하고 AnyKey 상태의 실제 source API가 설계 밖 파일에 있어 FILES를 요청한다. 나머지 producer/custody/runtime-witness/bridge 연결은 이 경계에서 구현하지 않았다.

## 설계 선택

Watch 안 clone/stamp→밖 history/fold 분리안보다 독립 reader가 activation 접점 수가 적다. history→Watch 잠금 순서를 만들지 않고 native 경로에서 Watch를 전혀 받지 않는다. source.offset 앞의 외부 FIFO 부채를 버리면 B1 의미가 달라지므로 전체 prefix를 poll budget으로 재생한다. activity tail/Reload::Full cursor 수명과 독립이고, pending ticket이 없어지면 facts를 해제한다. 등록·receipt가 변하면 source가 그대로여도 후보를 재평가한다. 이중 read 비용은 ticket이 존재하는 동안에만 발생한다.

Native capture/fold의 현재 consumer는 시험 포트뿐이므로 cfg(all(test,unix))로 유지했다. 일반 queue 분류·v3 schema reader·기존 prepare/bind/project·boot 보존은 production에 유지한다. 사용처가 사라진 native_capture_enabled만 제거했다. 무의미한 production 호출이나 allow/baseline 인상으로 경고를 숨기지 않는다.

## stamp와 단일 결과

Observation에 namespace/full ticket/frame hash/receipts의 fingerprint를 추가했다. preview는 두 fingerprint를 비교하고, NativePrepared admission은 현재 registry와 비교한다. 결과 writer는 기존 history transaction lock 안에서 다시 비교한 뒤 publish한다. 새 Executed 결과는 membership UUID가 receipt 집합에 확정되어 있어야 한다. 이미 같은 ticket/outcome으로 확정된 결과의 cleanup 재시도는 후속 receipt를 새 완료로 재해석하지 않고 멱등 재개한다. None follower도 기존 full ticket 결과 계약을 유지한다.

## 상태 차트

NativeInputPreparation의 receipt/member/fifo/pending guard를 상태 조건으로 바꾸고 Mode가 proof를 직접 써 넣던 경로를 제거했다. 기존 Before/Fixed 위반 집합을 유지한다. StampBefore/Fixed는 snapshot→receipt version 변경→결과 CAS→새 receipt 재평가를 모델링한다. NativeInputAttribution 기존20(+Debt3)는 unchanged이며 producer/custody/witness/neutral bridge 연결이 없으므로 운영 release/row/wake GREEN이라 하지 않는다. boot/WARN/lost wake는 2b다.

37cfg 전체 rc0·expected safety/liveness 집합 일치,115.7초. source/cfg39개 SHA 및 원시 b2b2a-tlc.txt를 보존했다. Native tail 시험의 초기 fixture 오류(native input 시작 줄, identity 없는 assistant prefix)는 행위 RED로 세지 않는다. fixture를 정상 schema로 고친 뒤 guard 제거 뮤턴트와 정상 경로를 별도 확인한다.

## AnyKey FILES

설계 표의 owner_input은 provider_dispatch/owner_input.rs다. 실제 key 시도 상태는 설계 목록 밖 src/services/claude_tui/owner_input.rs::Scope.keyed / keyed(:115)에만 있고, read API가 없다. capture_scope(:450)는 결과와 NotSentProof만 내보내며 proof 없음은 AnyKey 증명이 아니다. 기존 start_payload(:101)는 content 직전 commit이고 key 시도와 같은 행동이 아니므로 대입하지 않는다.

요청: 위 파일에 scoped keyed 상태의 최소 production read port(약3~6줄)를 허용한다. 저장 위치/flag/setter/기존 capture_scope 서명은 그대로 둔다. 그 read를 effect leaf의 정상 종료·unwind 전에 소비하는 연결은 이미 허용된 first_input/pump/producer에서 한다. AnyKey getter를 시험 seam으로만 만들거나 no-key 원본을 completed/withdrawn으로 바꾸지 않는다.

## 활성화와 인접 경계

Default producer·native actor·bridge 활성화0. boot/custody-only/old generation/Cancel/Terminate/clear/stop/operator release/WARN/observer replay/lost wake/herdr e2e는 변경0이고 2b NOT_RUN이다. d1 entry census의 소유를 바꾸지 않는다. Native response scope/neutral exit 및 native lock-held runtime witness도 FILES 경계 뒤의 2a 잔여다. no-key 원본은 retained/queued/unknown을 유지하며 시간 자동 완료0.


## 검증 경계

정상 schema fixture로 기존268개와 새3개의 ID를 대조해 271개 모두 재실행했다. 선택=실행=통과271, 실패/ignored0, 중복0. 실제 PostgreSQL17.11·단일 시험 thread·PG skip0이며 전체 lib 실행은 하지 않았다. 기본 producer/bridge가 없으므로 이 결과는 운영 DoD 완료가 아니다. native idle WARN와 herdr relay e2e는2b NOT_RUN이다.

6개 원인 제거 뮤턴트(capture/preview/transaction fingerprint, confirmed receipt, Watch 안 history IO, tail capture) 각각 선택1·실패1·ignored0(rc101), 정상31/31(rc0)이다. 임시 변경4파일의 복원 SHA와 전체 Rust diff SHA가 정본과 동일하며 훅은 커밋0이다. 원시 근거는 레인 b2b2a-mutants-results.json 및 b2b2a-mutant-*.txt다.
