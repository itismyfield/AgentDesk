import { BookOpen, ChevronRight, Search } from "lucide-react";
import { useEffect, useId, useMemo, useState } from "react";
import type { SkillTrendPoint } from "../api/analytics";
import { getSkillCatalog, getSkillRanking, getSkillTrend } from "../api/client";
import { localeName } from "../i18n";
import { Button } from "../kit/free/button-clean";
import { EmptyState } from "../kit/free/empty-state-clean";
import { SegmentedControl } from "../kit/free/segmented-control-clean";
import { TextField } from "../kit/free/text-field-clean";
import { CompactBreakdown } from "../kit/pro/compact-breakdown";
import { KpiChart, type KpiChartText } from "../kit/pro/kpi-chart";
import AgentPersona from "./AgentPersona";
import { PERSONA } from "./agent-manager/AgentRow";
import type { Translator } from "./agent-manager/types";
import { Cap, CardNote, CardSkeleton, Row, RowButton, SectionCard } from "./common/KitPrimitives";
import type { HomeAgent } from "./home/homeModel";
import {
  SKILL_CATEGORIES,
  categorizeSkill,
  compareSkills,
  normalizeSkillText,
  plainSkillText,
  skillUsers,
  topSkillItems,
  trendDayLabel,
  type SkillCategoryId,
} from "./SkillCatalogModel";

const TREND_DAYS = 30;
const DAY_MS = 86_400_000;
const KST_OFFSET_MS = 9 * 3_600_000;
const quiet = { suppressErrorToast: true };
const readCatalog = () => getSkillCatalog();
// The catalog holds a few dozen skills, so 50 rows cover every skill called in the window.
const readRanking = () => getSkillRanking("30d", 50);
const readTrend = () => getSkillTrend(TREND_DAYS, quiet);

const toKstDateKey = (ms: number) => new Date(ms + KST_OFFSET_MS).toISOString().slice(0, 10);

function fillSkillTrendKst(points: SkillTrendPoint[], days: number, now = Date.now()): SkillTrendPoint[] {
  const counts = new Map<string, number>();
  for (const point of points) {
    const key = point.day.includes("T") ? toKstDateKey(Date.parse(point.day)) : point.day.slice(0, 10);
    counts.set(key, (counts.get(key) ?? 0) + point.count);
  }
  return Array.from({ length: days }, (_, index) => {
    const day = toKstDateKey(now - (days - 1 - index) * DAY_MS);
    return { day, count: counts.get(day) ?? 0 };
  });
}

function formatDateShortKst(ts: number | null, isKo: boolean, now = Date.now()): string {
  if (!ts) return isKo ? "기록 없음" : "No record";
  const hours = Math.floor((now - ts) / 3_600_000);
  const days = Math.floor(hours / 24);
  if (hours < 1) return isKo ? "방금" : "Just now";
  if (hours < 24) return isKo ? `${hours}시간 전` : `${hours}h ago`;
  if (days === 1) return isKo ? "어제" : "Yesterday";
  if (days < 7) return isKo ? `${days}일 전` : `${days}d ago`;
  return new Date(ts).toLocaleDateString(isKo ? "ko-KR" : "en-US", { month: "short", day: "numeric", timeZone: "Asia/Seoul" });
}

type Load<T> = { status: "loading" } | { status: "error" } | { status: "ready"; data: T };

/** One GET per attempt; a new attempt reads as loading until it answers. */
function useRead<T>(read: () => Promise<T>, attempt: number): Load<T> {
  const [result, setResult] = useState<{ attempt: number; load: Load<T> } | null>(null);
  useEffect(() => {
    let live = true;
    read().then(
      (data) => live && setResult({ attempt, load: { status: "ready", data } }),
      () => live && setResult({ attempt, load: { status: "error" } }),
    );
    return () => {
      live = false;
    };
  }, [read, attempt]);
  return result?.attempt === attempt ? result.load : { status: "loading" };
}

