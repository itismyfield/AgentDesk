import { ChevronDown } from "lucide-react";
import { useState } from "react";
import { updateCampaignNode, type Campaign, type CampaignNode, type CampaignNodeLive } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import { Button } from "../../kit/free/button-clean";
import { Select } from "../../kit/free/select-clean";
import { TextArea } from "../../kit/free/text-area-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { Eyebrow, InspSection, KeyValue, RuleItem } from "../common/KitPrimitives";
import { WidgetState } from "../common/WidgetState";
import { NODE_STATUSES, campaignTaskTitle, safeCampaignLink } from "./campaignModel";
import { legacyTask } from "./campaignOverviewModel";
import { Badge, LABELS, LiveChip, cardStatusLabel, dispatchLabel, formatCampaignTime, queueLabel, sessionLabel, type Tr } from "./campaignPresentation";
import { RecentList, StatusCard, useScreenTitle } from "./TaskScreen";

export interface CampaignDraft {
  campaign: Campaign;
  node: CampaignNode;
}
export default function CampaignNodeDetails({
  campaign,
  node,
  live,
  tr,
  onSaved,
  editing,
  onDraftChange,
}: {
  campaign: Campaign;
  node: CampaignNode;
  live?: CampaignNodeLive;
  tr: Tr;
  onSaved: (campaign: Campaign) => void;
  editing: CampaignDraft | null;
  onDraftChange: (draft: CampaignDraft | null, expected?: CampaignDraft) => void;
}) {
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const draft = editing?.node;
  const shown = legacyTask(node, live); // the status and blocker the overview shows
  const inScreen = useScreenTitle(campaignTaskTitle(node));
  const pr = safeCampaignLink(node.pr_url);
  const prNumber = pr && new URL(pr).pathname.match(/\/pull\/(\d+)/)?.[1];
  const patch = (changes: Partial<CampaignNode>) => {
    if (editing) onDraftChange({ ...editing, node: { ...editing.node, ...changes } });
  };
  const save = async () => {
    if (!editing || saving) return;
    setSaving(true);
    setError(null);
    try {
      const updated = await updateCampaignNode(editing.campaign, editing.node);
      onSaved(updated);
      onDraftChange(null, editing);
    } catch (cause) {
      setError(
        cause instanceof ApiRequestError && cause.status === 409
          ? tr(
              "다른 곳에서 이 캠페인을 수정했습니다. 취소 후 새로고침하여 최신 내용을 확인해 주세요.",
              "This campaign changed elsewhere. Cancel and refresh before editing again.",
            )
          : cause instanceof Error
            ? cause.message
            : tr("저장하지 못했습니다.", "Could not save changes."),
      );
    } finally {
      setSaving(false);
    }
  };
  return (
    <section className="campaign-node-detail" aria-label={tr("작업 상세", "Task details")}>
      {!inScreen && <div className="campaign-row"><h3>{node.title}</h3></div>}
      {draft ? (
        <form onSubmit={(event) => { event.preventDefault(); void save(); }}>
          <p className="campaign-muted">
            {tr("다른 작업으로 이동하거나 상세를 닫아도 이 초안은 유지됩니다.", "This draft stays available when you switch tasks or close details.")}
          </p>
          <div className="campaign-edit-grid grid grid-cols-1 gap-3 text-sm sm:grid-cols-2">
            <div className="grid gap-1.5">
              <Eyebrow>{tr("상태", "Status")}</Eyebrow>
              <Select aria-label={tr("상태", "Status")} size="lg" value={draft.status}
                options={NODE_STATUSES.map((status) => ({ value: status, label: tr(...LABELS[status]) }))}
                onChange={(value) => patch({ status: value as CampaignNode["status"] })} />
            </div>
            <label className="grid gap-1.5">
              <Eyebrow>{tr("단계", "Stage")}</Eyebrow>
              <TextField size="lg" required value={draft.stage} onChange={(event) => patch({ stage: event.target.value })} />
            </label>
            <label className="grid gap-1.5">
              <Eyebrow>{tr("그룹", "Group")}</Eyebrow>
              <TextField size="lg" maxLength={128} value={draft.group ?? ""} placeholder={tr("미분류", "Ungrouped")} onChange={(event) => patch({ group: event.target.value || null })} />
            </label>
            <label className="grid gap-1.5">
              <Eyebrow>{tr("라운드", "Round")}</Eyebrow>
              <TextField size="lg" type="number" min={1} step={1} required value={String(draft.round)} onChange={(event) => patch({ round: Number(event.target.value) })} />
            </label>
            <label className="grid gap-1.5">
              <Eyebrow>{tr("담당", "Assignee")}</Eyebrow>
              <TextField size="lg" value={draft.assignee ?? ""} onChange={(event) => patch({ assignee: event.target.value || null })} />
            </label>
            <label className="grid gap-1.5">
              <Eyebrow>{tr("담당 세션", "Session")}</Eyebrow>
              <TextField size="lg" value={draft.session_id ?? ""} onChange={(event) => patch({ session_id: event.target.value || null })} />
            </label>
            <label className="grid gap-1.5">
              <Eyebrow>{tr("프로바이더", "Provider")}</Eyebrow>
              <TextField size="lg" value={draft.provider ?? ""} onChange={(event) => patch({ provider: event.target.value || null })} />
            </label>
          </div>
          <label className="grid gap-1.5">
            <Eyebrow>{tr("선행 작업 (여러 개 선택 가능)", "Dependencies (multiple selection)")}</Eyebrow>
            <select
              multiple
              size={Math.min(5, Math.max(2, campaign.nodes.length - 1))}
              value={draft.dependencies}
              onChange={(event) => patch({ dependencies: Array.from(event.target.selectedOptions, (option) => option.value) })}
              className="min-h-11 w-full rounded-[var(--r-sm)] border border-[var(--line)] bg-[var(--bg-2)] p-2 text-sm text-[var(--fg)]"
            >
              {campaign.nodes.filter((candidate) => candidate.id !== node.id).map((candidate) => (
                <option key={candidate.id} value={candidate.id}>{candidate.title}</option>
              ))}
            </select>
          </label>
          <label className="grid gap-1.5">
            <Eyebrow>{tr("다음 행동", "Next action")}</Eyebrow>
            <TextArea minRows={3} value={draft.next_action ?? ""} onChange={(event) => patch({ next_action: event.target.value || null })} />
          </label>
          <label className="grid gap-1.5">
            <Eyebrow>{tr("막힌 이유", "Blocker")}</Eyebrow>
            <TextArea minRows={2} value={draft.blocker ?? ""} onChange={(event) => patch({ blocker: event.target.value || null })} />
          </label>
          <label className="grid gap-1.5">
            <Eyebrow>{tr("한 줄 요지", "One-line gist")}</Eyebrow>
            <TextField size="lg" value={draft.summary ?? ""} onChange={(event) => patch({ summary: event.target.value || null })} />
          </label>
          <label className="grid gap-1.5">
            <Eyebrow>{tr("기대효과", "Expected benefit")}</Eyebrow>
            <TextField size="lg" value={draft.benefit ?? ""} onChange={(event) => patch({ benefit: event.target.value || null })} />
          </label>
          {error && <WidgetState kind="error" title={error} compact />}
          <div className="campaign-actions">
            <Button type="submit" variant="primary" size="sm" disabled={saving}>{saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}</Button>
            <Button variant="secondary" size="sm" disabled={saving} onClick={() => { onDraftChange(null); setError(null); }}>{tr("취소", "Cancel")}</Button>
          </div>
        </form>
      ) : (
        <>
          {campaign.nodes_partial && <p role="status" className="campaign-muted">{tr("세부 내용과 검증 기록을 불러오는 중이에요…", "Loading details and verification records…")}</p>}
          <StatusCard label={tr("지금 상태", "Now")} warn={shown.attention}>
            <span className="flex flex-wrap items-center gap-2"><Badge status={shown.status} tr={tr} /><LiveChip live={live} tr={tr} /></span>
            <p className="campaign-description text-[var(--fg)]">{node.stage || "—"} · {tr(`라운드 ${node.round}`, `round ${node.round}`)}</p>
            {shown.blocker && <p className={shown.attention ? "text-[var(--warn)]" : ""}>{shown.status === "pending" ? tr("대기 사유", "Waiting on") : tr("막힌 이유", "Blocker")}: {shown.blocker}</p>}
            {node.post_merge_hold && <p className="text-[var(--warn)]">{tr("보류: 머지 뒤 남은 일", "On hold: post-merge work")}: {node.post_merge_hold}</p>}
            {node.next_action && <p className="campaign-next !rounded-none !bg-transparent !p-0 whitespace-pre-wrap">{tr("다음 행동", "Next action")}: {node.next_action}</p>}
          </StatusCard>
          <InspSection as="section" aria-label={tr("진행 기록", "Progress record")} className="px-0 sm:px-0">
            <Eyebrow>{tr("진행 기록", "Progress record")}</Eyebrow>
            <dl className="m-0 grid gap-1.5">
              <KeyValue dl label={tr("선행 작업", "Dependencies")}>
                {node.dependencies.length
                  ? node.dependencies.map((id) => campaign.nodes.find((candidate) => candidate.id === id)?.title || id).join(" · ")
                  : tr("없음", "None")}
              </KeyValue>
              {pr && <KeyValue dl label="PR"><a href={pr} target="_blank" rel="noopener noreferrer">{prNumber ? `#${prNumber}` : tr("PR 보기", "View PR")} ↗</a></KeyValue>}
              {node.head_sha && <KeyValue dl label={tr("커밋", "Commit")}><code title={node.head_sha}>{node.head_sha.slice(0, 7)}</code></KeyValue>}
            </dl>
            {!!node.evidence_records?.length && (
              <ol aria-label={tr("단계 기록", "Stage record")}>
                {[...node.evidence_records].sort((a, b) => (Date.parse(a.recorded_at ?? "") || 0) - (Date.parse(b.recorded_at ?? "") || 0)).map((record, index) => (
                  <li key={index}>
                    {record.recorded_at && <time className="campaign-muted" dateTime={record.recorded_at}>{formatCampaignTime(record.recorded_at, tr)} · </time>}
                    <strong>{record.summary}</strong>
                    {record.result && <p>{record.result}</p>}
                    {record.command && <code>{record.command}</code>}
                    {record.head_sha && <p title={record.head_sha}>Commit: {record.head_sha.slice(0, 7)}</p>}
                    {record.references.map((reference, refIndex) => <p key={refIndex}>{reference}</p>)}
                  </li>
                ))}
              </ol>
            )}
          </InspSection>
          {node.evidence.length > 0 && <RecentList label={tr("검증 근거", "Evidence")} items={node.evidence} tr={tr} />}
          {(node.summary || node.benefit) && (
            <p className="campaign-description">
              {node.summary}{node.summary && node.benefit && "\n"}{node.benefit && `${tr("기대효과", "Benefit")}: ${node.benefit}`}
            </p>
          )}
          {!!node.acceptance?.length && (
            <InspSection className="px-0 sm:px-0">
              <Eyebrow>{tr("완료 조건", "Acceptance criteria")}</Eyebrow>
              <div className="grid gap-1.5">{node.acceptance.map((value, index) => <RuleItem key={index}>{value}</RuleItem>)}</div>
            </InspSection>
          )}
          {node.details && (
            <details className="group min-w-0">
              <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 text-xs text-[var(--fg-muted)] [&::-webkit-details-marker]:hidden">
                <ChevronDown aria-hidden className="size-3.5 -rotate-90 transition-transform group-open:rotate-0 motion-reduce:transition-none" />{tr("작업 설명", "Task notes")}
              </summary>
              <p className="campaign-description">{node.details}</p>
            </details>
          )}
          {!!node.findings?.length && (
            <InspSection className="px-0 sm:px-0">
              <Eyebrow>{tr("발견 사항", "Findings")}</Eyebrow>
              <div className="grid gap-1.5">{node.findings.map((value, index) => <RuleItem key={index} tone="warn">{value}</RuleItem>)}</div>
            </InspSection>
          )}
          {safeCampaignLink(node.issue_url) && (
            <div className="campaign-actions">
              <a href={safeCampaignLink(node.issue_url)} target="_blank" rel="noopener noreferrer">{tr("이슈 보기", "View issue")} ↗</a>
            </div>
          )}
          {live && (
            <InspSection className="campaign-live-detail px-0 sm:px-0">
              <Eyebrow>{tr("실제 실행 상태", "Live execution")}</Eyebrow>
              <dl className="m-0 grid gap-1.5">
                <KeyValue dl label={tr("이슈 카드 단계", "Issue card step")}>{cardStatusLabel(live.card_status, tr)}</KeyValue>
                <KeyValue dl label={live.running ? tr("진행 중 디스패치", "Running dispatch") : tr("최근 디스패치", "Latest dispatch")}>{dispatchLabel(live, tr) ?? tr("없음", "None")}</KeyValue>
                {(live.running || live.dispatch_status) && <KeyValue dl label={tr("세션", "Session")}>{sessionLabel(live, tr)}</KeyValue>}
                <KeyValue dl label={tr("자동 큐", "Auto-queue")}>{queueLabel(live, tr) ?? tr("큐에 없음", "Not queued")}</KeyValue>
              </dl>
            </InspSection>
          )}
          <InspSection as="dl" className="px-0 sm:px-0">
            <KeyValue dl label={tr("그룹", "Group")}>{node.group || tr("미분류", "Ungrouped")}</KeyValue>
            <KeyValue dl label={tr("담당", "Assignee")}>{node.assignee || tr("미배정", "Unassigned")}</KeyValue>
            <KeyValue dl label={tr("담당 세션", "Session")}>{node.session_id || tr("연결 없음", "Not linked")}</KeyValue>
            <KeyValue dl label={tr("프로바이더", "Provider")}>{node.provider || "—"}</KeyValue>
            <KeyValue dl num label={tr("작업 기록 시각", "Task updated")}>{formatCampaignTime(node.updated_at, tr)}</KeyValue>
          </InspSection>
          <div className="campaign-actions">
            <Button variant="secondary" size="sm" onClick={() => onDraftChange({ campaign, node: { ...node } })}>{tr("작업 수정", "Edit task")}</Button>
          </div>
        </>
      )}
    </section>
  );
}
