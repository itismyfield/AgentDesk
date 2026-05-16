import { useEffect, useMemo, useState } from "react";

import * as api from "../../api/client";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import {
  readLocalStorageValue,
  writeLocalStorageValue,
} from "../../lib/useLocalStorage";
import type { TFunction } from "./model";
import {
  DEFAULT_BOTTLENECK_THRESHOLDS,
  LONG_BLOCKED_DAYS,
  REVIEW_DELAY_DAYS,
  REWORK_ALERT_THRESHOLD,
  buildBottleneckGroups,
  type BottleneckRow,
  type BottleneckThresholds,
} from "./dashboardInsights";

const BOTTLE_NECK_THRESHOLDS_STORAGE_KEY = STORAGE_KEYS.dashboardBottleneckThresholds;

function formatRelativeAge(days: number, t: TFunction): string {
  if (days <= 0) return t({ ko: "오늘", en: "today", ja: "今日", zh: "今天" });
  return t({
    ko: `${days}일`,
    en: `${days}d`,
    ja: `${days}日`,
    zh: `${days}天`,
  });
}

function sanitizeThreshold(value: number, fallback: number, min = 1, max = 30): number {
  if (!Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, Math.round(value)));
}

function readStoredBottleneckThresholds(): BottleneckThresholds {
  const parsed = readLocalStorageValue<Partial<BottleneckThresholds> | null>(
    BOTTLE_NECK_THRESHOLDS_STORAGE_KEY,
    null,
  );
  if (!parsed || typeof parsed !== "object") {
    return DEFAULT_BOTTLENECK_THRESHOLDS;
  }
  return {
    review_delay_days: sanitizeThreshold(parsed.review_delay_days ?? NaN, REVIEW_DELAY_DAYS),
    long_blocked_days: sanitizeThreshold(parsed.long_blocked_days ?? NaN, LONG_BLOCKED_DAYS),
    rework_alert_threshold: sanitizeThreshold(parsed.rework_alert_threshold ?? NaN, REWORK_ALERT_THRESHOLD, 1, 20),
  };
}

function persistBottleneckThresholds(thresholds: BottleneckThresholds) {
  writeLocalStorageValue(BOTTLE_NECK_THRESHOLDS_STORAGE_KEY, thresholds);
}

// ── Bottleneck Widget ──

interface BottleneckWidgetProps {
  t: TFunction;
}

