import type { KanbanCard, KanbanRepoSource } from "../types";
import { request, SLOW_MUTATION_TIMEOUT_MS } from "./httpClient";

export async function getKanbanCards(filters?: {
  status?: string;
  repoId?: string;
  assigneeAgentId?: string;
}): Promise<KanbanCard[]> {
  const params = new URLSearchParams();
  if (filters?.status) params.set("status", filters.status);
  if (filters?.repoId) params.set("repo_id", filters.repoId);
  if (filters?.assigneeAgentId)
    params.set("assigned_agent_id", filters.assigneeAgentId);
  const q = params.toString();
  const data = await request<{ cards: KanbanCard[] }>(
    `/api/kanban-cards${q ? `?${q}` : ""}`,
  );
  return data.cards;
}

export async function createKanbanCard(
  card: Partial<KanbanCard> & { title: string; before_card_id?: string | null },
): Promise<KanbanCard> {
  return request("/api/kanban-cards", {
    method: "POST",
    body: JSON.stringify(card),
  });
}

export async function updateKanbanCard(
  id: string,
  patch: Partial<KanbanCard> & { before_card_id?: string | null },
): Promise<KanbanCard> {
  const res = await request<{ card: KanbanCard }>(`/api/kanban-cards/${id}`, {
    method: "PATCH",
    body: JSON.stringify(patch),
  });
  return res.card;
}

export async function deleteKanbanCard(id: string): Promise<void> {
  await request(`/api/kanban-cards/${id}`, { method: "DELETE" });
}

export interface KanbanDispatchMutationResponse {
  card: KanbanCard;
  new_dispatch_id: string | null;
  cancelled_dispatch_id: string | null;
  next_action: string;
}

function isObjectRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function hasOwn(value: Record<string, unknown>, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(value, key);
}

function requireFields(
  endpoint: string,
  value: Record<string, unknown>,
  keys: string[],
): void {
  for (const key of keys) {
    if (!hasOwn(value, key)) {
      throw new Error(
        `${endpoint} response contract invalid: missing required field '${key}'`,
      );
    }
  }
}

function parseKanbanDispatchMutationResponse(
  endpoint: string,
  raw: unknown,
): KanbanDispatchMutationResponse {
  if (!isObjectRecord(raw)) {
    throw new Error(`${endpoint} response contract invalid: expected object`);
  }
  requireFields(endpoint, raw, [
    "card",
    "new_dispatch_id",
    "cancelled_dispatch_id",
    "next_action",
  ]);
  if (!isObjectRecord(raw.card)) {
    throw new Error(
      `${endpoint} response contract invalid: field 'card' must be an object`,
    );
  }
  if (raw.new_dispatch_id !== null && typeof raw.new_dispatch_id !== "string") {
    throw new Error(
      `${endpoint} response contract invalid: field 'new_dispatch_id' must be string or null`,
    );
  }
  if (
    raw.cancelled_dispatch_id !== null &&
    typeof raw.cancelled_dispatch_id !== "string"
  ) {
    throw new Error(
      `${endpoint} response contract invalid: field 'cancelled_dispatch_id' must be string or null`,
    );
  }
  if (typeof raw.next_action !== "string" || raw.next_action.trim() === "") {
    throw new Error(
      `${endpoint} response contract invalid: field 'next_action' must be a non-empty string`,
    );
  }
  return {
    card: raw.card as unknown as KanbanCard,
    new_dispatch_id: raw.new_dispatch_id,
    cancelled_dispatch_id: raw.cancelled_dispatch_id,
    next_action: raw.next_action,
  };
}

export async function retryKanbanCard(
  id: string,
  payload?: { assignee_agent_id?: string | null; request_now?: boolean },
): Promise<KanbanDispatchMutationResponse> {
  const endpoint = `/api/kanban-cards/${id}/retry`;
  // #2050 P3 finding 15 — retry hits GitHub + Discord; 15s would race
  // ahead of a still-processing server. Bump to 60s.
  const res = await request<unknown>(endpoint, {
    method: "POST",
    body: JSON.stringify(payload ?? {}),
    timeoutMs: SLOW_MUTATION_TIMEOUT_MS,
  });
  return parseKanbanDispatchMutationResponse(endpoint, res);
}

