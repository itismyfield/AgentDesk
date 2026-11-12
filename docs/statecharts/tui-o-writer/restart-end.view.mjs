export const title = '같은 source의 exact End witness와 준비된 답';
function label(r) {
  if (r.witnessEver && !r.witness) return r.sealTried && !r.installedAtSeal ? 'durable witness 유실 뒤 witness 없는 seal' : 'durable witness가 crash에서 유실됨';
  if (r.effects > 1) return '같은 답이 두 번 게시됨';
  if (r.newOpened && r.open !== 2) return '옛 End가 새 Open을 닫음';
  if (r.applied && r.scope !== 'exact') return '다른 nonce 또는 cutoff를 닫음';
  if (r.absorbed) return r.sealTried && !r.installedAtSeal ? '새 prompt 흡수 뒤 witness 없이 seal' : '새 prompt가 옛 Open에 흡수됨';
  if (r.preparedEver && !r.acked && !r.prepared) return '재시작이 준비된 답을 잃음';
  if (r.stage === 'boot') return r.effects === 1 && !r.acked ? 'writer 재시작, POST 확인 미완료' : 'writer 재시작, durable 기록 유지';
  if (r.stage === 'held') return 'witness를 확인하지 못해 보류';
  if (r.confirmed === 'posted' && !r.acked) return '기존 POST 발견, Posted receipt 전';
  if (r.receipt === 'missing') return '부재 확인 receipt, 새 Prepared 전';
  if (r.freshPrepared) return 'durable Prepared receipt, POST 허가';
  if (r.effects === 1 && !r.acked) return '기존 Prepared의 POST 확인 대기';
  if (r.stage === 'start') return r.prepared ? '기존 Prepared, 결과 미확인' : '옛 Open, native closer 없음';
  if (r.stage === 'install') return '물리 종료 확인, witness 읽기 전';
  if (r.stage === 'old') return r.installed ? 'witness 설치 후 옛 opener 읽기' : 'witness 없이 옛 opener 읽기';
  if (r.stage === 'boundary') return 'old cutoff 앞, 새 prompt 읽기 전';
  if (r.stage === 'new') return r.oldIdle ? '옛 Open만 Idle, 다음 record 읽기' : '옛 Open이 아직 닫히지 않음';
  if (r.stage === 'seal') return r.newOpened ? '새 Open 유지, old prefix seal 전' : 'old prefix seal 전';
  if (r.acked) return '기존 게시 확인, 중복 없음';
  if (r.prepared) return 'durable Prepared, 게시 대기';
  return '옛 Open이 Unknown에 남음';
}
export function state(v) {
  const r = v.restart;
  const end = r.stage === 'held' || (r.stage === 'ready' && !r.oldIdle && !r.prepared);
  return label(r) + (end ? r.restarted ? ' [재시작 기회 소진]' : ' [재시작 가능]' : '');
}
export function step(action, from, to) {
  if (action === 'RestartInstall') return to.restart.stage === 'held' ? 'historical scope를 검증하지 못해 보류' : to.restart.installed ? 'historical binding과 prefix 검증 후 witness 설치' : 'witness 설치 없이 source 재생';
  if (action === 'RestartBoundary') return to.restart.nativeRead ? '실제 native Closed record 처리' : to.restart.applied ? 'cutoff에서 old opener만 reset' : 'closer 없이 old Open 유지';
  if (action === 'RestartPost') return from.restart.freshPrepared ? '새 Prepared receipt의 허가로 POST 시작' : 'confirm 허가 없이 기존 Prepared 재게시';
  if (action === 'RestartConfirm') return to.restart.confirmed === 'posted' ? '정확한 기존 POST를 history에서 확인' : '읽기 가능한 history로 부재 확인';
  return ({RestartPhysicalEnd:'old incarnation의 실제 종료 기록',RestartReadOld:'old opener 재생',RestartReadNew:'cutoff 뒤 새 prompt 처리',RestartSeal:'old prefix seal, 기존 ledger debt 보존',RestartLateInstall:'seal 뒤 늦게 witness 설치',RestartReplayOld:'새 Open 뒤 old End 재관측',RestartHistoryUnavailable:'history 미확인, POST 허가 없음',RestartRecordMissing:'확인된 부재를 ledger에 기록',RestartPrepareRetry:'같은 unit의 새 Prepared receipt 기록',RestartRecordPosted:'POST 결과를 durable Posted receipt로 기록',RestartCrash:'writer process가 중단됨 [이전 재시작 없음]',RestartBoot:'같은 source와 ledger를 다시 읽음',RestartRest:null})[action] ?? action;
}
export const group = v => v.restart.stage === 'boot' ? 'boot' : v.restart.acked ? 'done' : 'work';
export const colors = {boot:'yellow',done:'teal',work:'blue'};