export function BottleneckWidget({ t }: BottleneckWidgetProps) {
  const [cards, setCards] = useState<api.KanbanCard[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showThresholdControls, setShowThresholdControls] = useState(false);
  const [thresholds, setThresholds] = useState<BottleneckThresholds>(() => readStoredBottleneckThresholds());

  useEffect(() => {
    persistBottleneckThresholds(thresholds);
  }, [thresholds]);

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      if (mounted) setLoading(true);
      try {
        const next = await api.getKanbanCards();
        if (!mounted) return;
        setCards(next);
        setError(null);
      } catch (nextError) {
        if (!mounted) return;
        setError(nextError instanceof Error ? nextError.message : String(nextError));
      } finally {
        if (mounted) setLoading(false);
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, []);

  const groups = useMemo(() => buildBottleneckGroups(cards, Date.now(), thresholds), [cards, thresholds]);
  const totalAlerts = useMemo(() => {
    const ids = new Set<string>();
    for (const row of groups.review_delay) ids.add(row.id);
    for (const row of groups.repeat_rework) ids.add(row.id);
    for (const row of groups.long_blocked) ids.add(row.id);
    return ids.size;
  }, [groups]);

  const updateThreshold = (
    key: keyof BottleneckThresholds,
    nextValue: number,
    fallback: number,
    min = 1,
    max = 30,
  ) => {
    setThresholds((current) => ({
      ...current,
      [key]: sanitizeThreshold(nextValue, fallback, min, max),
    }));
  };

  return (
    <div
      className="rounded-2xl border p-4 sm:p-5"
      style={{
        borderColor: "var(--th-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 91%, #ef4444 9%), var(--th-surface))",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "병목 감지", en: "Bottleneck Detection", ja: "ボトルネック検知", zh: "瓶颈检测" })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "리뷰 지연, 반복 리워크, 장기 블로킹 카드를 바로 추려냅니다",
              en: "Pull delayed reviews, repeated reworks, and long blocks into one action board",
              ja: "レビュー遅延、反復リワーク、長期ブロックを一つのアクションボードに集約します",
              zh: "将审查延迟、反复返工、长期阻塞集中到一个动作面板",
            })}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            className="rounded-full px-3 py-1 text-[11px] font-semibold"
            style={{
              color: "var(--th-text)",
              background: "rgba(148,163,184,0.14)",
              border: "1px solid rgba(148,163,184,0.2)",
            }}
            onClick={() => setShowThresholdControls((current) => !current)}
          >
            {showThresholdControls
              ? t({ ko: "기준 닫기", en: "Hide thresholds", ja: "基準を閉じる", zh: "收起阈值" })
              : t({ ko: "기준 조정", en: "Tune thresholds", ja: "基準調整", zh: "调整阈值" })}
          </button>
          <span
            className="rounded-full px-3 py-1 text-xs font-semibold"
            style={{ color: "#fca5a5", background: "rgba(239,68,68,0.14)" }}
          >
            {totalAlerts} {t({ ko: "경고", en: "alerts", ja: "警告", zh: "警报" })}
          </span>
        </div>
      </div>

      {showThresholdControls ? (
        <div className="mt-4 grid gap-3 rounded-2xl border p-3 text-[11px] sm:grid-cols-3" style={{ borderColor: "rgba(255,255,255,0.08)", background: "rgba(15,23,42,0.18)" }}>
          <label className="flex min-w-0 flex-col gap-1">
            <span style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "리뷰 지연 일수", en: "Review delay days", ja: "レビュー遅延日数", zh: "审查延迟天数" })}
            </span>
            <input
              type="number"
              min={1}
              max={30}
              value={thresholds.review_delay_days}
              onChange={(event) => updateThreshold("review_delay_days", Number(event.target.value), REVIEW_DELAY_DAYS)}
              className="rounded-xl border px-3 py-2 text-sm"
              style={{ borderColor: "rgba(255,255,255,0.1)", background: "var(--th-bg-surface)", color: "var(--th-text)" }}
            />
          </label>
          <label className="flex min-w-0 flex-col gap-1">
            <span style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "장기 블록 일수", en: "Blocked days", ja: "長期ブロック日数", zh: "长期阻塞天数" })}
            </span>
            <input
              type="number"
              min={1}
              max={30}
              value={thresholds.long_blocked_days}
              onChange={(event) => updateThreshold("long_blocked_days", Number(event.target.value), LONG_BLOCKED_DAYS)}
              className="rounded-xl border px-3 py-2 text-sm"
              style={{ borderColor: "rgba(255,255,255,0.1)", background: "var(--th-bg-surface)", color: "var(--th-text)" }}
            />
          </label>
          <label className="flex min-w-0 flex-col gap-1">
            <span style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "리워크 경고 횟수", en: "Rework threshold", ja: "リワーク閾値", zh: "返工阈值" })}
            </span>
            <input
              type="number"
              min={1}
              max={20}
              value={thresholds.rework_alert_threshold}
              onChange={(event) => updateThreshold("rework_alert_threshold", Number(event.target.value), REWORK_ALERT_THRESHOLD, 1, 20)}
              className="rounded-xl border px-3 py-2 text-sm"
              style={{ borderColor: "rgba(255,255,255,0.1)", background: "var(--th-bg-surface)", color: "var(--th-text)" }}
            />
          </label>
        </div>
      ) : null}

      {error ? (
        <div className="mt-4 rounded-2xl border px-3 py-2 text-xs" style={{ borderColor: "rgba(251,191,36,0.28)", background: "rgba(251,191,36,0.12)", color: "#fde68a" }}>
          {cards.length > 0
            ? t({
                ko: `최근 카드 스냅샷을 유지 중이며 새 동기화에 실패했습니다. (${error})`,
                en: `Keeping the last card snapshot because refresh failed. (${error})`,
                ja: `最新同期に失敗したため、直近のカードスナップショットを維持しています。(${error})`,
                zh: `最新同步失败，正在保留最近一次卡片快照。(${error})`,
              })
            : t({
                ko: `칸반 카드를 불러오지 못했습니다. (${error})`,
                en: `Unable to load kanban cards. (${error})`,
                ja: `kanban カードを読み込めませんでした。(${error})`,
                zh: `无法加载 kanban 卡片。(${error})`,
              })}
        </div>
      ) : null}

      {loading && totalAlerts === 0 ? (
        <div className="py-10 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "운영 병목을 확인하는 중입니다", en: "Scanning bottlenecks", ja: "ボトルネックを確認中", zh: "正在扫描瓶颈" })}
        </div>
      ) : (
        <div className="mt-4 grid grid-cols-1 gap-4 xl:grid-cols-3">
          <BottleneckColumn
            title={t({ ko: "리뷰 지연", en: "Review Delay", ja: "レビュー遅延", zh: "审查延迟" })}
            hint={t({
              ko: `${thresholds.review_delay_days}일 이상 review`,
              en: `${thresholds.review_delay_days}+ days in review`,
              ja: `${thresholds.review_delay_days}日以上 review`,
              zh: `review 超过 ${thresholds.review_delay_days} 天`,
            })}
            rows={groups.review_delay}
            emptyLabel={t({ ko: "지연된 review 카드가 없습니다", en: "No delayed review cards", ja: "遅延レビューカードはありません", zh: "暂无延迟审查卡片" })}
            accent="#f59e0b"
            t={t}
          />
          <BottleneckColumn
            title={t({ ko: "반복 리워크", en: "Repeat Rework", ja: "反復リワーク", zh: "重复返工" })}
            hint={t({
              ko: `오늘 완료, ${thresholds.rework_alert_threshold}회 이상 rework`,
              en: `Closed today, ${thresholds.rework_alert_threshold}+ reworks`,
              ja: `本日完了、${thresholds.rework_alert_threshold}回以上リワーク`,
              zh: `今日完成、${thresholds.rework_alert_threshold} 次以上返工`,
            })}
            rows={groups.repeat_rework}
            emptyLabel={t({ ko: "오늘 완료된 반복 리워크 카드가 없습니다", en: "No repeat rework cards closed today", ja: "本日完了の反復リワークカードはありません", zh: "今日暂无完成的重复返工卡片" })}
            accent="#a78bfa"
            t={t}
          />
          <BottleneckColumn
            title={t({ ko: "장기 블로킹", en: "Long Blocked", ja: "長期ブロック", zh: "长期阻塞" })}
            hint={t({
              ko: `${thresholds.long_blocked_days}일 이상 blocked`,
              en: `${thresholds.long_blocked_days}+ days blocked`,
              ja: `${thresholds.long_blocked_days}日以上 blocked`,
              zh: `blocked 超过 ${thresholds.long_blocked_days} 天`,
            })}
            rows={groups.long_blocked}
            emptyLabel={t({ ko: "장기 블로킹 카드는 없습니다", en: "No long blocked cards", ja: "長期ブロックカードはありません", zh: "暂无长期阻塞卡片" })}
            accent="#f87171"
            t={t}
          />
        </div>
      )}
    </div>
  );
}

