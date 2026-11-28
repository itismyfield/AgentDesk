export const title = '같은 본문의 후속 흡수 입력 공지';

export function state(v) {
  if (v.replaying) return '재시작 후 원장과 원본 재생';
  if (v.next.posted) return '미소유 입력의 공지 한 건 확인';
  if (v.next.prepared) return '후속 공지 전송 또는 확인 대기';
  if (v.next.verdict === 'send') return '후속 입력의 공지 필요';
  if (v.next.verdict === 'excluded') return '새 등록 입력의 공지 제외';
  if (v.next.verdict === 'pending') {
    if (v.kind === 'contested' || v.kind === 'unreadable') return '귀속 증거가 불충분해 보류';
    return v.restarted ? '재시작해도 후속 공지 정산이 멈춤' : '후속 공지 정산이 멈춤';
  }
  if (v.next.row === 'materialized') return '후속 흡수 입력의 근거를 검토';
  if (v.first === 'excluded') return '첫 공지의 제외와 소비 범위를 원장에 보존';
  return '첫 흡수 입력의 공지 정산 대기';
}

export const group = v => v.replaying ? 'restart'
  : (v.next.posted || v.next.verdict === 'excluded') ? 'done' : 'active';
export const colors = {restart: 'yellow', done: 'teal', active: 'blue'};

export function step(action, from, to) {
  if (action === 'ReviewNext') return {
    excluded: '후속 입력을 새 등록에 귀속',
    send: '남은 등록이 없음을 확인',
    pending: '현재 자료로 귀속을 결정할 수 없음',
  }[to.next.verdict];
  return ({
    FirstCapture: '첫 흡수 행을 캡처해 원장에 기록',
    FirstClose: '첫 턴의 native 종료 근거를 확인',
    FirstSettle: '첫 등록 소유권과 소비 범위를 기록',
    NextEnqueue: '같은 본문의 후속 입력을 큐에 기록',
    NextCapture: '후속 흡수 행을 캡처해 원장에 기록',
    NextClose: '후속 턴의 native 종료 근거를 확인',
    PrepareNext: '후속 공지를 준비',
    PostNext: 'Discord에 실제 공지를 게시',
    ConfirmNext: '게시 결과를 확인해 원장에 기록',
    Restart: 'writer 재시작',
    Replay: '원장에서 이전 소비 범위를 복원',
  })[action] ?? action;
}
