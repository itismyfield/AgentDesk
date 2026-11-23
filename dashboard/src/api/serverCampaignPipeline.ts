import { z } from "zod";
import { ApiRequestError, request } from "./httpClient";

// Mirror src/domain/campaign/pipeline.rs; kinds and gates stay open strings, so one the server adds later shows by its id.
export const PIPELINE_KINDS = ["agent_task", "auto_check", "auto_action", "human_decision", "wait", "end_success", "end_failure"] as const;
const comparisonSchema = z.looseObject({ field: z.string(), op: z.string(), value: z.union([z.boolean(), z.number(), z.string()]) });
export type PipelineComparison = z.infer<typeof comparisonSchema>;
type Condition = PipelineComparison | { all: Condition[] };
const conditionSchema: z.ZodType<Condition> = z.lazy(() => z.union([comparisonSchema, z.looseObject({ all: z.array(conditionSchema) })]));

const pipelineSchema = z.looseObject({
  start: z.string(),
  states: z.array(z.looseObject({
    id: z.string(), kind: z.string(), title: z.string().nullish(), owner: z.string().nullish(), instructions: z.string().nullish(),
    timeout: z.looseObject({ after_seconds: z.number(), transition_id: z.string() }).nullish(),
    executor: z.looseObject({ id: z.string(), version: z.number(), parameters: z.looseObject({ effect_gates: z.array(z.string()).optional() }).optional() }).nullish(),
  })),
  transitions: z.array(z.looseObject({
    id: z.string(), from: z.string(), to: z.string(), priority: z.number(), condition: conditionSchema,
    retry: z.looseObject({ budget_id: z.string(), max: z.number(), then_to: z.string() }).nullish(), gate: z.string().nullish(),
  })),
  events: z.array(z.looseObject({ id: z.string(), state_id: z.string(), type: z.string() })).default([]),
  template_id: z.string().nullish(),
  template_version: z.number().nullish(),
});
export const campaignPipelineSchema = z.looseObject({
  pipeline: pipelineSchema,
  definition_revision: z.number(),
  // Both go once the server drops its legacy projection; the tab reads fine without them.
  source: z.string().optional(),
  editable: z.boolean(),
  migration_available: z.boolean().optional(),
  execution: z.looseObject({ mode: z.string(), run_id: z.string().nullable(), state_ids: z.record(z.string(), z.string().nullable()) }),
});
const fieldsSchema = z.looseObject({ fields: z.array(z.looseObject({ field: z.string(), type: z.string(), operators: z.array(z.string()), enum_values: z.array(z.string()), available: z.boolean() })) });
const diagnosticSchema = z.looseObject({ code: z.string(), path: z.string(), state_id: z.string().nullable(), transition_id: z.string().nullable() });
const diagnosticsSchema = z.looseObject({ errors: z.array(diagnosticSchema), warnings: z.array(diagnosticSchema).default([]) });
const savedSchema = z.looseObject({ result: z.looseObject({ definition_revision: z.number() }) });
const refusalSchema = z.looseObject({ rejection: z.string().optional(), definition_error: z.looseObject({ pipeline_errors: z.array(diagnosticSchema).optional() }).optional() });
const templateSchema = z.looseObject({ id: z.string(), version: z.number() });
const templateListSchema = z.looseObject({ templates: z.array(templateSchema) });
const templateDetailSchema = z.looseObject({ template: templateSchema, pipeline: pipelineSchema });

export type PipelineDefinition = z.infer<typeof pipelineSchema>;
export type PipelineState = PipelineDefinition["states"][number];
export type PipelineTransition = PipelineDefinition["transitions"][number];
export type PipelineCondition = Condition;
export type CampaignPipeline = z.infer<typeof campaignPipelineSchema>;
export type PipelineTemplate = z.infer<typeof templateDetailSchema>;
export type PipelineFieldSpec = z.infer<typeof fieldsSchema>["fields"][number];
export type PipelineDiagnostic = z.infer<typeof diagnosticSchema>;

const read = <T>(url: string, schema: z.ZodType<T>, signal?: AbortSignal) =>
  request(url, { method: "GET", signal, suppressErrorToast: true, shareInflight: false }, schema);

export function getServerCampaignPipeline(id: string, signal?: AbortSignal) {
  return read(`/api/server-campaigns/${encodeURIComponent(id)}/pipeline`, campaignPipelineSchema, signal);
}

/** The built-in template `migrate` attaches (campaign-development-9), or the first listed, at its newest version. */
export async function getDefaultPipelineTemplate(signal?: AbortSignal): Promise<PipelineTemplate | null> {
  const { templates } = await read("/api/server-campaigns/pipeline/templates", templateListSchema, signal);
  const pool = templates.some((value) => value.id === "campaign-development-9") ? templates.filter((value) => value.id === "campaign-development-9") : templates.filter((value) => value.id === templates[0]?.id);
  const chosen = pool.sort((a, b) => b.version - a.version)[0];
  if (!chosen) return null;
  return read(`/api/server-campaigns/pipeline/templates/${encodeURIComponent(chosen.id)}/${chosen.version}`, templateDetailSchema, signal);
}

/** The condition vocabulary for a state of this kind and executor. */
export async function getPipelineConditionFields(kind: string, executor: { id: string; version: number } | null | undefined, signal?: AbortSignal) {
  const query = new URLSearchParams({ kind, ...executor ? { executor: executor.id, executor_version: String(executor.version) } : {} });
  return (await read(`/api/server-campaigns/pipeline/condition-fields?${query}`, fieldsSchema, signal)).fields;
}

const pipelineUrl = (id: string) => `/api/server-campaigns/${encodeURIComponent(id)}/pipeline`;
/** The server's checks on a draft, with the run's occupied states; nothing is saved. */
export function validateServerCampaignPipeline(id: string, pipeline: PipelineDefinition, signal?: AbortSignal) {
  return request(`${pipelineUrl(id)}/validate`, { method: "POST", body: JSON.stringify({ pipeline }), signal, suppressErrorToast: true }, diagnosticsSchema);
}

/** Replaces the pipeline when the definition is still at `expectedRevision`; the same key may be resent safely. */
export async function saveServerCampaignPipeline(id: string, expectedRevision: number, pipeline: PipelineDefinition, idempotencyKey: string) {
  const body = { idempotency_key: idempotencyKey, expected_definition_revision: expectedRevision, pipeline };
  return (await request(pipelineUrl(id), { method: "PUT", body: JSON.stringify(body), suppressErrorToast: true }, savedSchema)).result;
}

/** A refused save: its status, rejection and the graph diagnostics a 422 or an occupied state carries; null when no answer arrived. */
export function pipelineRefusal(error: unknown) {
  if (!(error instanceof ApiRequestError)) return null;
  const context = refusalSchema.safeParse((error.body as { context?: unknown } | undefined)?.context ?? {}).data;
  return { status: error.status, rejection: context?.rejection ?? null, diagnostics: context?.definition_error?.pipeline_errors ?? [], message: error.message };
}
