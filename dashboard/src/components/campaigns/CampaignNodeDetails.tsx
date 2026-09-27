import { useState } from "react";
import { updateCampaignNode, type Campaign, type CampaignNode } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import { WidgetState } from "../common/WidgetState";
import { NODE_STATUSES, safeCampaignLink } from "./campaignModel";
import { Badge, LABELS, type Tr } from "./campaignPresentation";

export interface CampaignDraft { campaign: Campaign; node: CampaignNode }
export default function CampaignNodeDetails({ campaign, node, tr, onSaved, editing, onDraftChange }: {
  campaign: Campaign; node: CampaignNode; tr: Tr; onSaved: (campaign: Campaign) => void;
  editing: CampaignDraft | null; onDraftChange: (draft: CampaignDraft | null, expected?: CampaignDraft) => void;
}) {
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const draft = editing?.node;
  const patch = (changes: Partial<CampaignNode>) => { if (editing) onDraftChange({ ...editing, node: { ...editing.node, ...changes } }); };
  const save = async () => {
    if (!editing || saving) return;
    setSaving(true); setError(null);
    try {
      const updated = await updateCampaignNode(editing.campaign, editing.node);
      onSaved(updated); onDraftChange(null, editing);
    } catch (cause) {
      setError(cause instanceof ApiRequestError && cause.status === 409
        ? tr("다른 곳에서 이 캠페인을 수정했습니다. 취소 후 새로고침하여 최신 내용을 확인해 주세요.", "This campaign changed elsewhere. Cancel and refresh before editing again.")
        : cause instanceof Error ? cause.message : tr("저장하지 못했습니다.", "Could not save changes."));
    } finally { setSaving(false); }
  };
  return <section className="campaign-node-detail" aria-label={tr("작업 상세", "Task details")}>
    <div className="campaign-row"><h3>{node.title}</h3><Badge status={node.status} tr={tr} /></div>
    {draft ? <form onSubmit={(event) => { event.preventDefault(); void save(); }}>
      <p className="campaign-muted">{tr("다른 작업으로 이동하거나 상세를 닫아도 이 초안은 유지됩니다.", "This draft stays available when you switch tasks or close details.")}</p>
      <div className="campaign-edit-grid">
        <label>{tr("상태", "Status")}<select value={draft.status} onChange={(event) => patch({ status: event.target.value as CampaignNode["status"] })}>{NODE_STATUSES.map((status) => <option key={status} value={status}>{tr(...LABELS[status])}</option>)}</select></label>
        <label>{tr("단계", "Stage")}<input required value={draft.stage} onChange={(event) => patch({ stage: event.target.value })} /></label>
        <label>{tr("그룹", "Group")}<input maxLength={128} value={draft.group ?? ""} placeholder={tr("미분류", "Ungrouped")} onChange={(event) => patch({ group: event.target.value || null })} /></label>
        <label>{tr("라운드", "Round")}<input type="number" min={1} step={1} required value={draft.round} onChange={(event) => patch({ round: Number(event.target.value) })} /></label>
        <label>{tr("담당", "Assignee")}<input value={draft.assignee ?? ""} onChange={(event) => patch({ assignee: event.target.value || null })} /></label>
        <label>{tr("담당 세션", "Session")}<input value={draft.session_id ?? ""} onChange={(event) => patch({ session_id: event.target.value || null })} /></label>
        <label>{tr("프로바이더", "Provider")}<input value={draft.provider ?? ""} onChange={(event) => patch({ provider: event.target.value || null })} /></label>
      </div>
      <label>{tr("선행 작업 (여러 개 선택 가능)", "Dependencies (multiple selection)")}<select multiple size={Math.min(5, Math.max(2, campaign.nodes.length - 1))} value={draft.dependencies} onChange={(event) => patch({ dependencies: Array.from(event.target.selectedOptions, (option) => option.value) })}>{campaign.nodes.filter((candidate) => candidate.id !== node.id).map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.title}</option>)}</select></label>
      <label>{tr("다음 행동", "Next action")}<textarea rows={3} value={draft.next_action ?? ""} onChange={(event) => patch({ next_action: event.target.value || null })} /></label>
      <label>{tr("막힌 이유", "Blocker")}<textarea rows={2} value={draft.blocker ?? ""} onChange={(event) => patch({ blocker: event.target.value || null })} /></label>
      <label>{tr("한 줄 요지", "One-line gist")}<input value={draft.summary ?? ""} onChange={(event) => patch({ summary: event.target.value || null })} /></label>
      <label>{tr("기대효과", "Expected benefit")}<input value={draft.benefit ?? ""} onChange={(event) => patch({ benefit: event.target.value || null })} /></label>
      {error && <WidgetState kind="error" title={error} compact />}
      <div className="campaign-actions"><button type="submit" disabled={saving}>{saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}</button><button type="button" disabled={saving} onClick={() => { onDraftChange(null); setError(null); }}>{tr("취소", "Cancel")}</button></div>
    </form> : <>
      {(node.summary || node.benefit) && <p className="campaign-description">{node.summary}{node.summary && node.benefit && "\n"}{node.benefit && `${tr("기대효과", "Benefit")}: ${node.benefit}`}</p>}
      {node.details && <p className="campaign-description">{node.details}</p>}
      <dl className="campaign-detail-grid">
        <div><dt>{tr("단계 · 라운드", "Stage · round")}</dt><dd>{node.stage || "—"} · {node.round}</dd></div>
        <div><dt>{tr("그룹", "Group")}</dt><dd>{node.group || tr("미분류", "Ungrouped")}</dd></div>
        <div><dt>{tr("담당", "Assignee")}</dt><dd>{node.assignee || tr("미배정", "Unassigned")}</dd></div>
        <div><dt>{tr("담당 세션", "Session")}</dt><dd>{node.session_id || tr("연결 없음", "Not linked")}</dd></div>
        <div><dt>{tr("프로바이더", "Provider")}</dt><dd>{node.provider || "—"}</dd></div>
        <div><dt>{tr("작업 기록 시각", "Task updated")}</dt><dd>{node.updated_at ? new Date(node.updated_at).toLocaleString() : "—"}</dd></div>
        <div><dt>{tr("선행 작업", "Dependencies")}</dt><dd>{node.dependencies.length ? node.dependencies.map((id) => campaign.nodes.find((candidate) => candidate.id === id)?.title || id).join(" · ") : tr("없음", "None")}</dd></div>
        {node.head_sha && <div><dt>Commit</dt><dd>{node.head_sha}</dd></div>}
      </dl>
      <div className="campaign-next"><h4>{tr("다음 행동", "Next action")}</h4><p>{node.next_action || tr("아직 기록된 다음 행동이 없습니다.", "No next action recorded yet.")}</p></div>
      {node.blocker && <WidgetState kind="stale" title={tr("막힌 이유", "Blocker")} description={node.blocker} compact />}
      {node.evidence.length > 0 && <div><h4>{tr("검증 근거", "Evidence")}</h4><ul>{node.evidence.map((evidence, index) => <li key={index}>{evidence}</li>)}</ul></div>}
      {!!node.acceptance?.length && <div><h4>{tr("완료 조건", "Acceptance criteria")}</h4><ul>{node.acceptance.map((value, index) => <li key={index}>{value}</li>)}</ul></div>}
      {!!node.findings?.length && <div><h4>{tr("발견 사항", "Findings")}</h4><ul>{node.findings.map((value, index) => <li key={index}>{value}</li>)}</ul></div>}
      {!!node.evidence_records?.length && <div><h4>{tr("검증 기록", "Verification records")}</h4><ul>{node.evidence_records.map((record, index) => <li key={index}><strong>{record.summary}</strong>{record.result && <p>{record.result}</p>}{record.command && <code>{record.command}</code>}{record.head_sha && <p>Commit: {record.head_sha}</p>}{record.recorded_at && <p>{record.recorded_at}</p>}{record.references.map((reference, refIndex) => <p key={refIndex}>{reference}</p>)}</li>)}</ul></div>}
      <div className="campaign-actions">
        {safeCampaignLink(node.issue_url) && <a href={safeCampaignLink(node.issue_url)} target="_blank" rel="noopener noreferrer">{tr("이슈 보기", "View issue")} ↗</a>}
        {safeCampaignLink(node.pr_url) && <a href={safeCampaignLink(node.pr_url)} target="_blank" rel="noopener noreferrer">{tr("PR 보기", "View PR")} ↗</a>}
        <button onClick={() => onDraftChange({ campaign, node: { ...node } })}>{tr("작업 수정", "Edit task")}</button>
      </div>
    </>}
  </section>;
}
