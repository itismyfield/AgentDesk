key: 100-hosted-cleanup
| `hosted-cleanup` (#7353 S4-11) | `1d5f2657f0` + service ownership move | core Before/After preserve Legacy-only observed-marker protection and Known Retired deletion; NoRecheck breaks the existing row fence | bulk marker-only wait/alias and explicit activity gaps remain identical in BeforeGaps/AfterGaps; see [report](hosted-cleanup/hosted-cleanup.report.md) |
