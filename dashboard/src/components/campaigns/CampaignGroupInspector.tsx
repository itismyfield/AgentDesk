import { TriangleAlert } from "lucide-react";
import type { CampaignNode, CampaignNodeLive } from "../../api/campaigns";
import { Button } from "../../kit/free/button-clean";
import { Cap, Desc, Eyebrow, InspSection, MeterBar, Num, RowButton } from "../common/KitPrimitives";
import type { GroupOverview } from "./CampaignGroupOverview";
import { campaignGroup, campaignTaskTitle, safeCampaignLink } from "./campaignModel";
import { cleanBlocker, effectiveRelationCounts, legacyTask } from "./campaignOverviewModel";
import { LABELS, type Tr } from "./campaignPresentation";
import { taskLabel, taskName } from "./relationsEditorModel";

/** Docked to the canvas's right from 1280px; narrower, it follows below the canvas. */
const ASIDE = "min-h-0 overflow-y-auto border-t border-[var(--line-soft)] bg-[var(--bg-1)] min-[1280px]:border-t-0 min-[1280px]:border-l";
const ROW = "min-h-9 justify-between gap-3 rounded-[var(--r-sm)] px-2 py-1.5 sm:px-2";

function Counts({ values, tr }: { values: [ko: string, en: string, value: number][]; tr: Tr }) {
  return <dl className="m-0 grid gap-2" style={{ gridTemplateColumns: `repeat(${values.length}, minmax(0, 1fr))` }}>{values.map(([ko, en, value]) => <div key={en} className="grid gap-0.5">
    <dt><Cap>{tr(ko, en)}</Cap></dt><dd className="m-0"><Num className="text-base text-[var(--fg)]">{value}</Num></dd>
  </div>)}</dl>;
}

/** A titled list of rows with a trailing value, or a quiet line when it is empty. */
function RowList({ title, empty, rows }: { title: string; empty: string; rows: { key: string; name: string; value?: string; open: () => void }[] }) {
  return <InspSection>
    <div className="flex items-center justify-between"><Eyebrow>{title}</Eyebrow><Cap><Num>{rows.length}</Num></Cap></div>
    {rows.length ? <ul aria-label={title} className="m-0 grid list-none gap-0.5 p-0 text-sm">{rows.map((row) => <li key={row.key} className="min-w-0">
      <RowButton onClick={row.open} className={ROW}><span className="min-w-0 truncate">{row.name}</span>{row.value && <Cap className="shrink-0"><Num>{row.value}</Num></Cap>}</RowButton>
    </li>)}</ul> : <Desc>{empty}</Desc>}
  </InspSection>;
}

/** The relations view's right column: the chosen group's state, its blocked tasks and its neighbours, or the campaign's.
 * `opened` is set inside that group's task graph, where 그룹 열기 has nothing left to open. */
