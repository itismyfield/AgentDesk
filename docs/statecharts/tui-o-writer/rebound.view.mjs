export const title = '소실 원본 park: 최초 연결 순서와 현재 binding 판정';
export const state = v => {
  const names = {
    adopted: 'A 채택 · link 없음', boundB: 'B 연결 · A 다음 B',
    rotationSaved: 'A 재연결 rotation 저장 · checkpoint는 아직 B',
    reboundA: 'A 재연결 완료', prepared: '전체 binding log와 복원 상태 준비',
    missing: '원본 소실 · resume 시작',
    following: '원본 park 판정 뒤 · ready 전 follow',
  };
  if (v.phase === 'lateBind' || v.phase === 'lateRefine') return v.ready
    ? 'park된 A 재연결 또는 refinement 뒤에도 running' : 'park된 A 재연결 또는 refinement는 halt · 다음 host가 재검증';
  if (v.phase !== 'checked') return names[v.phase] ?? v.phase;
  if (!v.ready) return '소실 또는 follow 실패 · halted · ready=false';
  if (v.checkpoint < v.required) return '유효 새 bind 미적용인데 ready';
  if (v.view === 'unknown') return `현재성 Unknown · park · producer ${v.presence}`;
  if (v.viewTarget === 'A' && v.parked.includes('A') && v.presence !== 'absent')
    return '현재 A를 historical로 오판 · park · halted 없음';
  return `ready · 현재 ${v.viewTarget} · producer ${v.presence}`;
};
export const step = action => ({
  BindB: 'A → B 신규 bind',
  SaveRebindRotation: 'B → A 재연결 · rotation만 먼저 저장',
  CommitCheckpoint: '해당 bind checkpoint 저장',
  InterruptAfterRotation: 'rotation 저장 뒤 checkpoint 저장 전 중단',
  BindC: 'A → C 신규 bind',
  PrepareOther: 'oldless · 미적용 · Pending · 실패 log 시나리오',
  LoseSource: '원본 pathname Missing',
  ResumeMissing: '전체 log에서 current 판정 후 park 여부 결정',
  FollowBeforeReady: '미적용 유효 bind 적용 · 새 source 열기',
  BindParkedAfterView: '판정 tail 뒤 parked 원본으로 다시 bind',
  RefineParkedWithinView: 'parked 원본의 boundary refinement 적용',
  Rest: null,
})[action] ?? (action === 'Rest' ? null : action);
