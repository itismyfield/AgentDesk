import type { RoundTableMeeting, RoundTableMeetingChannelOption, SkillCatalogEntry } from "../types";
import { request, SLOW_MUTATION_TIMEOUT_MS } from "./httpClient";
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

// ── Round Table Meetings ──

export async function getRoundTableMeetings(): Promise<RoundTableMeeting[]> {
  const data = await request<{ meetings: RoundTableMeeting[] }>(
    "/api/round-table-meetings",
  );
  return data.meetings;
}

export async function getRoundTableMeeting(
  id: string,
): Promise<RoundTableMeeting> {
  const data = await request<{ meeting: RoundTableMeeting }>(
    `/api/round-table-meetings/${id}`,
  );
  return data.meeting;
}

export async function getRoundTableMeetingChannels(): Promise<
  RoundTableMeetingChannelOption[]
> {
  const data = await request<{ channels: RoundTableMeetingChannelOption[] }>(
    "/api/round-table-meetings/channels",
  );
  return data.channels;
}

export async function deleteRoundTableMeeting(
  id: string,
): Promise<{ ok: boolean }> {
  return request(`/api/round-table-meetings/${id}`, { method: "DELETE" });
}

export async function updateRoundTableMeetingIssueRepo(
  id: string,
  repo: string | null,
): Promise<{ ok: boolean; meeting: RoundTableMeeting }> {
  return request(`/api/round-table-meetings/${id}/issue-repo`, {
    method: "PATCH",
    body: JSON.stringify({ repo }),
  });
}

export interface RoundTableIssueCreationResponse {
  ok: boolean;
  skipped?: boolean;
  results: Array<{
    key: string;
    title: string;
    assignee: string;
    ok: boolean;
    discarded?: boolean;
    error?: string | null;
    issue_url?: string | null;
    attempted_at: number;
  }>;
  summary: {
    total: number;
    created: number;
    failed: number;
    discarded: number;
    pending: number;
    all_created: boolean;
    all_resolved: boolean;
  };
}

export async function createRoundTableIssues(
  id: string,
  repo?: string,
): Promise<RoundTableIssueCreationResponse> {
  return request(`/api/round-table-meetings/${id}/issues`, {
    method: "POST",
    body: JSON.stringify({ repo }),
  });
}

export async function discardRoundTableIssue(
  id: string,
  key: string,
): Promise<{
  ok: boolean;
  meeting: RoundTableMeeting;
  summary: RoundTableIssueCreationResponse["summary"];
}> {
  return request(`/api/round-table-meetings/${id}/issues/discard`, {
    method: "POST",
    body: JSON.stringify({ key }),
  });
}

export async function discardAllRoundTableIssues(id: string): Promise<{
  ok: boolean;
  meeting: RoundTableMeeting;
  summary: RoundTableIssueCreationResponse["summary"];
  results: RoundTableIssueCreationResponse["results"];
  skipped?: boolean;
}> {
  return request(`/api/round-table-meetings/${id}/issues/discard-all`, {
    method: "POST",
  });
}

export async function startRoundTableMeeting(
  agenda: string,
  channelId: string,
  primaryProvider: string,
  reviewerProvider: string,
  fixedParticipants: string[] = [],
): Promise<{ ok: boolean; message?: string }> {
  return request("/api/round-table-meetings/start", {
    method: "POST",
    body: JSON.stringify({
      agenda,
      channel_id: channelId,
      primary_provider: primaryProvider,
      reviewer_provider: reviewerProvider,
      fixed_participants: fixedParticipants,
    }),
  });
}

// ── Skill Catalog ──

export async function getSkillCatalog(): Promise<SkillCatalogEntry[]> {
  const data = await request<{ catalog: SkillCatalogEntry[] }>(
    "/api/skills/catalog",
  );
  return data.catalog;
}
