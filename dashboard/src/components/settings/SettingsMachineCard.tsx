import type { ReactNode } from "react";
import { Monitor, Server } from "lucide-react";
import type { ClusterNode } from "../../api/clusterNodes";
import { nodePlatformLabel, nodeRoleLabel, runtimeModeLabel } from "../../lib/nodeLabels";
import { Cap, KitCard, Well } from "../common/KitPrimitives";
import { StatusBadge } from "../common/StatusBadge";
import { SettingsMachineResources } from "./SettingsMachineResources";
import { useMachineResourceHistory } from "./useMachineResourceHistory";
import {
  machineApiOrigin, machineConnection, machineOnline, machineReadinessReason, machineRole,
} from "./SettingsMachineModel";
import type { SettingsTr } from "./SettingsPanelTypes";

function Detail({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs leading-4 text-[var(--fg-muted)]">{label}</dt>
      <dd className="m-0 mt-0.5 break-words text-[13px] leading-5 [overflow-wrap:anywhere]">{children}</dd>
    </div>
  );
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
  const history = useMachineResourceHistory(node.instance_id);
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

  return (
    <KitCard
      as="article"
      pad="md"
      style={{ gap: 0 }}
      data-testid={`machine-node-${node.instance_id}`}
      aria-label={node.hostname || node.instance_id}
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex min-w-0 items-start gap-3">
          <Well aria-hidden className="grid size-9 shrink-0 place-items-center rounded-[var(--r-sm)] text-[var(--fg-muted)]">
            <DeviceIcon size={18} />
          </Well>
          <div className="min-w-0">
            <h3 className="m-0 break-words text-sm leading-5 font-medium [overflow-wrap:anywhere]">{node.hostname || node.instance_id}</h3>
            <div className="mt-2 flex flex-wrap gap-2">
              <StatusBadge tone={role === "Hub" ? "info" : "idle"}>{role || nodeRoleLabel(node.effective_role, tr)}</StatusBadge>
              <StatusBadge tone={online ? "healthy" : "warning"}>
                {stale ? tr("정보 만료", "Stale") : online ? tr("연결 신호 정상", "Heartbeat online") : tr("연결 신호 없음", "Heartbeat unavailable")}
              </StatusBadge>
            </div>
          </div>
        </div>
        <StatusBadge tone={connection.tone}>{connection.label}</StatusBadge>
      </div>
      <Cap className="mt-3 block leading-5">{connection.detail}</Cap>
      <Cap className="mt-1 block">{probe ? `${nodePlatformLabel(probe.os, tr)} / ${probe.arch}` : unknown} · {runtimeModeLabel(probe?.runtime_profile, tr)}</Cap>
      <SettingsMachineResources resources={node.capabilities.machine_resources} history={history.data ?? []} stale={stale || !online} now={now} tr={tr} />
      {history.isError ? (
        <Cap className="mt-2 block">
          {tr("저장된 이력을 불러오지 못했어요. 다음에 다시 시도해요.", "Saved history is unavailable. It will be retried on the next refresh.")}
        </Cap>
      ) : null}
      <details className="mt-4 rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-1)] px-3 py-1">
        <summary className="flex min-h-11 cursor-pointer items-center text-xs font-medium text-[var(--fg-dim)] hover:text-[var(--fg)]">
          {tr("장치 및 연결 상세", "Device and connection details")}
        </summary>
        <dl className="m-0 mt-1 mb-3 grid min-w-0 grid-cols-1 gap-x-5 gap-y-3 @min-[37.5rem]:grid-cols-2">
          <Detail label={tr("장치 ID", "Device ID")}>{node.instance_id}</Detail>
          <Detail label={tr("등록된 API 주소", "Advertised API address")}>{origin || tr("등록 안 됨", "Not advertised")}</Detail>
          <Detail label={tr("프로세스 ID", "Process ID")}>{node.process_id ?? unknown}</Detail>
          <Detail label={tr("마지막 연결 신호", "Last heartbeat")}>{machineTimestamp(node.last_heartbeat_at, tr)}</Detail>
          <Detail label={tr("최초 등록 시각", "First registered")}>{machineTimestamp(node.started_at, tr)}</Detail>
          <Detail label={tr("실행 방식", "Execution backends")}>{probe?.backends.join(", ") || unknown}</Detail>
          <Detail label={tr("장치 라벨", "Device labels")}>{node.labels?.join(", ") || tr("없음", "None")}</Detail>
        </dl>
      </details>
      <div className="mt-4 border-t border-[var(--line-soft)] pt-4">
        <h4 className="m-0 text-[13px] leading-5 font-medium">{tr("실행 준비 상태", "Execution readiness")}</h4>
        <dl className="m-0 mt-3 grid grid-cols-2 gap-3 @min-[37.5rem]:grid-cols-4">
          <Detail label={tr("실행 중", "Executing")}>{node.execution_active ?? unknown}</Detail>
          <Detail label={tr("점유 / 용량", "Occupied / capacity")}>{occupied ?? "—"} / {slots ?? "—"}</Detail>
          <Detail label={tr("활성 세션", "Active sessions")}>{sessionCountsUnavailable ? unknown : node.active_session_count ?? unknown}</Detail>
          <Detail label={tr("전달 처리 중", "Dispatches in transit")}>{node.active_dispatch_count ?? unknown}</Detail>
        </dl>
        <Cap className="mt-3 block">{tr("실행 검사 시각", "Execution probe time")}: {machineTimestamp(probe?.observed_at_ms, tr)}</Cap>
        <div className="mt-3 space-y-2">
          <h5 className="m-0 text-xs font-medium">{tr("설치된 프로바이더 CLI", "Installed provider CLIs")} <span className="text-[var(--fg-muted)]">{installedProviders.length}</span></h5>
          {installedProviders.length === 0 ? (
            <p className="m-0 text-[13px] text-[var(--fg-muted)]">
              {probe?.providers
                ? tr("설치가 확인된 프로바이더 CLI가 없습니다.", "No installed provider CLIs were detected.")
                : tr("CLI 설치 정보를 기다리는 중입니다.", "Waiting for CLI installation information.")}
            </p>
          ) : null}
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
            return (
              <div key={provider} className="flex flex-wrap items-center gap-2 text-xs">
                <strong>{provider}</strong>
                <StatusBadge tone={ready ? "healthy" : "warning"}>{label}</StatusBadge>
                <span className="text-[var(--fg-muted)]">{report?.reasons.map((reason) => machineReadinessReason(reason, tr)).join(" · ")}</span>
              </div>
            );
          })}
        </div>
        <Cap className="mt-3 block leading-5">
          {tr("실행 준비 상태는 이 장치의 CLI와 로그인 설정을 검사한 결과예요.", "Readiness checks local CLI and credential configuration.")}
        </Cap>
      </div>
    </KitCard>
  );
}
