import type { SkillCatalogEntry } from "../types";
import {
  request,
  readCachedSnapshot,
  SLOW_MUTATION_TIMEOUT_MS,
  type CachedApiSnapshot,
  type RequestOptions,
} from "./httpClient";
import type { GitHubIssuesResponse } from "./analytics";

export interface ChatMessage {
  id: number;
  sender_type: "ceo" | "agent" | "system";
  sender_id: string | null;
  receiver_type: "agent" | "department" | "all";
  receiver_id: string | null;
  receiver_name?: string | null;
  receiver_name_ko?: string | null;
  content: string;
  message_type: string;
  /** ISO 8601 timestamp from the server (`created_at::TEXT`). */
  created_at: string;
  sender_name?: string | null;
  sender_name_ko?: string | null;
  sender_avatar?: string | null;
}

export async function getMessages(opts?: {
  receiverId?: string;
  receiverType?: string;
  messageType?: string;
  limit?: number;
  /** ISO 8601 timestamp (matches server `before` TIMESTAMPTZ binding). */
  before?: string;
}): Promise<{ messages: ChatMessage[] }> {
  const params = new URLSearchParams();
  if (opts?.receiverId) params.set("receiverId", opts.receiverId);
  if (opts?.receiverType) params.set("receiverType", opts.receiverType);
  if (opts?.messageType && opts.messageType !== "all")
    params.set("messageType", opts.messageType);
  if (opts?.limit) params.set("limit", String(opts.limit));
  if (opts?.before) params.set("before", opts.before);
  const q = params.toString();
  return request(`/api/messages${q ? `?${q}` : ""}`);
}

export async function sendMessage(payload: {
  sender_type?: string;
  sender_id?: string | null;
  receiver_type: string;
  receiver_id?: string | null;
  discord_target?: string | null;
  content: string;
  message_type?: string;
}): Promise<ChatMessage> {
  return request("/api/messages", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
}

// ── GitHub Issues ──

export async function getGitHubIssues(
  repo?: string,
  state: "open" | "closed" | "all" = "open",
  limit = 20,
): Promise<GitHubIssuesResponse> {
  const params = new URLSearchParams({ state, limit: String(limit) });
  if (repo) params.set("repo", repo);
  return request(`/api/github-issues?${params}`);
}

export async function closeGitHubIssue(
  repo: string,
  issueNumber: number,
): Promise<{ ok: boolean; repo: string; number: number }> {
  const [owner, repoName] = repo.split("/");
  return request(
    `/api/github-issues/${owner}/${repoName}/${issueNumber}/close`,
    {
      method: "PATCH",
      // #2050 P3 finding 15 — GitHub close round-trip exceeds 15s under
      // rate-limit pressure; treat as slow mutation to avoid double-close.
      timeoutMs: SLOW_MUTATION_TIMEOUT_MS,
    },
  );
}

// ── Skill Catalog ──

const SKILL_CATALOG_TIMEOUT_MS = 60_000;

export async function getSkillCatalog(
  opts?: RequestOptions,
): Promise<SkillCatalogEntry[]> {
  const data = await request<{ catalog: SkillCatalogEntry[] }>(
    "/api/skills/catalog",
    {
      timeoutMs: SKILL_CATALOG_TIMEOUT_MS,
      suppressErrorToast: true,
      ...opts,
    },
  );
  return data.catalog;
}

export function getCachedSkillCatalog(): CachedApiSnapshot<SkillCatalogEntry[]> | null {
  const cached = readCachedSnapshot<{ catalog: SkillCatalogEntry[] }>(
    "/api/skills/catalog",
  );
  return cached
    ? { data: cached.data.catalog, fetchedAt: cached.fetchedAt }
    : null;
}
