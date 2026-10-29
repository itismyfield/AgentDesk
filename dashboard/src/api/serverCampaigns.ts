import { z } from "zod";
import { ApiRequestError, request, type RequestOptions } from "./httpClient";

// Mirror campaign_execution::{definition,model,view} serde and the GET envelope.
const repositorySchema = z.enum(["adk", "cooking_heart"]);
const timestampSchema = z.iso.datetime({ offset: true });
// Reject integers JSON/JavaScript cannot represent exactly instead of rounding identities.
const unsignedSchema = z.number().int().nonnegative();
const u32Schema = unsignedSchema.max(4_294_967_295);
const stageSchema = z.enum([
  "requirements", "design", "design_review", "implementation", "review",
  "landing", "main_verification", "final_verification", "complete",
]);
const reasonSchema = z.enum([
  "execution_unavailable", "review_resources", "host_resources", "owned_elsewhere",
  "predecessor_main", "predecessor_failed", "observation_unknown", "state_needs_inspection",
]);
const rejectionSchema = z.enum([
  "invalid_definition", "stale_definition", "stale_run", "stale_binding", "invalid_state",
  "execution_unavailable", "sessions_not_gone", "unreconciled_actions", "fence_incomplete",
  "stale_fence", "idempotency_conflict", "missing_change_reason", "completed_node_changed", "already_exists",
]);

const controlSchema = z.enum(["prepared", "running", "stopping", "stopped", "succeeded"]);

export const serverCampaignDefinitionSchema = z.strictObject({
  repository: repositorySchema,
  title: z.string(),
  goal: z.string(),
  profile_version: z.string(),
  nodes: z.array(z.strictObject({
    id: z.string(),
    title: z.string(),
    issue_url: z.string(),
    acceptance: z.array(z.strictObject({
      id: z.string(),
      description: z.string(),
      verification: z.enum(["test", "main", "deployment", "operational"]),
    })),
    dependencies: z.array(z.string()),
    group: z.string().nullable(),
  })),
});

const hostGateSchema = z.looseObject({
  host: z.string(),
  gate: z.string(),
  incarnation: z.string(),
});
const sessionIdentitySchema = z.looseObject({
  gate: hostGateSchema,
  session: z.string(),
  incarnation: z.string(),
});
const sessionSchema = z.looseObject({
  identity: sessionIdentitySchema,
  attempt_id: z.string(),
  observation: z.discriminatedUnion("state", [
    z.looseObject({ state: z.literal("alive"), observed_at: timestampSchema }),
    z.looseObject({ state: z.literal("gone"), observed_at: timestampSchema, evidence: z.string() }),
    z.looseObject({
      state: z.literal("unknown"), reason: z.string(), last_observed_at: timestampSchema.nullable(),
    }),
  ]),
});
const actionSchema = z.looseObject({
  id: z.string(),
  binding: z.looseObject({
    campaign_id: z.string(),
    run_id: z.string(),
    node_id: z.string(),
    series: u32Schema,
    stage: stageSchema,
    attempt_id: z.string(),
    role: z.enum(["implementation", "review"]),
    session: sessionIdentitySchema,
    repository: repositorySchema,
    hashes: z.looseObject({ requirements: z.string(), acceptance: z.string(), profile: z.string() }),
    workflow_version: z.string(),
    base_sha: z.string(),
    candidate_sha: z.string(),
    control_version: unsignedSchema,
  }),
  kind: z.enum(["launch", "deliver_stage", "create_pr", "merge", "deploy"]),
  target: hostGateSchema,
  state: z.enum(["planned", "claimed", "start_pending", "started", "reconciled", "cancelled", "unknown"]),
  initiation_receipt: z.string().nullable(),
  reconciliation: z.string().nullable(),
});
const nodeViewSchema = z.looseObject({
  id: z.string(),
  runtime: z.looseObject({
    stage: stageSchema,
    disposition: z.enum(["pending", "assigned", "running", "verifying", "failed", "blocked", "interrupted", "complete"]),
    series: u32Schema,
    quality_failures: u32Schema,
    repair_cycles: u32Schema,
    review_rounds: u32Schema,
    current_attempt: z.string().nullable(),
    main_integrated: z.boolean(),
    acceptance_complete: z.boolean(),
    evidence_current: z.boolean(),
    wait_reason: reasonSchema.nullable(),
  }),
  direct_successors: z.array(z.string()),
  ready_after_main: z.array(z.string()),
});

