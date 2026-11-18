import { useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from "react";
import { Bot, ChartNoAxesCombined, ChevronRight, Cpu, SearchX } from "lucide-react";
import { Badge } from "../../kit/free/badge-clean";
import { Dialog } from "../../kit/free/dialog-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Skeleton } from "../../kit/free/skeleton-clean";
import type { TFunction } from "../dashboard/model";
import { cx } from "../dashboard/ui";
import {
  formatCurrency,
  formatPercent,
  formatTokens,
  msg,
  skillsForAgent,
  type AgentSkillRow,
  type BreakdownCategory,
  type BreakdownItem,
  type DailySeriesDescriptor,
  type ShareSegment,
  type TokenComposition,
} from "./statsModel";
import { NUMERIC_STYLE, StatsCard, StatsEmpty, StatsMeter, StatsSection, StatsSwatch } from "./StatsCardPrimitives";

// Off the split layout, the kit dialog becomes a right-hand drawer from 521px up (the kit keeps its bottom sheet below).
// `[&>div]` is the dialog's panel (its only element child besides the injected style).
const DRAWER_CLASS = [
  "min-[521px]:justify-end! min-[521px]:p-0!",
  "min-[521px]:[&>div]:h-full min-[521px]:[&>div]:max-h-none!",
  "min-[521px]:[&>div]:rounded-[var(--r-lg)_0_0_var(--r-lg)]!",
].join(" ");

// Split the list and its detail only on wide screens with room for both columns; otherwise the detail opens as a drawer.
const SPLIT_MEDIA = "(min-width: 900px)";
const SPLIT_MIN_WIDTH = 720;

function useSplitLayout(ref: RefObject<HTMLElement | null>) {
  const [split, setSplit] = useState(false);
  useLayoutEffect(() => {
    const node = ref.current;
    if (!node || typeof window.matchMedia !== "function") return;
    const media = window.matchMedia(SPLIT_MEDIA);
    const update = () => setSplit(media.matches && node.clientWidth >= SPLIT_MIN_WIDTH);
    update();
    const observer = typeof ResizeObserver === "function" ? new ResizeObserver(update) : null;
    observer?.observe(node);
    media.addEventListener?.("change", update);
    return () => {
      observer?.disconnect();
      media.removeEventListener?.("change", update);
    };
  }, [ref]);
  return split;
}

/** Whole percent for lists; tiny non-zero shares read as "<1". */
export function shareLabel(share: number): string {
  if (share <= 0) return "0";
  if (share < 1) return "<1";
  return String(Math.round(share));
}

const COMPOSITION_KEYS: Array<{ key: keyof TokenComposition; series: DailySeriesDescriptor["key"] }> = [
  { key: "cacheRead", series: "cache_read_tokens" },
  { key: "cacheWrite", series: "cache_creation_tokens" },
  { key: "input", series: "input_tokens" },
  { key: "output", series: "output_tokens" },
];

function itemSubline(t: TFunction, item: BreakdownItem): string {
  if (item.category === "models") return item.sublabel;
  return item.cacheHitRate !== null
    ? t(msg(`캐시 히트 ${formatPercent(item.cacheHitRate, 0)}`, `Cache hit ${formatPercent(item.cacheHitRate, 0)}`))
    : "";
}

function BreakdownRow({
  t,
  item,
  maxTokens,
  current,
  onSelect,
}: {
  t: TFunction;
  item: BreakdownItem;
  maxTokens: number;
  current: boolean;
  onSelect: (item: BreakdownItem) => void;
}) {
  const share = shareLabel(item.share);
  const sub = itemSubline(t, item);
  const shareText = t(msg(`점유율 ${share}%`, `${share}% share`));
  return (
    <li className="min-w-0">
      <button
        type="button"
        data-testid={`stats-breakdown-row-${item.id}`}
        data-item={item.id}
        aria-current={current ? "true" : undefined}
        aria-label={`${item.label}, ${formatTokens(item.tokens)} tokens, ${shareText}, ${formatCurrency(item.cost)}${sub ? `, ${sub}` : ""}`}
        title={`${item.label} · ${formatTokens(item.tokens)}`}
        onClick={() => onSelect(item)}
        className="flex min-h-11 w-full min-w-0 items-center gap-3 rounded-[var(--r-sm)] px-3 py-1.5 text-left text-[13px] text-[var(--fg)] transition-colors duration-150 hover:bg-[color-mix(in_oklch,var(--fg)_5%,transparent)] aria-[current=true]:bg-[color-mix(in_oklch,var(--fg)_8%,transparent)] motion-reduce:transition-none @max-[620px]:py-2"
      >
        <StatsSwatch color={item.color} />
        <span className="flex min-w-0 shrink-0 basis-[150px] flex-col @max-[620px]:flex-1 @max-[620px]:basis-auto">
          <span className="truncate font-medium @max-[620px]:whitespace-normal @max-[620px]:[overflow-wrap:anywhere]">{item.label}</span>
          <small className={cx("truncate text-xs text-[var(--fg-muted)]", !sub && "hidden @max-[620px]:block")} style={NUMERIC_STYLE}>
            <span className="hidden @max-[620px]:inline">{shareText}{sub ? " · " : ""}</span>
            {sub}
          </small>
        </span>
        <StatsMeter ratio={maxTokens > 0 ? item.tokens / maxTokens : 0} color={item.color} className="flex-1 @max-[620px]:hidden" />
        <span className="w-10 shrink-0 text-right text-xs text-[var(--fg-faint)] @max-[620px]:hidden" style={NUMERIC_STYLE}>{share}%</span>
        <span className="flex shrink-0 items-center gap-3 @max-[420px]:flex-col @max-[420px]:items-end @max-[420px]:gap-0" style={NUMERIC_STYLE}>
          <span className="min-w-[56px] text-right">{formatTokens(item.tokens)}</span>
          <span className="min-w-[52px] text-right text-xs text-[var(--fg-muted)]">{formatCurrency(item.cost)}</span>
        </span>
        <ChevronRight size={12} aria-hidden="true" className="shrink-0 text-[var(--fg-faint)]" />
      </button>
    </li>
  );
}

function RowsSkeleton({ label }: { label: string }) {
  return (
    <div role="status" aria-label={label} className="flex flex-col gap-1 p-1.5">
      {[96, 72, 84, 64].map((width) => (
        <div key={width} aria-hidden="true" className="flex min-h-11 items-center gap-3 px-3">
          <Skeleton width={8} height={8} radius={2} />
          <Skeleton width={width} height={10} />
          <span className="flex-1" />
          <Skeleton width={48} height={10} />
        </div>
      ))}
    </div>
  );
}

function CompositionBlock({ t, composition, series }: { t: TFunction; composition: TokenComposition; series: DailySeriesDescriptor[] }) {
  const total = COMPOSITION_KEYS.reduce((sum, part) => sum + Math.max(0, composition[part.key]), 0);
  const parts = COMPOSITION_KEYS.map((part) => {
    const descriptor = series.find((entry) => entry.key === part.series);
    const value = Math.max(0, composition[part.key]);
    return { ...part, label: descriptor?.label ?? part.key, color: descriptor?.color ?? "var(--fg-muted)", value, ratio: total > 0 ? value / total : 0 };
  });
  return (
    <section className="flex min-w-0 flex-col gap-2" aria-label={t(msg("토큰 구성", "Token mix"))}>
      <h4 className="text-xs font-medium text-[var(--fg-muted)]">{t(msg("토큰 구성", "Token mix"))}</h4>
      <div aria-hidden="true" className="flex h-2 overflow-hidden rounded-full bg-[var(--line-soft)]">
        {parts.map((part) => (part.ratio > 0 ? <span key={part.key} style={{ width: `${part.ratio * 100}%`, background: part.color }} /> : null))}
      </div>
      <ul className="divide-y divide-[var(--line-soft)] rounded-[var(--r-sm)] border border-[var(--line-soft)]">
        {parts.map((part) => (
          <li key={part.key} className="flex items-center gap-2 px-3 py-2 text-xs" style={NUMERIC_STYLE}>
            <StatsSwatch color={part.color} />
            <span className="min-w-0 flex-1 text-[var(--fg-dim)]">{part.label}</span>
            <span className="text-[var(--fg)]">{formatTokens(part.value)}</span>
            <span className="w-11 text-right text-[var(--fg-muted)]">{total > 0 ? `${shareLabel(part.ratio * 100)}%` : "—"}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

function DetailFacts({ facts }: { facts: Array<{ label: string; value: string }> }) {
  return (
    <dl className="grid grid-cols-2 gap-px overflow-hidden rounded-[var(--r-sm)] border border-[var(--line-soft)] bg-[var(--line-soft)]">
      {facts.map((fact) => (
        <div key={fact.label} className="flex min-w-0 flex-col gap-0.5 bg-[var(--bg-1)] px-3 py-2 last:odd:col-span-2">
          <dt className="text-[11px] text-[var(--fg-muted)]">{fact.label}</dt>
          <dd className="text-sm font-medium text-[var(--fg)] [overflow-wrap:anywhere]" style={NUMERIC_STYLE}>{fact.value}</dd>
        </div>
      ))}
    </dl>
  );
}

function BreakdownDetail({
  t,
  item,
  missing,
  showTitle,
  series,
  agentSkills,
}: {
  t: TFunction;
  item: BreakdownItem | null;
  missing: boolean;
  showTitle: boolean;
  series: DailySeriesDescriptor[];
  agentSkills: AgentSkillRow[];
}) {
  if (!item) {
    return missing ? (
      <StatsEmpty
        icon={<SearchX size={18} />}
        testId="stats-detail-missing"
        title={t(msg("이 기간에는 선택한 항목이 없어요", "The selected item is not in this range"))}
        text={t(msg("기간을 바꾸면서 목록에서 빠졌어요. 다른 항목을 골라 보세요.", "It dropped out when the range changed. Pick another row."))}
      />
    ) : (
      <StatsEmpty
        icon={<ChartNoAxesCombined size={18} />}
        testId="stats-detail-empty"
        title={t(msg("선택한 항목이 없어요", "Nothing selected"))}
        text={t(msg("모델이나 에이전트를 고르면 토큰 구성과 비용이 보여요.", "Pick a model or an agent to see its token mix and cost."))}
      />
    );
  }

  const kind = item.category === "models" ? t(msg("모델", "Model")) : t(msg("에이전트", "Agent"));
  const facts: Array<{ label: string; value: string }> = [{ label: t(msg("비용", "Cost")), value: formatCurrency(item.cost) }];
  if (item.costWithoutCache !== null) facts.push({ label: t(msg("캐시 없이", "Without cache")), value: formatCurrency(item.costWithoutCache) });
  if (item.savedCost !== null) facts.push({ label: t(msg("캐시 절감", "Cache savings")), value: formatCurrency(item.savedCost) });
  if (item.cacheHitRate !== null) facts.push({ label: t(msg("캐시 히트율", "Cache hit rate")), value: formatPercent(item.cacheHitRate) });
  if (item.promptTokens !== null) facts.push({ label: t(msg("prompt 토큰", "Prompt tokens")), value: formatTokens(item.promptTokens) });
  const skills = agentSkills.slice(0, 5);

  return (
    <div className="flex min-w-0 flex-col gap-5" data-testid="stats-detail-body" data-item={item.id}>
      {showTitle ? (
        <div className="flex min-w-0 items-center gap-2.5">
          <StatsSwatch color={item.color} />
          <h3 className="min-w-0 text-[15px] font-semibold text-[var(--fg)] [overflow-wrap:anywhere]">{item.label}</h3>
          <Badge className="ml-auto">{kind}</Badge>
        </div>
      ) : null}
      <div className={cx("flex flex-wrap items-baseline gap-x-2.5 gap-y-1", showTitle && "-mt-3")}>
        <span className="text-[22px] font-semibold leading-7 text-[var(--fg)]" style={NUMERIC_STYLE}>{formatTokens(item.tokens)}</span>
        <span className="text-[13px] text-[var(--fg-muted)]" style={NUMERIC_STYLE}>
          {t(msg(`분류 합계의 ${shareLabel(item.share)}%`, `${shareLabel(item.share)}% of this breakdown`))}
          {showTitle && item.sublabel ? ` · ${item.sublabel}` : ""}
        </span>
      </div>
      <DetailFacts facts={facts} />
      {item.composition ? (
        <CompositionBlock t={t} composition={item.composition} series={series} />
      ) : (
        <p className="text-xs leading-5 text-[var(--fg-muted)]">{t(msg("이 항목은 토큰 구성을 보내지 않았어요.", "No token split was reported for this item."))}</p>
      )}
      {item.category === "agents" ? (
        <section className="flex min-w-0 flex-col gap-2" aria-label={t(msg("상위 스킬 조합", "Top skills"))}>
          <h4 className="text-xs font-medium text-[var(--fg-muted)]">{t(msg("상위 스킬 조합", "Top skills"))}</h4>
          {skills.length ? (
            <ul className="divide-y divide-[var(--line-soft)] rounded-[var(--r-sm)] border border-[var(--line-soft)]">
              {skills.map((skill) => (
                <li key={skill.id} className="flex min-w-0 items-center justify-between gap-3 px-3 py-2 text-xs">
                  <span className="min-w-0 text-[var(--fg)] [overflow-wrap:anywhere]">{skill.skillName}</span>
                  <span className="shrink-0 text-[var(--fg-muted)]" style={NUMERIC_STYLE}>{skill.calls.toLocaleString()} {t(msg("회", "calls", "回", "次"))}</span>
                </li>
              ))}
            </ul>
          ) : (
            <p className="text-xs leading-5 text-[var(--fg-muted)]">{t(msg("이 기간에 기록된 스킬 호출이 없어요.", "No skill calls were recorded in this range."))}</p>
          )}
        </section>
      ) : null}
    </div>
  );
}

/** "Where tokens went": model/agent tabs, share rows and a row detail (split on wide screens, drawer below). */
export function StatsBreakdown({
  t,
  loading,
  failed,
  models,
  agents,
  modelSegments,
  totalTokens,
  overallCacheHitRate,
  agentSkillRows,
  series,
}: {
  t: TFunction;
  loading: boolean;
  failed: boolean;
  models: BreakdownItem[];
  agents: BreakdownItem[];
  modelSegments: ShareSegment[];
  totalTokens: number;
  overallCacheHitRate: number;
  agentSkillRows: AgentSkillRow[];
  series: DailySeriesDescriptor[];
}) {
  const id = useId();
  const panelId = `${id}-panel`;
  const splitRef = useRef<HTMLDivElement>(null);
  const split = useSplitLayout(splitRef);
  const [category, setCategory] = useState<BreakdownCategory>("models");
  const [selection, setSelection] = useState<{ category: BreakdownCategory; id: string } | null>(null);
  const [drawerOpen, setDrawerOpen] = useState(false);

  useEffect(() => {
    if (split) setDrawerOpen(false);
  }, [split]);

  const list = category === "models" ? models : agents;
  const maxTokens = Math.max(0, ...list.map((item) => item.tokens));
  const selectedItem = selection
    ? (selection.category === "models" ? models : agents).find((item) => item.id === selection.id) ?? null
    : null;
  const missing = selection !== null && selectedItem === null && !loading;
  const selectedSkills = selectedItem?.category === "agents" ? skillsForAgent(agentSkillRows, selectedItem.id) : [];

  const onSelect = (item: BreakdownItem) => {
    setSelection({ category: item.category, id: item.id });
    if (!split) setDrawerOpen(true);
  };

  const categoryLabel = category === "models" ? t(msg("모델", "Models")) : t(msg("에이전트", "Agents"));
  let listBody: ReactNode;
  if (list.length > 0) {
    listBody = (
      <ul className="flex flex-col gap-px p-1.5">
        {list.map((item) => (
          <BreakdownRow
            key={item.id}
            t={t}
            item={item}
            maxTokens={maxTokens}
            current={selection?.category === item.category && selection.id === item.id}
            onSelect={onSelect}
          />
        ))}
      </ul>
    );
  } else if (loading) {
    listBody = (
      <RowsSkeleton
        label={category === "models"
          ? t(msg("모델 분포를 불러오는 중입니다.", "Loading model distribution."))
          : t(msg("에이전트 캐시 데이터를 불러오는 중입니다.", "Loading agent cache data."))}
      />
    );
  } else {
    listBody = (
      <StatsEmpty
        icon={category === "models" ? <Cpu size={18} /> : <Bot size={18} />}
        title={failed
          ? t(msg("분류를 불러오지 못했어요", "The breakdown could not be loaded"))
          : category === "models"
            ? t(msg("표시할 모델 데이터가 없습니다.", "No model data available."))
            : t(msg("표시할 에이전트 캐시 데이터가 없습니다.", "No agent cache data available."))}
        text={failed
          ? t(msg("위의 다시 시도를 누르면 다시 불러와요.", "Use Retry above to load it again."))
          : t(msg("이 분류에는 이 기간 토큰이 없어요. 다른 분류를 골라 보세요.", "Nothing in this breakdown for the range. Try the other tab."))}
      />
    );
  }

  const detail = (
    <BreakdownDetail t={t} item={selectedItem} missing={missing} showTitle={split} series={series} agentSkills={selectedSkills} />
  );

  return (
    <StatsSection
      id={`${id}-title`}
      testId="stats-breakdown"
      icon={<ChartNoAxesCombined />}
      title={t(msg("어디에 썼나", "Where tokens went"))}
      actions={
        <div className="w-64 max-w-full">
          <SegmentedControl<BreakdownCategory>
            kind="tabs"
            variant="tray"
            fullWidth
            value={category}
            onChange={setCategory}
            aria-label={t(msg("토큰 분류", "Token breakdown"))}
            options={[
              { value: "models", label: t(msg("모델", "Models")), icon: <Cpu size={13} />, count: models.length, controls: panelId },
              { value: "agents", label: t(msg("에이전트", "Agents")), icon: <Bot size={13} />, count: agents.length, controls: panelId },
            ]}
          />
        </div>
      }
    >
      <div
        ref={splitRef}
        data-split={split || undefined}
        className="grid min-w-0 items-start gap-3 data-[split]:grid-cols-[minmax(0,1fr)_minmax(300px,360px)]"
      >
        <StatsCard
          id={panelId}
          role="tabpanel"
          aria-label={categoryLabel}
          data-testid={category === "models" ? "stats-model-share" : "stats-agent-cache"}
          className="@container"
        >
          <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-1.5 border-b border-[var(--line-soft)] px-4 py-3">
            <p className="min-w-0 text-xs leading-5 text-[var(--fg-muted)] [word-break:keep-all]">
              {category === "models"
                ? t(msg("Claude / Codex 모델별 토큰 배분", "Token share by Claude / Codex models", "Claude / Codex モデル別トークン配分", "按 Claude / Codex 模型划分的 Token 占比"))
                : t(msg("에이전트별 토큰 점유율과 캐시 히트율", "Token share and cache hit rate by agent"))}
            </p>
            {category === "models" ? (
              <Badge style={NUMERIC_STYLE}>{formatTokens(totalTokens)} total</Badge>
            ) : (
              <Badge tone="success" style={NUMERIC_STYLE}>
                {formatPercent(overallCacheHitRate)} {t(msg("전체", "overall", "全体", "整体"))}
              </Badge>
            )}
            {category === "models" && modelSegments.length > 0 ? (
              <div className="flex h-2 w-full overflow-hidden rounded-full" aria-hidden="true">
                {modelSegments.map((segment) => (
                  <span
                    key={segment.id}
                    title={`${segment.label} ${formatPercent(segment.percentage)}`}
                    style={{ width: `${segment.percentage}%`, background: segment.color, minWidth: segment.percentage > 0 ? "6px" : 0 }}
                  />
                ))}
              </div>
            ) : null}
          </div>
          {listBody}
        </StatsCard>
        {split ? (
          <StatsCard
            role="region"
            aria-label={t(msg("항목 상세", "Item detail"))}
            data-testid="stats-detail"
            className="sticky top-4 p-4"
          >
            {detail}
          </StatsCard>
        ) : null}
      </div>
      {!split ? (
        <Dialog
          open={drawerOpen && selection !== null}
          onOpenChange={setDrawerOpen}
          title={selectedItem?.label ?? t(msg("항목 상세", "Item detail"))}
          description={selectedItem
            ? [selectedItem.category === "models" ? t(msg("모델", "Model")) : t(msg("에이전트", "Agent")), selectedItem.sublabel].filter(Boolean).join(" · ")
            : undefined}
          closeLabel={t(msg("닫기", "Close"))}
          width={420}
          className={DRAWER_CLASS}
          data-testid="stats-detail-dialog"
        >
          {detail}
        </Dialog>
      ) : null}
    </StatsSection>
  );
}
