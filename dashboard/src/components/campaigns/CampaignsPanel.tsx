import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Background, Controls, MarkerType, Position, ReactFlow } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { getCampaigns, updateCampaignNode, type Campaign, type CampaignNode } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { readLocalStorageValue, writeLocalStorageValue } from "../../lib/useLocalStorage";
import { WidgetState } from "../common/WidgetState";
import { NODE_STATUSES, campaignPositions, campaignProgress, safeCampaignLink } from "./campaignModel";
import "./campaigns.css";

const LABELS: Record<string, [string, string]> = {
  planned: ["계획됨", "Planned"], active: ["진행 중", "Active"], paused: ["일시 중지", "Paused"],
  completed: ["완료", "Completed"], cancelled: ["취소됨", "Cancelled"], pending: ["대기", "Pending"],
  running: ["진행 중", "Running"], blocked: ["막힘", "Blocked"], failed: ["실패", "Failed"], skipped: ["건너뜀", "Skipped"],
};
const COLORS: Record<string, string> = { completed: "#16a34a", running: "#3b82f6", blocked: "#f59e0b", failed: "#ef4444", skipped: "#94a3b8", pending: "#94a3b8" };
type Tr = (ko: string, en: string) => string;

function Badge({ status, tr }: { status: string; tr: Tr }) {
  return <span className={`campaign-badge campaign-status-${status}`}>{LABELS[status] ? tr(...LABELS[status]) : status}</span>;
}

function CampaignGraph({ campaign, selected, onSelect, tr }: { campaign: Campaign; selected: string | null; onSelect: (id: string) => void; tr: Tr }) {
  const positions = useMemo(() => campaignPositions(campaign.nodes), [campaign.nodes]);
  const nodes = useMemo(() => campaign.nodes.map((node) => ({
    id: node.id,
    position: positions.get(node.id)!,
    sourcePosition: Position.Right,
    targetPosition: Position.Left,
    selected: selected === node.id,
    ariaLabel: `${node.title}, ${tr(...LABELS[node.status])}`,
    data: { label: <div className="campaign-graph-node"><strong>{node.title}</strong><span>{node.stage || tr("단계 미지정", "No stage")} · {tr("라운드", "Round")} {node.round}</span><Badge status={node.status} tr={tr} /></div> },
    style: { width: 230, border: `2px solid ${COLORS[node.status]}`, borderRadius: 12, background: "var(--th-card-bg)", color: "var(--th-text-primary)", boxShadow: selected === node.id ? "0 0 0 3px var(--th-accent-info)" : undefined },
  })), [campaign.nodes, positions, selected, tr]);
  const edges = useMemo(() => campaign.nodes.flatMap((node) => node.dependencies.map((dependency) => ({
    id: `${dependency}:${node.id}`, source: dependency, target: node.id, type: "smoothstep",
    markerEnd: { type: MarkerType.ArrowClosed }, style: { stroke: "var(--th-text-muted)", strokeWidth: 2 },
  }))), [campaign.nodes]);
  return <div className="campaign-graph" aria-label={tr("작업 의존 관계", "Task dependencies")}>
    <ReactFlow key={campaign.id} nodes={nodes} edges={edges} fitView minZoom={0.2} maxZoom={1.5} nodesDraggable={false} nodesConnectable={false} onNodeClick={(_, node) => onSelect(node.id)}>
      <Background /><Controls showInteractive={false} />
    </ReactFlow>
  </div>;
}

