key: 250-input-single-path-stop-before-first-key
| `input-single-path` stop before first key (#7754) | `e85948c2c3` + branch `feat/i7754-stop-gate` | without the writer's NoKeyYet gate, or with `Cancelled{0}` held as NotSent, an input a stop took before its first key still reaches Claude | the gate config holds every rule; the two mutant configs document the guarded gaps |
