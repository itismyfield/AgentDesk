export const title = "Hosted cleanup: 관측과 조건부 삭제";
export const state = v => {
 if (v.deleted) return `삭제됨 (${v.route}, 관측 ${v.seen}, 현재 ${v.markerAtDelete})`;
 if (v.phase === "done") return v.failed ? "rollback 후 보존" : "후보 보존";
 return ({start:"다음 정리 대기", candidate:"DB 후보 읽음", waiting:"다른 row lock 대기", postwait:"lock 종료 후 재관측", ready:"삭제 판정 완료", applied:"transaction 조건부 적용"})[v.phase] + (v.rowChanged ? " · row 변경" : "") + (v.row.alias !== v.snapshot.alias ? " · alias 추가" : "");
};
export const step = a => ({ReadCandidate:"DB 후보 snapshot",ObserveMarker:"host marker 관측",WriteMarkerDuringWait:"lock 보유자가 marker 기록",NewTurnOrRespawn:"새 turn 또는 재spawn",AddAlias:"다른 host marker가 있는 alias 추가",ReleasePriorLock:"먼저 잡은 row lock 해제",ObserveAfterWait:"row와 marker 재관측",ConditionalApply:"조건부 재검증 및 삭제",Commit:"transaction commit",Rollback:"오류로 transaction rollback",Restart:"재시작 후 후보 다시 읽기",Retry:"다음 정리 호출",Done:"보존 또는 완료"})[a] ?? a;
