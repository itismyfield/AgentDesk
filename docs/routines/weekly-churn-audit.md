# Weekly regression-churn audit

Issue #4265 adds the offline `routines/monitoring/weekly_churn_audit.py` cron entry point. It reads
the local repository's `git log --since='7 days'`, recognizes conventional `fix:` and
`fix(scope):` subjects (including the optional breaking-change `!` marker), and counts each fix
commit once per changed file and once per containing module directory. A file with at least the
configured threshold (default `3`) is reported as a `재설계 후보`.

Genuine issue references such as a leading `#4262` and references in the commit body are parsed in
text order. Terminal squash-merge PR suffixes such as `(#4511) (#4523)` are removed from the
subject before parsing, so they never become regression generations. Adjacent remaining references
form `#A→#B` edges; edges that meet across commits are joined, and the longest chain in each
connected lineage is reported with its generation count. Lineage path exploration is bounded and
logs a warning if a dense component must be truncated. The audit uses only commit text from every
commit in the window and the local git object database for this analysis; the file/module churn
tallies remain restricted to conventional fix commits.

## Weekly cron entry point

The default invocation is a dry run and prints the report to stdout:

```bash
ROOT="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"
python3 "$ROOT/routines/monitoring/weekly_churn_audit.py" \
  --repo-root "$ROOT" \
  --runtime-root "$ROOT"
```

For example, an operator-managed KST cron can run it at 09:20 every Monday:

```cron
20 9 * * 1 python3 "$HOME/.adk/release/routines/monitoring/weekly_churn_audit.py" --repo-root "$HOME/.adk/release" --runtime-root "$HOME/.adk/release"
```

Optional configuration:

- `AGENTDESK_CHURN_AUDIT_THRESHOLD`: positive candidate threshold, default `3`. Invalid values log
  a warning and fall back to the default.
- `AGENTDESK_CHURN_AUDIT_SINCE`: local git window, default `7 days`.
- `AGENTDESK_CHURN_AUDIT_REPO`: repository used only by confirmed GitHub dedup/creation, default
  `itismyfield/AgentDesk`.
- `AGENTDESK_CHURN_AUDIT_API`: local AgentDesk send endpoint, default
  `http://127.0.0.1:8791/api/discord/send`.
- `AGENTDESK_CHURN_AUDIT_CHANNEL_ID`: weekly operations channel or thread ID.

## Default-off side effects

Both side-effect paths require a literal, human-set confirmation:

- `AGENTDESK_CHURN_AUDIT_POST_CHANNEL=confirmed` posts the stdout report to
  `AGENTDESK_CHURN_AUDIT_CHANNEL_ID`. The default is `off`. A successful report fingerprint is
  stored under `runtime/weekly-churn-audit/post-state.json`, so rerunning an identical weekly report
  does not post it twice.
- `AGENTDESK_CHURN_AUDIT_CREATE_ISSUE=confirmed` enables the GitHub open-issue scan and pending
  draft emission. The default is `off`, so an ordinary run makes no `gh` or network call and writes
  no draft. If dedup is unavailable or truncated, draft emission fails closed.

Pending drafts use the stable writer shared with the daily log digest and live under
`runtime/pending-issue-drafts/weekly-churn-audit/`. Confirmation alone does not create an issue:
the operator must review a draft and add its adjacent `.approved` marker. Only a subsequent run
with both that marker and `AGENTDESK_CHURN_AUDIT_CREATE_ISSUE=confirmed` can invoke issue creation.
Created drafts include a stable per-file `churn-audit:candidate` marker; later runs use that exact
marker to suppress duplicates while the issue remains open. This is the same two-step human
approval boundary used by the sibling daily digest.
