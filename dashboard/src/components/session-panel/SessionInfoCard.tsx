import type { ReactNode } from "react";
import type { Department, DispatchedSession } from "../../types";
import { localeName } from "../../i18n";
import { getProviderMeta } from "../../app/providerTheme";
import { formatRelativeTime } from "../../app/shellFormatting";
import { agentStateLabel } from "../home/homeModel";
import type { Translator } from "../agent-manager/types";
import { normalizeTimestamp, sessionState } from "./SessionPanelModel";

/** What the server sends about one unlinked session, under its row: state, Provider, summary, signals and the key. */
export function SessionInfoCard({ id, session: s, departments, locale, tr, isKo }: {
  id: string;
  session: DispatchedSession;
  departments: Department[];
  locale: string;
  tr: Translator;
  isKo: boolean;
}) {
  const department = departments.find((entry) => entry.id === s.department_id);
  const lastSeen = normalizeTimestamp(s.last_seen_at);
  const connected = normalizeTimestamp(s.connected_at);
  const at = (time: number) => new Date(time).toLocaleString(isKo ? "ko-KR" : "en-US");
  // Rows the server left empty are dropped, never filled in.
  const rows = ([
    [tr("상태", "State"), `${agentStateLabel(sessionState(s), tr)} (${s.status})`],
    ["Provider", getProviderMeta(s.provider).label],
    [tr("모델", "Model"), s.model],
    [tr("세션 요약", "Summary"), s.session_info, "whitespace-pre-line"],
    [tr("마지막 신호", "Last signal"), lastSeen && `${formatRelativeTime(lastSeen, isKo)} · ${at(lastSeen)}`],
    [tr("접속", "Connected"), connected && at(connected)],
    [tr("부서", "Department"), department && localeName(locale, department)],
    [tr("세션 ID", "Session ID"), s.id, "font-mono"],
    [tr("세션 키", "Session key"), s.session_key, "font-mono text-xs break-all"],
  ] satisfies Array<[string, ReactNode, string?]>).filter(([, value]) => value);
  return (
    <dl id={id} className="m-0 grid min-w-0 grid-cols-[5.5rem_minmax(0,1fr)] gap-x-3 gap-y-1.5 border-t border-(--line-soft) bg-(--bg-1) px-3 py-3 text-[13px] leading-5 sm:px-4">
      {rows.map(([label, value, valueClass]) => (
        <div key={label} className="contents">
          <dt className="text-(--fg-muted)">{label}</dt>
          <dd className={`m-0 min-w-0 text-(--fg) [overflow-wrap:anywhere] ${valueClass ?? ""}`}>{value}</dd>
        </div>
      ))}
    </dl>
  );
}
