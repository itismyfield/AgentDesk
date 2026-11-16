key: 340-input-single-path-duplicate-intake
| `input-single-path` duplicate intake (#7857) | `0bf73d0e2f` + repair | Before: live/catch-up retry after delivery warns because the receipt survives its original | Fixed uses the existing committed Discord ticket; both arrival orders and restart preserve one delivery per message; see [report](input-single-path/duplicate-intake.report.md). |