// Projection fields are always serialized, including null Options; no client defaults.
export const serverCampaignViewSchema = z.looseObject({
  execution_mode: z.literal("server"),
  campaign_id: z.string(),
  as_of: timestampSchema,
  definition_revision: unsignedSchema,
  run_id: z.string().nullable(),
  control_version: unsignedSchema.nullable(),
  last_event_sequence: unsignedSchema,
  last_event_at: timestampSchema.nullable(),
  control: controlSchema,
  stop_mode: z.enum(["immediate", "finish_stage"]).nullable(),
  stop_confirmed: z.boolean(),
  display: z.enum(["stopping", "stopped", "prepared", "succeeded", "running", "waiting", "unable_to_progress", "needs_inspection"]),
  execution_readiness: z.literal("unavailable"),
  unknown: z.boolean(),
  failed_count: unsignedSchema,
  blocked_count: unsignedSchema,
  reasons: z.array(z.looseObject({ code: reasonSchema, target: z.string() })),
  commands: z.array(z.looseObject({
    command: z.enum(["edit_definition", "start", "stop_immediate", "stop_finish_stage", "resume", "prepare_retry"]),
    rejection: rejectionSchema.nullable(),
  })),
  nodes: z.array(nodeViewSchema),
  sessions: z.array(sessionSchema),
  unreconciled_actions: z.array(actionSchema),
});
export const serverCampaignDocumentSchema = z.looseObject({
  definition: serverCampaignDefinitionSchema,
  view: serverCampaignViewSchema,
});
export const serverCampaignListResponseSchema = z.looseObject({
  campaigns: z.array(serverCampaignDocumentSchema),
});
export const serverCampaignDetailResponseSchema = z.looseObject({
  campaign: serverCampaignDocumentSchema,
});

// Mirrors campaign_execution::profile::RepositoryProfile. The view does not carry it yet, so it is
// read apart from the document schema: an absent or different shape hides the start check only.
const profileSchema = z.looseObject({
  repository: repositorySchema,
  version: z.string(),
  models: z.array(z.looseObject({
    role: z.enum(["implementation", "review"]),
    selection: z.discriminatedUnion("state", [
      z.looseObject({ state: z.literal("pinned"), model: z.string(), effort: z.string() }),
      z.looseObject({ state: z.literal("unspecified"), reason: z.string() }),
    ]),
  })),
  limits: z.looseObject({ concurrent_tasks: u32Schema, quality_repairs: u32Schema, design_review_rounds: u32Schema }),
  readiness: z.literal("unavailable"),
  unavailable_reason: z.string(),
});
export type ServerCampaignProfile = z.infer<typeof profileSchema>;

export type ServerCampaignDefinition = z.infer<typeof serverCampaignDefinitionSchema>;
export type ServerCampaignView = z.infer<typeof serverCampaignViewSchema>;
export type ServerCampaignDocument = z.infer<typeof serverCampaignDocumentSchema>;
export type ServerCampaignListResponse = z.infer<typeof serverCampaignListResponseSchema>;
export type ServerCampaignDetailResponse = z.infer<typeof serverCampaignDetailResponseSchema>;
export type ServerCampaignReadOptions = Pick<RequestOptions, "signal" | "suppressErrorToast">;

export async function getServerCampaigns(options: ServerCampaignReadOptions = {}): Promise<ServerCampaignDocument[]> {
  const result = await request("/api/server-campaigns", {
    method: "GET", signal: options.signal, suppressErrorToast: options.suppressErrorToast ?? true, shareInflight: false,
  }, serverCampaignListResponseSchema);
  return result.campaigns;
}

export async function getServerCampaign(id: string, options: ServerCampaignReadOptions = {}): Promise<ServerCampaignDocument> {
  const result = await request(`/api/server-campaigns/${encodeURIComponent(id)}`, {
    method: "GET", signal: options.signal, suppressErrorToast: options.suppressErrorToast ?? true, shareInflight: false,
  }, serverCampaignDetailResponseSchema);
  return result.campaign;
}

