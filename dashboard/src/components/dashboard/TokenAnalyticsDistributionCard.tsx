import type { TFunction } from "./model";
import { dashboardBadge, dashboardCard } from "./ui";
import { LoadingIndicator } from "./TokenAnalyticsCards";
import { formatTokens, type ModelSegment } from "./tokenAnalyticsModels";

export function ModelDistributionCard({
  t,
  segments,
  donutBackground,
  totalTokens,
  loading,
}: {
  t: TFunction;
  segments: ModelSegment[];
  donutBackground: string;
  totalTokens: number;
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
              ko: "모델 분포",
              en: "Model Distribution",
              ja: "モデル分布",
              zh: "模型分布",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "Claude/Codex/Gemini/OpenCode/Qwen 모델이 토큰을 어떻게 나눠 쓰는지 확인합니다",
              en: "See how Claude, Codex, Gemini, OpenCode, and Qwen models split token volume",
              ja: "Claude/Codex/Gemini/OpenCode/Qwen モデルのトークン構成を確認します",
              zh: "查看 Claude/Codex/Gemini/OpenCode/Qwen 模型如何分摊 Token 量",
            })}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <span
            className={dashboardBadge.large}
            style={{ color: "#f59e0b", background: "rgba(245,158,11,0.12)" }}
          >
            {formatTokens(totalTokens)}
          </span>
          {loading ? (
            <LoadingIndicator
              compact
              label={t({
                ko: "모델 분포 갱신 중",
                en: "Refreshing model distribution",
                ja: "モデル分布を更新中",
                zh: "刷新模型分布中",
              })}
            />
          ) : null}
        </div>
      </div>

      {segments.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {loading
            ? t({
                ko: "모델 분포를 동기화하는 중입니다",
                en: "Syncing model distribution",
                ja: "モデル分布を同期中",
                zh: "正在同步模型分布",
              })
            : t({
                ko: "모델 분포 데이터가 없습니다",
                en: "No model distribution data",
                ja: "モデル分布データがありません",
                zh: "暂无模型分布数据",
              })}
        </div>
      ) : (
        <div
          className="mt-5 grid gap-5 md:grid-cols-[180px_minmax(0,1fr)] md:items-center"
          style={{ opacity: loading ? 0.58 : 1 }}
        >
          <div className="mx-auto flex w-full max-w-[180px] items-center justify-center">
            <div
              className="relative h-40 w-40 rounded-full"
              style={{ background: donutBackground }}
            >
              <div
                className="absolute inset-[18%] rounded-full border"
                style={{
                  background:
                    "color-mix(in srgb, var(--th-surface) 88%, #0f172a 12%)",
                  borderColor: "rgba(255,255,255,0.06)",
                }}
              />
              <div className="absolute inset-0 flex flex-col items-center justify-center text-center">
                <div
                  className="text-[11px] uppercase tracking-[0.18em]"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  Mix
                </div>
                <div
                  className="mt-1 text-xl font-black"
                  style={{ color: "var(--th-text)" }}
                >
                  {segments.length}
                </div>
              </div>
            </div>
          </div>

          <div className="space-y-2">
            {segments.map((segment) => (
              <div
                key={segment.id}
                className={dashboardCard.nestedCompact}
                style={{
                  borderColor: "rgba(255,255,255,0.06)",
                  background: "var(--th-bg-surface)",
                }}
              >
                <div className="flex items-center justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex items-center gap-2">
                      <span
                        className="h-2.5 w-2.5 rounded-full"
                        style={{ background: segment.color }}
                      />
                      <span
                        className="truncate text-sm font-semibold"
                        style={{ color: "var(--th-text)" }}
                      >
                        {segment.label}
                      </span>
                    </div>
                    <div
                      className="mt-1 text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {segment.provider}
                    </div>
                  </div>
                  <div className="text-right">
                    <div
                      className="text-sm font-bold"
                      style={{ color: "var(--th-text)" }}
                    >
                      {segment.percentage.toFixed(1)}%
                    </div>
                    <div
                      className="text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {formatTokens(segment.tokens)}
                    </div>
                  </div>
                </div>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
