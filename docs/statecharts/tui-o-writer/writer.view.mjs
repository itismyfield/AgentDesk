export const title = '소실 원본을 park한 기록기의 입력 승인과 첫 응답';
export const state = v => {
  const cycle = v.restarted ? '재시작 뒤' : '최초 실행';
  if (v.writer === 'halted' || v.writer === 'ended') {
    return `${v.writer === 'halted' ? '소실로 중단' : '기록기 종료'}${v.boots > 1 ? ' · 반복 부팅' : ''}`;
  }
  if (v.writer === 'booting') return `${cycle} · 원본 재열기${v.boots > 1 ? ' · 반복 부팅' : ''}`;
  if (v.posted === 1) return `${cycle} · 첫 응답 1회 게시`;
  return `${cycle} · 원본 park · ${({ waiting: '입력 대기', held: '입력 보류', accepted: '입력 승인', launched: '새 세션과 새 원본' })[v.input]}`;
};
export const step = action => {
  const labels = {
    ResumePark: '소실 확정·안전 조건 충족 → 원본만 park',
    ResumeHalt: '안전 조건 불충족 → 기존 halt',
    HostEnds: '기록기 종료·자리 반환',
    PeriodicBoot: '주기 정리가 기록기 다시 요청',
    InputArrives: '새 입력 도착',
    AdmitInput: 'ready gate가 입력 승인',
    NewSessionSource: '첫 prompt 뒤 fresh 세션·startup 원본 생성',
    PostFirstReply: '첫 응답 게시·durable Posted 기록',
    Restart: '프로세스 재시작·메모리만 초기화',
    DeliverRetainedSpool: '재생한 옛 spool 의무 전달',
    RepeatNotification: null,
    Rest: null,
  };
  return Object.hasOwn(labels, action) ? labels[action] : action;
};
