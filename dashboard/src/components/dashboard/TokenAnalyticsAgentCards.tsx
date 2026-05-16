import type { ReceiptSnapshotAgentShare } from "../../types";
import type { TFunction } from "./model";
import { dashboardCard } from "./ui";
import TooltipLabel from "../common/TooltipLabel";
import { buildAgentRoiRows } from "./dashboardInsights";
import { LoadingIndicator } from "./TokenAnalyticsCards";
import {
  formatCost,
  formatPercentage,
  formatTokens,
  modelColor,
  type AgentCacheRow,
} from "./tokenAnalyticsModels";

export function AgentSpendCard({
  t,
  agents,
  numberFormatter,
  loading,
}: {
  t: TFunction;
  agents: ReceiptSnapshotAgentShare[];
  numberFormatter: Intl.NumberFormat;
  loading: boolean;
}) {
  const maxCost = Math.max(0.01, ...agents.map((agent) => agent.cost));

  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "에이전트별 비용 비교",
              en: "Agent Cost Comparison",
              ja: "エージェント別コスト比較",
              zh: "按代理比较成本",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "상위 에이전트의 토큰 소비와 비용을 함께 봅니다",
              en: "Compare token volume and spend for the busiest agents",
              ja: "主要エージェントのトークン量とコストを並べて確認します",
              zh: "对比主要代理的 Token 量与成本",
            })}
          </p>
        </div>
        {loading ? (
          <LoadingIndicator
            compact
            label={t({
              ko: "에이전트 비용 비교 갱신 중",
              en: "Refreshing agent cost comparison",
              ja: "エージェント別コスト比較を更新中",
              zh: "刷新代理成本比较中",
            })}
          />
        ) : null}
      </div>

      {agents.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {loading
            ? t({
                ko: "에이전트 사용량을 동기화하는 중입니다",
                en: "Syncing agent usage",
                ja: "エージェント使用量を同期中",
                zh: "正在同步代理使用量",
              })
            : t({
                ko: "에이전트 사용량 데이터가 없습니다",
                en: "No agent usage data",
                ja: "エージェント使用量データがありません",
                zh: "暂无代理使用数据",
              })}
        </div>
      ) : (
        <div
          className="mt-4 space-y-2.5"
          style={{ opacity: loading ? 0.58 : 1 }}
        >
          {agents.map((agent, index) => (
            <div
              key={agent.agent}
              className={dashboardCard.nested}
              style={{
                borderColor: "rgba(255,255,255,0.06)",
                background: "var(--th-bg-surface)",
              }}
            >
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span
                      className="flex h-6 w-6 items-center justify-center rounded-full text-xs font-bold"
                      style={{
                        color: "#0f172a",
                        background: modelColor("default", index),
                      }}
                    >
                      {index + 1}
                    </span>
                    <span
                      className="truncate text-sm font-semibold"
                      style={{ color: "var(--th-text)" }}
                    >
                      {agent.agent}
                    </span>
                  </div>
                  <div
                    className="mt-1 text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {formatTokens(agent.tokens)} tokens ·{" "}
                    {numberFormatter.format(
                      Math.round(agent.percentage * 10) / 10,
                    )}
                    %
                  </div>
                </div>
                <div className="text-right">
                  <div
                    className="text-sm font-bold"
                    style={{ color: "#22c55e" }}
                  >
                    {formatCost(agent.cost)}
                  </div>
                </div>
              </div>

              <div className="mt-3 h-2 rounded-full bg-slate-800/50">
                <div
                  className="h-full rounded-full"
                  style={{
                    width: `${Math.max(6, (agent.cost / maxCost) * 100)}%`,
                    background: "linear-gradient(90deg, #22c55e, #14b8a6)",
                  }}
                />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export function AgentCacheHitCard({
  t,
  rows,
  loading,
}: {
  t: TFunction;
  rows: AgentCacheRow[];
  loading: boolean;
}) {
  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "에이전트별 캐시 히트율",
              en: "Agent Cache Hit Rate",
              ja: "エージェント別キャッシュヒット率",
              zh: "按代理查看缓存命中率",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "prompt 볼륨이 큰 에이전트를 기준으로 캐시 효율을 비교합니다",
              en: "Compare cache efficiency across the busiest prompt-heavy agents",
              ja: "prompt ボリュームが大きいエージェントを基準にキャッシュ効率を比較します",
              zh: "以 prompt 体量较大的代理为基准比较缓存效率",
            })}
          </p>
        </div>
        {loading ? (
          <LoadingIndicator
            compact
            label={t({
              ko: "에이전트별 캐시 히트율 갱신 중",
              en: "Refreshing agent cache hit rate",
              ja: "エージェント別キャッシュヒット率を更新中",
              zh: "刷新代理缓存命中率中",
            })}
          />
        ) : null}
      </div>

      {rows.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {loading
            ? t({
                ko: "에이전트 캐시 히트율을 동기화하는 중입니다",
                en: "Syncing agent cache hit rate",
                ja: "エージェントキャッシュヒット率を同期中",
                zh: "正在同步代理缓存命中率",
              })
            : t({
                ko: "에이전트 캐시 데이터가 없습니다",
                en: "No agent cache data",
                ja: "エージェントキャッシュデータがありません",
                zh: "暂无代理缓存数据",
              })}
        </div>
      ) : (
        <div
          className="mt-4 space-y-2.5"
          style={{ opacity: loading ? 0.58 : 1 }}
        >
          {rows.map((row, index) => (
            <div
              key={row.id}
              className={dashboardCard.nested}
              style={{
                borderColor: "rgba(255,255,255,0.06)",
                background: "var(--th-bg-surface)",
              }}
            >
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span
                      className="flex h-6 w-6 items-center justify-center rounded-full text-xs font-bold"
                      style={{
                        color: "#0f172a",
                        background: modelColor("Codex", index),
                      }}
                    >
                      {index + 1}
                    </span>
                    <span
                      className="truncate text-sm font-semibold"
                      style={{ color: "var(--th-text)" }}
                    >
                      {row.label}
                    </span>
                  </div>
                  <div
                    className="mt-1 text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {formatTokens(row.cacheReadTokens)} cache reads ·{" "}
                    {formatCost(row.savings)} saved
                  </div>
                </div>
                <div className="text-right">
                  <div
                    className="text-sm font-bold"
                    style={{ color: "#22c55e" }}
                  >
                    {formatPercentage(row.hitRate)}
                  </div>
                  <div
                    className="text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {formatTokens(row.promptTokens)} prompt
                  </div>
                </div>
              </div>

              <div className="mt-3 h-2 rounded-full bg-slate-800/50">
                <div
                  className="h-full rounded-full"
                  style={{
                    width: `${row.hitRate > 0 ? Math.max(6, row.hitRate) : 0}%`,
                    background: "linear-gradient(90deg, #22c55e, #38bdf8)",
                  }}
                />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export function AgentRoiCard({
  t,
  rows,
  loading,
  numberFormatter,
}: {
  t: TFunction;
  rows: ReturnType<typeof buildAgentRoiRows>;
  loading: boolean;
  numberFormatter: Intl.NumberFormat;
}) {
  const maxScore = Math.max(
    0.01,
    ...rows.map((row) => row.cards_per_million_tokens),
  );

  return (
    <div
      className="rounded-2xl border p-4 sm:p-5"
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <TooltipLabel
            text={t({
              ko: "에이전트 ROI",
              en: "Agent ROI",
              ja: "エージェント ROI",
              zh: "代理 ROI",
            })}
            tooltip={t({
              ko: "선택 기간 동안 완료 카드 수를 토큰 소비량으로 나눈 값입니다. 카드 / 100만 토큰 기준으로 비교합니다.",
              en: "Completed cards divided by token usage in the selected window. Compared as cards per 1M tokens.",
              ja: "選択期間の完了カード数をトークン消費量で割った値です。100万トークンあたりのカード数で比較します。",
              zh: "按所选时间窗用完成卡片数除以 Token 消耗量，并以每 100 万 Token 的完成卡片数比较。",
            })}
            className="text-sm font-semibold"
          />
          <p className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "완료 카드 수와 토큰 사용량을 함께 보며 효율이 높은 담당자를 찾습니다",
              en: "Compare completed cards against token volume to spot efficient agents",
              ja: "完了カード数とトークン量を合わせて見て、効率の高い担当者を見つけます",
              zh: "将完成卡片数与 Token 用量一起比较，找出效率更高的代理",
            })}
          </p>
        </div>
        <span
          className="rounded-full px-3 py-1 text-xs font-semibold"
          style={{ color: "#38bdf8", background: "rgba(56,189,248,0.12)" }}
        >
          {numberFormatter.format(
            rows.reduce((sum, row) => sum + row.completed_cards, 0),
          )}{" "}
          {t({ ko: "완료", en: "done", ja: "完了", zh: "完成" })}
        </span>
      </div>

      {loading && rows.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {t({
            ko: "ROI 지표를 계산하는 중입니다",
            en: "Calculating ROI",
            ja: "ROI を計算中",
            zh: "正在计算 ROI",
          })}
        </div>
      ) : rows.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {t({
            ko: "선택 기간에 계산할 ROI 데이터가 없습니다",
            en: "No ROI data for this window",
            ja: "この期間の ROI データがありません",
            zh: "当前时间窗暂无 ROI 数据",
          })}
        </div>
      ) : (
        <div className="mt-4 space-y-2.5">
          {rows.map((row, index) => (
            <div
              key={row.id}
              className="rounded-xl border px-3 py-3"
              style={{
                borderColor: "rgba(255,255,255,0.06)",
                background: "var(--th-bg-surface)",
              }}
            >
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span
                      className="flex h-6 w-6 items-center justify-center rounded-full text-xs font-bold"
                      style={{
                        color: "#0f172a",
                        background: modelColor("Codex", index),
                      }}
                    >
                      {index + 1}
                    </span>
                    <span
                      className="truncate text-sm font-semibold"
                      style={{ color: "var(--th-text)" }}
                    >
                      {row.label}
                    </span>
                  </div>
                  <div
                    className="mt-1 text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {numberFormatter.format(row.completed_cards)}{" "}
                    {t({ ko: "카드", en: "cards", ja: "カード", zh: "卡片" })}
                    {" · "}
                    {formatTokens(row.tokens)} tokens
                    {" · "}
                    {formatCost(row.cost)}
                  </div>
                </div>
                <div className="text-right">
                  <div
                    className="text-sm font-bold"
                    style={{ color: "#38bdf8" }}
                  >
                    {row.cards_per_million_tokens.toFixed(2)}
                  </div>
                  <div
                    className="text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {t({
                      ko: "카드 / 1M",
                      en: "cards / 1M",
                      ja: "カード / 1M",
                      zh: "卡片 / 1M",
                    })}
                  </div>
                </div>
              </div>

              <div className="mt-3 h-2 rounded-full bg-slate-800/50">
                <div
                  className="h-full rounded-full"
                  style={{
                    width: `${Math.max(6, (row.cards_per_million_tokens / maxScore) * 100)}%`,
                    background: "linear-gradient(90deg, #38bdf8, #818cf8)",
                  }}
                />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
