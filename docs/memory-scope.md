# Memento Memory Scope Contract

Memento memories have three audiences: the current agent, the agent's group,
and everyone. This contract governs explicit MCP `context` / `recall` /
`remember` calls and AgentDesk's automatic Memento paths. It is referenced by
shared agent prompts and [`source-of-truth.md`](source-of-truth.md).

## Three tiers

An optional `agents[].memory_group` selects the group's workspace. For example,
`ch-td` with `memory_group: cookingheart` uses the following parameters:

| Operation / audience | workspace | agentId |
| --- | --- | --- |
| Read (private + group + global) | `cookingheart` | `ch-td` |
| Default write (private) | `cookingheart` | `ch-td` |
| Group shared write | `cookingheart` | `default` |
| Global write | omit the parameter | `default` |

The runtime scope hint spells these out as `read`, `default write (private)`,
`group shared`, and `global` (omit workspace). An agent with a group receives
no separate `agentdesk-<cwd>` agent-private workspace instruction.

The server's workspace filter reads the requested workspace plus global
(`NULL`) records. Its agentId filter reads the requested agent plus `default`.
Together these include all three tiers. Write private memories by default;
use `agentId=default` only for knowledge intended for the group or everyone.
Never put agent-private knowledge into a global record.

`agentId` is the agent's role id, not its display name or provider. Unbound
channels retain their existing `agentdesk-channel-<channel_id>` identity.
A missing, empty, or whitespace-only `memory_group` keeps the legacy cwd-based
project/agent-private scope hint and automatic workspace mapping unchanged.
Nonempty group values are trimmed and used as workspace names without a cwd
prefix. A per-agent group applies across its provider channels and routine turns.

Explicit automatic-path workspace overrides retain their existing priority.
Explicit remember workspace/global parameters keep their existing meaning.
For channel-derived automatic remember requests with a group, an omitted agentId
is filled from the channel role so the default remains private. Without a group,
the existing omitted-agentId payload is preserved; explicit agentIds are retained.
Session anchor retrieval uses the group's workspace and `agentId=default`,
which selects group and global shared anchors. Without a group it retains the
existing git common-directory workspace and session cache behavior.
Anchor cache keys include the group so changing groups within a session fetches
that group’s own shared anchors instead of reusing the previous group’s snapshot.

## Choosing an audience

- **Private (default):** confirmed preferences or facts relevant to this agent,
  decision rationale, and reusable lessons not yet intended for other agents.
- **Group shared:** confirmed project knowledge or procedures needed by the
  group's agents. For example, one CookingHeart agent can share a lesson with
  the other CookingHeart agents using `cookingheart` / `default`.
- **Global:** confirmed knowledge that every agent should know, such as a
  user-wide communication preference. Omit workspace and use `default`.

Audience is independent of durability. The older `permanent` versus
`workspace` / `session` wording describes lifetime or record classification;
it does not select private, group, or global visibility. Durable knowledge can
remain private. Sharing or widening visibility requires an explicit decision,
not automatic promotion based on age or repeated recall.

Store only confirmed, previously unstored knowledge. Amend existing facts for
corrections, and store nothing when there is no new knowledge. Hypotheses and
active issue progress belong in task/session records until confirmed; they are
not globally shared memory. Raw AgentDesk turns and transcript-tail summaries
are session records: automatic capture/reflect must not persist them as
semantic memory. Only explicit, atomic model-authored writes enter Memento.

## Canonical files stay authoritative

Do not copy canonical configuration or policy text into Memento at any tier.
Re-read the authoritative file instead. This includes:

| Content | Authoritative surface |
| --- | --- |
| Shared rules and per-agent identity | Shared and per-agent prompt files |
| Runtime, Discord, database and MCP declarations | Runtime YAML and settings surfaces in [`config-domains.md`](config-domains.md) |
| Policy hooks and pipeline | `policies/` |
| Skill instructions | The skill's `SKILL.md` |
| SAM / SAK / long-term file memory | The memory tiers in [`source-of-truth.md`](source-of-truth.md) |
| Operational paths and ownership | [`source-of-truth.md`](source-of-truth.md) |

Treat canonical files as authoritative when a memory duplicates them. Broad
cleanup, audience changes, and migration of existing memories require review
of the affected records; this contract does not authorize automatic deletion
or redistribution. The maintenance `memory.memento_consolidation` job performs
workspace deduplication and low-importance cleanup, not audience promotion.

## PostgreSQL fallback

This contract describes MCP-backed Memento. The PostgreSQL `local_memory`
fallback stores route-level rows with workspace metadata; it does not implement
Memento's agentId visibility filters or its permanent/promotion/amend semantics.
Audit fallback rows and MCP-backed memories separately as described in
[`source-of-truth.md`](source-of-truth.md).
