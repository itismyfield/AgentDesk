import type { ClusterNode } from "../../api/clusterNodes";
import type { SettingsTr } from "./SettingsPanelTypes";
import type { SystemHealthTone } from "../../theme/statusTokens";

// Match the existing cluster view; three missed polls make its snapshot stale.
export const MACHINE_REFRESH_INTERVAL_MS = 5_000;
export const MACHINE_SNAPSHOT_MAX_AGE_MS = MACHINE_REFRESH_INTERVAL_MS * 3;
export const MACHINE_CLOCK_INTERVAL_MS = 1_000;
const MILLISECONDS_PER_SECOND = 1_000;

export function machineRole(role: string | null | undefined): "Hub" | "Runner" | null {
  if (role === "hub" || role === "leader") return "Hub";
  if (role === "runner" || role === "worker") return "Runner";
  return null;
}

export function machineOnline(node: ClusterNode, now: number, leaseTtlSeconds?: number | null): boolean {
  if (node.status !== "online") return false;
  const heartbeat = Date.parse(node.last_heartbeat_at ?? "");
  return !leaseTtlSeconds || !Number.isFinite(heartbeat)
    || now - heartbeat < leaseTtlSeconds * MILLISECONDS_PER_SECOND;
}

export type MachineConnection = { tone: SystemHealthTone; label: string; detail: string };

export function machineConnection(
  node: ClusterNode, localId: string | null | undefined, stale: boolean,
  now: number, leaseTtlSeconds: number | null | undefined, tr: SettingsTr,
): MachineConnection {
  if (stale) return {
    tone: "warning", label: tr("정보 갱신 필요", "Snapshot stale"),
    detail: tr("마지막 조회 결과입니다. 현재 연결 상태를 다시 확인해야 합니다.", "This is the last snapshot. Current connectivity needs a fresh check."),
  };
  if (!machineOnline(node, now, leaseTtlSeconds)) return {
    tone: node.status === "online" || node.status === "offline" ? "critical" : "unknown",
    label: node.status === "online" || node.status === "offline" ? tr("오프라인", "Offline") : tr("상태 미확인", "Status unknown"),
    detail: tr("유효한 온라인 heartbeat가 확인되지 않았습니다.", "No current online heartbeat has been confirmed."),
  };
  if (node.instance_id === localId) return {
    tone: "healthy", label: tr("현재 서버", "Current server"),
    detail: tr("이 대시보드의 API를 제공하는 머신입니다.", "This machine serves the API used by this dashboard."),
  };
  const forwarding = node.forwarding_diagnostics;
  if (!forwarding?.configured) return {
    tone: "warning", label: tr("연결 설정 필요", "Connection setup needed"),
    detail: tr("Heartbeat는 수신 중이지만 신뢰할 원격 전달 경로가 설정되지 않았습니다.", "Heartbeats are arriving, but a trusted remote forwarding route is not configured."),
  };
  if (!forwarding.trust_validated || !forwarding.reachability_verified || (forwarding.expires_at_ms ?? 0) <= now) return {
    tone: "warning", label: tr("연결 확인 필요", "Connection unverified"),
    detail: tr("Heartbeat와 별도로 원격 경로의 인증·대상 노드 식별을 확인해야 합니다.", "Remote authentication and target identity need verification separately from heartbeats."),
  };
  return {
    tone: "healthy", label: tr("연결 확인됨", "Connection verified"),
    detail: tr("현재 서버에서 대상 머신까지 인증과 노드 식별을 확인했습니다.", "Authentication and target identity have been verified from the current server."),
  };
}

export function machineApiOrigin(value: string | null | undefined): string | null {
  if (!value) return null;
  try {
    const url = new URL(value);
    return ["http:", "https:"].includes(url.protocol) ? url.origin : null;
  } catch { return null; }
}

const READINESS_REASONS: Record<string, [string, string]> = {
  node_offline: ["노드 오프라인", "Node offline"],
  execution_evidence_missing: ["실행 검사 대기", "Execution probe pending"],
  execution_evidence_stale: ["실행 검사 만료", "Execution probe expired"],
  provider_not_probed: ["프로바이더 검사 없음", "Provider not probed"],
  provider_cli_unavailable: ["CLI 실행 불가", "CLI unavailable"],
  provider_credentials_missing: ["로컬 인증 설정 없음", "Local credentials missing"],
  intake_poller_stale: ["작업 수신 상태 확인 필요", "Intake progress stale"],
};

export function machineReadinessReason(reason: string, tr: SettingsTr): string {
  const labels = READINESS_REASONS[reason];
  return labels ? tr(...labels) : reason;
}
