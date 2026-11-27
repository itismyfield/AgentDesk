key: writer-idle
| `tui-o-writer` idle polling (#8385) | `b5609d74b3` + idle backoff | Fixed polling delays hook, input and transition command wakes. Removing the maximum timer can strand a transcript or spool change. | The 1/5/15-second model wakes on published work and eventually processes every modeled event. See [report](tui-o-writer/writer-idle.report.md). |
