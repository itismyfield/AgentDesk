key: 030-intake-readiness
| `intake-readiness` (#7353 S4-05) | `b20a366c8812348f14a9e08a7439cb8df6cfb09a` | StaleSnapshot: lock 전 profile/cache 재사용; EarlyTxEnd: 다른 transaction으로 적용 | Current/Proposed GREEN; 같은 lock→live 관측→write/commit 유지, modern unknown/offline fail-closed·legacy 호환 보존. [보고서](intake-readiness/intake-readiness.report.md) |
