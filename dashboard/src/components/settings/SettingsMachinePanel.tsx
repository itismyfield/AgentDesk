import { useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";
import { useMachineNodes } from "./useMachineNodes";
import { StatusBadge } from "../common/StatusBadge";
import { SettingsMachineCard, machineTimestamp } from "./SettingsMachineCard";
import {
  MACHINE_CLOCK_INTERVAL_MS, MACHINE_SNAPSHOT_MAX_AGE_MS,
  machineOnline, machineRole,
} from "./SettingsMachineModel";
import type { SettingsTr } from "./SettingsPanelTypes";

export function SettingsMachinePanel({ tr }: { tr: SettingsTr }) {
  const nodes = useMachineNodes();
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), MACHINE_CLOCK_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, []);
  const stale = nodes.isError || !nodes.dataUpdatedAt || now - nodes.dataUpdatedAt >= MACHINE_SNAPSHOT_MAX_AGE_MS;
  const cluster = nodes.data?.cluster;
  const machines = cluster?.enabled ? [...(nodes.data?.nodes ?? [])].sort((a, b) => {
    const hubOrder = Number(machineRole(b.effective_role) === "Hub") - Number(machineRole(a.effective_role) === "Hub");
    return hubOrder || (a.hostname || a.instance_id).localeCompare(b.hostname || b.instance_id);
  }) : [];
  const online = machines.filter(node => machineOnline(node, now, cluster?.lease_ttl_secs)).length;

  return <section className="min-w-0 space-y-4" data-testid="settings-machine-panel" aria-label={tr("Hub·Runner 머신", "Hub and Runner machines")}>
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="min-w-0 space-y-1">
        <p className="text-xs text-th-text-muted">{tr("마지막 조회", "Last updated")}: {machineTimestamp(nodes.dataUpdatedAt || null, tr)}</p>
        <p className="text-xs text-th-text-muted">{tr("5초마다 자동 갱신합니다.", "Refreshes automatically every 5 seconds.")}</p>
      </div>
      <button type="button" className="inline-flex min-h-[44px] items-center justify-center gap-2 rounded-xl border border-th-border px-3 py-2 text-sm disabled:opacity-50" disabled={nodes.isFetching}
        onClick={() => void nodes.refetch()}>
        <RefreshCw size={14} aria-hidden className={nodes.isFetching ? "animate-spin" : undefined} />
        {nodes.isFetching ? tr("확인 중…", "Checking…") : tr("상태 새로고침", "Refresh status")}
      </button>
    </div>
    {nodes.isPending && <p role="status">{tr("머신 정보를 불러오는 중…", "Loading machine details…")}</p>}
    {nodes.isError && <p role="alert" className="rounded-xl border border-amber-400/30 bg-amber-400/10 p-3 text-sm">{nodes.data
      ? tr("머신 상태 갱신에 실패했습니다. 마지막 조회 정보를 표시하며 연결 상태는 미확인으로 처리합니다.", "Machine refresh failed. Showing the last snapshot with connectivity marked unverified.")
      : tr("머신 정보를 불러오지 못했습니다. 서버 연결을 확인한 뒤 다시 시도해 주세요.", "Unable to load machines. Check the server connection and retry.")}</p>}
    {cluster && !cluster.enabled && <p role="status" className="rounded-xl border border-th-border p-4">{tr(
      "현재 서버는 단독 운영 중입니다. Hub·Runner 클러스터가 활성화되어 있지 않습니다.",
      "This server runs standalone. The Hub and Runner cluster is not enabled.",
    )}</p>}
    {cluster?.enabled && <>
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <StatusBadge tone={stale ? "warning" : "info"}>{stale ? tr("정보 갱신 필요", "Snapshot stale") : tr(`온라인 ${online} / 전체 ${machines.length}`, `Online ${online} / total ${machines.length}`)}</StatusBadge>
        <span>Hub {machines.filter(node => machineRole(node.effective_role) === "Hub").length}</span>
        <span>Runner {machines.filter(node => machineRole(node.effective_role) === "Runner").length}</span>
      </div>
      <div className="flex flex-wrap gap-x-5 gap-y-2 text-xs text-th-text-muted">
        <span className="break-all">{tr("현재 서버 ID", "Current server ID")}: {cluster.local_instance_id || tr("미확인", "Unknown")}</span>
        {cluster.heartbeat_interval_secs != null && <span>{tr("Heartbeat 주기", "Heartbeat interval")}: {cluster.heartbeat_interval_secs}{tr("초", "s")}</span>}
        {cluster.lease_ttl_secs != null && <span>{tr("오프라인 판정 시간", "Offline threshold")}: {cluster.lease_ttl_secs}{tr("초", "s")}</span>}
      </div>
      {machines.length === 0 && <p role="status">{tr("등록된 머신이 없습니다. 연결된 장치가 heartbeat를 보내면 여기에 표시됩니다.", "No registered machines. Connected devices appear here after reporting a heartbeat.")}</p>}
      {nodes.data?.session_owner_error && <p role="status" className="text-sm text-amber-400">{tr("활성 세션 수를 조회하지 못했습니다. 장치 연결 정보는 별도로 표시합니다.", "Active session counts are unavailable. Device connectivity is shown separately.")}</p>}
      <div className="grid min-w-0 gap-4 xl:grid-cols-2">
        {machines.map(node => <SettingsMachineCard key={node.instance_id} node={node} localId={cluster.local_instance_id}
          leaseTtlSeconds={cluster.lease_ttl_secs} stale={stale} now={now} tr={tr}
          sessionCountsUnavailable={Boolean(nodes.data?.session_owner_error)} />)}
      </div>
    </>}
  </section>;
}
