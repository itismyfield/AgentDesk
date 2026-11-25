import { isAuditBindingMissing, type AuditOrigin, type AuditProvider, type PromptSection, type PromptSource } from "../../api/promptAudit";
import { isTestAgent } from "../../lib/agentHelpers";
import type { Agent } from "../../types";

type Tr = (ko: string, en: string) => string;
type BoundAgent = Pick<Agent, "id" | "cli_provider" | "discord_channel_id" | "discord_channel_alt" | "discord_channel_cc" | "discord_channel_cdx">;

const bound = (value: string | null | undefined) => Boolean(value?.trim());

/** Providers with a channel, read from the columns db/agents.rs resolves; a lone legacy channel belongs to the configured provider. */
export function auditProviders(agent: BoundAgent): AuditProvider[] {
  const list: AuditProvider[] = [];
  if (bound(agent.discord_channel_cc)) list.push("claude");
  if (bound(agent.discord_channel_cdx) || bound(agent.discord_channel_alt)) list.push("codex");
  if (list.length === 0 && bound(agent.discord_channel_id)) list.push(agent.cli_provider === "codex" ? "codex" : "claude");
  return list;
}

/** The agent's own provider when it has a channel, else the first bound one (an antigravity agent still audits its claude side). */
export function defaultAuditProvider(agent: BoundAgent): AuditProvider | null {
  const list = auditProviders(agent);
  return list.find((provider) => provider === agent.cli_provider) ?? list[0] ?? null;
}

/** E2E test harness agents (`e2e` in id or display name). */
export function isAuditTestAgent(agent: Pick<Agent, "id"> & { name?: string | null; name_ko?: string | null }): boolean {
  return isTestAgent(agent) || /e2e/i.test(agent.id) || /e2e/i.test(agent.name ?? "") || /e2e/i.test(agent.name_ko ?? "");
}

/** Agents with at least one channel binding; relay test harness agents go last (or are omitted when `includeTests` is false). */
export function auditAgents<T extends BoundAgent & { name?: string | null; name_ko?: string | null }>(agents: T[], includeTests = true): T[] {
  const listed = agents.filter((agent) => auditProviders(agent).length > 0);
  const real = listed.filter((agent) => !isAuditTestAgent(agent));
  return includeTests ? [...real, ...listed.filter(isAuditTestAgent)] : real;
}

export function matchesAgent(agent: Pick<Agent, "id" | "name" | "name_ko" | "alias">, query: string): boolean {
  const needle = query.trim().toLowerCase();
  return !needle || [agent.id, agent.name, agent.name_ko, agent.alias].some((value) => value?.toLowerCase().includes(needle));
}

export type AuditState = "loading" | "ready" | "unavailable" | "unbound" | "error";

/** Screen state of one audit read: a failed last read wins over older data, and a missing binding is not an error. */
export function auditState(error: unknown, origin: AuditOrigin | undefined): AuditState {
  if (error) return isAuditBindingMissing(error) ? "unbound" : "error";
  if (!origin) return "loading";
  return origin === "unavailable" ? "unavailable" : "ready";
}

const SECTION_LABELS: Record<string, [string, string]> = {
  role_prompt: ["역할 정의", "Role definition"],
  shared_agent_rules: ["공유 규칙", "Shared rules"],
  shared_knowledge: ["공유 지식", "Shared knowledge"],
  memento_anchors: ["메멘토 앵커", "Memento anchors"],
  queued_turn_rules: ["대기 턴 규칙", "Queued turn rules"],
  tool_output_efficiency: ["도구 출력 규칙", "Tool output rules"],
  context_compression: ["맥락 압축 규칙", "Context compression"],
  memory_guidance: ["기억 사용 안내", "Memory guidance"],
  api_friction_guidance: ["ADK API 안내", "ADK API guidance"],
  role_binding_contract: ["채널 역할 연결", "Channel role binding"],
  runtime: ["런타임 문맥", "Runtime context"],
};

/** A label per section in order; a label that repeats (the server sends `runtime` more than once) gets a number. */
export function sectionLabels(sections: Pick<PromptSection, "name">[], tr: Tr): string[] {
  const labels = sections.map(({ name }) => (SECTION_LABELS[name] ? tr(...SECTION_LABELS[name]) : name));
  const seen = new Map<string, number>();
  return labels.map((label) => {
    if (labels.indexOf(label) === labels.lastIndexOf(label)) return label;
    const n = (seen.get(label) ?? 0) + 1;
    seen.set(label, n);
    return `${label} ${n}`;
  });
}

export const PROMPT_SOURCES: PromptSource[] = ["canon_file", "memento", "runtime", "conditional"];

const SOURCE_LABELS: Record<PromptSource, [string, string]> = {
  canon_file: ["정본 파일", "Canon file"],
  memento: ["메멘토", "Memento"],
  runtime: ["런타임", "Runtime"],
  conditional: ["조건부", "Conditional"],
};

export const sourceLabel = (source: PromptSource, tr: Tr) => tr(...SOURCE_LABELS[source]);

/** Characters per source in a fixed order, with whole percentages of the prompt. */
export function sourceShares(sections: Pick<PromptSection, "source" | "chars">[]) {
  const total = sections.reduce((sum, section) => sum + section.chars, 0);
  return PROMPT_SOURCES.map((source) => {
    const chars = sections.reduce((sum, section) => (section.source === source ? sum + section.chars : sum), 0);
    return { source, chars, percent: total ? Math.round((chars / total) * 100) : 0 };
  });
}

/** Section texts in order; a blank line goes in only where two sections would otherwise run together. */
export function fullPromptText(sections: Pick<PromptSection, "text">[]): string {
  return sections.reduce((all, { text }) => (!all || all.endsWith("\n") || text.startsWith("\n") ? all + text : `${all}\n\n${text}`), "");
}
