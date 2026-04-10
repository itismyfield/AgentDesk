# Meeting Domain Expert Orchestration

This document records the runtime contract for dashboard-started round table meetings.

## Participant Cap

- Default meeting participant cap is `5`.
- Runtime config may set `meeting.max_participants` / `meeting.maxParticipants`.
- Values are clamped to `2..=5` so operator sessions cannot accidentally create an unbounded meeting.
- `primary_provider` and `reviewer_provider` are operators, not participants, and do not count toward this cap.

## Candidate Registry

- `MeetingConfig.available_agents` is the canonical specialist candidate pool.
- `org.yaml` path: a non-empty `meeting.available_agents` list selects that subset from `agents`; an omitted or empty list falls back to all registered agents.
- `role_map.json` path: a non-empty object list under `meeting.available_agents` is used as meeting-specific specialist config; an omitted or empty/invalid list falls back to unique `byChannelId` / `byChannelName` role bindings.
- Provider-named specialist role IDs such as `qwen` and `gemini` remain valid candidates when registered as agents. Do not filter them out just because their role ID matches an operator provider name.

## Memory Post-Processing

- Meetings run with `meeting_readonly` specialist policy.
- During the meeting, `memory write` and `capture` are denied.
- After the meeting, the transcript and markdown record are saved, but automatic memory write/capture remains disabled.
- Post-meeting memory ingestion must be an explicit approval flow: an operator reviews the saved transcript, chooses what to store, then triggers a separate memory write/capture action.

## Provider And Model Extension Point

- Current dashboard flow selects operator providers: `primary_provider` drafts/finalizes and `reviewer_provider` critiques.
- Specialist execution should read provider/model from agent registry or meeting-specific override when supported.
- Future per-specialist `provider | model` picker should update the registry override layer, not alter transcript speaker identity or participant counting.
- Codex specialist CLI prompts must be passed after `--` so prompts that start with option-like text are not parsed as flags.

## Specialist Metadata Template

Use this shape when adding or improving meeting specialists:

```yaml
agents:
  qwen:
    display_name: "Qwen Specialist"
    keywords: ["analysis", "reasoning"]
    domain_summary: "Long-context reasoning and alternative-solution analysis."
    strengths:
      - "deep comparison"
      - "edge-case analysis"
    task_types:
      - "architecture review"
      - "research synthesis"
    anti_signals:
      - "short status notification"
      - "pure UI copy tweak"
    provider_hint: "qwen"
```

Selection prompts serialize `keywords`, `domain_summary`, `strengths`, `task_types`, `anti_signals`, and `provider_hint` into candidate cards. Missing fields are marked as `metadata_missing` so low-quality candidate data stays observable without excluding legacy agents.
