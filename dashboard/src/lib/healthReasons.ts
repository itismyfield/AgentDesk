// Shared humanizer and aggregator for server `degraded_reasons` / `startup_degraded_reasons` across Home and Settings.

export interface AggregatedHealthReason {
  groupKey: string;
  label: string;
  summary: string;
  count: number;
  rawKeys: string[];
  rawKey: string;
}

const SNOWFLAKE_RE = /\b\d{15,22}\b/g;

function formatDurationShort(seconds: number, isKo: boolean): string {
  const safe = Math.max(0, Math.round(seconds));
  if (safe >= 86_400) {
    const days = Math.floor(safe / 86_400);
    const hours = Math.floor((safe % 86_400) / 3_600);
    return isKo ? `${days}일 ${hours}시간` : `${days}d ${hours}h`;
  }
  if (safe >= 3_600) {
    const hours = Math.floor(safe / 3_600);
    const minutes = Math.floor((safe % 3_600) / 60);
    return isKo ? `${hours}시간 ${minutes}분` : `${hours}h ${minutes}m`;
  }
  if (safe >= 60) {
    const minutes = Math.floor(safe / 60);
    const rem = safe % 60;
    return isKo ? `${minutes}분 ${rem}초` : `${minutes}m ${rem}s`;
  }
  return isKo ? `${safe}초` : `${safe}s`;
}

function isReadableChannelOrRole(value: string): boolean {
  const trimmed = value.trim();
  return Boolean(trimmed) && trimmed !== "~" && !/^\d+$/.test(trimmed) && !/\d{15,}/.test(trimmed);
}

function stripSnowflakes(value: string): string {
  return value.replace(SNOWFLAKE_RE, "").replace(/[:_-]{2,}/g, ":").replace(/^[:_\s-]+|[:_\s-]+$/g, "");
}

/** Humanizes one raw degraded-reason key without leaking Discord channel snowflakes into primary copy. */
export function describeDegradedReason(reason: string, isKo: boolean): string {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const trimmed = reason.trim();
  if (!trimmed) return "";

  if (trimmed === "db_slow") return tr("데이터베이스 응답 지연", "Database response slow");
  if (trimmed === "db_unavailable") return tr("데이터베이스 연결 안 됨", "Database unavailable");
  if (trimmed === "no_providers_registered") return tr("등록된 프로바이더 없음", "No providers registered");
  if (trimmed === "mailbox_snapshot_unavailable") return tr("메일박스 상태 조회 불가", "Mailbox snapshot unavailable");

  const provider = trimmed.match(/^provider:([^:]+):(.+)$/);
  if (provider) {
    const [, name, detail] = provider;
    if (detail === "disconnected") return tr(`${name} 연결 끊김`, `${name} disconnected`);
    if (detail === "restart_pending") return tr(`${name} 재시작 대기`, `${name} restart pending`);
    if (detail === "reconcile_in_progress") return tr(`${name} 상태 맞추는 중`, `${name} reconciling`);
    const deferred = detail.match(/^deferred_hooks_backlog:(\d+)$/);
    if (deferred) return tr(`${name} 후처리 대기 ${deferred[1]}건`, `${name} deferred hooks ${deferred[1]}`);
    const queue = detail.match(/^pending_queue_depth:(\d+)$/);
    if (queue) return tr(`${name} 처리 대기 ${queue[1]}건`, `${name} queue depth ${queue[1]}`);
    const recovering = detail.match(/^recovering_channels:(\d+)$/);
    if (recovering) return tr(`${name} 채널 ${recovering[1]}개 복구 중`, `${name} recovering ${recovering[1]} channels`);
    const cleanDetail = stripSnowflakes(detail).replaceAll("_", " ");
    return cleanDetail
      ? tr(`${name} 프로바이더 주의 (${cleanDetail})`, `${name} provider issue (${cleanDetail})`)
      : tr(`${name} 프로바이더 주의`, `${name} provider issue`);
  }

  const outbox = trimmed.match(/^dispatch_outbox_oldest_pending_age:(\d+)$/);
  if (outbox) {
    const age = formatDurationShort(Number(outbox[1]), isKo);
    return tr(`전달 대기 ${age}째`, `Dispatch outbox waiting ${age}`);
  }

  const relay = trimmed.match(/^relay_verdict_unknown[_:]([a-z0-9-]+)[_:]\d+$/i);
  if (relay) return tr(`${relay[1]} 응답 전달 확인 안 됨`, `${relay[1]} relay delivery unconfirmed`);

  if (trimmed.startsWith("tui_o:reader_retention_stalled")) {
    return tr("TUI 리더 보존 지연", "TUI reader retention stalled");
  }
  if (trimmed.startsWith("tui_o:boundary_pending")) return tr("TUI 출력 마무리 대기", "TUI output waiting to finish");
  if (trimmed.startsWith("tui_o:unresolved")) return tr("TUI 출력 확인 필요", "TUI output unresolved");
  if (trimmed.startsWith("tui_o:cleanup_retired")) {
    return tr("끝난 TUI 출력 정리 대기", "Retired TUI output awaiting cleanup");
  }
  if (trimmed.startsWith("tui_o:")) return tr("TUI 출력 경보", "TUI output alarm");

  const presence = trimmed.match(/^turn_presence_unknown:([^:]+):([^:]*)/);
  if (presence) {
    const [, providerName, target] = presence;
    if (isReadableChannelOrRole(target)) {
      return tr(`${target} 턴 상태 확인 불가 (${providerName})`, `${target} turn state unknown (${providerName})`);
    }
    return tr(`${providerName} 턴 상태 미확인 세션`, `${providerName} turn state unknown`);
  }

  const leading = stripSnowflakes(trimmed.split(":")[0].replace(/_\d{6,}$/, "")).replaceAll("_", " ").trim();
  return leading || tr("기타 주의 신호", "Other health signal");
}

