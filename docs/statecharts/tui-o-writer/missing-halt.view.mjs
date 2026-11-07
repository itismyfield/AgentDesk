export const title = '같은 원본 소실의 재부팅 억제와 새 증거에 의한 재개';
export const state = v => {
  const label = {booting:'원본 재열기',halted:'원본 소실로 중단',held:'중단·claim 유지',ended:'종료·claim 반환',running:'복구·입력 승인'}[v.phase];
  return `${label} · 실행 ${v.boots} · 증거 ${v.evidence}${v.otherPaneLive ? ' · 다른 pane의 provider는 살아 있음' : ''}`;
};
export const step = action => ({
  Halt:'정확한 원본이 Missing',Run:'원본 검증·정상 resume',Hold:'actor 종료 확인·기존 claim 유지',
  End:'기존 동작: claim 반환',Boot:'owner cleanup이 다시 host',Wake:'새 증거 1회 소비·같은 claim으로 재개',
  NewEvidence:'정확한 원본 복귀 또는 분류·nonce·binding 변화',OtherPane:'다른 pane만 살아 있음·증거 불변',
  ReadFailure:"재개 직전 관측 읽기 실패·claim 유지",ReadRecovery:"관측 복구·같은 증거로 한 번 시작",Retire:'실행 권한 철회·stop 전달',Rest:null,
}[action] ?? (action === 'Rest' ? null : action));
