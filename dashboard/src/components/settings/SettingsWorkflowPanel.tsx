import type { SettingsNotificationType } from "./SettingsModel";
import type { SettingsPanelContext, SourceStatus } from "./SettingsPanelTypes";
import { SettingsPipelineEditorSection } from "./SettingsPipelineEditorSection";
import { SettingsRowSection } from "./SettingsRowSection";

interface Props {
  ctx: SettingsPanelContext;
  configStatus: SourceStatus;
  runtimeStatus: SourceStatus;
  editorOpen: boolean;
  onToggleEditor: () => void;
  notify: (ko: string, en: string, type?: SettingsNotificationType) => void;
}

const EDITOR_KEYWORDS = "세부 흐름 편집기 고급 편집 파이프라인 상태 전환 훅 workflow editor pipeline state transition hook";

export function SettingsWorkflowPanel({ ctx, configStatus, runtimeStatus, editorOpen, onToggleEditor, notify }: Props) {
  return (
    <div className="space-y-5">
      <SettingsRowSection ctx={ctx} panel="workflow" section="timeouts" status={configStatus} />
      <SettingsRowSection ctx={ctx} panel="workflow" section="retries" status={runtimeStatus} />
      <SettingsRowSection ctx={ctx} panel="workflow" section="autoProgress" status={runtimeStatus} />
      <SettingsRowSection ctx={ctx} panel="workflow" section="review" status={configStatus} />
      <SettingsRowSection ctx={ctx} panel="workflow" section="other" />
      {ctx.matches(EDITOR_KEYWORDS) ? (
        <SettingsPipelineEditorSection ctx={ctx} open={editorOpen} onToggle={onToggleEditor} notify={notify} />
      ) : null}
    </div>
  );
}