/** Collapses repeated degraded-reason keys by kind/provider or humanized label while preserving raw keys for tooltips. */
export function aggregateDegradedReasons(reasons: ReadonlyArray<string>, isKo: boolean): AggregatedHealthReason[] {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const uniqueRaw = [...new Set(reasons.map((item) => item.trim()).filter(Boolean))];
  const groups = new Map<string, { kind: "presence" | "generic"; provider?: string; label: string; rawKeys: string[] }>();

  for (const raw of uniqueRaw) {
    const presence = raw.match(/^turn_presence_unknown:([^:]+):([^:]*)/);
    if (presence) {
      const [, providerName] = presence;
      const groupKey = `turn_presence_unknown:${providerName}`;
      const existing = groups.get(groupKey);
      if (existing) {
        existing.rawKeys.push(raw);
      } else {
        groups.set(groupKey, {
          kind: "presence",
          provider: providerName,
          label: tr(`${providerName} 턴 상태 미확인 세션`, `${providerName} turn state unknown`),
          rawKeys: [raw],
        });
      }
      continue;
    }

    const label = describeDegradedReason(raw, isKo);
    if (!label) continue;
    const groupKey = `label:${label}`;
    const existing = groups.get(groupKey);
    if (existing) {
      existing.rawKeys.push(raw);
    } else {
      groups.set(groupKey, { kind: "generic", label, rawKeys: [raw] });
    }
  }

  return [...groups.entries()].map(([groupKey, group]) => {
    const count = group.rawKeys.length;
    const rawKey = group.rawKeys.join(", ");
    if (group.kind === "presence") {
      const providerName = group.provider ?? "";
      const summary = tr(
        `${providerName} 턴 상태 미확인 세션 ${count}개`,
        `${providerName} turn state unknown (${count} ${count === 1 ? "session" : "sessions"})`,
      );
      const singleLabel = count === 1 ? describeDegradedReason(group.rawKeys[0], isKo) : group.label;
      return { groupKey, label: singleLabel, summary, count, rawKeys: group.rawKeys, rawKey };
    }
    const summary = count > 1 ? tr(`${group.label} ${count}건`, `${group.label} (${count})`) : group.label;
    return { groupKey, label: group.label, summary, count, rawKeys: group.rawKeys, rawKey };
  });
}
