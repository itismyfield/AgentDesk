key: 220-host-migration
| `host-migration` (tmux ↔ Herdr, #7503) | `db99888121` (branch `feat/i7503-claude-resume-both`) | old tmux Claude left running after a fresh forward launch (C1), which then blocks every Return turn (C2); id-only channel's queued input never starts Return (C3) | C1 already in main; C2 new, not raised by #7503 reviews; C3 is review B2 P1-1 (repair B-2) |
