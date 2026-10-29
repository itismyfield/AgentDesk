import { useState } from "react";
import {
  createServerCampaign, editServerCampaignDefinition, getServerCampaign, isDefinitiveAnswer, serverCampaignRejection,
  type ServerCampaignCommandResult, type ServerCampaignDefinition, type ServerCampaignDocument,
} from "../../api/serverCampaigns";
import type { Tr } from "./campaignPresentation";
import { changedDefinitionValues, newIdempotencyKey, nodeDefinitionText, rejectionText } from "./serverCampaignModel";

type Definition = ServerCampaignDefinition;
type Node = Definition["nodes"][number];
type Acceptance = Node["acceptance"][number];
type Status =
  | { kind: "error"; text: string; reasonMissing?: boolean }
  | { kind: "lost" }
  | { kind: "saved"; revision: number }
  | { kind: "conflict"; revision: number; latest: Definition; changed: Array<{ field: string; value: string }> };

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

function submittedChange(draft: DefinitionDraft): DefinitionDraft["change"] | null {
  return draft.mode === "edit" && (draft.change.reference || draft.change.explanation) ? draft.change : null;
}

export function createDraft(copyOf: Definition | null, newKey = newIdempotencyKey): DefinitionDraft {
  return {
    mode: "create", id: "", baseRevision: null, baseDefinition: null,
    definition: structuredClone(copyOf ?? EMPTY), change: { reference: "", explanation: "" }, sent: null, status: null, newKey,
  };
}

export function editDraft(document: ServerCampaignDocument, newKey = newIdempotencyKey): DefinitionDraft {
  return {
    mode: "edit", id: document.view.campaign_id, baseRevision: document.view.definition_revision,
    baseDefinition: document.definition, definition: structuredClone(document.definition),
    change: { reference: "", explanation: "" }, sent: null, status: null, newKey,
  };
}

/** A create form nobody has typed into yet. */
export function isUntouchedCreate(draft: DefinitionDraft): boolean {
  return draft.mode === "create" && !draft.id && draft.sent === null && JSON.stringify(draft.definition) === JSON.stringify(EMPTY);
}

