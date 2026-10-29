import { useEffect, useRef } from "react";
import {
  createServerCampaign, editServerCampaignDefinition, getServerCampaign, isDefinitiveAnswer, serverCampaignRejection,
  type ServerCampaignCommandResult, type ServerCampaignDefinition, type ServerCampaignDocument,
} from "../../api/serverCampaigns";
import type { Tr } from "./campaignPresentation";
import { changedDefinitionFields, newIdempotencyKey, rejectionText } from "./serverCampaignModel";

type Definition = ServerCampaignDefinition;
type Node = Definition["nodes"][number];
type Acceptance = Node["acceptance"][number];
type Status =
  | { kind: "error"; text: string; reasonMissing?: boolean }
  | { kind: "lost" }
  | { kind: "conflict"; revision: number; latest: Definition; changed: ReturnType<typeof changedDefinitionFields> };

/** A create or edit in progress. The panel owns it so polls and task switches never reset it. */
export interface DefinitionDraft {
  mode: "create" | "edit";
  id: string;
  baseRevision: number | null;
  baseDefinition: Definition | null;
  definition: Definition;
  change: { reference: string; explanation: string };
  /** The last body sent without a definitive answer: resending that same body reuses its key. */
  sent: { key: string; body: string } | null;
  /** A save's answer is outstanding. Kept here so the lock outlives the form leaving the screen. */
  pending: boolean;
  /** What a create form was filled from, to tell an untouched form from one being written. */
  seed: Definition;
  /** Per task: true when it came from the saved definition, whose task IDs stay fixed. */
  saved: boolean[];
  status: Status | null;
  newKey: () => string;
}

const EMPTY: Definition = { repository: "adk", title: "", goal: "", profile_version: "v1", nodes: [] };
const VERIFICATIONS: Array<[Acceptance["verification"], [string, string]]> = [
  ["test", ["시험", "Tests"]],
  ["main", ["main 반영", "In main"]],
  ["deployment", ["배포 확인", "Deployment"]],
  ["operational", ["운영 확인", "Operational"]],
];
const DEFINITION_ERRORS: Record<string, [string, string]> = {
  invalid_field: ["입력 형식이 맞지 않습니다", "A field is not valid"],
  unsupported_profile: ["지원하지 않는 프로필입니다", "The profile is not supported"],
  empty_acceptance: ["수용조건이 없는 작업이 있습니다", "A task has no acceptance condition"],
  duplicate_acceptance: ["수용조건 ID가 겹칩니다", "An acceptance ID repeats"],
  duplicate_node: ["작업 ID가 겹칩니다", "A task ID repeats"],
  missing_dependency: ["없는 작업을 선행으로 지정했습니다", "A dependency names no task"],
  repeated_dependency: ["같은 선행 작업을 두 번 지정했습니다", "A dependency is listed twice"],
  cycle: ["선행 관계가 순환합니다", "The dependencies form a cycle"],
};

export function createDraft(copyOf: Definition | null, newKey = newIdempotencyKey): DefinitionDraft {
  return {
    mode: "create", id: "", baseRevision: null, baseDefinition: null, definition: structuredClone(copyOf ?? EMPTY), seed: copyOf ?? EMPTY,
    saved: (copyOf ?? EMPTY).nodes.map(() => false),
    change: { reference: "", explanation: "" }, sent: null, pending: false, status: null, newKey,
  };
}

export function editDraft(document: ServerCampaignDocument, newKey = newIdempotencyKey): DefinitionDraft {
  return {
    mode: "edit", id: document.view.campaign_id, baseRevision: document.view.definition_revision,
    baseDefinition: document.definition, definition: structuredClone(document.definition), seed: document.definition,
    saved: document.definition.nodes.map(() => true),
    change: { reference: "", explanation: "" }, sent: null, pending: false, status: null, newKey,
  };
}

/** A create form nobody has typed into since it was opened or filled by a copy. */
export function isUntouchedCreate(draft: DefinitionDraft): boolean {
  return draft.mode === "create" && !draft.id && draft.sent === null && !draft.pending
    && JSON.stringify(draft.definition) === JSON.stringify(draft.seed);
}

