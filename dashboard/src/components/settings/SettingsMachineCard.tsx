import type { ReactNode } from "react";
import { Monitor, Server } from "lucide-react";
import type { ClusterNode } from "../../api/clusterNodes";
import { nodePlatformLabel, nodeRoleLabel, runtimeModeLabel } from "../../lib/nodeLabels";
import { StatusBadge } from "../common/StatusBadge";
import { SettingsMachineResources } from "./SettingsMachineResources";
import {
  machineApiOrigin, machineConnection, machineOnline, machineReadinessReason, machineRole,
} from "./SettingsMachineModel";
import type { SettingsTr } from "./SettingsPanelTypes";

function Detail({ label, children }: { label: string; children: ReactNode }) {
  return <div className="min-w-0">
    <dt className="text-xs text-th-text-muted">{label}</dt>
    <dd className="mt-1 break-words text-sm [overflow-wrap:anywhere]">{children}</dd>
  </div>;
}

export function machineTimestamp(value: string | number | null | undefined, tr: SettingsTr): string {
  if (value == null) return tr("미확인", "Unknown");
  const date = new Date(value);
  return Number.isFinite(date.getTime()) ? date.toLocaleString(tr("ko-KR", "en-US")) : tr("미확인", "Unknown");
}

type Props = {
  node: ClusterNode;
  localId?: string | null;
  leaseTtlSeconds?: number | null;
  stale: boolean;
  sessionCountsUnavailable: boolean;
  now: number;
  tr: SettingsTr;
};