function BottleneckColumn({
  title,
  hint,
  rows,
  emptyLabel,
  accent,
  t,
}: {
  title: string;
  hint: string;
  rows: BottleneckRow[];
  emptyLabel: string;
  accent: string;
  t: TFunction;
}) {
  const [expanded, setExpanded] = useState(false);
  const visibleRows = expanded ? rows : rows.slice(0, 4);
  const hiddenCount = Math.max(0, rows.length - visibleRows.length);

  return (
    <div
      className="rounded-2xl border p-3"
      style={{ borderColor: `${accent}33`, background: "rgba(15,23,42,0.18)" }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {title}
          </div>
          <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
            {hint}
          </div>
        </div>
        <span
          className="rounded-full px-2 py-1 text-[11px] font-semibold"
          style={{ color: accent, background: `${accent}1f` }}
        >
          {rows.length}
        </span>
      </div>

      {rows.length === 0 ? (
        <div className="py-8 text-center text-xs" style={{ color: "var(--th-text-muted)" }}>
          {emptyLabel}
        </div>
      ) : (
        <div className="mt-3 space-y-2">
          {visibleRows.map((row) => (
            <div
              key={row.id}
              className="rounded-xl border px-3 py-2"
              style={{ borderColor: "rgba(255,255,255,0.06)", background: "var(--th-bg-surface)" }}
            >
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0">
                  <div className="truncate text-sm font-medium" style={{ color: "var(--th-text)" }}>
                    {row.title}
                  </div>
                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {row.repo || "global"}
                    {row.github_issue_number ? ` · #${row.github_issue_number}` : ""}
                  </div>
                </div>
                <span className="text-[11px] shrink-0" style={{ color: accent }}>
                  {formatRelativeAge(row.age_days, t)}
                </span>
              </div>
              {row.rework_count > 0 && (
                <div className="mt-2 text-[11px]" style={{ color: accent }}>
                  {t({ ko: "리워크", en: "Rework", ja: "リワーク", zh: "返工" })} {row.rework_count}
                </div>
              )}
            </div>
          ))}
          {hiddenCount > 0 ? (
            <button
              type="button"
              className="w-full rounded-xl border px-3 py-2 text-xs font-medium"
              style={{ borderColor: `${accent}33`, color: accent, background: "transparent" }}
              onClick={() => setExpanded(true)}
            >
              {t({
                ko: `${hiddenCount}건 더 보기`,
                en: `Show ${hiddenCount} more`,
                ja: `${hiddenCount}件をさらに表示`,
                zh: `再显示 ${hiddenCount} 条`,
              })}
            </button>
          ) : rows.length > 4 ? (
            <button
              type="button"
              className="w-full rounded-xl border px-3 py-2 text-xs font-medium"
              style={{ borderColor: `${accent}33`, color: "var(--th-text-muted)", background: "transparent" }}
              onClick={() => setExpanded(false)}
            >
              {t({
                ko: "접기",
                en: "Collapse",
                ja: "折りたたむ",
                zh: "收起",
              })}
            </button>
          ) : null}
        </div>
      )}
    </div>
  );
}

