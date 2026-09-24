import { useQuery } from "@tanstack/react-query";
import { getMachineResourceHistory } from "../../api/machineResourceHistory";

const HISTORY_REFRESH_MS = 10_000;

export function useMachineResourceHistory(instanceId: string) {
  return useQuery({
    queryKey: ["machine-resource-history", instanceId],
    queryFn: ({ signal }) => getMachineResourceHistory(instanceId, signal),
    refetchInterval: HISTORY_REFRESH_MS, staleTime: HISTORY_REFRESH_MS, retry: false,
  });
}
