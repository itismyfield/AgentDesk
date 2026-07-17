import { z } from "zod";

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

const nonEmptyStringSchema = z.string().trim().min(1);
const timestampSchema = z.string();
const nullableTimestampSchema = timestampSchema.nullable();
const kanbanCardStatusSchema = z.string().regex(/^[a-z][a-z0-9_]*$/);

const kanbanCardSchema = z.looseObject({
  id: nonEmptyStringSchema,
  title: z.string(),
  description: z.string().nullable(),
  status: kanbanCardStatusSchema,
  github_repo: z.string().nullable(),
  owner_agent_id: z.string().nullable(),
  requester_agent_id: z.string().nullable(),
  assignee_agent_id: z.string().nullable(),
  parent_card_id: z.string().nullable(),
  latest_dispatch_id: z.string().nullable(),
  sort_order: z.number(),
  priority: z.string(),
  depth: z.number(),
  blocked_reason: z.string().nullable(),
  review_notes: z.string().nullable(),
  github_issue_number: z.number().nullable(),
  github_issue_url: z.string().nullable(),
  metadata_json: z.string().nullable(),
  pipeline_stage_id: z.string().nullable(),
  review_status: z.string().nullable(),
  created_at: timestampSchema,
  updated_at: timestampSchema,
  started_at: nullableTimestampSchema,
  requested_at: nullableTimestampSchema,
  completed_at: nullableTimestampSchema,
}) satisfies z.ZodType<KanbanCard>;

const kanbanDispatchMutationResponseSchema = z.object({
  card: kanbanCardSchema,
  new_dispatch_id: z.string().nullable(),
  cancelled_dispatch_id: z.string().nullable(),
  next_action: nonEmptyStringSchema,
});

export type KanbanDispatchMutationResponse = z.infer<
  typeof kanbanDispatchMutationResponseSchema
>;

export async function retryKanbanCard(
  id: string,
  payload?: { assignee_agent_id?: string | null; request_now?: boolean },
): Promise<KanbanDispatchMutationResponse> {
  const endpoint = `/api/kanban-cards/${id}/retry`;
  // #2050 P3 finding 15 — retry hits GitHub + Discord; 15s would race
  // ahead of a still-processing server. Bump to 60s.
  return request(
    endpoint,
    {
      method: "POST",
      body: JSON.stringify(payload ?? {}),
      timeoutMs: SLOW_MUTATION_TIMEOUT_MS,
    },
    kanbanDispatchMutationResponseSchema,
  );
}

export async function redispatchKanbanCard(
  id: string,
  payload?: { reason?: string | null },
): Promise<KanbanDispatchMutationResponse> {
  const endpoint = `/api/kanban-cards/${id}/redispatch`;
  // #2050 P3 finding 15 — same external-I/O envelope as retry.
  return request(
    endpoint,
    {
      method: "POST",
      body: JSON.stringify(payload ?? {}),
      timeoutMs: SLOW_MUTATION_TIMEOUT_MS,
    },
    kanbanDispatchMutationResponseSchema,
  );
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

const assignmentResultSchema = z.object({
  ok: z.boolean(),
  agent_id: nonEmptyStringSchema,
});

const assignmentTransitionResultSchema = z.object({
  attempted: z.boolean(),
  ok: z.boolean(),
  from: z.string().optional(),
  to: z.string().optional(),
  target: z.string().optional(),
  target_status: nonEmptyStringSchema,
  next_action: nonEmptyStringSchema,
  steps: z.array(z.string()).optional(),
  completed_steps: z
    .array(
      z.object({
        from: z.string().optional(),
        to: z.string().optional(),
        changed: z.boolean().optional(),
      }),
    )
    .optional(),
  failed_step: z.string().optional(),
  error: z.string().nullable(),
});

const assignKanbanIssueResponseSchema = z.object({
  card: kanbanCardSchema,
  deduplicated: z.boolean().optional(),
  assignment: assignmentResultSchema,
  transition: assignmentTransitionResultSchema,
});

export type AssignmentResult = z.infer<typeof assignmentResultSchema>;
export type AssignmentTransitionResult = z.infer<
  typeof assignmentTransitionResultSchema
>;
export type AssignKanbanIssueResponse = z.infer<
  typeof assignKanbanIssueResponseSchema
>;

export async function assignKanbanIssue(payload: {
  github_repo: string;
  github_issue_number: number;
  github_issue_url?: string | null;
  title: string;
  description?: string | null;
  assignee_agent_id: string;
}): Promise<AssignKanbanIssueResponse> {
  const endpoint = "/api/kanban-cards/assign-issue";
  return request(
    endpoint,
    {
      method: "POST",
      body: JSON.stringify(payload),
    },
    assignKanbanIssueResponseSchema,
  );
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
