export const title = '출력 writer의 유휴 대기';
export function state({idle}) {
  if (idle.pending) return `${idle.event} 처리 대기 (${idle.age}초)`;
  if (idle.event !== 'none') return `${idle.event} 처리됨`;
  return `${idle.delay}초 대기 (${idle.left}초 남음)`;
}
export const step = action => ({
  IdleArrive: '변화 발생',
  IdleClock: '1초 경과',
  IdlePoll: 'writer 처리',
  IdleSkip: '스탬프 일치, 본문 단락',
})[action] ?? action;