function chartText(tr: Translator): Partial<KpiChartText> {
  return {
    guidance: tr("그래프를 누르거나 방향키로 날짜를 고르면 그날 호출 수가 보여요.", "Tap the chart or use the arrow keys to read a day's calls."),
    plot: (title, count) => tr(`${title}. 좌우 방향키로 ${count}일의 값을 읽어요.`, `${title}. Use the left and right arrow keys to read ${count} days.`),
    help: () => tr("좌우 방향키로 날짜를 옮기고 Home·End로 처음과 끝을 골라요. Escape를 누르면 상세를 숨겨요.", "Arrow keys move between days; Home and End jump to the ends. Escape hides the details."),
    noValue: tr("값 없음", "No value"),
  };
}

/** Skills tab (read-only): 30-day calls as a chart, the most used skills (a row lists who used it), then the catalog.
 * Text follows the settings language through `tr`, not the browser. */
export default function SkillCatalogView({ tr, locale, roster, onOpenAgent }: {
  tr: Translator;
  locale: string;
  /** Real agents with home's states, so avatars read as on the other tabs. */
  roster: HomeAgent[];
  onOpenAgent: (agentId: string) => void;
}) {
  const isKo = locale.startsWith("ko");
  const panelId = useId();
  const [attempt, setAttempt] = useState(0);
  const catalogLoad = useRead(readCatalog, attempt);
  const rankingLoad = useRead(readRanking, attempt);
  const trendLoad = useRead(readTrend, attempt);
  const [search, setSearch] = useState("");
  const [category, setCategory] = useState<SkillCategoryId>("all");
  const [picked, setPicked] = useState<string | null>(null);
  const label = (id: SkillCategoryId) => tr(SKILL_CATEGORIES[id].ko, SKILL_CATEGORIES[id].en);

  const trend = useMemo(() => (trendLoad.status === "ready" ? fillSkillTrendKst(trendLoad.data, TREND_DAYS) : []), [trendLoad]);
  const trendTotal = trend.reduce((sum, point) => sum + point.count, 0);
  const ranking = rankingLoad.status === "ready" ? rankingLoad.data : null;
  const top = useMemo(() => topSkillItems(ranking?.overall ?? []), [ranking]);
  const users = useMemo(() => (ranking && picked ? skillUsers(ranking.byAgent, picked, roster.map((entry) => entry.agent)) : []), [picked, ranking, roster]);

  const catalog = useMemo(() => {
    const skills = catalogLoad.status === "ready" ? catalogLoad.data : [];
    return [...skills].sort(compareSkills).map((skill) => ({ skill, category: categorizeSkill(skill), text: normalizeSkillText(skill) }));
  }, [catalogLoad]);
  const counts = useMemo(() => {
    const out: Record<string, number> = { all: catalog.length };
    for (const entry of catalog) out[entry.category] = (out[entry.category] ?? 0) + 1;
    return out;
  }, [catalog]);
  const categoryKeys = (Object.keys(SKILL_CATEGORIES) as SkillCategoryId[]).filter((id) => id === "all" || counts[id] || id === category);
  const query = search.trim().toLowerCase();
  const shown = catalog.filter((entry) =>
    (category === "all" || entry.category === category)
    && (!query || entry.text.includes(query) || label(entry.category).toLowerCase().includes(query)));

  const drill = picked ? (
    <div data-testid="skills-picked" className="flex min-w-0 flex-col">
      <Cap className="px-1 pb-1">{tr(`쓴 에이전트 · ${picked}`, `Used by · ${picked}`)}</Cap>
      {users.length === 0 ? <Cap className="px-1">{tr("에이전트 기록이 없어요.", "No agent recorded it.")}</Cap> : null}
      {users.map(({ row, agent }) => {
        const body = (
          <>
            {agent ? <span aria-hidden className="flex"><AgentPersona agent={agent} state={PERSONA[roster.find((entry) => entry.agent.id === agent.id)?.state ?? "unknown"]} size={24} /></span> : null}
            <span className="min-w-0 flex-1 truncate text-sm">{agent ? localeName(locale, agent) : row.agent_name}</span>
            <Cap className="font-display tabular-nums">{tr(`${row.calls}회`, `${row.calls}`)}</Cap>
          </>
        );
        return agent ? (
          <RowButton key={row.agent_role_id} className="px-1 sm:px-1" onClick={() => onOpenAgent(agent.id)}>
            {body}
            <ChevronRight aria-hidden size={15} className="shrink-0 text-(--fg-faint)" />
          </RowButton>
        ) : (
          <Row key={row.agent_role_id} density="sm" className="px-1 sm:px-1">{body}</Row>
        );
      })}
    </div>
  ) : null;

  const list = catalogLoad.status === "loading" ? (
    <CardSkeleton label={tr("스킬을 불러오는 중", "Loading skills")} rows={4} />
  ) : catalogLoad.status === "error" ? (
    <CardNote className="flex flex-wrap items-center gap-3">
      <span className="min-w-0 flex-1">{tr("스킬 목록을 불러오지 못했어요.", "Could not load the skills.")}</span>
      <Button variant="secondary" size="sm" onClick={() => setAttempt((value) => value + 1)}>{tr("다시 불러오기", "Reload")}</Button>
    </CardNote>
  ) : shown.length === 0 ? (
    <div className="border-t border-(--line-soft) p-3 sm:p-4">
      <EmptyState
        icon={catalog.length ? <Search size={18} /> : <BookOpen size={18} />}
        title={catalog.length ? tr("찾는 스킬이 없어요", "No skills found") : tr("등록된 스킬이 없어요", "No skills yet")}
        action={catalog.length ? (
          <Button variant="secondary" size="sm" onClick={() => { setSearch(""); setCategory("all"); }}>{tr("조건 지우기", "Clear filters")}</Button>
        ) : undefined}
      />
    </div>
  ) : (
    <ul data-testid="skills-list" className="m-0 min-w-0 list-none border-t border-(--line-soft) p-0">
      {shown.map(({ skill, category: kind }) => {
        const description = plainSkillText(isKo ? skill.description_ko || skill.description : skill.description);
        return (
          <li key={skill.name} data-testid={`skills-row-${skill.name}`} className="flex min-w-0 items-start gap-3 border-t border-(--line-soft) px-3 py-3 first:border-t-0 sm:px-4">
            <span className="min-w-0 flex-1">
              <span className="block text-sm leading-5 font-medium text-(--fg) [overflow-wrap:anywhere]">{skill.name}</span>
              {description ? <span className="mt-0.5 line-clamp-2 text-xs leading-4 text-(--fg-muted) [overflow-wrap:anywhere]">{description}</span> : null}
            </span>
            <span className="flex shrink-0 flex-col items-end gap-0.5 text-right">
              <span className="font-display text-xs leading-4 text-(--fg) tabular-nums">
                {skill.total_calls > 0 ? tr(`누적 ${skill.total_calls.toLocaleString()}회`, `${skill.total_calls.toLocaleString()} calls`) : tr("쓴 기록 없음", "Never used")}
              </span>
              <Cap>{[label(kind), skill.total_calls > 0 ? formatDateShortKst(skill.last_used_at, isKo) : null].filter(Boolean).join(" · ")}</Cap>
            </span>
          </li>
        );
      })}
    </ul>
  );

  return (
    <SectionCard
      title={tr("스킬", "Skills")}
      count={catalogLoad.status === "ready" ? catalog.length : null}
      aside={catalogLoad.status === "loading" ? <Cap>—</Cap> : undefined}
      testId="skills-catalog"
    >
      <div className="@container border-t border-(--line-soft) px-3 py-3 sm:px-4">
        <div className="grid min-w-0 grid-cols-1 gap-4 @min-[720px]:grid-cols-2 [&_.aui-compact-breakdown]:max-w-none! [&_.aui-kpi-chart]:max-w-none!">
          <div data-testid="skills-trend" className="min-w-0">
            <KpiChart
              title={tr(`스킬 호출 · ${TREND_DAYS}일`, `Skill calls · ${TREND_DAYS} days`)}
              value={trendLoad.status === "ready" ? tr(`${trendTotal.toLocaleString()}회`, trendTotal.toLocaleString()) : "—"}
              labels={trend.map((point) => trendDayLabel(point.day, isKo))}
              tooltipLabels={trend.map((point) => trendDayLabel(point.day, isKo, true))}
              series={[{ id: "calls", label: tr("호출", "Calls"), values: trend.map((point) => point.count), color: "var(--aui-accent)" }]}
              emptyLabel={trendLoad.status === "loading"
                ? tr("불러오는 중이에요", "Loading")
                : trendLoad.status === "error"
                  ? tr("호출 기록을 불러오지 못했어요", "Could not load the calls")
                  : tr("이 기간에 호출이 없어요", "No calls in this range")}
              text={chartText(tr)}
              footer={tr("날짜는 KST 기준이에요. 턴 기록에서 찾은 호출도 세요.", "Dates in KST (UTC+9). Calls found in turn records count too.")}
            />
          </div>
          <div data-testid="skills-top" className="flex min-w-0 flex-col gap-2">
            <CompactBreakdown
              items={top.items}
              title={tr(`많이 쓴 스킬 · ${TREND_DAYS}일`, `Most used · ${TREND_DAYS} days`)}
              totalLabel={rankingLoad.status === "ready" ? tr(`상위 ${top.items.length}개`, `Top ${top.items.length}`) : "—"}
              format={(value) => (rankingLoad.status === "ready" ? value.toLocaleString() : "—")}
              onSelect={(id) => setPicked((current) => (current === id ? null : id))}
              selectedId={picked}
              hideFooter
              labels={{
                list: tr("스킬별 호출과 비중", "Calls and share by skill"),
                row: (name, value, share) => tr(`${name}, ${value}회, 상위 스킬 호출의 ${share}`, `${name}, ${value} calls, ${share} of the top skills`),
                summary: (total, amount) => (rankingLoad.status === "ready" ? `${total}: ${amount}` : "—"),
                empty: rankingLoad.status === "loading"
                  ? tr("순위를 불러오는 중이에요.", "Loading the ranking.")
                  : rankingLoad.status === "error"
                    ? tr("순위를 불러오지 못했어요.", "Could not load the ranking.")
                    : tr("이 기간에 기록된 호출이 없어요.", "No calls recorded in this range."),
                selectHint: tr("누르면 이 스킬을 쓴 에이전트가 보여요.", "Shows the agents that used this skill."),
              }}
            />
            {ranking ? (
              <Cap className="px-1">
                {tr(
                  `${TREND_DAYS}일 동안 직접 부른 스킬은 ${top.called}개예요. 순위는 직접 부른 기록만 세서 그래프보다 적을 수 있어요.`,
                  `${top.called} skills were called directly in ${TREND_DAYS} days. The ranking counts direct calls only, so it can be lower than the chart.`,
                )}
              </Cap>
            ) : null}
            {drill}
          </div>
        </div>
      </div>
      <div className="@container border-t border-(--line-soft) px-3 py-3 sm:px-4">
        <div className="grid min-w-0 grid-cols-1 items-center gap-2 @3xl:grid-cols-2">
          <TextField
            type="search"
            aria-label={tr("스킬 검색", "Search skills")}
            placeholder={tr("스킬 검색", "Search skills")}
            prefix={<Search size={14} aria-hidden />}
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            clearable
            onClear={() => setSearch("")}
            className="min-w-0"
          />
          {/* The kit control is an inline-size container, so its slot needs a definite width. */}
          <div className="min-w-0">
            <SegmentedControl<SkillCategoryId>
              kind="tabs"
              variant="tray"
              aria-label={tr("스킬 분류", "Skill categories")}
              value={category}
              onChange={setCategory}
              options={categoryKeys.map((id) => ({
                value: id,
                label: label(id),
                count: catalogLoad.status === "ready" ? (counts[id] ?? 0) : "—",
                controls: panelId,
              }))}
            />
          </div>
        </div>
      </div>
      <div id={panelId} role="tabpanel" aria-label={label(category)} className="min-w-0">
        {list}
      </div>
    </SectionCard>
  );
}