export default function ServerCampaignForm({ draft, onDraftChange, onSaved, onConflict, onCancel, disabled, tr }: {
  draft: DefinitionDraft;
  /** Updates apply to the latest draft; returning null closes it. */
  onDraftChange: (update: (current: DefinitionDraft) => DefinitionDraft | null) => void;
  onSaved: (id: string, result: ServerCampaignCommandResult) => void;
  /** The server holds a newer definition; the caller refreshes what it shows. */
  onConflict?: () => void;
  onCancel: () => void;
  /** The snapshot is stale or the server refuses edits: nothing may be sent. */
  disabled: boolean;
  tr: Tr;
}) {
  const [saving, setSaving] = useState(false);
  const { definition } = draft;
  const update = (changes: Partial<DefinitionDraft>) => onDraftChange((current) => ({ ...current, ...changes }));
  const setDefinition = (changes: Partial<Definition>) => update({ definition: { ...definition, ...changes } });
  const setNode = (index: number, changes: Partial<Node>) =>
    setDefinition({ nodes: definition.nodes.map((node, at) => at === index ? { ...node, ...changes } : node) });
  const setAcceptance = (index: number, acIndex: number, changes: Partial<Acceptance>) =>
    setNode(index, { acceptance: definition.nodes[index].acceptance.map((ac, at) => at === acIndex ? { ...ac, ...changes } : ac) });
  const removeNode = (index: number) => {
    const removed = definition.nodes[index].id;
    setDefinition({
      nodes: definition.nodes.filter((_, at) => at !== index)
        .map((node) => ({ ...node, dependencies: node.dependencies.filter((id) => id !== removed) })),
    });
  };
  const addNode = () => {
    let next = definition.nodes.length + 1;
    while (definition.nodes.some((node) => node.id === `task-${next}`)) next++;
    setDefinition({ nodes: [...definition.nodes, { id: `task-${next}`, title: "", issue_url: "", acceptance: [], dependencies: [], group: null }] });
  };
  const reasonMissing = draft.status?.kind === "error" && draft.status.reasonMissing === true;

  const save = async (expectedRevision = draft.baseRevision, baseDefinition = draft.baseDefinition) => {
    if (saving || disabled) return;
    const change = submittedChange(draft);
    const body = JSON.stringify([draft.mode, draft.id, expectedRevision, definition, change]);
    const key = draft.sent?.body === body ? draft.sent.key : draft.newKey();
    // Answers merge only their own state into the latest draft, so edits made while waiting survive.
    const settle = (status: Status, decided: boolean) => onDraftChange((current) => ({
      ...current, status, sent: decided && current.sent?.key === key ? null : current.sent,
    }));
    onDraftChange((current) => ({ ...current, baseRevision: expectedRevision, baseDefinition, sent: { key, body }, status: null }));
    setSaving(true);
    try {
      const result = draft.mode === "create"
        ? await createServerCampaign(draft.id, definition, key)
        : await editServerCampaignDefinition(draft.id, expectedRevision ?? 0, definition, change, key);
      // Edits made after sending stay open as a draft on top of the saved revision.
      onDraftChange((current) => JSON.stringify([current.definition, submittedChange(current)]) === JSON.stringify([definition, change]) ? null : {
        ...current, mode: "edit", id: draft.id, baseRevision: result.definition_revision, baseDefinition: definition,
        sent: null, status: { kind: "saved", revision: result.definition_revision },
      });
      onSaved(draft.id, result);
    } catch (error) {
      const refusal = serverCampaignRejection(error);
      if (!isDefinitiveAnswer(refusal)) {
        settle({ kind: "lost" }, false);
        return;
      }
      // A decided request keeps its answer under its key; the next save sends a new key.
      if (!refusal) return;
      if (refusal.status === 409 && refusal.rejection === "stale_definition" && draft.mode === "edit") {
        // Show the newer values next to the draft; the draft itself is never overwritten.
        onConflict?.();
        const latest = await getServerCampaign(draft.id).catch(() => null);
        settle(latest && baseDefinition
          ? { kind: "conflict", revision: latest.view.definition_revision, latest: latest.definition, changed: changedDefinitionValues(baseDefinition, latest.definition) }
          : { kind: "error", text: tr("다른 곳에서 정의가 바뀌었습니다. 초안은 그대로 둡니다.", "The definition changed elsewhere. Your draft is kept.") }, true);
      } else {
        const detail = refusal.definitionError;
        const text = detail
          ? [tr(...(DEFINITION_ERRORS[detail.code] ?? [detail.code, detail.code])), detail.path.length ? detail.path.join(" → ") : detail.target].filter(Boolean).join(" · ")
          : refusal.rejection ? rejectionText(refusal.rejection, tr)
            : refusal.status === 409 ? rejectionText("idempotency_conflict", tr) : refusal.message;
        settle({ kind: "error", text, reasonMissing: refusal.rejection === "missing_change_reason" }, true);
      }
    } finally {
      setSaving(false);
    }
  };

  const status = draft.status;
  return <form className="campaign-server-form" noValidate onSubmit={(event) => { event.preventDefault(); void save(); }}>
    <div className="campaign-edit-grid">
      {draft.mode === "create" && <label>{tr("캠페인 ID", "Campaign ID")}<input aria-label={tr("캠페인 ID", "Campaign ID")} autoFocus disabled={saving} value={draft.id} onChange={(event) => update({ id: event.target.value })} /></label>}
      <label>{tr("이름", "Name")}<input aria-label={tr("이름", "Name")} autoFocus={draft.mode === "edit"} value={definition.title} onChange={(event) => setDefinition({ title: event.target.value })} /></label>
      <label>{tr("저장소", "Repository")}<select aria-label={tr("저장소", "Repository")} value={definition.repository} onChange={(event) => setDefinition({ repository: event.target.value as Definition["repository"] })}><option value="adk">AgentDesk</option><option value="cooking_heart">CookingHeart</option></select></label>
    </div>
    <label>{tr("목표", "Goal")}<textarea aria-label={tr("목표", "Goal")} rows={2} value={definition.goal} onChange={(event) => setDefinition({ goal: event.target.value })} /></label>
    {definition.nodes.map((node, index) => {
      const existing = draft.baseDefinition?.nodes.some((candidate) => candidate.id === node.id) ?? false;
      return <fieldset key={index} className="campaign-server-task">
        <legend>{tr(`작업 ${index + 1}`, `Task ${index + 1}`)}</legend>
        <div className="campaign-edit-grid">
          <label>{tr("작업 ID", "Task ID")}<input aria-label={tr("작업 ID", "Task ID")} readOnly={existing} value={node.id} onChange={(event) => setNode(index, { id: event.target.value })} /></label>
          <label>{tr("작업 제목", "Task title")}<input aria-label={tr("작업 제목", "Task title")} value={node.title} onChange={(event) => setNode(index, { title: event.target.value })} /></label>
          <label>{tr("이슈 링크", "Issue link")}<input aria-label={tr("이슈 링크", "Issue link")} inputMode="url" value={node.issue_url} onChange={(event) => setNode(index, { issue_url: event.target.value })} /></label>
          <label>{tr("그룹", "Group")}<input aria-label={tr("그룹", "Group")} value={node.group ?? ""} placeholder={tr("미분류", "Ungrouped")} onChange={(event) => setNode(index, { group: event.target.value || null })} /></label>
        </div>
        {definition.nodes.length > 1 && <div className="campaign-server-dependencies" role="group" aria-label={tr("선행 작업", "Dependencies")}>
          <span>{tr("선행 작업", "Dependencies")}</span>
          {definition.nodes.filter((other) => other !== node).map((other) => <label key={other.id} className="campaign-check">
            <input type="checkbox" aria-label={tr(`${node.id}의 선행: ${other.id}`, `${node.id} depends on ${other.id}`)} checked={node.dependencies.includes(other.id)}
              onChange={(event) => setNode(index, { dependencies: event.target.checked ? [...node.dependencies, other.id] : node.dependencies.filter((id) => id !== other.id) })} />{other.id}
          </label>)}
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
    {status?.kind === "lost" && <p role="status" className="campaign-filter-note">{tr("응답을 받지 못했습니다. 같은 내용으로 다시 보내도 두 번 적용되지 않습니다.", "No answer arrived. Saving again with the same content cannot apply it twice.")}</p>}
    {status?.kind === "saved" && <p role="status" className="campaign-filter-note">{tr(`revision ${status.revision}에 저장했습니다. 그 뒤 고친 내용은 아직 저장되지 않았습니다.`, `Saved as revision ${status.revision}. Changes made since then are not saved yet.`)}</p>}
    {status?.kind === "error" && <p role="alert" className="campaign-filter-note">{status.text}</p>}
    {status?.kind === "conflict" && <div role="alert" className="campaign-filter-note">
      <p>{tr(`다른 곳에서 정의가 바뀌었습니다 (revision ${status.revision}). 초안은 그대로 둡니다.`, `This definition changed (revision ${status.revision}). Your draft is kept.`)}</p>
      <p>{tr("다른 곳에서 바뀐 항목", "Changed elsewhere")}: {status.changed.map(({ field }) => field).join(", ") || tr("없음", "none")}</p>
      {status.changed.length > 0 && <dl className="campaign-server-latest" aria-label={tr(`revision ${status.revision}의 값`, `Values in revision ${status.revision}`)}>
        {status.changed.map(({ field, value }) => <div key={field}><dt>{field}</dt><dd>{value || "—"}</dd></div>)}
      </dl>}
      <details className="campaign-server-latest-definition">
        <summary>{tr(`revision ${status.revision}의 전체 정의`, `Full definition in revision ${status.revision}`)}</summary>
        <p>{status.latest.title} · {status.latest.repository} · {status.latest.goal}</p>
        <dl className="campaign-server-latest">
          {status.latest.nodes.map((node) => <div key={node.id}><dt>{node.id}</dt><dd>{nodeDefinitionText(node)}</dd></div>)}
        </dl>
      </details>
      <button type="button" disabled={saving || disabled} onClick={() => void save(status.revision, status.latest)}>{tr(`revision ${status.revision} 위에 저장`, `Save over revision ${status.revision}`)}</button>
    </div>}
    <div className="campaign-actions">
      <button type="submit" disabled={saving || disabled}>{saving ? tr("저장 중…", "Saving…") : draft.mode === "create" ? tr("생성", "Create") : tr("저장", "Save")}</button>
      <button type="button" disabled={saving} onClick={onCancel}>{tr("취소", "Cancel")}</button>
    </div>
  </form>;
}
