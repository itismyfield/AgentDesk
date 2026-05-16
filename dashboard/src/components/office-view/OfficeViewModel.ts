import type { UiLanguage } from "../../i18n";
import type { Agent, RoundTableMeeting } from "../../types";

function inferDisplayNameLocal(roleId: string): string {
  if (roleId.startsWith("ch-")) return roleId.slice(3).toUpperCase();
  if (roleId.endsWith("-agent")) return roleId.replace(/-agent$/, "");
  return roleId;
}

function matchParticipantToAgentId(name: string, agents: Agent[]): string | null {
  const lower = name.toLowerCase();
  const abbrev = lower.replace(/\s*\(.*$/, "").trim();
  for (const agent of agents) {
    if (agent.role_id) {
      const displayName = inferDisplayNameLocal(agent.role_id).toLowerCase();
      if (displayName === lower || displayName === abbrev) return agent.id;
    }
    const nameEn = agent.name.toLowerCase();
    if (nameEn === lower || nameEn === abbrev) return agent.id;
    const nameKo = agent.name_ko?.toLowerCase();
    if (nameKo && (nameKo === lower || nameKo === abbrev)) return agent.id;
    const alias = agent.alias?.toLowerCase();
    if (alias && (alias === lower || alias === abbrev)) return agent.id;
  }
  return null;
}

export function computeMeetingPresence(
  meeting: RoundTableMeeting | null | undefined,
  agents: Agent[],
): Array<{ agent_id: string; until: number }> | undefined {
  if (!meeting || meeting.status !== "in_progress") return undefined;
  const names = meeting.participant_names ?? [];
  if (names.length === 0) return undefined;
  const until = Date.now() + 60 * 60 * 1000;
  const result: Array<{ agent_id: string; until: number }> = [];
  for (const name of names) {
    const agentId = matchParticipantToAgentId(name, agents);
    if (agentId) result.push({ agent_id: agentId, until });
  }
  return result.length > 0 ? result : undefined;
}

export function formatOfficeClock(language: UiLanguage): string {
  const locale = language === "ko" ? "ko-KR" : "en-US";
  return new Intl.DateTimeFormat(locale, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).format(new Date());
}
