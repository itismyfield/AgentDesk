import { Lock, TriangleAlert, X } from "lucide-react";
import type { CampaignNode } from "../../api/campaigns";
import { Button } from "../../kit/free/button-clean";
import { Select } from "../../kit/free/select-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { Cap, Desc, Eyebrow, InspSection, Num, RowButton, RuleItem } from "../common/KitPrimitives";
import type { Tr } from "./campaignPresentation";
import { taskLabel, taskName } from "./relationsEditorModel";

export type InspectorActions = {
  field: (field: "title" | "issue_url", value: string) => void;
  add: (dependency: string) => void;
  remove: (dependency: string) => void;
  unlink: () => void;
  focus: () => void;
};

/** What the docked column shows with nothing chosen; `open` is set for a changed task drawn in this scope. */
export type ScopeOverview = { name: string; meta: string; changes: { id: string; name: string; fields: string; open?: () => void }[]; ties: number };

const ASIDE_CLASS = "min-h-0 flex-1 overflow-y-auto border-l border-[var(--line-soft)] bg-[var(--bg-1)]";

/** The editor's right column: the chosen task's fields and dependency chips, the chosen line, or the scope's summary. */
export default function RelationsInspector({ node, edge, lock, busy, nodes, cycles, errors, problems, overview, cycleText, act, tr }: {
  node: CampaignNode | null; edge: { source: string; target: string } | null;
  /** Why the chosen task (or the chosen line's dependent) cannot change, or null. */
  lock: string | null;
  /** A save is out: every control waits for its answer. */
  busy: boolean;
  nodes: ReadonlyMap<string, CampaignNode>; cycles: string[][]; errors: ReadonlySet<string>; problems: string[]; overview: ScopeOverview;
  cycleText: (path: string[]) => string; act: InspectorActions; tr: Tr;
}) {
  const name = (id: string) => nodes.get(id) ? taskName(nodes.get(id)!) : id;
  const lockNote = lock && <p className="m-0 flex items-center gap-1.5 text-xs text-[var(--fg-muted)]"><Lock aria-hidden className="size-3.5" />{lock}</p>;
  if (edge) return <aside aria-label={tr("선행 연결", "Dependency line")} className={ASIDE_CLASS}>
    <InspSection className="border-t-0">
      <Eyebrow>{tr("선행 연결", "Dependency line")}</Eyebrow>
      <p className="m-0 text-sm text-[var(--fg)]">{name(edge.source)} → {name(edge.target)}</p>
      {lockNote ?? <div><Button variant="secondary" size="sm" disabled={busy} onClick={act.unlink}>{tr("선 끊기", "Remove line")}</Button></div>}
    </InspSection>
  </aside>;
  if (!node) return <aside aria-label={tr("범위 요약", "Scope summary")} className={ASIDE_CLASS}>
    <InspSection className="border-t-0 gap-1">
      <Cap>{tr("편집 범위", "Scope")}</Cap>
      <h3 className="m-0 text-base text-[var(--fg)] [overflow-wrap:anywhere]">{overview.name}</h3>
      <Cap><Num>{overview.meta}</Num></Cap>
    </InspSection>
    <InspSection>
      <Eyebrow>{tr("검사", "Checks")}</Eyebrow>
      {problems.length ? <ul className="m-0 grid list-none gap-1.5 p-0 text-sm text-[var(--err)]">{problems.map((text) => <li key={text}>{text}</li>)}</ul>
        : <Desc>{tr("오류가 없어요.", "No errors.")}</Desc>}
    </InspSection>
    <InspSection>
      <div className="flex items-center justify-between"><Eyebrow>{tr("저장 전 바뀐 곳", "Unsaved changes")}</Eyebrow><Cap><Num>{overview.changes.length + overview.ties}</Num></Cap></div>
      {overview.changes.length + overview.ties ? <ul className="m-0 grid list-none gap-1 p-0 text-sm">
        {overview.changes.map((change) => <li key={change.id} className="min-w-0">
          {change.open ? <RowButton onClick={change.open} className="min-h-9 justify-between rounded-[var(--r-sm)] px-2 py-1.5 sm:px-2">
            <span className="min-w-0 truncate">{change.name}</span><span className="shrink-0 text-xs text-[var(--warn)]">{change.fields}</span></RowButton>
            : <span className="flex min-w-0 justify-between gap-3 px-2 py-1.5 text-[var(--fg-muted)]"><span className="min-w-0 truncate">{change.name}</span><span className="shrink-0 text-xs">{change.fields}</span></span>}
        </li>)}
        {overview.ties > 0 && <li className="flex justify-between gap-3 px-2 py-1.5 text-[var(--fg)]"><span>{tr("케이블타이", "Cable ties")}</span><Num className="text-xs text-[var(--warn)]">{overview.ties}</Num></li>}
      </ul> : <Desc>{tr("바뀐 곳이 없어요.", "Nothing changed yet.")}</Desc>}
    </InspSection>
    <p className="m-0 px-4 py-3 text-xs text-[var(--fg-muted)]">{tr("작업이나 선을 고르면 여기서 고쳐요.", "Choose a task or a line to edit it here.")}</p>
  </aside>;

  const locked = lock !== null;
  const candidates = [...nodes.values()].filter((other) => other.id !== node.id && !node.dependencies.includes(other.id));
  return <aside aria-label={tr("선택 작업", "Selected task")} className={ASIDE_CLASS}>
    <InspSection className="border-t-0 gap-1">
      <Cap><Num>{taskLabel(node)}</Num> · {node.group || tr("미분류", "Ungrouped")}</Cap>
      <h3 className="m-0 text-base text-[var(--fg)] [overflow-wrap:anywhere]">{node.title}</h3>
      {lockNote}
      <div><Button variant="ghost" size="sm" onClick={act.focus}>{tr("이 작업 주변만 보기", "Show only this task's neighbors")}</Button></div>
    </InspSection>
    {cycles.filter((path) => path.includes(node.id)).map((path) => {
      const at = path.lastIndexOf(node.id);
      return <InspSection key={path.join(">")} role="alert" className="bg-[var(--err-soft)]">
        <strong className="flex items-center gap-1.5 text-sm text-[var(--err-fg)]"><TriangleAlert aria-hidden className="size-4 shrink-0" />{cycleText(path)}</strong>
        <Desc className="text-[var(--fg-dim)]">{tr(`${taskLabel(nodes.get(path[at - 1])!)} 선행을 빼면 풀려요.`, `Remove the dependency on ${taskLabel(nodes.get(path[at - 1])!)} to break it.`)}</Desc>
      </InspSection>;
    })}
    <InspSection>
      <TextField label={tr("제목", "Title")} size="lg" value={node.title} disabled={locked || busy} maxLength={512}
        error={errors.has("title") ? tr("제목을 적어 주세요(512바이트까지).", "Enter a title (up to 512 bytes).") : undefined} onChange={(event) => act.field("title", event.target.value)} />
      <TextField label={tr("이슈", "Issue")} size="lg" type="url" value={node.issue_url ?? ""} disabled={locked || busy} placeholder="https://github.com/…/issues/123"
        error={errors.has("issue_url") ? tr("http나 https 주소를 적어 주세요.", "Enter an http or https address.") : undefined} onChange={(event) => act.field("issue_url", event.target.value)} />
    </InspSection>
    <InspSection>
      <Eyebrow>{tr("선행 작업", "Dependencies")}</Eyebrow>
      {node.dependencies.length ? <ul aria-label={tr("선행 작업", "Dependencies")} className="m-0 flex list-none flex-wrap gap-1.5 p-0">
        {node.dependencies.map((id) => <li key={id} className="min-w-0 max-w-full">
          <span className="inline-flex min-h-7 max-[899px]:min-h-11 [@media(pointer:coarse)]:min-h-11 max-w-full items-center gap-1 rounded-full border border-[var(--line)] bg-[var(--bg-2)] py-0.5 pl-2.5 pr-1 text-xs font-medium text-[var(--fg)]">
            <span className="min-w-0 truncate">{name(id)}</span>
            {!locked && <button type="button" disabled={busy} aria-label={tr(`선행 ${name(id)} 빼기`, `Remove dependency ${name(id)}`)} onClick={() => act.remove(id)}
              className="!m-0 grid size-6 max-[899px]:!size-11 [@media(pointer:coarse)]:!size-11 shrink-0 cursor-pointer place-items-center !rounded-full !border-0 !bg-transparent !p-0 text-[var(--fg-muted)] hover:!bg-[color-mix(in_oklch,var(--fg)_10%,transparent)]"><X aria-hidden className="size-3.5" /></button>}
          </span>
        </li>)}
      </ul> : <Desc>{tr("선행 작업이 없어요.", "No dependencies.")}</Desc>}
      {!locked && <Select aria-label={tr("선행 추가", "Add a dependency")} placeholder={tr("+ 선행 추가", "+ Add a dependency")} size="lg"
        disabled={busy} value={null} options={candidates.map((other) => ({ value: other.id, label: taskName(other) }))}
        onChange={(value) => value && act.add(value)} />}
    </InspSection>
    <InspSection>
      <div className="flex items-center justify-between"><Eyebrow>{tr("수용조건", "Acceptance")}</Eyebrow><Cap><Num>{tr(`${node.acceptance.length}개`, String(node.acceptance.length))}</Num></Cap></div>
      {node.acceptance.length ? <div className="grid gap-1.5">{node.acceptance.map((value, index) => <RuleItem key={index}>{value}</RuleItem>)}</div>
        : <Desc>{tr("수용조건이 비어 있어요.", "No acceptance criteria.")}</Desc>}
    </InspSection>
    <InspSection>
      <Eyebrow>{tr("변경 근거", "Reason for change")}</Eyebrow>
      <Desc>{tr("수용조건 편집은 변경 근거를 저장할 칸이 생기면 열려요(#8249).", "Acceptance editing opens once the reason for a change can be stored (#8249).")}</Desc>
    </InspSection>
  </aside>;
}
