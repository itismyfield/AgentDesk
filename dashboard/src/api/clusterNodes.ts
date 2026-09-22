import { z } from "zod";
import { request } from "./httpClient";

const readinessReport = z.object({ eligible: z.boolean(), reasons: z.array(z.string()) });
const nodeSchema = z.object({
  instance_id: z.string(), status: z.string().nullish(), effective_role: z.string().nullish(),
  hostname: z.string().nullish(),
  active_session_count: z.number().nullish(), active_dispatch_count: z.number().nullish(),
  execution_active: z.number().nullish(), execution_occupied: z.number().nullish(),
  capabilities: z.object({
    execution_capacity: z.object({ version: z.literal(1), slots: z.number().int().positive() }).nullish(),
    execution_readiness: z.object({
      os: z.string(), arch: z.string(), runtime_profile: z.string(),
      observed_at_ms: z.number(), expires_at_ms: z.number(), backends: z.array(z.string()),
    }).nullish(),
  }).passthrough(),
  execution_readiness: z.object({ providers: z.record(z.string(), readinessReport) }).nullish(),
  forwarding_diagnostics: z.object({
    advertised: z.boolean(), configured: z.boolean(), reachability_verified: z.boolean(),
    trust_validated: z.boolean().optional(), reachability_status: z.string().optional(),
    expires_at_ms: z.number().optional(),
  }).nullish(),
});
const nodesSchema = z.object({
  cluster: z.object({ enabled: z.boolean(), local_instance_id: z.string().nullish() }),
  nodes: z.array(nodeSchema),
});
const sessionSchema = z.object({
  id: z.union([z.number(), z.string()]), session_key: z.string(),
  instance_id: z.string().nullish(), name: z.string().nullish(), provider: z.string(), status: z.string(),
});
const outputSchema = z.object({
  recent_output: z.string(), backend: z.string(), available: z.boolean(),
  unavailable_reason: z.string().nullish(), output_format: z.string(), captured_at_ms: z.number(),
});
export type ClusterNode = z.infer<typeof nodeSchema>;
export type NodeSession = z.infer<typeof sessionSchema>;
export const getClusterNodes = (signal?: AbortSignal) => request("/api/cluster/nodes", {
  signal, cache: "no-store", suppressErrorToast: true, maxRetries: 0,
}, nodesSchema);
export const getNodeSessions = (signal?: AbortSignal) => request("/api/dispatched-sessions", {
  signal, cache: "no-store", suppressErrorToast: true, maxRetries: 0,
}, z.object({ sessions: z.array(sessionSchema) }));
export const getNodeSessionOutput = (id: number | string, signal?: AbortSignal) => request(
  `/api/sessions/${encodeURIComponent(id)}/output?lines=100`,
  { signal, cache: "no-store", suppressErrorToast: true, maxRetries: 0 }, outputSchema,
);
export const stopNodeSession = (sessionKey: string) => request(
  `/api/sessions/${encodeURIComponent(sessionKey)}/force-kill`,
  { method: "POST", body: JSON.stringify({ retry: false, reason: "Stopped from cluster node panel" }), maxRetries: 0 },
);

export function nodeControlUnavailable(node: ClusterNode, localId: string | null | undefined, stale: boolean): string | null {
  if (stale) return "stale";
  if (node.status !== "online") return "offline";
  if (node.instance_id !== localId && !node.forwarding_diagnostics?.configured) return "forwarding";
  if (node.instance_id !== localId && (!node.forwarding_diagnostics?.reachability_verified
    || (node.forwarding_diagnostics.expires_at_ms ?? 0) <= Date.now())) return "unreachable";
  return null;
}