const commandResultSchema = z.looseObject({
  result: z.looseObject({
    definition_revision: unsignedSchema,
    run_id: z.string().nullable(),
    control_version: unsignedSchema.nullable(),
    event_sequence: unsignedSchema,
    control: controlSchema,
  }),
});
const eventSchema = z.looseObject({
  sequence: unsignedSchema,
  run_id: z.string().nullable(),
  command_id: z.string(),
  observed_at: timestampSchema,
  kind: z.looseObject({ event: z.string() }),
});
const eventsResponseSchema = z.looseObject({ events: z.array(eventSchema) });

export type ServerCampaignCommandResult = z.infer<typeof commandResultSchema>["result"];
export type ServerCampaignEvent = z.infer<typeof eventSchema>;
export type ServerCampaignChange = { reference: string; explanation: string };
export type ServerCampaignCommand =
  | { command: "start"; expected_definition_revision: number }
  | { command: "stop"; expected_definition_revision: number; expected_run: string; mode: "immediate" | "finish_stage" }
  | { command: "resume"; expected_definition_revision: number; expected_run: string }
  | { command: "retry"; expected_definition_revision: number; expected_run: string; node_id: string };

const campaignUrl = (id: string) => `/api/server-campaigns/${encodeURIComponent(id)}`;
async function write(url: string, method: "POST" | "PUT", body: unknown) {
  const result = await request(url, {
    method, body: JSON.stringify(body), suppressErrorToast: true,
  }, commandResultSchema);
  return result.result;
}

export function createServerCampaign(id: string, definition: ServerCampaignDefinition, idempotencyKey: string) {
  return write("/api/server-campaigns", "POST", { id, idempotency_key: idempotencyKey, definition });
}

export function editServerCampaignDefinition(
  id: string, expectedRevision: number, definition: ServerCampaignDefinition,
  change: ServerCampaignChange | null, idempotencyKey: string,
) {
  return write(`${campaignUrl(id)}/definition`, "PUT", {
    idempotency_key: idempotencyKey, expected_definition_revision: expectedRevision, definition, change,
  });
}

export function sendServerCampaignCommand(id: string, command: ServerCampaignCommand, idempotencyKey: string) {
  return write(`${campaignUrl(id)}/commands`, "POST", { idempotency_key: idempotencyKey, command });
}

export async function getServerCampaignEvents(
  id: string,
  { after = 0, limit = 50, runId, signal }: { after?: number; limit?: number; runId?: string | null; signal?: AbortSignal } = {},
): Promise<ServerCampaignEvent[]> {
  const query = new URLSearchParams({ after: String(after), limit: String(limit) });
  if (runId) query.set("run_id", runId);
  const result = await request(`${campaignUrl(id)}/events?${query}`, {
    method: "GET", signal, suppressErrorToast: true, shareInflight: false,
  }, eventsResponseSchema);
  return result.events;
}

export interface ServerCampaignRejection {
  status: number;
  /** Null for refusals outside the command validator: format errors, a reused key, an unavailable database. */
  rejection: string | null;
  target: string | null;
  definitionError: { code: string; target: string | null; path: string[] } | null;
}

const rejectionContextSchema = z.looseObject({
  rejection: z.string().optional(),
  target: z.string().nullable().optional(),
  definition_error: z.looseObject({
    code: z.string(), target: z.string().nullable(), path: z.array(z.string()),
  }).optional(),
});

export function serverCampaignProfile(view: ServerCampaignView): ServerCampaignProfile | null {
  return profileSchema.safeParse((view as Record<string, unknown>).profile).data ?? null;
}

/** The server's answer to a refused write, or null when no answer arrived. */
export function serverCampaignRejection(error: unknown): ServerCampaignRejection | null {
  if (!(error instanceof ApiRequestError)) return null;
  const body = error.body as { context?: unknown } | undefined;
  const context = rejectionContextSchema.safeParse(body?.context ?? {}).data;
  const detail = context?.definition_error;
  return {
    status: error.status,
    rejection: context?.rejection ?? null,
    target: context?.target ?? null,
    definitionError: detail ? { code: detail.code, target: detail.target, path: detail.path } : null,
  };
}

/** A 4xx is the server's decision; only a lost answer or a 5xx may be resent under the same key. */
export function isDefinitiveAnswer(refusal: ServerCampaignRejection | null): boolean {
  return refusal !== null && refusal.status < 500;
}
