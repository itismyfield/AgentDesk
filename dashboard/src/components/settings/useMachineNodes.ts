import { useQuery } from "@tanstack/react-query";
import { getClusterNodes } from "../../api/clusterNodes";
import { MACHINE_REFRESH_INTERVAL_MS } from "./SettingsMachineModel";

// The panel and navigation badge observe the same cache and in-flight request.
export function useMachineNodes() {
  return useQuery({
    queryKey: ["cluster-nodes"], queryFn: ({ signal }) => getClusterNodes(signal),
    refetchInterval: MACHINE_REFRESH_INTERVAL_MS,
    staleTime: MACHINE_REFRESH_INTERVAL_MS, retry: false,
  });
}