export default function CampaignGroupInspector({ overview, nodes, live, chosen, opened = false, cycleText, onChoose, onOpenGroup, onOpenTask, tr }: {
  overview: GroupOverview; nodes: CampaignNode[]; live?: Record<string, CampaignNodeLive>; chosen: string | null; opened?: boolean; cycleText: (path: string[]) => string;
  onChoose: (group: string) => void; onOpenGroup: (group: string) => void; onOpenTask: (id: string) => void; tr: Tr;
}) {
  const label = (key: string) => key || tr("미분류", "Ungrouped");
  const keyOf = (id: string) => overview.groups.find((group) => group.id === id)?.key ?? "";
  const group = chosen === null ? undefined : overview.groups.find((value) => value.key === chosen);
  const loops = (paths: string[][]) => paths.map((path) => <InspSection key={path.join(">")} className="bg-[var(--err-soft)]">
    <strong className="flex items-center gap-1.5 text-sm text-[var(--err)]"><TriangleAlert aria-hidden className="size-4 shrink-0" />{cycleText(path)}</strong>
    <Desc className="text-[var(--fg-dim)]">{tr("그룹끼리 서로 선행으로 이어져 있어요.", "These groups depend on each other in a loop.")}</Desc>
  </InspSection>);

  if (!group) {
    const { counts, percent } = effectiveRelationCounts(nodes, live);
    const crossing = overview.connections.reduce((sum, line) => sum + line.count, 0);
    const stuck = overview.groups.filter((value) => value.progress.counts.blocked > 0);
    return <aside aria-label={tr("캠페인 요약", "Campaign summary")} className={ASIDE}>
      <InspSection className="gap-1 border-t-0">
        <Cap>{tr("캠페인 전체", "Whole campaign")}</Cap>
        <h3 className="m-0 text-base text-[var(--fg)]"><Num>{tr(`그룹 ${overview.groups.length}개 · 작업 ${nodes.length}개`, `${overview.groups.length} groups · ${nodes.length} tasks`)}</Num></h3>
        <Cap><Num>{tr(`완료 ${percent}%`, `${percent}% done`)}</Num></Cap>
        <MeterBar ratio={percent / 100} tone="ok" className="mt-1" />
      </InspSection>
      <InspSection><Counts tr={tr} values={[["완료", "Done", counts.completed], ["진행", "Running", counts.running], ["막힘", "Blocked", counts.blocked], ["대기", "Waiting", counts.pending]]} /></InspSection>
      <InspSection>
        <Eyebrow>{tr("그룹 사이 연결", "Between groups")}</Eyebrow>
        <Desc><Num>{tr(`의존성 ${crossing}개 · 그룹 안 연결 ${overview.internalDependencies}개`, `${crossing} dependencies · ${overview.internalDependencies} inside groups`)}</Num></Desc>
      </InspSection>
      {loops(overview.cycles)}
      <RowList title={tr("막힌 그룹", "Blocked groups")} empty={tr("막힌 그룹이 없어요.", "No blocked groups.")}
        rows={stuck.map((value) => ({ key: value.id, name: label(value.key), value: tr(`막힘 ${value.progress.counts.blocked}`, `${value.progress.counts.blocked} blocked`), open: () => onChoose(value.key) }))} />
      <p className="m-0 px-4 py-3 text-xs text-[var(--fg-muted)]">{tr("그룹을 고르면 여기서 요약을 봐요.", "Choose a group to see its summary here.")}</p>
    </aside>;
  }

  const { counts, percent, total, blockedIds } = group.progress;
  /** Dependencies per neighbouring group, on the given side of this one. */
  const neighbours = (side: "source" | "target") => {
    const sums = new Map<string, number>();
    for (const line of overview.connections) if (line[side === "source" ? "target" : "source"] === group.id) sums.set(line[side], (sums.get(line[side]) ?? 0) + line.count);
    return [...sums].map(([id, count]) => ({ key: id, name: label(keyOf(id)), value: tr(`의존성 ${count}`, `${count} dependencies`), open: () => onChoose(keyOf(id)) }));
  };
  return <aside aria-label={tr("선택 그룹", "Selected group")} className={ASIDE}>
    <InspSection className="gap-1 border-t-0">
      <Cap>{tr("그룹", "Group")}</Cap>
      <h3 className="m-0 text-base text-[var(--fg)] [overflow-wrap:anywhere]">{label(group.key)}</h3>
      <Cap><Num>{tr(`완료 ${counts.completed}/${total} · ${percent}%`, `${counts.completed}/${total} done · ${percent}%`)}</Num></Cap>
      <MeterBar ratio={percent / 100} tone="ok" className="mt-1" />
      {!opened && <div className="pt-2"><Button variant="primary" onClick={() => onOpenGroup(group.key)}>{tr("그룹 열기", "Open group")}</Button></div>}
    </InspSection>
    {loops(overview.cycles.filter((_, index) => overview.components[index].has(group.id)))}
    <InspSection><Counts tr={tr} values={[["작업", "Tasks", total], ["완료", "Done", counts.completed], ["진행", "Running", counts.running], ["막힘", "Blocked", counts.blocked]]} /></InspSection>
    <RowList title={tr("막힌 작업", "Blocked tasks")} empty={tr("막힌 작업이 없어요.", "No blocked tasks.")}
      rows={group.nodes.filter((node) => blockedIds.has(node.id)).map((node) => ({ key: node.id, name: node.title, open: () => onOpenTask(node.id) }))} />
    <RowList title={tr("선행 그룹", "Groups it depends on")} empty={tr("선행 그룹이 없어요.", "It depends on no group.")} rows={neighbours("source")} />
    <RowList title={tr("다음 그룹", "Groups that depend on it")} empty={tr("다음 그룹이 없어요.", "No group depends on it.")} rows={neighbours("target")} />
  </aside>;
}

