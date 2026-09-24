import { z } from "zod";
import { request } from "./httpClient";
import { machineResourcesSchema } from "./machineResources";

const responseSchema = z.object({
  instance_id: z.string(),
  samples: z.array(machineResourcesSchema.nullable().catch(null)),
});

export async function getMachineResourceHistory(instanceId: string, signal?: AbortSignal) {
  const query = new URLSearchParams({ instance_id: instanceId, limit: "240" });
  const response = await request(`/api/cluster/machine-resources/history?${query}`, {
    signal, cache: "no-store", suppressErrorToast: true, maxRetries: 0,
  }, responseSchema);
  return response.samples.filter((sample): sample is NonNullable<typeof sample> => sample !== null);
}
