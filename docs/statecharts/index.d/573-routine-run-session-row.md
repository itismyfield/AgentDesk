key: 573-routine-run-session-row
| `routine-run` run session row (#8117 PR3b-2) | `8752805029e9dfded84791b0a9a79b6a22d511a3` | Before: 끝난 headless run 의 세션 행이 닫히지 않음(liveness RED); primary 행까지 닫는 mutant RED. thread 실패 경로는 구현이 기존 GREEN cfg 로 옴 | 기존 RoutineRun 갱신 24cfg: GREEN 8, 기대 RED 16. [모델 보고서](routine-run/routine-run.report.md) |
