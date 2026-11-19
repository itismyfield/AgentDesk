import { Lock, TriangleAlert, X } from "lucide-react";
import type { CampaignNode } from "../../api/campaigns";
import { Button } from "../../kit/free/button-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { Select } from "../common/Select";
import type { Tr } from "./campaignPresentation";
import { taskLabel, taskName } from "./relationsEditorModel";

export type InspectorActions = {
  field: (field: "title" | "issue_url", value: string) => void;
  add: (dependency: string) => void;
  remove: (dependency: string) => void;
  unlink: () => void;
  focus: () => void;
};

const SECTION = "grid gap-2 border-b border-[var(--line)] px-4 py-3 [&>*]:min-w-0";
const HEADING = "m-0 text-xs font-medium text-[var(--fg-muted)]";

/** The editor's right column: the chosen task's fields and dependency chips, the chosen line, or the check results. */
export default function RelationsInspector({ node, edge, lock, nodes, cycles, errors, problems, cycleText, act, tr }: {
  node: CampaignNode | null; edge: { source: string; target: string } | null;
  /** Why the chosen task (or the chosen line's dependent) cannot change, or null. */
  lock: string | null;
  nodes: ReadonlyMap<string, CampaignNode>; cycles: string[][]; errors: ReadonlySet<string>; problems: string[];
  cycleText: (path: string[]) => string; act: InspectorActions; tr: Tr;
}) {
  const name = (id: string) => nodes.get(id) ? taskName(nodes.get(id)!) : id;
  const lockNote = lock && <p className="m-0 flex items-center gap-1.5 text-xs text-[var(--fg-muted)]"><Lock aria-hidden className="size-3.5" />{lock}</p>;
  if (edge) return <aside aria-label={tr("선행 연결", "Dependency line")} className="min-h-0 flex-1 overflow-y-auto border-l border-[var(--line)]">
    <div className={SECTION}>
      <h3 className={HEADING}>{tr("선행 연결", "Dependency line")}</h3>
      <p className="m-0 text-sm text-[var(--fg)]">{name(edge.source)} → {name(edge.target)}</p>
      {lockNote ?? <div><Button variant="secondary" size="sm" onClick={act.unlink}>{tr("선 끊기", "Remove line")}</Button></div>}
    </div>
  </aside>;
  if (!node) return <aside aria-label={tr("검사", "Checks")} className="min-h-0 flex-1 overflow-y-auto border-l border-[var(--line)]">
    <div className={SECTION}>
      <h3 className={HEADING}>{tr("검사", "Checks")}</h3>
      {problems.length ? <ul className="m-0 grid list-none gap-1.5 p-0 text-sm text-[var(--err)]">{problems.map((text) => <li key={text}>{text}</li>)}</ul>
        : <p className="m-0 text-sm text-[var(--fg-muted)]">{tr("오류가 없어요.", "No errors.")}</p>}
      <p className="m-0 text-xs text-[var(--fg-muted)]">{tr("작업을 고르면 여기서 고쳐요.", "Choose a task to edit it here.")}</p>
    </div>
  </aside>;

  const locked = lock !== null;
  const candidates = [...nodes.values()].filter((other) => other.id !== node.id && !node.dependencies.includes(other.id));
  return <aside aria-label={tr("선택 작업", "Selected task")} className="min-h-0 flex-1 overflow-y-auto border-l border-[var(--line)]">
    <div className={SECTION}>
      <span className="text-xs text-[var(--fg-muted)] tabular-nums">{taskLabel(node)} · {node.group || tr("미분류", "Ungrouped")}</span>
      <h3 className="m-0 text-base text-[var(--fg)] [overflow-wrap:anywhere]">{node.title}</h3>
      {lockNote}
      <div><Button variant="ghost" size="sm" onClick={act.focus}>{tr("이 작업 주변만 보기", "Show only this task's neighbors")}</Button></div>
    </div>
    {cycles.filter((path) => path.includes(node.id)).map((path) => {
      const at = path.lastIndexOf(node.id);
      return <div key={path.join(">")} role="alert" className="grid gap-1 border-b border-[var(--line)] bg-[color-mix(in_oklch,var(--err)_14%,transparent)] px-4 py-3 text-sm">
        <strong className="flex items-center gap-1.5 text-[var(--err)]"><TriangleAlert aria-hidden className="size-4" />{cycleText(path)}</strong>
        <span className="text-[var(--fg)]">{tr(`${taskLabel(nodes.get(path[at - 1])!)} 선행을 빼면 풀려요.`, `Remove the dependency on ${taskLabel(nodes.get(path[at - 1])!)} to break it.`)}</span>
      </div>;
    })}
    <div className={SECTION}>
      <TextField label={tr("제목", "Title")} size="lg" value={node.title} disabled={locked} maxLength={512}
        error={errors.has("title") ? tr("제목을 적어 주세요(512바이트까지).", "Enter a title (up to 512 bytes).") : undefined} onChange={(event) => act.field("title", event.target.value)} />
      <TextField label={tr("이슈", "Issue")} size="lg" type="url" value={node.issue_url ?? ""} disabled={locked} placeholder="https://github.com/…/issues/123"
        error={errors.has("issue_url") ? tr("http나 https 주소를 적어 주세요.", "Enter an http or https address.") : undefined} onChange={(event) => act.field("issue_url", event.target.value)} />
    </div>
    <div className={SECTION}>
      <h4 className={HEADING}>{tr("선행 작업", "Dependencies")}</h4>
      {node.dependencies.length ? <ul aria-label={tr("선행 작업", "Dependencies")} className="m-0 flex list-none flex-wrap gap-1.5 p-0">
        {node.dependencies.map((id) => <li key={id} className="flex min-w-0 max-w-full items-center gap-1 rounded-full border border-[var(--line)] py-0.5 pr-1 pl-2.5 text-xs text-[var(--fg)]">
          <span className="min-w-0 truncate">{name(id)}</span>
          {!locked && <button type="button" aria-label={tr(`선행 ${name(id)} 빼기`, `Remove dependency ${name(id)}`)} onClick={() => act.remove(id)}
            className="!m-0 grid size-6 shrink-0 place-items-center !rounded-full !border-0 !bg-transparent !p-0 text-[var(--fg-muted)] hover:!bg-[color-mix(in_oklch,var(--fg)_8%,transparent)]"><X aria-hidden className="size-3.5" /></button>}
        </li>)}
      </ul> : <p className="m-0 text-xs text-[var(--fg-muted)]">{tr("선행 작업이 없어요.", "No dependencies.")}</p>}
      {!locked && <Select aria-label={tr("선행 추가", "Add a dependency")} value="" onChange={(event) => event.target.value && act.add(event.target.value)}>
        <option value="">{tr("+ 선행 추가", "+ Add a dependency")}</option>
        {candidates.map((other) => <option key={other.id} value={other.id}>{taskName(other)}</option>)}
      </Select>}
    </div>
    <div className={SECTION}>
      <h4 className={`${HEADING} flex justify-between`}><span>{tr("수용조건", "Acceptance")}</span><span className="tabular-nums">{tr(`${node.acceptance.length}개`, String(node.acceptance.length))}</span></h4>
      {node.acceptance.length ? <ul className="m-0 grid gap-1 pl-4 text-sm text-[var(--fg)]">{node.acceptance.map((value, index) => <li key={index} className="[overflow-wrap:anywhere]">{value}</li>)}</ul>
        : <p className="m-0 text-xs text-[var(--fg-muted)]">{tr("수용조건이 비어 있어요.", "No acceptance criteria.")}</p>}
    </div>
    <div className={SECTION}>
      <h4 className={HEADING}>{tr("변경 근거", "Reason for change")}</h4>
      <p className="m-0 text-xs text-[var(--fg-muted)]">{tr("수용조건 편집은 변경 근거를 저장할 칸이 생기면 열려요(#8249).", "Acceptance editing opens once the reason for a change can be stored (#8249).")}</p>
    </div>
  </aside>;
}
