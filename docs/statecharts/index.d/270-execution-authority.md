key: 270-execution-authority
| `execution-authority` (#7731) | `b66f40ba19` | main: one transient PG read failure (or an owner change) on a channel hosted under the gateway lease holds live input forever, because the refresh waits for a host that never retires | fixed by #7731 (`fix1`); the `nowait` mutant breaks the permit-host fence |
