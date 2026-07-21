# Local agent worktree inventory

`routines/local-worktree-gc.js` is the canonical operator routine for issue #4684. Despite the retained script reference, it is report-only and performs no cleanup.

Each run inventories `.claude/worktrees/agent-*` candidates and reports the absolute candidate path, age, lock state, HEAD/ref, git worktree registration, and inspection state. Registered entries with missing directories and unregistered matching directories remain visible in the report.

Age, an unlocked state, path naming, git registration, ref/HEAD ancestry, cleanliness, and process visibility do not prove lifecycle ownership. Without a lifecycle owner record or owner-issued cleanup authorization, every candidate reports `positive_ownership_proof: false`; unknown, active, locked, dirty, unmerged, and registered worktrees are preserved. The structured result must report `destructive_actions: 0`.
