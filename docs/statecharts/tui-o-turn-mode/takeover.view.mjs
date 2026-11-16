export const title = "Codex takeover 이후 입력 재개";
export const state = v => v.executions ? "대기 입력 전달됨" : v.confirmed ? "턴 모드 확정" : v.legacy ? "기존 턴 종료 대기" : v.original === "archived" ? "원본 압축 · 확정 대기" : v.binding === "advanced" ? "후속 원본 진행 · 확정 대기" : v.ended ? "원본 턴 종료 · 확정 대기" : "원본 종료 증거 대기";
export const step = a => ({Confirm:"턴 모드 확인",Deliver:"보관한 입력 전달",Archive:"종료 원본 압축",Advance:"후속 원본으로 이동",End:"원본 턴 종료",LegacyEnds:"기존 턴 정리",Restart:"재시작",Rest:null}[a] ?? null);