export async function redispatchKanbanCard(
  id: string,
  payload?: { reason?: string | null },
): Promise<KanbanDispatchMutationResponse> {
  const endpoint = `/api/kanban-cards/${id}/redispatch`;
  // #2050 P3 finding 15 — same external-I/O envelope as retry.
  const res = await request<unknown>(endpoint, {
    method: "POST",
    body: JSON.stringify(payload ?? {}),
    timeoutMs: SLOW_MUTATION_TIMEOUT_MS,
  });
  return parseKanbanDispatchMutationResponse(endpoint, res);
}

export async function patchKanbanDeferDod(
  id: string,
  payload: {
    items?: Array<{ label: string }>;
    verify?: string;
    unverify?: string;
    remove?: string;
  },
): Promise<KanbanCard> {
  const res = await request<{ card: KanbanCard }>(
    `/api/kanban-cards/${id}/defer-dod`,
    {
      method: "PATCH",
      body: JSON.stringify(payload),
    },
  );
  return res.card;
}

export interface AssignmentResult {
  ok: boolean;
  agent_id: string;
}

export interface AssignmentTransitionResult {
  attempted: boolean;
  ok: boolean;
  from?: string;
  to?: string;
  target?: string;
  target_status: string;
  next_action: string;
  steps?: string[];
  completed_steps?: Array<{ from?: string; to?: string; changed?: boolean }>;
  failed_step?: string;
  error: string | null;
}

export interface AssignKanbanIssueResponse {
  card: KanbanCard;
  deduplicated?: boolean;
  assignment: AssignmentResult;
  transition: AssignmentTransitionResult;
}

function parseAssignKanbanIssueResponse(
  endpoint: string,
  raw: unknown,
): AssignKanbanIssueResponse {
  if (!isObjectRecord(raw)) {
    throw new Error(`${endpoint} response contract invalid: expected object`);
  }
  requireFields(endpoint, raw, ["card", "assignment", "transition"]);

  const assignment = raw.assignment;
  const transition = raw.transition;
  if (!isObjectRecord(raw.card)) {
    throw new Error(
      `${endpoint} response contract invalid: field 'card' must be an object`,
    );
  }
  if (!isObjectRecord(assignment)) {
    throw new Error(
      `${endpoint} response contract invalid: field 'assignment' must be an object`,
    );
  }
  if (!isObjectRecord(transition)) {
    throw new Error(
      `${endpoint} response contract invalid: field 'transition' must be an object`,
    );
  }

  requireFields(endpoint, assignment, ["ok", "agent_id"]);
  requireFields(endpoint, transition, [
    "attempted",
    "ok",
    "target_status",
    "error",
    "next_action",
  ]);

  if (typeof assignment.ok !== "boolean") {
    throw new Error(
      `${endpoint} response contract invalid: field 'assignment.ok' must be boolean`,
    );
  }
  if (
    typeof assignment.agent_id !== "string" ||
    assignment.agent_id.trim() === ""
  ) {
    throw new Error(
      `${endpoint} response contract invalid: field 'assignment.agent_id' must be a non-empty string`,
    );
  }
  if (
    typeof transition.attempted !== "boolean" ||
    typeof transition.ok !== "boolean"
  ) {
    throw new Error(
      `${endpoint} response contract invalid: transition booleans must be boolean`,
    );
  }
  if (
    typeof transition.target_status !== "string" ||
    transition.target_status.trim() === ""
  ) {
    throw new Error(
      `${endpoint} response contract invalid: field 'transition.target_status' must be a non-empty string`,
    );
  }
  if (transition.error !== null && typeof transition.error !== "string") {
    throw new Error(
      `${endpoint} response contract invalid: field 'transition.error' must be string or null`,
    );
  }
  if (
    typeof transition.next_action !== "string" ||
    transition.next_action.trim() === ""
  ) {
    throw new Error(
      `${endpoint} response contract invalid: field 'transition.next_action' must be a non-empty string`,
    );
  }

  return {
    card: raw.card as unknown as KanbanCard,
    deduplicated:
      typeof raw.deduplicated === "boolean" ? raw.deduplicated : undefined,
    assignment: assignment as unknown as AssignmentResult,
    transition: transition as unknown as AssignmentTransitionResult,
  };
}

