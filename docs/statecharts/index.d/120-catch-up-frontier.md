key: 120-catch-up-frontier
| `catch-up-frontier` (#8045) | `5918c79bb3` | Main: a V2 channel's delivered inputs read as queued, so the phase-1 cursor never passes them; PendingPass (mutant): a waiting input passes and is lost | fixed passes settled-only ids and keeps queued, attempted and retained ids open; see [report](catch-up-frontier/catch-up-frontier.report.md) |
