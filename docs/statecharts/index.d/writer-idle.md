key: writer-idle
| `tui-o-writer` idle polling (#8385) | `e47058ab2d` | A stamp match can strand a restart notice. Quiet retired readers can keep the full path active. | The 1-second observation keeps pending work and reader deadlines independent of the stamp. See [report](tui-o-writer/writer-idle.report.md). |
