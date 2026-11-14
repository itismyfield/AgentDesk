export const title="인스턴스 FD 상속과 첫 효과 전 검증";
const rs=["A","B"];
export function state(v) {
 const holders=rs.filter(r=>v.parentFd[r]||v.childFd[r]||v.helperFd[r]), live=rs.filter(r=>v.gateway[r]);
 const broken=[];
 if(live.length+(v.duplicate!=="none"&&v.gateway[v.duplicate]?1:0)>1) broken.push("Gateway 중복");
 if(holders.some(r=>v.owner!==r)) broken.push("보유 FD 소유권 상실");
 if(live.some(r=>!v.childFd[r]||!v.identity[r]||!v.witness[r]||v.owner!==r||!v.cloexec[r])) broken.push("검증 없는 효과");
 if(v.foreignCleanup) broken.push("외부 소유 정리");
 if(v.lateSpawn) broken.push("기한 뒤 생성");
 if(broken.length) return broken.join(" / ")+" · "+rs.map(r=>`${v.phase[r]}:${v.childFd[r]?"상속FD":""}${v.gateway[r]?"Gateway":""}`).sort().join(" + ");
 const running=rs.find(r=>v.phase[r]==="Admitted"||v.phase[r]==="Stopping"), inherited=rs.find(r=>v.phase[r]==="Inherited"), held=rs.find(r=>v.phase[r]==="Held");
 const who=running??inherited??held;
 if(who) {
  const names={Held:"부모가 임대 보유",Inherited:"FD 상속 뒤 검증 대기",Admitted:"검증과 CLOEXEC 복구 뒤 효과 가능",Stopping:v.gateway[who]?"기한 종료 뒤 효과 가능 및 자식 정리 대기":"기한 종료 뒤 검증 또는 자식 종료 대기"};
  return names[v.phase[who]]+(["Admitted","Stopping"].includes(v.phase[who])?"":(v.parentFd[who]?" · 부모 FD 유지":" · 자식 FD만 유지"));
 }
 if(rs.some(r=>v.helperFd[r])&&!rs.some(r=>v.parentFd[r]||v.childFd[r])) return "금지된 helper만 FD 보유";
 if(holders.length) return "자식 종료 뒤 부모 FD 유지";
 return rs.some(r=>v.phase[r]==="Released")?"요청 종료 및 보유 FD 없음":"두 요청 대기";
}
export function step(a) {
 const names={ObserveFree:"빈 잠금 관측",Acquire:"독점 임대 획득",Spawn:"자식 exec와 FD 상속",Admit:"FD와 witness 검증 후 CLOEXEC 복구",Reject:"검증 실패로 자식 종료",Tick:"시간 경과",Timeout:"기한 도달로 정리 시작",ParentExit:"부모 FD 닫기",ChildExit:"자기 자식 종료 및 FD 닫기",ForeignCleanup:"외부 자원 정리",Wait:"변화 대기"};
 for(const [n,label] of Object.entries(names))if(a.startsWith(n))return label;
 return a;
}
