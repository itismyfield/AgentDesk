export const title = "이름 이전 등록 트랜잭션: 유한 모델(기존 수명 전체 아님)";
export const state = (v) => {
  const n = v.msg && typeof v.msg === "object" ? v.msg : v.handoff;
  if (n.oldLive) return n.stage === "old" ? "옛 세션 실행 중" : (n.fresh ? "옛 세션 실행 중 새 세션 생성" : "새 이름 요청, 옛 실행 유지");
  if (n.stage === "requested") return n.absence === "absent" ? "옛 host 부재 확인" : "옛 host 종료, 부재 확인 대기";
  if (n.stage === "prepared") return ({idle:"새 실행 준비, 등록 전",snapshot:"비권한 snapshot 발견",locators:"old/new locator 정렬 잠금",identity:"identity 잠금",rows:"row/alias 잠금, CAS 재검증",updated:"트랜잭션 안 primary 갱신",aliased:"옛 alias 보존, commit 대기"})[n.tx];
  if (n.stage === "refused") return "증거/CAS 충돌, 부분 승격 없이 롤백";
  if (n.stage === "registered") return n.primary === "new" ? "새 primary와 옛 alias 원자 등록" : "새 실행인데 primary는 옛 이름";
  return n.kills > 1 ? "같은 실행을 두 번 종료" : (n.primary === "old" ? "늦은 옛 hook가 primary 덮어씀" : "새 실행 종료, 재요청은 무효");
};
const record = (v) => v.msg && typeof v.msg === "object" ? v.msg : v.handoff;
export const step = (_action, from, to) => {
  const a = record(from), b = record(to);
  if (a.tx !== b.tx && b.stage === "prepared") return ({snapshot:"old row/primary/nonce를 관측함",locators:"두 locator를 정렬해 잠금",identity:"그 뒤 identity를 잠금",rows:"row와 alias를 잠그고 snapshot 재검증",updated:"허용 actor CAS 뒤 새 primary로 UPDATE",aliased:"UPDATE 뒤 옛 key를 alias로 보존"})[b.tx] || "등록 증거 재확인";
  if (a.stage === "old" && b.stage === "requested") return "새 이름을 요청함";
  if (a.oldLive && !b.oldLive) return "옛 host가 자연 종료됨";
  if (b.stage === "prepared" && a.stage !== b.stage) return "부재와 nonce를 확인하고 successor를 준비함";
  if (b.stage === "registered" && a.stage !== b.stage) return b.primary === "new" ? "새 primary와 옛 alias를 원자 커밋함" : "승격 없이 새 실행만 등록함";
  if (b.stage === "refused" && a.stage !== b.stage) return "nonce 충돌로 자기 successor만 정리함";
  if (b.stage === "stopped" && a.stage !== b.stage) return "현재 primary가 가리키는 실행을 종료함";
  if (a.absence !== "absent" && b.absence === "absent") return "옛 host 부재를 확인함";
  if (!a.nonceContested && b.nonceContested) return "등록 전에 다른 실행 nonce가 나타남";
  if (!a.oldHookSeen && b.oldHookSeen) return b.staleRejected ? "옛 nonce hook를 거절함" : "늦은 옛 hook가 primary를 덮어씀";
  if (b.kills > a.kills) return "종료 효과가 다시 발생함";
  return "같은 실행의 증거를 재확인함";
};
