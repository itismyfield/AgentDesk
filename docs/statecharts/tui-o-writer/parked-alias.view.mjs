export const title = 'park된 저장 원본 A0의 확정 raw 이름 A1 뒤 fresh 응답';
export const state = v => {
  const phases = {
    storedEmpty: '저장 cursor A0 · session 빈 값', emptyBound: 'A0 bind 적용 · cp1',
    prefixApplied: '파일 있을 때 A0→A1 적용 · cp2 · 저장 A0 유지',
    missingA: 'A pathname 소실 · 세션 없음', parkedReady: '저장 A0 park · 입력 승인 가능',
    bindQueued: 'raw old A1→fresh B · Startup · cp2',
    referenceMapped: '확정 old를 저장 A0로 참조 · B 검증 전',
    replyPending: 'fresh B 검증·attach · cp3 · 첫 답변 대기',
    settled: '첫 답변 1회 durable Posted', duplicateChecked: '중복 bind/refresh · 추가 답변 0회',
    restarting: '같은 store 재시작 · Posted 보존', finished: '재시작 뒤 답변 여전히 1회',
    recovering: '이전 원본 reopen 실패 뒤 재호스트',
  };
  if (v.caseName !== 'confirmedOld') {
    if (v.phase === 'halted') return 'new/parent/미확정·모호 alias는 기존 검증에서 halt';
    if (v.phase === 'referenceMapped') return '경계: 참조 통과 · 새 target 엄격 검증/C6 전';
    return '경계: new/parent/미확정·모호 alias 준비 · old 예외로 넘기지 않음';
  }
  if (v.phase === 'halted') return v.caseName === 'confirmedOld'
    ? `raw old A1이 Missing A0 재open 요구 · halt ${v.halts}회`
    : 'new/parent/미확정·모호 alias는 기존 검증에서 halt';
  if (v.phase === 'bindQueued' && v.rehosts > 0)
    return `같은 old A1→B 재시도 · cp2 · 이전 halt ${v.halts} · 재호스트 ${v.rehosts}`;
  if (v.phase === 'recovering') return `이전 원본 reopen 실패 뒤 재호스트 ${v.rehosts} · halt ${v.halts}`;
  if (v.caseName === 'unseenOld' && v.phase === 'prefixApplied')
    return '경계 검증 · cp2 prefix에 A1 이름 확정 없음';
  if (v.caseName === 'ambiguousOld' && v.phase === 'prefixApplied')
    return '경계 검증 · stored 이름 대응 모호 · 추측 금지';
  if (v.caseName === 'parkedNewReturned' && v.phase === 'parkedReady' && v.aPresent)
    return '경계 검증 · parked A0 파일 복귀 · new 대상 C6 유지';
  if (v.caseName !== 'confirmedOld' && v.phase === 'bindQueued')
    return `경계 검증 · raw A1을 ${v.referenceRole}로 참조 · old 전용 예외 확인`;
  if (v.caseName !== 'confirmedOld' && v.phase === 'referenceMapped')
    return '경계 검증 · strict prefix 성공 · parked new C6 검사 전';
  if (v.caseName !== 'confirmedOld') return ['storedEmpty','emptyBound','prefixApplied','missingA','parkedReady','bindQueued','referenceMapped'].includes(v.phase)
    ? `경계 검증 · ${phases[v.phase]}` : phases[v.phase];
  return phases[v.phase];
};
export const step = (action, from, to) => {
  if (action === 'ResolveRawReference') return to.phase === 'halted'
    ? (from.caseName === 'confirmedOld' ? 'old A1의 저장 A0 재open 요구 · Missing 실패' : 'new/parent/미확정·모호 alias는 strict 검증 거부')
    : (from.caseName === 'confirmedOld' ? '확정된 parked old alias만 A0로 참조' : '복귀 파일의 기존 prefix 검증 성공 [new target]');
  return ({
  ApplyEmptyBound: 'Bound(A0) 적용', ApplyFilledName: '존재하는 파일에서 이름 확정·prefix 적용',
  LoseOriginal: '원본 소실', ParkStoredA: '부재 확인 후 named A0 park',
  ReturnOriginalForNew: 'new 재bind 경계: 같은 원본 복귀',
  QueueFreshBound: '입력 승인·bind 기록 [정상 경로 old=A1,new=B,Startup]',
  ResolveRawReference: 'old 역할과 적용 prefix 확인 또는 기존 strict 검증',
  AttachNewOrRejectParkedTarget: 'fresh B 검증·attach 또는 parked new C6 halt',
  PostFirstReply: '첫 assistant 응답 게시', DuplicateBindRefresh: '중복 bind/refresh 통지',
  RestartAfterPosted: 'actor 재시작', RestorePosted: 'old park 복원·Posted 재생',
  RehostAfterHalt: 'owner cleanup 재호스트', ResumeParkAgain: 'A0 다시 park 후 같은 seq3 재시도',
  Rest: null,
})[action] ?? (action === 'Rest' ? null : action);
};
