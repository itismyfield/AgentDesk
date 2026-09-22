import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getClusterNodes, getNodeSessions, getNodeSessionOutput, nodeControlUnavailable, stopNodeSession, type NodeSession } from "../../api/clusterNodes";
import { StatusBadge } from "../common/StatusBadge";
import { FreshnessIndicator } from "../common/FreshnessIndicator";

const reasons: Record<string, [string, string]> = {
  node_offline: ["노드 오프라인", "Node offline"],
  execution_evidence_missing: ["실행 검사 대기", "Execution probe pending"],
  execution_evidence_stale: ["실행 검사 만료", "Execution probe expired"],
  provider_not_probed: ["Provider 검사 없음", "Provider not probed"],
  provider_cli_unavailable: ["CLI 실행 불가", "CLI unavailable"],
  provider_credentials_missing: ["로컬 인증 설정 없음", "Local credentials missing"],
  intake_poller_stale: ["작업 수신 진행 확인 필요", "Intake progress stale"],
};

export default function ClusterNodesPanel({ isKo }: { isKo: boolean }) {
  const tr = (ko: string, en: string) => isKo ? ko : en;
  const queryClient = useQueryClient();
  const nodes = useQuery({ queryKey: ["cluster-nodes"], queryFn: ({ signal }) => getClusterNodes(signal), refetchInterval: 5_000 });
  const sessions = useQuery({ queryKey: ["cluster-node-sessions"], queryFn: ({ signal }) => getNodeSessions(signal), refetchInterval: 5_000, enabled: Boolean(nodes.data?.cluster.enabled) });
  const [selected, setSelected] = useState<NodeSession | null>(null);
  const [confirmStop, setConfirmStop] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());
  useEffect(() => { const timer = window.setInterval(() => setNow(Date.now()), 1_000); return () => window.clearInterval(timer); }, []);
  const stale = nodes.isError || !nodes.dataUpdatedAt || now - nodes.dataUpdatedAt > 15_000;
  const selectedNode = nodes.data?.nodes.find(node => node.instance_id === selected?.instance_id);
  const canControlSelected = selectedNode && !nodeControlUnavailable(selectedNode, nodes.data?.cluster.local_instance_id, stale);
  const output = useQuery({
    queryKey: ["cluster-session-output", selected?.id],
    queryFn: ({ signal }) => getNodeSessionOutput(selected!.id, signal),
    enabled: Boolean(selected && canControlSelected), refetchInterval: 5_000, retry: false,
  });
  const stop = useMutation({ mutationFn: stopNodeSession, onSuccess: () => {
    setConfirmStop(null);
    void queryClient.invalidateQueries({ queryKey: ["cluster-node-sessions"] });
  } });

  return <section className="card mb-5 min-w-0" aria-label={tr("클러스터 노드", "Cluster nodes")} data-testid="cluster-nodes-panel">
    <div className="card-head flex flex-wrap items-center justify-between gap-3">
      <div><h2 className="card-title">{tr("클러스터 노드", "Cluster nodes")}</h2>
        <p className="mt-1 text-xs text-th-text-muted">{tr("노드 연결과 실행 준비 상태를 따로 확인합니다. 5초마다 갱신합니다.", "Connectivity and execution readiness are separate. Refreshes every 5 seconds.")}</p></div>
      <FreshnessIndicator timestamp={nodes.dataUpdatedAt || null} staleAfterSeconds={15} criticalAfterSeconds={30} compact />
    </div>
    <div className="card-body space-y-3 min-w-0">
      {nodes.isPending && <p>{tr("노드 불러오는 중…", "Loading nodes…")}</p>}
      {nodes.isError && <p role="alert" className="text-amber-400">{tr("노드 갱신 실패. 마지막 정보를 표시하며 제어를 잠시 중단합니다.", "Node refresh failed. Showing the last snapshot; controls are disabled.")}</p>}
      {nodes.data && !nodes.data.cluster.enabled && <p className="text-th-text-muted">{tr("단일 노드로 실행 중입니다.", "Running as a single node.")}</p>}
      {nodes.data?.cluster.enabled && nodes.data.nodes.length === 0 && <p>{tr("등록된 노드가 없습니다.", "No registered nodes.")}</p>}
      <div className="grid gap-3 md:grid-cols-2">
        {nodes.data?.nodes.map(node => {
          const probe = node.capabilities.execution_readiness;
          const reports = Object.entries(node.execution_readiness?.providers ?? {});
          const expired = !probe || probe.expires_at_ms <= now || stale;
          const slots = node.capabilities.execution_capacity?.slots;
          const capacityAvailable = slots === undefined || (node.execution_occupied ?? slots) < slots;
          const controlReason = nodeControlUnavailable(node, nodes.data.cluster.local_instance_id, stale);
          const owned = sessions.data?.sessions.filter(s => s.instance_id === node.instance_id) ?? [];
          return <article key={node.instance_id} className="rounded-lg border border-th-border p-3 min-w-0 space-y-2">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="font-semibold break-all">{node.instance_id}</h3>
              <StatusBadge tone={!stale && node.status === "online" ? "healthy" : "warning"}>{stale ? "STALE" : node.status ?? "unknown"}</StatusBadge>
            </div>
            <p className="text-xs text-th-text-muted">{node.effective_role ?? "unknown"} · {probe ? `${probe.os} / ${probe.arch} · ${probe.runtime_profile}` : tr("실행 정보 대기", "Awaiting execution evidence")}</p>
            <p className="text-xs">{tr("지원 backend", "Available backends")}: {probe?.backends.join(", ") || "—"}</p>
            {reports.length === 0 && <p className="text-xs text-amber-400">{tr("Provider 준비 상태 미확인", "Provider readiness unknown")}</p>}
            {reports.map(([provider, report]) => <div key={provider} className="text-xs flex flex-wrap gap-2 items-center">
              <strong>{provider}</strong><StatusBadge tone={report.eligible && !expired && capacityAvailable ? "healthy" : "warning"}>{report.eligible && !expired ? capacityAvailable ? tr("신규 실행 가능", "Ready for new work") : tr("실행 용량 대기", "Waiting for capacity") : tr("실행 보류", "Not ready")}</StatusBadge>
              <span>{(expired ? ["execution_evidence_stale"] : report.reasons).map(reason => reasons[reason]?.[isKo ? 0 : 1] ?? reason).join(" · ")}</span>
            </div>)}
            <p className="text-xs text-th-text-muted">{tr("계정의 원격 인증·quota는 미검증입니다.", "Remote account authentication and quota are unverified.")}</p>
            <p className="text-xs">{tr("중앙 제어", "Central control")}: {node.instance_id === nodes.data.cluster.local_instance_id ? tr("현재 노드", "Local node") : !controlReason ? tr("인증·노드 식별 확인", "Authentication and node identity verified") : node.forwarding_diagnostics?.configured ? tr("전달 연결 확인 필요", "Forwarding connection requires verification") : tr("전달 설정 필요", "Forwarding configuration required")}</p>
            {node.forwarding_diagnostics?.configured && controlReason === "unreachable" && <p className="text-xs text-amber-400">{node.forwarding_diagnostics.reachability_status}</p>}
            <p className="text-xs text-th-text-muted">{tr("전달 처리 중 dispatch", "Dispatch deliveries in progress")}: {node.active_dispatch_count ?? "—"}</p>
            <p className="text-xs">{tr("실행 중", "Executing")}: {node.execution_active ?? "—"} · {tr("예약 포함 점유 / 용량", "Occupied including reservations / capacity")}: {node.execution_occupied ?? "—"} / {node.capabilities.execution_capacity?.slots ?? "—"}</p>
            {sessions.isError && <p className="text-xs text-amber-400">{tr("세션 소유권 갱신 실패", "Session ownership refresh failed")}</p>}
            {owned.slice(0, 20).map(session => <div key={session.id} className="border-t border-th-border pt-2 space-y-2">
              <p className="text-xs break-all">{session.name || session.session_key} · {session.provider} · {session.status}</p>
              <div className="flex flex-wrap gap-2">
                <button className="btn sm" type="button" disabled={Boolean(controlReason) || sessions.isError} onClick={() => setSelected(session)}>{tr("출력 보기", "View output")}</button>
                <button className="btn sm" type="button" disabled={Boolean(controlReason) || sessions.isError || stop.isPending} onClick={() => setConfirmStop(session.session_key)}>{tr("실행 중지", "Stop execution")}</button>
                {confirmStop === session.session_key && <><span className="text-xs self-center">{tr("이 세션의 실행을 중지합니다.", "Stop this session's execution.")}</span>
                  <button className="btn sm" type="button" disabled={Boolean(controlReason) || sessions.isError || stop.isPending} onClick={() => stop.mutate(session.session_key)}>{tr("중지 확인", "Confirm stop")}</button>
                  <button className="btn sm" type="button" onClick={() => setConfirmStop(null)}>{tr("닫기", "Dismiss")}</button></>}
              </div>
              {controlReason && <p className="text-xs text-amber-400">{controlReason === "stale" ? tr("최신 노드 상태 확인 후 제어할 수 있습니다.", "Controls require a fresh node snapshot.") : controlReason === "offline" ? tr("오프라인 노드입니다.", "Node is offline.") : controlReason === "unreachable" ? tr("전달 경로의 인증과 대상 노드 확인이 필요합니다.", "Forwarding authentication and target identity must be verified.") : tr("신뢰할 전달 주소를 먼저 설정해야 합니다.", "Configure a trusted forwarding origin first.")}</p>}
            </div>)}
          </article>;
        })}
      </div>
      {stop.isError && <p role="alert" className="text-amber-400">{stop.error.message}</p>}
      {selected && <div className="rounded-lg border border-th-border p-3 min-w-0 space-y-2" aria-label={tr("세션 출력", "Session output")}>
        <div className="flex flex-wrap items-center justify-between gap-2"><strong className="break-all">{selected.name || selected.session_key}</strong><button className="btn sm" type="button" onClick={() => setSelected(null)}>{tr("출력 닫기", "Close output")}</button></div>
        {!canControlSelected && <p role="status">{tr("소유 노드 상태를 다시 확인해야 합니다.", "Owner node status must be refreshed.")}</p>}
        {output.isError && <p role="alert" className="text-amber-400">{output.error.message}</p>}
        {output.isFetching && !output.data && <p>{tr("출력 불러오는 중…", "Loading output…")}</p>}
        {output.data && <><p className="text-xs text-th-text-muted">{output.data.backend} · {output.data.output_format} · {new Date(output.data.captured_at_ms).toLocaleTimeString()}</p>
          {!output.data.available && <p>{output.data.unavailable_reason || tr("출력에 연결할 수 없습니다.", "Output is unavailable.")}</p>}
          <pre className="max-h-80 overflow-auto whitespace-pre-wrap break-words text-xs" style={{ overflowWrap: "anywhere" }}>{output.data.recent_output}</pre></>}
      </div>}
    </div>
  </section>;
}