export function SettingsMachineCard({ node, localId, leaseTtlSeconds, stale, sessionCountsUnavailable, now, tr }: Props) {
  const role = machineRole(node.effective_role);
  const connection = machineConnection(node, localId, stale, now, leaseTtlSeconds, tr);
  const online = !stale && machineOnline(node, now, leaseTtlSeconds);
  const probe = node.capabilities.execution_readiness;
  const probeCurrent = !stale && online && Boolean(probe && probe.expires_at_ms > now);
  const slots = node.capabilities.execution_capacity?.slots;
  const occupied = node.execution_occupied;
  const capacityKnown = slots != null && occupied != null;
  const capacityAvailable = capacityKnown && occupied < slots;
  const installedProviders = Object.entries(probe?.providers ?? {}).filter(([, evidence]) =>
    evidence.cli_installed ?? evidence.cli_usable);
  const origin = machineApiOrigin(node.api_base_url);
  const unknown = tr("미확인", "Unknown");
  const DeviceIcon = role === "Hub" ? Server : Monitor;

  return <article className="min-w-0 rounded-2xl border border-th-border bg-th-bg-surface p-4 sm:p-5"
    data-testid={`machine-node-${node.instance_id}`} aria-label={node.hostname || node.instance_id}>
    <div className="flex flex-wrap items-start justify-between gap-3">
      <div className="flex min-w-0 items-start gap-3">
        <DeviceIcon size={22} className="mt-1 shrink-0 text-th-text-muted" aria-hidden />
        <div className="min-w-0">
          <h3 className="break-words font-semibold [overflow-wrap:anywhere]">{node.hostname || node.instance_id}</h3>
          <div className="mt-2 flex flex-wrap gap-2">
            <StatusBadge tone={role === "Hub" ? "info" : "idle"}>{role || nodeRoleLabel(node.effective_role, tr)}</StatusBadge>
            <StatusBadge tone={online ? "healthy" : "warning"}>
              {stale ? tr("정보 만료", "Stale") : online ? tr("Heartbeat 정상", "Heartbeat online") : tr("Heartbeat 미확인", "Heartbeat unavailable")}
            </StatusBadge>
          </div>
        </div>
      </div>
      <StatusBadge tone={connection.tone}>{connection.label}</StatusBadge>
    </div>
    <p className="mt-3 text-xs leading-5 text-th-text-muted">{connection.detail}</p>
    <p className="mt-1 text-xs text-th-text-muted">{probe ? `${nodePlatformLabel(probe.os, tr)} / ${probe.arch}` : unknown} · {runtimeModeLabel(probe?.runtime_profile, tr)}</p>
    <SettingsMachineResources resources={node.capabilities.machine_resources} stale={stale || !online} now={now} tr={tr} />
    <details className="mt-4 rounded-xl border border-th-border p-3">
      <summary className="cursor-pointer text-xs font-medium">{tr("장치 및 연결 상세", "Device and connection details")}</summary>
    <dl className="mt-3 grid min-w-0 grid-cols-1 gap-x-5 gap-y-3 sm:grid-cols-2">
      <Detail label={tr("장치 ID", "Device ID")}>{node.instance_id}</Detail>
      <Detail label={tr("등록된 API 주소", "Advertised API address")}>{origin || tr("등록 안 됨", "Not advertised")}</Detail>
      <Detail label={tr("프로세스 ID", "Process ID")}>{node.process_id ?? unknown}</Detail>
      <Detail label={tr("마지막 heartbeat", "Last heartbeat")}>{machineTimestamp(node.last_heartbeat_at, tr)}</Detail>
      <Detail label={tr("최초 등록 시각", "First registered")}>{machineTimestamp(node.started_at, tr)}</Detail>
      <Detail label={tr("실행 backend", "Execution backends")}>{probe?.backends.join(", ") || unknown}</Detail>
      <Detail label={tr("장치 라벨", "Device labels")}>{node.labels?.join(", ") || tr("없음", "None")}</Detail>
    </dl>
    </details>
    <div className="mt-4 border-t border-th-border pt-4">
      <h4 className="text-sm font-semibold">{tr("실행 준비 상태", "Execution readiness")}</h4>
      <dl className="mt-3 grid grid-cols-2 gap-3 text-sm sm:grid-cols-4">
        <Detail label={tr("실행 중", "Executing")}>{node.execution_active ?? unknown}</Detail>
        <Detail label={tr("점유 / 용량", "Occupied / capacity")}>{occupied ?? "—"} / {slots ?? "—"}</Detail>
        <Detail label={tr("활성 세션", "Active sessions")}>{sessionCountsUnavailable ? unknown : node.active_session_count ?? unknown}</Detail>
        <Detail label={tr("전달 처리 중", "Dispatches in transit")}>{node.active_dispatch_count ?? unknown}</Detail>
      </dl>
      <p className="mt-3 text-xs text-th-text-muted">{tr("실행 검사 시각", "Execution probe time")}: {machineTimestamp(probe?.observed_at_ms, tr)}</p>
      <div className="mt-3 space-y-2">
        <h5 className="text-xs font-medium">{tr("설치된 프로바이더 CLI", "Installed provider CLIs")} <span className="text-th-text-muted">{installedProviders.length}</span></h5>
        {installedProviders.length === 0 && <p className="text-sm text-th-text-muted">{probe?.providers
          ? tr("설치가 확인된 프로바이더 CLI가 없습니다.", "No installed provider CLIs were detected.")
          : tr("CLI 설치 정보를 기다리는 중입니다.", "Waiting for CLI installation information.")}</p>}
        {installedProviders.map(([provider, evidence]) => {
          const report = node.execution_readiness?.providers[provider];
          const ready = evidence.cli_usable && report?.eligible && probeCurrent && capacityAvailable;
          const label = !probeCurrent ? tr("검사 갱신 필요", "Probe refresh needed")
            : !evidence.cli_usable ? tr("CLI 확인 필요", "CLI needs attention")
            : !report ? tr("실행 미설정", "Execution not configured")
            : !report.eligible ? tr("실행 보류", "Not ready")
            : !capacityKnown ? tr("용량 미확인", "Capacity unknown")
            : !capacityAvailable ? tr("용량 대기", "Waiting for capacity")
            : tr("신규 실행 가능", "Ready for new work");
          return <div key={provider} className="flex flex-wrap items-center gap-2 text-xs">
            <strong>{provider}</strong><StatusBadge tone={ready ? "healthy" : "warning"}>{label}</StatusBadge>
            <span className="text-th-text-muted">{report?.reasons.map(reason => machineReadinessReason(reason, tr)).join(" · ")}</span>
          </div>;
        })}
      </div>
      <p className="mt-3 text-xs leading-5 text-th-text-muted">{tr(
        "실행 준비 상태는 로컬 CLI·인증 설정 검사 결과입니다.",
        "Readiness checks local CLI and credential configuration.",
      )}</p>
    </div>
  </article>;
}
