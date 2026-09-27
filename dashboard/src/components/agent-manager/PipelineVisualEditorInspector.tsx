import PipelineVisualEditorMobileSelector from "./PipelineVisualEditorMobileSelector";
import PipelineVisualEditorPhaseGatePanel from "./PipelineVisualEditorPhaseGatePanel";
import PipelineVisualEditorStatePanel from "./PipelineVisualEditorStatePanel";
import PipelineVisualEditorTransitionPanel from "./PipelineVisualEditorTransitionPanel";
import {
  EMPTY_PANEL_STYLE,
  FSM_DETAIL_PANEL_STYLE,
  MUTED_TEXT_STYLE,
  formatSelectionTitle,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorInspector({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <>
      {ctx.useScrollableMobileFsmCanvas && ctx.pipelineDraft && (
        <PipelineVisualEditorMobileSelector ctx={ctx} actions={actions} />
      )}

      <div className="flex flex-wrap items-center justify-between gap-2">
        <h4
          className="text-sm font-semibold"
          style={{ color: "var(--th-text-heading)" }}
          data-testid="pipeline-selection-title"
        >
          {formatSelectionTitle(tr, ctx.selection, ctx.pipelineDraft)}
        </h4>
        {ctx.isFsmVariant && ctx.useScrollableMobileFsmCanvas ? (
          <span className="text-xs" style={MUTED_TEXT_STYLE}>
            {tr("모바일은 위 목록으로 선택하고 이 패널에서 바로 수정합니다.", "Use the quick selector above, then edit here.")}
          </span>
        ) : ctx.selection?.kind === "state" ? (
          <span className="text-xs" style={MUTED_TEXT_STYLE}>
            {tr("노드 클릭으로 선택됨", "Selected from graph")}
          </span>
        ) : null}
      </div>

      {ctx.selectedState && <PipelineVisualEditorStatePanel ctx={ctx} actions={actions} />}
      {ctx.selectedTransition && <PipelineVisualEditorTransitionPanel ctx={ctx} actions={actions} />}

      {ctx.isFsmVariant && !ctx.selectedTransition && !ctx.selectedState && (
        <div
          className="rounded-[20px] border px-4 py-6 text-sm"
          style={{
            ...EMPTY_PANEL_STYLE,
            borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
            background: "#11141b",
          }}
        >
          {tr(
            ctx.useScrollableMobileFsmCanvas
              ? "모바일은 위 빠른 선택 목록에서 전환이나 상태를 고른 뒤 이 패널에서 event, hook, policy를 편집합니다."
              : "전환선을 선택하면 우측 280px 패널에서 event, hook, policy를 바로 편집할 수 있습니다.",
            ctx.useScrollableMobileFsmCanvas
              ? "On mobile, choose a transition or state from the quick selector above, then edit its event, hook, and policy here."
              : "Select an edge to edit its event, hook, and policy in the 280px side panel.",
          )}
        </div>
      )}

      {!ctx.isFsmVariant && ctx.selection?.kind === "phase_gate" && ctx.pipelineDraft && (
        <PipelineVisualEditorPhaseGatePanel ctx={ctx} actions={actions} />
      )}
    </>
  );
}
