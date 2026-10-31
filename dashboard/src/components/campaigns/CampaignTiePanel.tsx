import type { Tr } from "./campaignPresentation";

/** One original dependency as listed to the user: dependency first, with both tasks named. */
export type TieStrand = { id: string; from: string; to: string; ties: number };
export type TieSummary = { id: string; label: string; strands: TieStrand[] };

export type TiePanelActions = {
  toggleMode: () => void; toggleDraft: (id: string) => void; confirm: () => void; cancel: () => void;
  selectTie: (id: string | null) => void; move: (dx: number, dy: number) => void; remove: () => void;
  clear: () => void; undo: () => void; showOriginal: (value: boolean) => void; focus: (id: string | null) => void;
  retry: () => void; adoptPending: () => void;
};

const STEP = 40;

const strandLabel = (strand: TieStrand) => `${strand.from} → ${strand.to}`;

/**
 * Every tie operation as a button or a checkbox, so mouse, keyboard and touch reach the same commands
 * that dragging on the graph uses.
 */
export default function CampaignTiePanel({ tr, strands, ties, tieMode, draft, selectedTie, focused, canUndo, showOriginal, dragging, status, act }: {
  tr: Tr; strands: TieStrand[]; ties: TieSummary[]; tieMode: boolean; draft: ReadonlySet<string>;
  selectedTie: string | null; focused: string | null; canUndo: boolean; showOriginal: boolean; dragging: boolean;
  status: { saveFailed: boolean; recovered: boolean; pending: boolean }; act: TiePanelActions;
}) {
  const tie = ties.find((candidate) => candidate.id === selectedTie) ?? null;
  const focusedStrand = tie?.strands.find((strand) => strand.id === focused) ?? null;
  const moves: [string, string, number, number][] = [
    ["←", tr("타이를 왼쪽으로 이동", "Move tie left"), -STEP, 0], ["↑", tr("타이를 위로 이동", "Move tie up"), 0, -STEP],
    ["↓", tr("타이를 아래로 이동", "Move tie down"), 0, STEP], ["→", tr("타이를 오른쪽으로 이동", "Move tie right"), STEP, 0],
  ];
  return <div className="campaign-tie-panel" aria-label={tr("선 묶기", "Cable ties")}>
    <div className="campaign-tie-toolbar" role="toolbar" aria-label={tr("선 묶기 도구", "Cable tie tools")}>
      <button aria-pressed={tieMode} onClick={act.toggleMode}>{tr("선 묶기", "Tie lines")}</button>
      <button aria-pressed={showOriginal} onClick={() => act.showOriginal(!showOriginal)}>{tr("원래 선 보기", "Show original lines")}</button>
      <button disabled={!canUndo || dragging} onClick={act.undo}>{tr("되돌리기", "Undo")}</button>
      <button disabled={ties.length === 0 || dragging} onClick={act.clear}>{tr("전체 정리", "Clear all ties")}</button>
    </div>
    {status.saveFailed && <p role="alert" className="campaign-tie-note">{tr("표시 설정을 저장하지 못했습니다. 화면과 되돌리기는 그대로 유지됩니다.", "Could not save the view. The screen and undo history are kept.")} <button onClick={act.retry}>{tr("다시 저장", "Retry save")}</button></p>}
    {status.recovered && <p role="status" className="campaign-tie-note">{tr("저장된 표시 설정이 손상되었거나 버전이 달라 원래 선으로 표시합니다. 실행 상태는 바뀌지 않았습니다.", "The saved view was damaged or from another version, so original lines are shown. Execution state is unchanged.")}</p>}
    {status.pending && <p role="status" className="campaign-tie-note">{tr("다른 탭에서 표시 설정이 바뀌었습니다. 진행 중인 조작이 끝나면 반영합니다.", "Another tab changed this view. It applies when your current action ends.")} {!dragging && <button onClick={act.adoptPending}>{tr("지금 반영", "Apply now")}</button>}</p>}
    {tieMode && <fieldset className="campaign-tie-draft">
      <legend>{tr("묶을 선 고르기", "Choose lines to tie")}</legend>
      <p className="campaign-muted">{tr("그래프에서 선을 가로질러 드래그하거나 아래 목록에서 고르세요. 묶기 모드에서는 화면 이동이 멈춥니다.", "Drag across lines on the graph or pick them below. Panning pauses while tying.")}</p>
      <ul className="campaign-tie-list" aria-label={tr("연결 목록", "Connections")}>
        {strands.map((strand) => <li key={strand.id}><label className="campaign-check">
          <input type="checkbox" checked={draft.has(strand.id)} onChange={() => act.toggleDraft(strand.id)} />
          <span>{strandLabel(strand)}</span>{strand.ties > 0 && <small>{tr(`타이 ${strand.ties}개`, `${strand.ties} ties`)}</small>}
        </label></li>)}
      </ul>
      <div className="campaign-tie-toolbar">
        <button disabled={draft.size === 0} onClick={act.confirm}>{tr(`선 ${draft.size}개 묶기`, `Tie ${draft.size} lines`)}</button>
        <button onClick={act.cancel}>{tr("취소", "Cancel")}</button>
      </div>
    </fieldset>}
    {ties.length > 0 && <div className="campaign-tie-toolbar" aria-label={tr("타이 목록", "Ties")}>
      {ties.map((candidate) => <button key={candidate.id} aria-pressed={candidate.id === selectedTie} onClick={() => act.selectTie(candidate.id === selectedTie ? null : candidate.id)}>
        {candidate.label} · {tr(`가닥 ${candidate.strands.length}개`, `${candidate.strands.length} strands`)}
      </button>)}
    </div>}
    {tie && <section className="campaign-tie-detail" aria-label={tr(`${tie.label} 상세`, `${tie.label} details`)}>
      <div className="campaign-row"><strong>{tie.label}</strong><span>{tr(`원본 선 ${tie.strands.length}개 · 표시 요소이며 의존관계가 아닙니다`, `${tie.strands.length} original lines · a display aid, not a dependency`)}</span></div>
      <ul className="campaign-tie-list" aria-label={tr("묶인 원본 선 (선행 → 후속)", "Tied original lines (dependency → dependent)")}>
        {tie.strands.map((strand) => <li key={strand.id}><button aria-pressed={strand.id === focused} onClick={() => act.focus(strand.id === focused ? null : strand.id)}>{strandLabel(strand)}</button></li>)}
      </ul>
      {focusedStrand && <p role="status">{tr(`선행 ${focusedStrand.from} → 후속 ${focusedStrand.to}`, `Dependency ${focusedStrand.from} → dependent ${focusedStrand.to}`)}</p>}
      <div className="campaign-tie-toolbar">
        {moves.map(([glyph, label, dx, dy]) => <button key={glyph} aria-label={label} disabled={dragging} onClick={() => act.move(dx, dy)}>{glyph}</button>)}
        <button disabled={dragging} onClick={act.remove}>{tr("타이 해제", "Remove tie")}</button>
      </div>
    </section>}
  </div>;
}
