export const title = 'Writer ready 전후 입력 보관과 동일 입력 재회수';
export const state = v => {
  if (v.bootHandler === 'retry_required') return '안내 실패 · 처리 미완료';
  if (v.bootExecutions === 1) return '동일 입력 1회 실행';
  if (v.bootHandler === 'done' && !v.bootCustody) return !v.bootNotice ? '안내 실패에도 처리 종료' : v.bootReason === 'transient' ? '일시 거절 안내 완료 · 실행 없음' : '거절 안내 완료 · 실행 없음';
  if (v.bootCustody) return !v.bootReady ? '입력 보관 완료 · writer 대기' : v.bootWake ? '보관 완료 · 실행 준비' : '보관 완료 · ready · wake 대기';
  if (v.bootHandler === 'unseen') return '입력 도착 전';
  if (v.bootHandler === 'classify') return '입력 처리 중 · 승인 판정 전';
  if (v.bootHandler === 'custody') return '입력 처리 중 · 보관 결과 대기';
  if (v.bootReason === 'transient') return 'writer 미준비 · 보관 없이 안내';
  return '거절 또는 보관 실패 · 안내 중';
};
const labels = {GatewayArrival:'Discord 입력 도착', GatewayClassify:'승인 조건과 writer 상태 판정', GatewayCustody:'동일 입력 ID로 보관 성공', GatewayCustodyFailure:'보관 실패 → 재시도 안내', NoticeFailure:'안내 전송 실패 → 처리 미완료', NoticeDelivered:'거절·실패 안내 도착', NoticeRetry:'안내 전송을 다시 시도', GatewayComplete:'접수 처리 종료와 커서 판정', WriterReady:'writer 복구 완료', ReadyWake:'ready 전환이 보관 입력 깨움', ExecuteSource:'보관한 동일 입력 1회 실행', CatchupSameSource:'catch-up이 동일 ID 재회수', BootRestart:'재시작 · 보관 유지 · 메모리 wake 소실', BootDone:null};
export const step = (action, from, to) => action === 'NoticeFailure' ? (to.bootHandler === 'retry_required' ? '안내 전송 실패 → 처리 미완료' : '안내 전송 실패를 삼키고 복귀') : Object.hasOwn(labels,action) ? labels[action] : action;
