import { useCallback, useEffect, useState, type ReactNode } from "react";
import { RefreshCw } from "lucide-react";
import type { Department, DispatchedSession } from "../../types";
import { getProviderMeta } from "../../app/providerTheme";
import { formatRelativeTime } from "../../app/shellFormatting";
import { Button } from "../../kit/free/button-clean";
import { CodeBlock } from "../../kit/free/code-block-clean";
import { ContextCompaction } from "../../kit/pro/context-compaction";
import { Cap } from "../common/KitPrimitives";
import type { Translator } from "../agent-manager/types";
import { fetchSessionOutput, normalizeTimestamp, sessionStateText, type SessionOutputSnapshot } from "./SessionPanelModel";

/** Infers model context window size from model identifier, defaulting to 200K. */
export function inferSessionContextLimit(model: string | null | undefined): number {
  const normalized = (model ?? "").toLowerCase();
  if (/\[1m\]|-1m\b|\b1m\b|1000k|gemini/.test(normalized)) return 1_000_000;
  if (/256k|qwen/.test(normalized)) return 256_000;
  if (/128k|gpt-4o\b/.test(normalized)) return 128_000;
  return 200_000;
}

/** What the server sends about one unlinked session, under its row: state, Provider, summary, signals, key and live output preview. */
export function SessionInfoCard({ id, session: s, tr, isKo }: {
  id: string; session: DispatchedSession; departments?: Department[]; locale?: string; tr: Translator; isKo: boolean;
}) {
  const [output, setOutput] = useState<SessionOutputSnapshot | null | undefined>(undefined);
  const [loadingOutput, setLoadingOutput] = useState(false);

  const loadOutput = useCallback((signal?: AbortSignal) => {
    setLoadingOutput(true);
    fetchSessionOutput(s.id, 80, signal)
      .then((res) => { if (!signal?.aborted) setOutput(res); })
      .catch(() => { if (!signal?.aborted) setOutput(null); })
      .finally(() => { if (!signal?.aborted) setLoadingOutput(false); });
  }, [s.id]);

  useEffect(() => {
    const ctrl = new AbortController();
    loadOutput(ctrl.signal);
    return () => ctrl.abort();
  }, [loadOutput]);

  const lastSeen = normalizeTimestamp(s.last_seen_at), connected = normalizeTimestamp(s.connected_at);
  const at = (time: number) => new Date(time).toLocaleString(isKo ? "ko-KR" : "en-US");
  const providerLabel = getProviderMeta(s.provider).label;
  const rows = ([
    [tr("상태", "State"), `${sessionStateText(s, tr)} (${s.status})`],
    ["Provider", providerLabel],
    [tr("모델", "Model"), s.model],
    [tr("세션 요약", "Summary"), s.session_info, "whitespace-pre-line"],
    [tr("마지막 신호", "Last signal"), lastSeen && `${formatRelativeTime(lastSeen, isKo)} · ${at(lastSeen)}`],
    [tr("접속", "Connected"), connected && at(connected)],
    [tr("세션 ID", "Session ID"), s.id, "font-mono"],
    [tr("세션 키", "Session key"), s.session_key, "font-mono text-xs break-all"],
  ] satisfies Array<[string, ReactNode, string?]>).filter(([, value]) => value);

  const outputText = output?.available ? output.recent_output.trim() : "";
  const hasTokenMetrics = typeof s.tokens === "number" && Number.isFinite(s.tokens) && s.tokens > 0;

  return (
    <div id={id} className="flex min-w-0 flex-col gap-3 border-t border-(--line-soft) bg-(--bg-1) px-3 py-3 text-[13px] leading-5 sm:px-4">
      <dl className="m-0 grid min-w-0 grid-cols-[5.5rem_minmax(0,1fr)] gap-x-3 gap-y-1.5">
        {rows.map(([label, value, valueClass]) => (
          <div key={label} className="contents">
            <dt className="text-(--fg-muted)">{label}</dt>
            <dd className={`m-0 min-w-0 text-(--fg) [overflow-wrap:anywhere] ${valueClass ?? ""}`}>{value}</dd>
          </div>
        ))}
      </dl>
      {hasTokenMetrics ? (
        <div data-testid={`session-context-${s.id}`} className="min-w-0">
          <ContextCompaction
            title={tr("컨텍스트 사용량", "Context usage")}
            model={s.model || providerLabel}
            limit={inferSessionContextLimit(s.model)}
            threshold={0.8}
            labels={{
              usedOfLimit: (used, lim) => <><b>{used}</b> {tr(`/ ${lim}`, `of ${lim}`)}</>,
              leftCompactsAt: (left, at) => tr(`남음 ${left} · 압축 기준 ${at}`, `${left} left · compacts at ${at}`),
              empty: tr("압축할 항목이 없어요.", "Nothing to compact."),
              compactingPrefix: tr("압축 중 ", "Compacting "),
            }}
            items={[
              {
                id: `session-${s.id}-active`,
                kind: "assistant",
                label: tr("활성 세션 토큰", "Active session tokens"),
                tokens: s.tokens,
              },
            ]}
          />
        </div>
      ) : null}
      <div className="flex min-w-0 flex-col gap-1.5 border-t border-(--line-soft) pt-2.5" data-testid={`session-output-${s.id}`}>
        <div className="flex items-center justify-between gap-2">
          <Cap>{tr("실시간 출력 미리보기 (최근 80줄)", "Live output preview (last 80 lines)")}</Cap>
          <Button variant="ghost" size="sm" className="shrink-0 [&_.aui-button-clean__label]:whitespace-nowrap" icon={<RefreshCw size={13} aria-hidden />} loading={loadingOutput} onClick={() => loadOutput()}>{tr("새로고침", "Refresh")}</Button>
        </div>
        {output === undefined ? <Cap>{tr("출력을 불러오는 중...", "Loading output...")}</Cap>
          : output === null ? <Cap>{tr("출력을 불러오지 못했어요.", "Could not load session output.")}</Cap>
          : outputText ? <CodeBlock code={outputText} filename={output.backend || "session"} showLineNumbers wrap maxHeight={240} />
          : <Cap>{output.unavailable_reason ? tr(`연결된 터미널 출력 없음 (${output.unavailable_reason})`, `No attached output (${output.unavailable_reason})`) : tr("표시할 최근 출력이 없어요.", "No recent output.")}</Cap>}
      </div>
    </div>
  );
}
