# Manual Intervention Recurrence

Use the recorder whenever an operator performs one of these manual recovery
actions:

- Clear a `.stuck-manual-*` marker: `--type marker-clear`
- Re-baseline a giant-file or watchdog baseline: `--type re-baseline`
- Force-restart dcserver: `--type force-restart`

Record the intervention immediately, with a concrete reason and the related
issue when one exists:

```bash
python scripts/intervention_log.py record \
  --type marker-clear \
  --note "relay-wedge hand recovery" \
  --node mac-mini \
  --issue 4206
```

The append-only store is `scripts/intervention_history.toml`. Counts are
monotonic per intervention type, so a force restart does not increment the
marker-clear recurrence count.

When a type's count exceeds the recurrence threshold, the recorder always
writes `logs/intervention-recurrence-<type>.draft.md` and prints a loud warning.
This draft promotes the pattern as a **판정 모델 재설계 후보**, following the
agentdesk-issue-pipeline §0 “첫 사고 때 판정 모델 재설계” principle.

Issue filing is default-off and remains a separate operator approval step. To
file the generated draft, repeat the recording command only for the actual new
intervention with the literal confirmation gate set:

```bash
AGENTDESK_INTERVENTION_CREATE_ISSUE=confirmed \
  python scripts/intervention_log.py record \
  --type marker-clear \
  --note "relay-wedge hand recovery recurred"
```

Any unset value or value other than literal `confirmed` retains the local draft
and never invokes `gh issue create`. Do not set confirmation merely to file an
older draft, because every `record` invocation appends a real intervention.

This v1 recorder is operator-invoked ops tooling. It does not hook deploy,
watchdog, or cross-node runtime authority automatically.
