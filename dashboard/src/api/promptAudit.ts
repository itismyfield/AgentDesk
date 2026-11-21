import { z } from "zod";
import { ApiRequestError, request, type Parser } from "./httpClient";

export type AuditProvider = "claude" | "codex";

const count = z.number().int().nonnegative();
const origin = z.enum(["current_session", "latest_saved", "unavailable"]);
const session = {
  agent_id: z.string(),
  provider: z.string(),
  session_id: z.string().nullish(),
  session_started_at: z.string().nullish(),
};

const promptSectionSchema = z.object({
  name: z.string(),
  source: z.enum(["canon_file", "memento", "runtime", "conditional"]),
  path: z.string().nullish(),
  chars: count,
  masked_count: count,
  text: z.string(),
});

const promptAuditSchema = z.object({ ...session, origin, masked_count: count, sections: z.array(promptSectionSchema) });

const anchorSchema = z.object({
  id: z.string().nullish(),
  type: z.string().nullish(),
  scope: z.string().nullish(),
  chars: count,
  limit: count.nullish(),
  text: z.string(),
  masked_count: count,
  overlap_candidates: z.array(z.discriminatedUnion("source", [
    z.object({ source: z.literal("anchor"), index: count }),
    z.object({ source: z.literal("canon_file"), path: z.string() }),
  ])),
  receiving_agents: z.array(z.string()),
});

const anchorsSchema = z.object({
  ...session,
  source: origin,
  memento: z.object({
    status: z.enum(["connected", "disconnected", "unknown", "unconfigured"]),
    checked_at: z.string().nullish(),
  }),
  anchor_count: count,
  anchors: z.array(anchorSchema),
});

export type AuditOrigin = z.infer<typeof origin>;
export type PromptSection = z.infer<typeof promptSectionSchema>;
export type PromptSource = PromptSection["source"];
export type PromptAudit = z.infer<typeof promptAuditSchema>;
export type AgentAnchor = z.infer<typeof anchorSchema>;
export type AgentAnchors = z.infer<typeof anchorsSchema>;

// The screen draws its own error states, so the global toast stays quiet; the server sends no-store.
function auditGet<T>(agentId: string, path: string, provider: AuditProvider, parser: Parser<T>, signal?: AbortSignal): Promise<T> {
  const url = `/api/agents/${encodeURIComponent(agentId)}/${path}?provider=${provider}`;
  return request(url, { signal, cache: "no-store", suppressErrorToast: true }, parser);
}

export const getPromptAudit = (agentId: string, provider: AuditProvider, signal?: AbortSignal) =>
  auditGet(agentId, "prompt-audit", provider, promptAuditSchema, signal);

export const getAgentAnchors = (agentId: string, provider: AuditProvider, signal?: AbortSignal) =>
  auditGet(agentId, "anchors", provider, anchorsSchema, signal);

/** The server answers an agent or provider without a usable channel with a 4xx `config` error, not only 404. */
export function isAuditBindingMissing(error: unknown): boolean {
  return error instanceof ApiRequestError && error.serverCode === "config" && error.status >= 400 && error.status < 500 && error.status !== 401;
}
