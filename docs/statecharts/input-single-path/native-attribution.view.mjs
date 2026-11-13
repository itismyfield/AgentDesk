export const title = 'Claude 입력: 자기 실행의 종료만 정리한다';
const phase = v => {
  if (v.staleReleased) return '옛 증거로 후속 입력을 정리';
  if (!v.booted) return '재시작: 원본과 결과 보존';
  if (v.lease === 2) return '후속 head 주입';
  if (v.lease === 0 && v.result?.[1] !== 'None') return '결과 보존: projection 정리와 wake';
  if (v.pc === 0) return v.outsideQueued ? '외부 입력 enqueue' : '원본 두 개 대기';
  if (v.pc === 1) return '외부 턴 실행';
  if (v.pc === 2) return 'AgentDesk 입력 주입';
  if (v.pc === 3) return 'Claude 내부 큐 수신';
  if (v.pc === 4) return '실행 합류 또는 철회';
  if (v.pc === 5) return '외부 assistant end: native 종료 전';
  if (v.pc === 6) return '외부 native 종료: 내 큐 입력은 미실행';
  if (v.pc === 7) return 'dequeue: 다음 user 대기';
  if (v.pc === 8 || v.pc === 9) return '자기 입력의 native 실행';
  return '자기 native 종료: 결과 확정 대기';
};
export const state = v => phase(v) + (v.foreignBody ? ' · 남의 본문 축적' : '');
const labels = {
  ExternalEnqueue:'외부 예약·알림 입력 enqueue', ExternalStart:'예약·알림·TUI 직접 입력 시작', Inject:'AgentDesk 입력 주입',
  Enqueue:'Claude 내부 큐 enqueue', MidTurnJoin:'remove 흡수·command로 진행 턴 합류',
  Withdraw:'queue remove: 철회', ExternalAssistantEnd:'외부 assistant end_turn',
  ExternalDuration:'외부 strict closer', Dequeue:'Claude 내부 큐 dequeue',
  OwnStart:'자기 user로 실행 시작', OwnAssistantEnd:'자기 assistant end_turn',
  OwnDuration:'자기 strict closer', ObserveProof:'입력별 결과 durable 확정',
  Settle:'같은 episode lease CAS', ProjectionClear:'같은 inflight row CAS',
  Kick:'QueueEligible와 owner wake', InjectHead:'원래 head 주입',
  HeadAssistantEnd:'head assistant end_turn', HeadDuration:'head strict closer',
  OperatorRelease:'기존 운영자 release: 성공 주장 없음',
  AdjacentEnd:'기존 Cancel·Terminate·clear·stop 결과', Restart:'프로세스 재시작',
  BootRestore:'boot: 원본과 cleanup debt 복원', Reconstruct:'같은 source로 귀속 재구성',
  WarnIdle:'native idle 미종결 경보 1회', ExecutionReplaced:'새 실행 incarnation',
  StaleEvidence:'옛 실행·episode 증거 거절', DuplicateSettlement:'중복 완료 기록 mutant'
};
export const step = action => labels[action] ?? action;
