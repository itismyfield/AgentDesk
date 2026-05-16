import {
  STATUS_ERROR_STYLE,
  STATUS_INFO_STYLE,
  STATUS_SUCCESS_STYLE,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
}

export default function PipelineVisualEditorStatus({ ctx }: Props) {
  const tr = ctx.tr;

  return (
    <>
      {(ctx.error || ctx.success || ctx.preservedKeys.length > 0) && (
        <div className="space-y-2">
          {ctx.error && (
            <div
              className="rounded-[22px] border px-4 py-3 text-xs leading-6 sm:text-sm"
              style={STATUS_ERROR_STYLE}
            >
              {ctx.error}
            </div>
          )}
          {ctx.success && (
            <div
              className="rounded-[22px] border px-4 py-3 text-xs leading-6 sm:text-sm"
              style={STATUS_SUCCESS_STYLE}
            >
              {ctx.success}
            </div>
          )}
          {ctx.preservedKeys.length > 0 && (
            <div
              className="rounded-[22px] border px-4 py-3 text-xs leading-6 sm:text-sm"
              style={STATUS_INFO_STYLE}
            >
              {tr("시각 편집기 밖의 override 키는 저장 시 유지됩니다.", "Non-visual override keys are preserved on save.")}{" "}
              <span style={{ color: "var(--th-text-primary)" }}>
                {ctx.preservedKeys.join(", ")}
              </span>
            </div>
          )}
        </div>
      )}

      {ctx.loading && ctx.pipelineDraft && ctx.graph && (
        <div
          data-testid="pipeline-refresh-indicator"
          className="flex items-center gap-2 rounded-[20px] border px-3.5 py-2 text-xs sm:text-sm"
          style={STATUS_INFO_STYLE}
        >
          <span
            className="inline-block h-3.5 w-3.5 animate-spin rounded-full border-2 border-current border-t-transparent"
            aria-hidden="true"
          />
          <span>
            {tr(
              "마지막 성공값을 먼저 보여주고 최신 값을 불러오는 중입니다…",
              "Showing the last successful pipeline while refreshing…",
            )}
          </span>
        </div>
      )}
    </>
  );
}