export async function assignKanbanIssue(payload: {
  github_repo: string;
  github_issue_number: number;
  github_issue_url?: string | null;
  title: string;
  description?: string | null;
  assignee_agent_id: string;
}): Promise<AssignKanbanIssueResponse> {
  const endpoint = "/api/kanban-cards/assign-issue";
  const res = await request<unknown>(endpoint, {
    method: "POST",
    body: JSON.stringify(payload),
  });
  return parseAssignKanbanIssueResponse(endpoint, res);
}

export async function getStalledCards(): Promise<KanbanCard[]> {
  return request("/api/kanban-cards/stalled");
}

// #1064: bulk-action consolidated into per-card POST /kanban-cards/{id}/transition.
// Server-side pipeline lookup for terminal/initial states is now the caller's
// responsibility — each action resolves a concrete target_status before iteration.
export async function bulkKanbanAction(
  action: "pass" | "reset" | "cancel" | "transition",
  card_ids: string[],
  targetStatus?: string,
): Promise<{
  action: string;
  results: Array<{ id: string; ok: boolean; error?: string }>;
}> {
  let resolvedTarget: string | undefined = targetStatus;
  if (action === "pass" || action === "cancel") {
    resolvedTarget = "done";
  } else if (action === "reset") {
    resolvedTarget = "backlog";
  } else if (action === "transition") {
    if (!resolvedTarget) {
      throw new Error("transition action requires target_status");
    }
  }

  // #2050 P3 finding 22 — chunked concurrency. The previous Promise.all
  // unleashed N parallel transitions, each emitting an immediate
  // kanban_card_updated broadcast. With N=100 the dashboard pile-driver
  // triggered a stats refresh per emit. Limiting to 5 in flight keeps
  // per-emit latency reasonable while preventing the broadcast storm.
  const CONCURRENCY = 5;
  const results: Array<{ id: string; ok: boolean; error?: string }> = [];
  for (let i = 0; i < card_ids.length; i += CONCURRENCY) {
    const chunk = card_ids.slice(i, i + CONCURRENCY);
    const chunkResults = await Promise.all(
      chunk.map(async (id) => {
        try {
          await request(`/api/kanban-cards/${encodeURIComponent(id)}/transition`, {
            method: "POST",
            body: JSON.stringify({ status: resolvedTarget }),
          });
          return { id, ok: true };
        } catch (error) {
          return {
            id,
            ok: false,
            error: error instanceof Error ? error.message : String(error),
          };
        }
      }),
    );
    results.push(...chunkResults);
  }

  return { action, results };
}

export async function getKanbanRepoSources(): Promise<KanbanRepoSource[]> {
  const data = await request<{ repos: KanbanRepoSource[] }>(
    "/api/kanban-repos",
  );
  return data.repos;
}

export async function addKanbanRepoSource(
  repo: string,
): Promise<KanbanRepoSource> {
  return request("/api/kanban-repos", {
    method: "POST",
    body: JSON.stringify({ repo }),
  });
}

export async function updateKanbanRepoSource(
  id: string,
  data: { default_agent_id?: string | null },
): Promise<KanbanRepoSource> {
  return request(`/api/kanban-repos/${id}`, {
    method: "PATCH",
    body: JSON.stringify(data),
  });
}

export async function deleteKanbanRepoSource(id: string): Promise<void> {
  await request(`/api/kanban-repos/${id}`, { method: "DELETE" });
}