export default function ServerCampaignForm({ draft, onDraftChange, onSaved, onConflict, onCancel, focus = 0, onFocused, disabled, tr }: {
  draft: DefinitionDraft;
  onDraftChange: (draft: DefinitionDraft | ((current: DefinitionDraft) => DefinitionDraft)) => void;
  /** `key` is the idempotency key the saved body went out under. */
  onSaved: (id: string, result: ServerCampaignCommandResult, key: string) => void;
  /** The server holds a newer definition; the caller refreshes what it shows. */
  onConflict?: () => void;
  onCancel: () => void;
  /** A new nonzero value moves focus to the first field; a form shown again keeps focus where it is. */
  focus?: number;
  onFocused?: () => void;
  /** The snapshot is stale or the server refuses edits: nothing may be sent. */
  disabled: boolean;
  tr: Tr;
}) {
  const saving = draft.pending;
  const sending = useRef(false);
  const fields = useRef<HTMLFieldSetElement>(null);
  useEffect(() => {
    if (!focus) return;
    fields.current?.querySelector<HTMLElement>("input, select, textarea")?.focus();
    onFocused?.();
  }, [focus]); // eslint-disable-line react-hooks/exhaustive-deps -- runs once per focus request
  const { definition } = draft;
  const update = (changes: Partial<DefinitionDraft>) => onDraftChange({ ...draft, ...changes });
  const setDefinition = (changes: Partial<Definition>) => update({ definition: { ...definition, ...changes } });
  const setNode = (index: number, changes: Partial<Node>) =>
    setDefinition({ nodes: definition.nodes.map((node, at) => at === index ? { ...node, ...changes } : node) });
  const setAcceptance = (index: number, acIndex: number, changes: Partial<Acceptance>) =>
    setNode(index, { acceptance: definition.nodes[index].acceptance.map((ac, at) => at === acIndex ? { ...ac, ...changes } : ac) });
  // A dependency goes with its task only when no remaining task still has that ID.
  const removeNode = (index: number) => {
    const removed = definition.nodes[index].id;
    const remaining = definition.nodes.filter((_, at) => at !== index);
    const gone = !remaining.some((node) => node.id === removed);
    update({
      saved: draft.saved.filter((_, at) => at !== index),
      definition: { ...definition, nodes: gone ? remaining.map((node) => ({ ...node, dependencies: node.dependencies.filter((id) => id !== removed) })) : remaining },
    });
  };
  // A new ID names no task and no leftover dependency, so nothing silently attaches to the new task.
  const addNode = () => {
    const taken = (id: string) => definition.nodes.some((node) => node.id === id || node.dependencies.includes(id));
    let next = definition.nodes.length + 1;
    while (taken(`task-${next}`)) next++;
    update({
      saved: [...draft.saved, false],
      definition: { ...definition, nodes: [...definition.nodes, { id: `task-${next}`, title: "", issue_url: "", acceptance: [], dependencies: [], group: null }] },
    });
  };
  const reasonMissing = draft.status?.kind === "error" && draft.status.reasonMissing === true;

  /** Saving over a newer revision also takes that revision as the base later conflicts are measured from. */
  const save = async (expectedRevision = draft.baseRevision, rebaseOn = draft.baseDefinition) => {
    if (sending.current || saving || disabled) return;
    const change = draft.mode === "edit" && (draft.change.reference || draft.change.explanation) ? draft.change : null;
    const body = JSON.stringify([draft.mode, draft.id, expectedRevision, definition, change]);
    const key = draft.sent?.body === body ? draft.sent.key : draft.newKey();
    // The fields stay locked until the answer, which patches only this bookkeeping and only on the draft it was sent from.
    const settle = (patch: Partial<DefinitionDraft>) =>
      onDraftChange((current) => current.sent?.key === key ? { ...current, pending: false, ...patch } : current);
    onDraftChange((current) => ({ ...current, baseRevision: expectedRevision, baseDefinition: rebaseOn, sent: { key, body }, pending: true, status: null }));
    sending.current = true;
    try {
      const result = draft.mode === "create"
        ? await createServerCampaign(draft.id, definition, key)
        : await editServerCampaignDefinition(draft.id, expectedRevision ?? 0, definition, change, key);
      onSaved(draft.id, result, key);
    } catch (error) {
      const refusal = serverCampaignRejection(error);
      if (!isDefinitiveAnswer(refusal) || !refusal) {
        settle({ status: { kind: "lost" } });
        return;
      }
      // A decided request keeps its answer under its key; the next save sends a new key.
      if (refusal.status === 409 && refusal.rejection === "stale_definition" && draft.mode === "edit") {
        onConflict?.();
        const latest = await getServerCampaign(draft.id).catch(() => null);
        settle({
          sent: null,
          status: latest && rebaseOn
            ? { kind: "conflict", revision: latest.view.definition_revision, latest: latest.definition, changed: changedDefinitionFields(rebaseOn, latest.definition) }
            : { kind: "error", text: tr("다른 곳에서 정의가 바뀌었습니다. 초안은 그대로 둡니다.", "The definition changed elsewhere. Your draft is kept.") },
        });
      } else {
        const detail = refusal.definitionError;
        const text = detail
          ? [tr(...(DEFINITION_ERRORS[detail.code] ?? [detail.code, detail.code])), detail.path.length ? detail.path.join(" → ") : detail.target].filter(Boolean).join(" · ")
          : refusal.rejection ? rejectionText(refusal.rejection, tr)
            : refusal.status === 409 ? rejectionText("idempotency_conflict", tr) : refusal.message;
        settle({ sent: null, status: { kind: "error", text, reasonMissing: refusal.rejection === "missing_change_reason" } });
      }
    } finally {
      sending.current = false;
    }
  };

  const status = draft.status;
  return <form className="campaign-server-form" noValidate onSubmit={(event) => { event.preventDefault(); void save(); }}>
    <fieldset ref={fields} className="campaign-server-fields" disabled={saving}>
      <div className="campaign-edit-grid">
        {draft.mode === "create" && <label>{tr("캠페인 ID", "Campaign ID")}<input aria-label={tr("캠페인 ID", "Campaign ID")} value={draft.id} onChange={(event) => update({ id: event.target.value })} /></label>}
        <label>{tr("이름", "Name")}<input aria-label={tr("이름", "Name")} value={definition.title} onChange={(event) => setDefinition({ title: event.target.value })} /></label>
        <label>{tr("저장소", "Repository")}<select aria-label={tr("저장소", "Repository")} value={definition.repository} onChange={(event) => setDefinition({ repository: event.target.value as Definition["repository"] })}><option value="adk">AgentDesk</option><option value="cooking_heart">CookingHeart</option></select></label>
      </div>
      <label>{tr("목표", "Goal")}<textarea aria-label={tr("목표", "Goal")} rows={2} value={definition.goal} onChange={(event) => setDefinition({ goal: event.target.value })} /></label>
      {definition.nodes.map((node, index) => {
        const others = definition.nodes.map((other, at) => ({ other, at })).filter(({ at }) => at !== index);
        // A dependency naming no other task (left by a rename, copied, or this task's own ID) stays visible to
        // re-point or remove; renames never rewrite dependencies, so a link cannot move to another task unseen.
        const missing = node.dependencies.filter((id) => !others.some(({ other }) => other.id === id));
        return <fieldset key={index} className="campaign-server-task">
          <legend>{tr(`작업 ${index + 1}`, `Task ${index + 1}`)}</legend>
          <div className="campaign-edit-grid">
            <label>{tr("작업 ID", "Task ID")}<input aria-label={tr("작업 ID", "Task ID")} readOnly={draft.saved[index] === true} value={node.id} onChange={(event) => setNode(index, { id: event.target.value })} /></label>
            <label>{tr("작업 제목", "Task title")}<input aria-label={tr("작업 제목", "Task title")} value={node.title} onChange={(event) => setNode(index, { title: event.target.value })} /></label>
            <label>{tr("이슈 링크", "Issue link")}<input aria-label={tr("이슈 링크", "Issue link")} inputMode="url" value={node.issue_url} onChange={(event) => setNode(index, { issue_url: event.target.value })} /></label>
            <label>{tr("그룹", "Group")}<input aria-label={tr("그룹", "Group")} value={node.group ?? ""} placeholder={tr("미분류", "Ungrouped")} onChange={(event) => setNode(index, { group: event.target.value || null })} /></label>
          </div>
          {(definition.nodes.length > 1 || missing.length > 0) && <div className="campaign-server-dependencies" role="group" aria-label={tr("선행 작업", "Dependencies")}>
            <span>{tr("선행 작업", "Dependencies")}</span>
            {/* Rows are keyed by position: IDs are free text and may repeat while being typed. */}
            {others.map(({ other, at }) => <label key={`task:${at}`} className="campaign-check">
              <input type="checkbox" aria-label={tr(`${node.id}의 선행: ${other.id}`, `${node.id} depends on ${other.id}`)} checked={node.dependencies.includes(other.id)}
                onChange={(event) => setNode(index, { dependencies: event.target.checked ? [...node.dependencies, other.id] : node.dependencies.filter((id) => id !== other.id) })} />{other.id}
            </label>)}
            {missing.map((id, at) => {
              const marker = id === node.id ? tr("(이 작업)", "(this task)") : tr("(없는 작업)", "(no such task)");
              return <label key={`missing:${at}`} className="campaign-check">
                <input type="checkbox" aria-label={tr(`${node.id}의 선행: ${id} ${marker}`, `${node.id} depends on ${id} ${marker}`)} checked
                  onChange={() => setNode(index, { dependencies: node.dependencies.filter((dependency) => dependency !== id) })} />{id} {marker}
              </label>;
            })}
          </div>}
          {node.acceptance.map((ac, acIndex) => <div key={acIndex} className="campaign-server-ac">
            <input aria-label={tr("수용조건 ID", "Acceptance ID")} value={ac.id} onChange={(event) => setAcceptance(index, acIndex, { id: event.target.value })} />
            <input aria-label={tr("수용조건 설명", "Acceptance description")} value={ac.description} onChange={(event) => setAcceptance(index, acIndex, { description: event.target.value })} />
            <select aria-label={tr("검증 종류", "Verification")} value={ac.verification} onChange={(event) => setAcceptance(index, acIndex, { verification: event.target.value as Acceptance["verification"] })}>
              {VERIFICATIONS.map(([value, label]) => <option key={value} value={value}>{tr(...label)}</option>)}
            </select>
            <button type="button" onClick={() => setNode(index, { acceptance: node.acceptance.filter((_, at) => at !== acIndex) })}>{tr("수용조건 삭제", "Remove acceptance")}</button>
          </div>)}
          <div className="campaign-actions">
            <button type="button" onClick={() => setNode(index, { acceptance: [...node.acceptance, { id: "", description: "", verification: "test" }] })}>{tr("수용조건 추가", "Add acceptance")}</button>
            <button type="button" onClick={() => removeNode(index)}>{tr("작업 삭제", "Remove task")}</button>
          </div>
        </fieldset>;
      })}
      <button type="button" onClick={addNode}>{tr("작업 추가", "Add task")}</button>
      {draft.mode === "edit" && <div className="campaign-edit-grid">
        <label>{tr("변경 근거 링크", "Change reference")}<input aria-label={tr("변경 근거 링크", "Change reference")} inputMode="url" aria-invalid={reasonMissing} value={draft.change.reference} onChange={(event) => update({ change: { ...draft.change, reference: event.target.value } })} /></label>
        <label>{tr("변경 설명", "Change explanation")}<input aria-label={tr("변경 설명", "Change explanation")} aria-invalid={reasonMissing} value={draft.change.explanation} onChange={(event) => update({ change: { ...draft.change, explanation: event.target.value } })} /></label>
      </div>}
    </fieldset>
    {status?.kind === "lost" && <p role="status" className="campaign-filter-note">{tr("응답을 받지 못했습니다. 같은 내용으로 다시 보내도 두 번 적용되지 않습니다.", "No answer arrived. Saving again with the same content cannot apply it twice.")}</p>}
    {status?.kind === "error" && <p role="alert" className="campaign-filter-note">{status.text}</p>}
    {status?.kind === "conflict" && <div role="alert" className="campaign-filter-note">
      <p>{tr(`다른 곳에서 정의가 바뀌었습니다 (revision ${status.revision}). 초안은 그대로 둡니다.`, `This definition changed (revision ${status.revision}). Your draft is kept.`)}</p>
      <p>{tr("다른 곳에서 바뀐 항목", "Changed elsewhere")}{status.changed.length ? "" : `: ${tr("없음", "none")}`}</p>
      {status.changed.length > 0 && <ul>{status.changed.map(({ field, value }) => <li key={field}>{field}{value !== null && `: ${value}`}</li>)}</ul>}
      <button type="button" disabled={saving || disabled} onClick={() => void save(status.revision, status.latest)}>{tr(`revision ${status.revision} 위에 저장`, `Save over revision ${status.revision}`)}</button>
    </div>}
    <div className="campaign-actions">
      <button type="submit" disabled={saving || disabled}>{saving ? tr("저장 중…", "Saving…") : draft.mode === "create" ? tr("생성", "Create") : tr("저장", "Save")}</button>
      <button type="button" disabled={saving} onClick={onCancel}>{tr("취소", "Cancel")}</button>
    </div>
  </form>;
}