function NodeDetails({ campaign, node, tr, onSaved }: { campaign: Campaign; node: CampaignNode; tr: Tr; onSaved: (campaign: Campaign) => void }) {
  const [editing, setEditing] = useState<{ campaign: Campaign; node: CampaignNode } | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const draft = editing?.node;
  const patch = (changes: Partial<CampaignNode>) => setEditing((current) => current ? { ...current, node: { ...current.node, ...changes } } : null);
  const save = async () => {
    if (!editing || saving) return;
    setSaving(true); setError(null);
    try {
      const updated = await updateCampaignNode(editing.campaign, editing.node);
      onSaved(updated); setEditing(null);
    } catch (cause) {
      setError(cause instanceof ApiRequestError && cause.status === 409
        ? tr("다른 곳에서 이 캠페인을 수정했습니다. 취소 후 새로고침하여 최신 내용을 확인해 주세요.", "This campaign changed elsewhere. Cancel and refresh before editing again.")
        : cause instanceof Error ? cause.message : tr("저장하지 못했습니다.", "Could not save changes."));
    } finally { setSaving(false); }
  };
  return <section className="campaign-node-detail" aria-label={tr("작업 상세", "Task details")}>
    <div className="campaign-row"><h3>{node.title}</h3><Badge status={node.status} tr={tr} /></div>
    {draft ? <form onSubmit={(event) => { event.preventDefault(); void save(); }}>
      <div className="campaign-edit-grid">
        <label>{tr("상태", "Status")}<select value={draft.status} onChange={(event) => patch({ status: event.target.value as CampaignNode["status"] })}>{NODE_STATUSES.map((status) => <option key={status} value={status}>{tr(...LABELS[status])}</option>)}</select></label>
        <label>{tr("단계", "Stage")}<input required value={draft.stage} onChange={(event) => patch({ stage: event.target.value })} /></label>
        <label>{tr("라운드", "Round")}<input type="number" min={1} step={1} required value={draft.round} onChange={(event) => patch({ round: Number(event.target.value) })} /></label>
        <label>{tr("담당", "Assignee")}<input value={draft.assignee ?? ""} onChange={(event) => patch({ assignee: event.target.value || null })} /></label>
        <label>{tr("담당 세션", "Session")}<input value={draft.session_id ?? ""} onChange={(event) => patch({ session_id: event.target.value || null })} /></label>
        <label>{tr("프로바이더", "Provider")}<input value={draft.provider ?? ""} onChange={(event) => patch({ provider: event.target.value || null })} /></label>
      </div>
      <label>{tr("선행 작업 (여러 개 선택 가능)", "Dependencies (multiple selection)")}<select multiple size={Math.min(5, Math.max(2, campaign.nodes.length - 1))} value={draft.dependencies} onChange={(event) => patch({ dependencies: Array.from(event.target.selectedOptions, (option) => option.value) })}>{campaign.nodes.filter((candidate) => candidate.id !== node.id).map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.title}</option>)}</select></label>
      <label>{tr("다음 행동", "Next action")}<textarea rows={3} value={draft.next_action ?? ""} onChange={(event) => patch({ next_action: event.target.value || null })} /></label>
      <label>{tr("막힌 이유", "Blocker")}<textarea rows={2} value={draft.blocker ?? ""} onChange={(event) => patch({ blocker: event.target.value || null })} /></label>
      {error && <WidgetState kind="error" title={error} compact />}
      <div className="campaign-actions"><button type="submit" disabled={saving}>{saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}</button><button type="button" disabled={saving} onClick={() => { setEditing(null); setError(null); }}>{tr("취소", "Cancel")}</button></div>
    </form> : <>
      {node.details && <p className="campaign-description">{node.details}</p>}
      <dl className="campaign-detail-grid">
        <div><dt>{tr("단계 · 라운드", "Stage · round")}</dt><dd>{node.stage || "—"} · {node.round}</dd></div>
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
        <button onClick={() => setEditing({ campaign, node: { ...node } })}>{tr("작업 수정", "Edit task")}</button>
      </div>
    </>}
  </section>;
}

