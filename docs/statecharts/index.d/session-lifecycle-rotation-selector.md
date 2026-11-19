key: session-lifecycle-rotation-selector
| `session-lifecycle` rotation selector (#8261) | `4f1fe5236d` → `864b80ff98` | Before: turn starts before settle and resumes the old session, newer selector overwritten, failed store write never retried | Fixed GREEN; restart during a store outage before the retry stays a known gap (expected RED). [Report](session-lifecycle/rotation-selector.report.md) |
