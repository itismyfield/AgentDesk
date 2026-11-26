export const title = 'Claude 턴 도중 사람 입력 공지';

function settled(v) {
  return (v.owned || v.hookObserved) && v.restarted && !v.replay.length
    && v.scanned.length === v.forms.length
    && v.posted.length + v.excluded.length === v.evidence.length;
}

export function state(v) {
  if (v.owned && v.effects.length) return '원본 경로가 소유한 입력의 공지가 추가 게시됨';
  if (settled(v) && v.owned) {
    return v.excluded.length ? '재생 뒤에도 논리 ID의 제외를 유지' : '논리 ID의 제외가 남지 않음';
  }
  if (v.effects.length > 1) return '같은 입력이 두 번 게시됨';
  if (settled(v)) return v.effects.length ? '재생까지 끝나고 공지 한 건 확인' : '재생까지 끝났지만 공지 없음';
  if (v.replay.length) return v.effects.length ? '기존 게시를 보존하며 spool 재생' : '원본 spool 재생 대기';
  if (v.excluded.length) return '물리 수신 근거로 논리 ID를 제외';
  if (v.posted.length) return '게시 확인 기록';
  if (v.effects.length) return 'Discord 게시, 원장 확인 대기';
  if (v.prepared.length) return '게시 준비를 원장에 기록';
  if (v.evidence.length) return '입력 근거를 원장에 기록';
  if (v.spool.length) return '원본 행을 spool에 보존';
  if (v.physicalReceipt) return '물리 행의 수신 확인';
  if (v.enqueued) return '큐의 입력이 흡수되길 기다림';
  if (v.hookObserved) return '관측기가 공지를 writer에 맡김';
  if (v.handed) return 'writer가 공지 권한을 인계받음';
  return '권한 인계 전';
}

export const group = v => v.replay.length ? 'restart' : (v.posted.length || v.excluded.length) ? 'posted' : 'active';
export const colors = {restart: 'yellow', posted: 'teal', active: 'blue'};

export function step(action, from, to) {
  if (action === 'Project' || action === 'Replay') {
    return from.evidence.length < to.evidence.length ? '원본 행에서 근거를 기록'
      : to.effects.length ? '같은 입력 키로 재생 처리' : '원본 행을 확인';
  }
  return ({
    Handoff: '동결 구간을 원장에 기록',
    Observe: '관측만 기록하고 직접 게시를 생략',
    Capture: '원본 파일의 완전한 행을 캡처',
    Enqueue: '아직 실행되지 않은 입력을 큐에 기록',
    RecordPhysicalReceipt: '명령 행의 물리 UUID로 수신 확인',
    SettleOwned: '논리 입력 ID의 제외를 원장에 기록',
    Prepare: '게시 의무를 원장에 기록',
    Post: '입력 공지를 Discord에 게시',
    Confirm: '게시 결과를 원장에 기록',
    Restart: '재시작 후 저장된 원본을 재생',
  })[action] ?? action;
}
