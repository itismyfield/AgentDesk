# Herdr clear record boundary fixtures

The clear tests use a private runtime root, a scripted host and a throwaway PostgreSQL database.

| Case | Evidence before clear | Required outcome |
| --- | --- | --- |
| `a_clear_ends_a_running_tmux_session_and_forgets_its_provider_session_pg` | Claude and Codex each have a running process, an old selector and no runtime source binding. | The process and tmux end, the queue is discarded, both selectors are forgotten and the record stores stay empty. |
| `late_source_bind_after_clear_cannot_revive_session_pg` | Herdr Codex is Pending before its first source bind. | Exactly the old pane closes, the same nonce retires, the selectors are cleared and a late old hook cannot bind it. |
| `owner_cleanup_herdr_postlude_archives_before_fresh_pg` | The Herdr owner's terminal delivery has an exact receipt and no native source. | Exactly its pane closes and its completed sidecar archives. The next owner is admitted fresh and submits input once. |
| `codex_herdr_clear_with_exact_source_exposes_record_gap_pg` | The old nonce is Bound and its strict binding resolves to the original rollout identity. | The same old pane closes once and both selectors clear. The next launch is fresh with a distinct nonce. The current record store has zero Ends. |
| `codex_herdr_clear_with_lost_source_exposes_warning_gap_pg` | The same Bound execution had strict source evidence before its rollout was moved away. | Exact close and the new fresh launch still succeed. The current clear reply has no warning and the record store remains empty. |

In E, the exact-source fixture should require one durable End for the old nonce. The lost-source fixture should require a recording warning and retain the zero-End expectation. Change those expectations before connecting the record path so their failures occur after the close, selector and fresh assertions. The fresh launch here ends before the successor's first native bind, where E's separate seal evidence will apply.
