# Native control custody (OFF)

native_prepare::enabled()=false를 유지한다. 기존 registration/전체 ticket/Registry path/captured actor를 scoped mailbox handle에 연결하고, first-key ack 전에 설치한다. Cancel/Terminate/clear/stop/operator release의 원래 actor/identity 조건 뒤에 nonblocking result admission을 둔다. policy close는 actor grant뿐 아니라 원래 acked-row receipt의 lock-held predicate까지 통과한 뒤 admission한다.

결과가 있으면 그대로 보존하고, control이 먼저면 full-ticket control proof와 Cancelled/OperatorReleased를 durable하게 쓴다. source 재독·외부 end_turn·시간으로 실행 성공을 합성하지 않는다. 결과 저장이나 try mutex/flock/durability 확인이 실패하면 release 이전에 거절한다. 확인된 NotSent는 원본을 보존하는 nonterminal 사실이며, 키가 없다고 추정해서 Cancelled/Withdrawn을 만들지 않는다.

기존 reconcile은 저장된 intent가 허용한 exact actor/id/nonce/start cutoff만 소비한다. lease가 끝난 뒤 현재 runtime row와 원래 intent snapshot이 같을 때만 CAS한다. successor 또는 runtime 불일치이면 삭제·wake 없이 debt만 종결한다. cleanup ack와 terminal custody가 모두 끝난 뒤 기존 ledger admission을 연다. 일반 terminal phase나 GC만으로 native debt를 없애지 않는다.

ControlBefore/Fixed, ControlCustodyBefore/Fixed, ControlStaleBefore/Fixed, ControlUnreadableFixed, ControlNoKeyFixed를 추가했다. 전체73cfg는 기대 위반 집합과 일치한다. prepared는 scoped 등록 ack/AnyKey, failed는 실제 읽기·영속 실패를 추상화한다. 확정 NotSent/첫 key/명시 primary의 상태는 기존 NativeInputPreparation과 Rust boot fixture로 검증한다. metadata JSON/namespace/full ticket/receipt와 mutex/flock는 모델에서 생략하며 실제 진입점 시험과 원인 제거 뮤턴트로 확인한다.

사용자 11-14 범위 축소: WARN·O observer·새 ticket replay·lost wake는 제외한다. 기존 O 증거 공급과 활성화·herdr relay e2e는 조각3/4이며 이번 변경은 활성화하지 않는다.
