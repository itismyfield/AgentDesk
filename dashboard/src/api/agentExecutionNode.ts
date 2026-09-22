import { z } from "zod";
import { request } from "./httpClient";

const selection = z.object({ default_node_id: z.string().nullable() });
const endpoint = (agentId: string) => `/api/agents/${encodeURIComponent(agentId)}/execution-node`;

export const getAgentExecutionNode = (agentId: string, signal?: AbortSignal) => request(
  endpoint(agentId), { signal, cache: "no-store", maxRetries: 0, suppressErrorToast: true },
  selection.extend({ routing_enforced: z.boolean() }),
);

export const setAgentExecutionNode = (agentId: string, nodeId: string | null) => request(
  endpoint(agentId), {
    method: "PUT", body: JSON.stringify({ default_node_id: nodeId }),
    maxRetries: 0, suppressErrorToast: true,
  }, selection,
);