export default function CampaignsPanel({ language }: { language: string }) {
  const tr: Tr = useCallback((ko, en) => language === "ko" ? ko : en, [language]);
  const [campaigns, setCampaigns] = useState<Campaign[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(() => readLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, null, { validate: (value): value is string | null => value === null || typeof value === "string" }));
  const [nodeId, setNodeId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshedAt, setRefreshedAt] = useState<number | null>(null);
  const mounted = useRef(false);
  const requestId = useRef(0);
  const refresh = useCallback(async () => {
    const id = ++requestId.current;
    setLoading(true);
    try {
      const values = await getCampaigns();
      if (!mounted.current || id !== requestId.current) return;
      setCampaigns(values); setError(null); setRefreshedAt(Date.now());
      setSelectedId((current) => values.some((value) => value.id === current) ? current : values.find((value) => value.status === "active")?.id ?? values[0]?.id ?? null);
    } catch (cause) {
      if (mounted.current && id === requestId.current) setError(cause instanceof Error ? cause.message : "Unable to load campaigns");
    } finally { if (mounted.current && id === requestId.current) setLoading(false); }
  }, []);
  useEffect(() => {
    mounted.current = true;
    void refresh();
    const interval = window.setInterval(() => { if (document.visibilityState === "visible") void refresh(); }, 15_000);
    return () => { mounted.current = false; requestId.current++; window.clearInterval(interval); };
  }, [refresh]);
  useEffect(() => { writeLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, selectedId); }, [selectedId]);
  const campaign = campaigns.find((value) => value.id === selectedId);
  const selectedNode = campaign?.nodes.find((node) => node.id === nodeId) ?? campaign?.nodes.find((node) => node.status === "running") ?? campaign?.nodes.find((node) => node.status === "blocked") ?? campaign?.nodes.find((node) => node.status === "pending") ?? campaign?.nodes[0];
  const progress = campaign ? campaignProgress(campaign.nodes) : null;
  const onSaved = (updated: Campaign) => {
    requestId.current++; // A poll started before this save must not replace its result.
    setLoading(false); setError(null); setRefreshedAt(Date.now());
    setCampaigns((current) => current.map((value) => value.id === updated.id ? updated : value));
  };
  return <section className="campaigns-panel">
    <header className="campaign-row"><div><h2>{tr("캠페인", "Campaigns")}</h2><p>{tr("작업 흐름과 담당 세션을 확인하고, 다음 행동부터 이어가세요.", "See the workflow, responsible sessions, and where to continue.")}</p></div><button disabled={loading} onClick={() => void refresh()}>{loading ? tr("불러오는 중…", "Refreshing…") : tr("새로고침", "Refresh")}</button></header>
    {refreshedAt && <p className="campaign-muted">{tr("마지막 확인", "Last checked")} {new Date(refreshedAt).toLocaleTimeString(language)} · {tr("15초마다 갱신", "Refreshes every 15 seconds")}</p>}
    {error && <WidgetState kind={campaigns.length ? "stale" : "error"} title={tr("캠페인을 갱신하지 못했습니다.", "Could not refresh campaigns.")} description={`${error}${campaigns.length ? tr(" · 이전 내용을 표시 중입니다.", " · Showing the previous snapshot.") : ""}`} action={<button onClick={() => void refresh()}>{tr("다시 시도", "Retry")}</button>} />}
    {!campaigns.length && !error && <WidgetState kind={loading ? "loading" : "empty"} title={loading ? tr("캠페인을 불러오고 있습니다.", "Loading campaigns.") : tr("등록된 캠페인이 없습니다.", "No campaigns yet.")} description={loading ? undefined : tr("캠페인이 등록되면 작업 흐름과 진행 상황이 여기에 표시됩니다.", "Registered campaigns and their progress will appear here.")} />}
    {campaign && progress && <div className="campaign-layout">
      <nav className="campaign-list" aria-label={tr("캠페인 목록", "Campaign list")}>
        {campaigns.map((value) => { const summary = campaignProgress(value.nodes); return <button key={value.id} aria-current={value.id === campaign.id ? "true" : undefined} onClick={() => { setSelectedId(value.id); setNodeId(null); }}><strong>{value.title}</strong><Badge status={value.status} tr={tr} /><span>{summary.counts.completed}/{summary.total} {tr("완료", "completed")} · {tr("라운드", "Round")} {value.round}</span></button>; })}
      </nav>
      <div className="campaign-main">
        <div className="campaign-row"><h3>{campaign.title}</h3><Badge status={campaign.status} tr={tr} /></div>
        {campaign.description && <p className="campaign-description">{campaign.description}</p>}
        <div className="campaign-row"><strong>{progress.percent}% {tr("완료", "completed")}</strong><span>{progress.counts.completed}/{progress.total} · {tr("라운드", "Round")} {campaign.round}</span></div>
        <progress className="campaign-progress" max={100} value={progress.percent} aria-label={tr("작업 완료율", "Task completion")} />
        <div className="campaign-counts">{NODE_STATUSES.map((status) => <span key={status}><Badge status={status} tr={tr} /> {progress.counts[status]}</span>)}</div>
        {campaign.nodes.length ? <>
          <CampaignGraph campaign={campaign} selected={selectedNode?.id ?? null} onSelect={setNodeId} tr={tr} />
          <label className="campaign-node-picker">{tr("작업 선택", "Select task")}<select value={selectedNode?.id ?? ""} onChange={(event) => setNodeId(event.target.value)}>{campaign.nodes.map((node) => <option key={node.id} value={node.id}>{node.title} · {tr(...LABELS[node.status])}</option>)}</select></label>
          {selectedNode && <NodeDetails key={`${campaign.id}:${selectedNode.id}`} campaign={campaign} node={selectedNode} tr={tr} onSaved={onSaved} />}
        </> : <WidgetState kind="empty" title={tr("아직 작업이 없습니다.", "No tasks yet.")} />}
      </div>
    </div>}
  </section>;
}
