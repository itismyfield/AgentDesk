export const title = 'Reader A: follower 결과와 boot 준비';
export const state = v => {
  const r=v.review;
  if (!r) return '미시작';
  if (!r.row && !r.preserved) return '준비 없이 row 삭제: ticket 복구 불가';
  if (r.phase==='Registered') return 'None follower 결과 대기: parent 보존';
  if (r.phase==='Done') return r.head ? '결과 1회·옛 row 정리·head 주입' : 'follower 결과 1회·parent 보존';
  if (r.phase==='Reaped') return 'custody 보존 후 옛 row 정리';
  if (r.prepared) return r.failed ? '준비 실패: row 보존' : 'ticket·결과 custody 준비됨';
  return 'boot 준비 전: 옛 row 보존';
};
const labels={ReviewFollowerCommit:'전체 ticket CAS: None도 허용',ReviewPrepare:'기존 prepare 포트: ticket·결과 보존',ReviewReap:'custody 뒤 reaper',ReviewRestore:'기존 restore·owner wake'};
export const step = (action, from) => action === 'ReviewReap' && !from.review.preserved ? '준비·custody 없이 reaper' : labels[action] ?? null;