// ── Kanban Reviews ──

export interface KanbanReview {
  id: string;
  card_id: string;
  round: number;
  original_dispatch_id: string | null;
  original_agent_id: string | null;
  original_provider: string | null;
  review_dispatch_id: string | null;
  reviewer_agent_id: string | null;
  reviewer_provider: string | null;
  verdict: string;
  items_json: string | null;
  github_comment_id: string | null;
  created_at: number;
  completed_at: number | null;
}

export async function getKanbanReviews(
  cardId: string,
): Promise<KanbanReview[]> {
  const data = await request<{ reviews: KanbanReview[] }>(
    `/api/kanban-cards/${cardId}/reviews`,
  );
  return data.reviews;
}

export async function saveReviewDecisions(
  reviewId: string,
  decisions: Array<{ item_id: string; decision: "accept" | "reject" }>,
): Promise<{ review: KanbanReview }> {
  return request(`/api/kanban-reviews/${reviewId}/decisions`, {
    method: "PATCH",
    body: JSON.stringify({ decisions }),
  });
}

export async function triggerDecidedRework(
  reviewId: string,
): Promise<{ ok: boolean }> {
  return request(`/api/kanban-reviews/${reviewId}/trigger-rework`, {
    method: "POST",
  });
}

// ── Card Audit Log & Comments ──

export interface CardAuditLogEntry {
  id: number;
  card_id: string;
  from_status: string | null;
  to_status: string | null;
  source: string | null;
  result: string | null;
  created_at: string | null;
}

export interface GitHubComment {
  author: { login: string };
  body: string;
  createdAt: string;
}

export async function getCardAuditLog(
  cardId: string,
): Promise<CardAuditLogEntry[]> {
  const data = await request<{ logs: CardAuditLogEntry[] }>(
    `/api/kanban-cards/${cardId}/audit-log`,
  );
  return data.logs;
}

export interface CardGitHubCommentsResult {
  comments: GitHubComment[];
  body: string;
}

export async function getCardGitHubComments(
  cardId: string,
): Promise<CardGitHubCommentsResult> {
  const data = await request<{ comments: GitHubComment[]; body?: string }>(
    `/api/kanban-cards/${cardId}/comments`,
  );
  return { comments: data.comments, body: data.body ?? "" };
}

// ── Pipeline ──

export interface PipelineStageInput {
  stage_name: string;
  entry_skill?: string | null;
  provider?: string | null;
  agent_override_id?: string | null;
  timeout_minutes?: number;
  on_failure?: "fail" | "retry" | "previous" | "goto";
  on_failure_target?: string | null;
  max_retries?: number;
  skip_condition?: string | null;
  parallel_with?: string | null;
  applies_to_agent_id?: string | null;
  trigger_after?: "ready" | "review_pass";
}

export async function getPipelineStages(
  repo: string,
): Promise<import("../types").PipelineStage[]> {
  const data = await request<{ stages: import("../types").PipelineStage[] }>(
    `/api/pipeline/stages?repo=${encodeURIComponent(repo)}`,
  );
  return data.stages;
}

export async function savePipelineStages(
  repo: string,
  stages: PipelineStageInput[],
): Promise<import("../types").PipelineStage[]> {
  const data = await request<{ stages: import("../types").PipelineStage[] }>(
    "/api/pipeline/stages",
    { method: "PUT", body: JSON.stringify({ repo, stages }) },
  );
  return data.stages;
}

export async function deletePipelineStages(repo: string): Promise<void> {
  await request(`/api/pipeline/stages?repo=${encodeURIComponent(repo)}`, {
    method: "DELETE",
  });
}

export async function getCardPipelineStatus(cardId: string): Promise<{
  stages: import("../types").PipelineStage[];
  history: import("../types").PipelineHistoryEntry[];
  current_stage: import("../types").PipelineStage | null;
}> {
  return request(`/api/pipeline/cards/${cardId}`);
}