/** The chosen task in a group's graph: its state, issue, block, counts and the tasks on either side; choosing one of
 * those goes to it, and 작업 상세 열기 opens the task as the list rows do. */
export function TaskInspector({ node, nodes, live, cycles, onChoose, onOpenTask, tr }: {
  node: CampaignNode; nodes: CampaignNode[]; live?: CampaignNodeLive; cycles: string[]; onChoose: (id: string) => void; onOpenTask: (id: string) => void; tr: Tr;
}) {
  const byId = new Map(nodes.map((value) => [value.id, value]));
  const before = (node.dependencies ?? []).flatMap((id) => byId.get(id) ?? []), after = nodes.filter((value) => (value.dependencies ?? []).includes(node.id));
  const row = (other: CampaignNode) => ({ key: other.id, name: taskName(other), value: campaignGroup(other) === campaignGroup(node) ? undefined : campaignGroup(other) || tr("미분류", "Ungrouped"), open: () => onChoose(other.id) });
  const issue = safeCampaignLink(node.issue_url), blocker = cleanBlocker(node.blocker);
  const taskView = legacyTask(node, live);
  return <aside aria-label={tr("선택 작업", "Selected task")} className={ASIDE}>
    <InspSection className="gap-1 border-t-0">
      <Cap><Num>{taskLabel(node)}</Num> · {campaignGroup(node) || tr("미분류", "Ungrouped")}</Cap>
      <h3 className="m-0 text-base text-[var(--fg)] [overflow-wrap:anywhere]">{campaignTaskTitle(node)}</h3>
      <Cap className="[overflow-wrap:anywhere]">{[tr(...LABELS[taskView.status]), taskView.stage].filter(Boolean).join(" · ")}</Cap>
      {issue && <a href={issue} target="_blank" rel="noreferrer" className="inline-flex items-center truncate text-xs text-[var(--accent-fg)] hover:underline max-[899px]:min-h-11 pointer-coarse:min-h-11">{issue.replace(/^https?:\/\//, "")}</a>}
      <div className="pt-2"><Button variant="primary" onClick={() => onOpenTask(node.id)}>{tr("작업 상세 열기", "Open task details")}</Button></div>
    </InspSection>
    {cycles.map((text) => <InspSection key={text} className="bg-[var(--err-soft)]"><strong className="flex items-center gap-1.5 text-sm text-[var(--err)]"><TriangleAlert aria-hidden className="size-4 shrink-0" />{text}</strong></InspSection>)}
    {blocker && <InspSection><Eyebrow>{tr("막힘 사유", "Blocked by")}</Eyebrow><Desc className="text-[var(--fg)] [overflow-wrap:anywhere]">{blocker}</Desc></InspSection>}
    <InspSection><Counts tr={tr} values={[["선행", "Needs", before.length], ["후행", "Next", after.length], ["수용조건", "Acceptance", (node.acceptance ?? []).length]]} /></InspSection>
    <RowList title={tr("선행 작업", "Dependencies")} empty={tr("선행 작업이 없어요.", "No dependencies.")} rows={before.map(row)} />
    <RowList title={tr("후행 작업", "Dependents")} empty={tr("후행 작업이 없어요.", "Nothing depends on it.")} rows={after.map(row)} />
  </aside>;
}
