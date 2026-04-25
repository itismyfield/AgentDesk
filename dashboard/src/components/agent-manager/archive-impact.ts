import type { Agent } from "../../types";

export interface ArchiveChannelImpact {
  /** Discord channel id (snowflake or env-style placeholder). */
  id: string;
  /** Channel role within the agent binding. */
  role: "primary" | "alt" | "codex";
}

/**
 * Resolve the discord channels that will be touched when archiving an agent.
 *
 * Mirrors the server-side `AgentChannelBindings::all_channels()` logic:
 * de-duplicates while preserving role priority (primary → alt → codex).
 */
export function resolveArchiveChannelImpact(
  agent: Pick<
    Agent,
    "discord_channel_id" | "discord_channel_id_alt" | "discord_channel_id_codex"
  >,
): ArchiveChannelImpact[] {
  const slots: Array<{ id?: string | null; role: ArchiveChannelImpact["role"] }> = [
    { id: agent.discord_channel_id, role: "primary" },
    { id: agent.discord_channel_id_alt, role: "alt" },
    { id: agent.discord_channel_id_codex, role: "codex" },
  ];

  const seen = new Set<string>();
  const impacts: ArchiveChannelImpact[] = [];
  for (const slot of slots) {
    const normalized = (slot.id ?? "").trim();
    if (!normalized) continue;
    if (seen.has(normalized)) continue;
    seen.add(normalized);
    impacts.push({ id: normalized, role: slot.role });
  }
  return impacts;
}

/**
 * Whether archiving the agent would block on an active turn.
 * Mirrors the API's 409 path so the UI can warn before the confirm.
 */
export function archiveBlockedByActiveTurn(
  agent: Pick<Agent, "status">,
): boolean {
  return agent.status === "working";
}
