import {
  EMPTY_PANEL_STYLE,
  SECTION_STYLE,
  STATUS_ERROR_STYLE,
} from "./pipeline-visual-editor-ui";
import PipelineVisualEditorContent from "./PipelineVisualEditorContent";
import PipelineVisualEditorStatus from "./PipelineVisualEditorStatus";
import PipelineVisualEditorToolbar from "./PipelineVisualEditorToolbar";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorView({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <section
      className="min-w-0 overflow-hidden rounded-[28px] border p-4 sm:p-5 space-y-5"
      style={SECTION_STYLE}
    >
      <button
        type="button"
        onClick={() => actions.setCollapsed((value: boolean) => !value)}
        className="flex w-full items-center justify-between gap-3 text-left"
      >
        <div className="min-w-0 space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {ctx.editorTitle}
            </h3>
            {ctx.pipelineDraft && (
              <span
                className="rounded-full border px-2.5 py-1 text-[11px] font-medium"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)",
                  background: "var(--th-accent-primary-soft)",
                  color: "var(--th-text-primary)",
                }}
              >
                {ctx.pipelineDraft.states.length} {tr("상태", "states")} /{" "}
                {ctx.pipelineDraft.transitions.length} {tr("전환", "transitions")}
                {!ctx.isFsmVariant && (
                  <>
                    {" / "}
                    {ctx.stageDrafts.length} {tr("스테이지", "stages")}
                  </>
                )}
              </span>
            )}
            {ctx.activeLayers.length > 1 && (
              <span
                className="rounded-full border px-2.5 py-1 text-[11px] font-medium"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-warn) 30%, var(--th-border) 70%)",
                  background: "color-mix(in srgb, var(--th-badge-amber-bg) 84%, var(--th-card-bg) 16%)",
                  color: "var(--th-text-primary)",
                }}
              >
                {ctx.activeLayers.join(" → ")}
              </span>
            )}
          </div>
        </div>
        <span
          className="shrink-0 text-lg transition-transform"
          style={{
            color: "var(--th-text-muted)",
            transform: ctx.collapsed ? "rotate(0deg)" : "rotate(180deg)",
          }}
        >
          ▼
        </span>
      </button>

      {!ctx.collapsed && (
        <>
          <PipelineVisualEditorToolbar ctx={ctx} actions={actions} />
          <PipelineVisualEditorStatus ctx={ctx} />
          {!ctx.pipelineDraft || !ctx.graph ? (
            <div
              className="rounded-[24px] border px-4 py-8 text-sm text-center"
              style={ctx.error ? STATUS_ERROR_STYLE : EMPTY_PANEL_STYLE}
            >
              <div className="flex items-center justify-center gap-2">
                {ctx.loading && (
                  <span
                    className="inline-block h-3.5 w-3.5 animate-spin rounded-full border-2 border-current border-t-transparent"
                    aria-hidden="true"
                  />
                )}
                <span>
                  {ctx.error ?? tr("비주얼 파이프라인을 불러오는 중…", "Loading visual pipeline…")}
                </span>
              </div>
            </div>
          ) : (
            <PipelineVisualEditorContent ctx={ctx} actions={actions} />
          )}
        </>
      )}
    </section>
  );
}
