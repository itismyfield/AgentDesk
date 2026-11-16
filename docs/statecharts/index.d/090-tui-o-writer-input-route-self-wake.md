key: 090-tui-o-writer-input-route-self-wake
| `tui-o-writer` input-route self-wake (#8055 P2) | `b1c74c75da` → `2d9fb9d663044929d9389647146067f24f706fe4` | host ready여도 Gateway Hold면 ready-only 재확인이 같은 원본의 self-wake를 다시 예약 | host ready와 Gateway를 함께 확인하는 cfg GREEN; 기존 owner 이벤트 공정성 아래 held 진행성 유지. [보고서](tui-o-writer/i8055-route-design.report.md) |
