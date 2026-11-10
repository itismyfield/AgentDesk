export const title = 'Host ready와 Gateway 판정을 분리한 보관 입력 self-wake';
export const state = v => {
  if (v.routeBadWake) return 'Gateway Hold인데 self-wake 재예약';
  if (v.bootExecutions === 1) return '동일 입력 1회 실행';
  if (v.routeCheck) return v.routeGateway ? 'hold 커밋 뒤 Gateway 재확인' : 'hold 커밋 뒤 아직 Gateway Hold';
  if (!v.routeGateway) return v.bootWake ? 'host ready · Gateway Hold · 일회성 wake' : 'host ready · Gateway Hold · 보관 유지';
  return v.bootWake ? 'host ready · Gateway · 실행 준비' : 'host ready · Gateway · 기존 이벤트 대기';
};
const labels = {RouteOwnerHold:'Gateway Hold로 원본 보관',RouteSelfWakeRecheck:'hold 뒤 현재 ready와 Gateway 재확인',RouteGatewayOpens:'외부 source 판정이 Gateway로 복구',RouteExistingOwnerWake:'기존 owner 이벤트가 다시 깨움',RouteExecute:'동일 원본 1회 실행',RouteDone:null};
export const step = (action, from, to) => action === 'RouteSelfWakeRecheck' && to.routeBadWake ? 'host ready만 보고 self-wake 재예약' : Object.hasOwn(labels,action) ? labels[action] : action;
