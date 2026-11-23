import { Check, X } from "lucide-react";
import type { CliProvider } from "../../types";
import { localeName } from "../../i18n";
import { summarizeRollback } from "../../api/agentsSetup";
import { catalogLabel, useProviderCatalog } from "../../api/providers";
import type { ReactNode } from "react";
import { SurfaceNotice } from "../common/SurfacePrimitives";
import { Button } from "../../kit/free/button-clean";
import { Checkbox } from "../../kit/free/checkbox-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { Select } from "../../kit/free/select-clean";
import { TextArea } from "../../kit/free/text-area-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { SectionWell } from "./AgentInfoCardSections";
import { AgentOverlay } from "./AgentOverlay";
import AgentPromptEditor from "./AgentPromptEditor";
import {
  parseSkills,
} from "./setupWizardHelpers";
import {
  labelForStep,
  steps,
  useAgentSetupWizardState,
  type AgentSetupWizardProps,
} from "./AgentSetupWizardModel";

/** One wizard step: a title, a quiet line, then its fields. */
function WizardSection({ title, description, children }: { title: ReactNode; description?: ReactNode; children: ReactNode }) {
  return (
    <section className="flex min-w-0 flex-col gap-4">
      <div>
        <h3 className="m-0 text-sm font-semibold leading-5 text-(--fg)">{title}</h3>
        {description ? <p className="m-0 mt-0.5 text-xs leading-5 text-(--fg-muted)">{description}</p> : null}
      </div>
      {children}
    </section>
  );
}

export default function AgentSetupWizard({
  open,
  mode,
  sourceAgent,
  departments,
  locale,
  tr,
  onClose,
  onDone,
}: AgentSetupWizardProps) {
  const {
    stepIndex,
    setStepIndex,
    draft,
    setDraft,
    preview,
    liveValidation,
    liveValidationError,
    validating,
    busy,
    error,
    rollback,
    providerAuto,
    channelName,
    providerTouchedRef,
    validationByStep,
    currentValid,
    currentStep,
    runPreview,
    runConfirm,
  } = useAgentSetupWizardState({ open, mode, sourceAgent, onDone });
  const providerCatalog = useProviderCatalog(draft.provider);

  if (!open) return null;

  const field = (
    label: string,
    value: string,
    onChange: (next: string) => void,
    placeholder?: string,
    type = "text",
  ) => (
    <TextField
      label={label}
      type={type}
      value={value}
      onChange={(event) => onChange(event.target.value)}
      placeholder={placeholder}
    />
  );

  const title = mode === "duplicate" ? tr("에이전트 복제", "Duplicate agent") : tr("에이전트 생성", "Create agent");
  const providerHint = providerCatalog.loading
    ? tr("Provider 목록을 불러오는 중이에요.", "Loading providers…")
    : providerCatalog.error
      ? tr("목록을 불러오지 못해 기본 목록을 보여 줘요.", "Could not load the list; showing the built-in one.")
      : providerAuto && !providerTouchedRef.current
        ? tr(`자동 추천: ${providerAuto}`, `Suggested: ${providerAuto}`)
        : undefined;

  return (
    <AgentOverlay
      variant="sheet"
      ariaLabel={tr("에이전트 설정 마법사", "Agent setup wizard")}
      onClose={onClose}
      widthClassName="sm:max-w-4xl"
      flushBody
      testId="agent-setup-wizard"
      header={
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <div className="text-xs font-medium text-(--fg-muted)">
              {tr(`${stepIndex + 1}/${steps.length}단계 · ${labelForStep(currentStep, tr)}`, `Step ${stepIndex + 1} of ${steps.length} · ${labelForStep(currentStep, tr)}`)}
            </div>
            <h2 className="m-0 mt-0.5 text-[15px] font-semibold leading-6 text-(--fg)">{title}</h2>
          </div>
          <IconButton icon={<X size={15} />} label={tr("닫기", "Close")} onClick={onClose} />
        </div>
      }
      footer={
        <div className="flex items-center justify-end">
          <div className="flex flex-wrap justify-end gap-2">
            <Button variant="ghost" size="md" onClick={onClose} disabled={busy}>
              {tr("취소", "Cancel")}
            </Button>
            <Button
              variant="secondary"
              size="md"
              onClick={() => setStepIndex((prev) => Math.max(0, prev - 1))}
              disabled={busy || stepIndex === 0}
            >
              {tr("이전", "Back")}
            </Button>
            {stepIndex < steps.length - 1 ? (
              <Button
                variant="primary"
                size="md"
                onClick={() => setStepIndex((prev) => Math.min(steps.length - 1, prev + 1))}
                disabled={!currentValid || busy}
              >
                {tr("다음", "Next")}
              </Button>
            ) : (
              <>
                <Button variant="secondary" size="md" onClick={runPreview} disabled={!currentValid || busy}>
                  {busy ? tr("확인하는 중…", "Checking…") : tr("미리 보기", "Preview")}
                </Button>
                <Button variant="primary" size="md" onClick={runConfirm} disabled={!currentValid || busy}>
                  {busy ? tr("만드는 중…", "Creating…") : mode === "duplicate" ? tr("복제", "Duplicate") : tr("만들기", "Create")}
                </Button>
              </>
            )}
          </div>
        </div>
      }
    >
        <div className="grid min-w-0 grid-cols-1 md:grid-cols-[220px_minmax(0,1fr)]">
          <nav aria-label={tr("설정 단계", "Setup steps")} className="border-b border-(--line-soft) p-4 md:border-r md:border-b-0">
            <ol className="m-0 grid list-none grid-cols-2 gap-2 p-0 sm:grid-cols-1">
              {steps.map((step, index) => (
                <li key={step} className="min-w-0">
                  <button
                    type="button"
                    onClick={() => setStepIndex(index)}
                    aria-current={stepIndex === index ? "step" : undefined}
                    className={`flex min-h-11 w-full items-center justify-between gap-3 rounded-(--r-md) px-3 py-2 text-left text-xs text-(--fg) transition-colors hover:bg-(--bg-2) focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--accent) motion-reduce:transition-none ${
                      stepIndex === index
                        ? "bg-(--accent-soft) shadow-[inset_0_0_0_1px_var(--accent)]"
                        : "shadow-[inset_0_0_0_1px_var(--line-soft)]"
                    }`}
                  >
                    <span className="min-w-0 truncate">{index + 1}. {labelForStep(step, tr)}</span>
                    {validationByStep[index] ? (
                      <Check size={14} className="shrink-0 text-(--ok)" aria-label={tr("채움", "Filled")} />
                    ) : null}
                  </button>
                </li>
              ))}
            </ol>
          </nav>

          <div className="@container min-w-0 p-4 sm:p-5">
            <div className="space-y-4">
              {currentStep === "role" && (
                <WizardSection
                  title={tr("역할", "Role")}
                  description={tr("역할 ID와 표시 이름은 만든 뒤에도 관리 화면에서 고칠 수 있어요.", "The role ID and names stay editable after creation.")}
                >
                  <div className="grid gap-4 md:grid-cols-2">
                    {field(tr("역할 ID", "Role ID"), draft.agentId, (agentId) => setDraft({ ...draft, agentId }), "adk-researcher")}
                    {field(tr("표시 이름", "Display name"), draft.name, (name) => setDraft({ ...draft, name }), "Researcher")}
                    {field(tr("한글 이름", "Korean name"), draft.nameKo, (nameKo) => setDraft({ ...draft, nameKo }), "리서처")}
                    <Select
                      label={tr("부서", "Department")}
                      value={draft.departmentId}
                      onChange={(departmentId) => setDraft({ ...draft, departmentId })}
                      options={[
                        { value: "", label: tr("미배정", "Unassigned") },
                        ...departments.map((department) => ({
                          value: department.id,
                          label: [department.icon, localeName(locale, department)].filter(Boolean).join(" "),
                        })),
                      ]}
                    />
                  </div>
                  {!validationByStep[0] && (
                    <p className="m-0 text-xs text-(--err)">
                      {tr("역할 ID는 영문·숫자·_·- 2자 이상이어야 해요. 표시 이름도 적어 주세요.", "The role ID needs 2+ letters, digits, _ or -, and a display name.")}
                    </p>
                  )}
                </WizardSection>
              )}

              {currentStep === "discord" && (
                <WizardSection
                  title={tr("Discord 채널", "Discord channel")}
                  description={tr("이 역할을 연결할 채널 ID를 적어요.", "The Discord channel ID this role answers in.")}
                >
                  <div className="grid gap-4 md:grid-cols-2">
                    {field(tr("채널 ID", "Channel ID"), draft.channelId, (channelId) => setDraft({ ...draft, channelId }), "123456789012345678")}
                    <Select
                      label="Provider"
                      value={draft.provider}
                      onChange={(provider) => {
                        providerTouchedRef.current = true;
                        setDraft({ ...draft, provider: provider as CliProvider });
                      }}
                      disabled={providerCatalog.loading}
                      hint={providerHint}
                      options={providerCatalog.selectableIds.map((provider) => ({
                        value: provider,
                        label: catalogLabel(providerCatalog.entries, provider),
                      }))}
                    />
                  </div>
                  {channelName && (
                    <p className="mt-3 text-xs text-(--fg-dim)">
                      {tr("채널 이름", "Channel name")}: <span className="text-(--fg)">#{channelName}</span>
                    </p>
                  )}
                  {!validationByStep[1] && (
                    <p className="m-0 text-xs text-(--err)">
                      {tr("채널 ID는 숫자여야 해요.", "The channel ID must be a number.")}
                    </p>
                  )}
                </WizardSection>
              )}

              {currentStep === "prompt" && (
                <WizardSection
                  title={tr("역할 프롬프트", "Role prompt")}
                  description={tr("템플릿을 먼저 복사해요. 본문을 적으면 그 내용으로 프롬프트 파일을 다시 써요.", "The template is copied first; a written body then replaces the prompt file.")}
                >
                  <div className="space-y-4">
                    {mode !== "duplicate" && field(
                      tr("템플릿 경로", "Template path"),
                      draft.promptTemplatePath,
                      (promptTemplatePath) => setDraft({ ...draft, promptTemplatePath }),
                      "~/.adk/release/config/agents/_shared.prompt.md",
                    )}
                    <div className="grid gap-4 @3xl:grid-cols-2">
                      <AgentPromptEditor
                        label={tr("프롬프트 본문", "Prompt content")}
                        value={draft.promptContent}
                        onChange={(promptContent) => setDraft({ ...draft, promptContent })}
                        minHeight={320}
                      />
                      <SectionWell>
                        <div className="text-xs font-medium text-(--fg-muted)">
                          {tr("_shared 병합 순서", "_shared merge order")}
                        </div>
                        <p className="mt-2 text-xs text-(--fg-dim)">
                          {tr("런타임이 아래 순서로 파일을 합쳐 역할 프롬프트를 만들어요.", "The runtime joins these files in order into the role prompt.")}
                        </p>
                        <ol className="mt-3 space-y-2 text-xs text-(--fg)">
                          <li>
                            <span className="font-mono text-(--accent)">1.</span>{" "}
                            <span className="font-mono">{draft.promptTemplatePath || "_shared.prompt.md"}</span>
                            <div className="text-[11px] text-(--fg-muted)">
                              {tr("조직 공용 머리말(만들 때 복사)", "Org-wide header (copied on setup)")}
                            </div>
                          </li>
                          <li>
                            <span className="font-mono text-(--accent)">2.</span>{" "}
                            <span className="font-mono">agents/{draft.agentId || "{role_id}"}.prompt.md</span>
                            <div className="text-[11px] text-(--fg-muted)">
                              {draft.promptContent.trim()
                                ? tr(`직접 쓴 본문 ${draft.promptContent.trim().length}자`, `Written body, ${draft.promptContent.trim().length} chars`)
                                : tr("본문이 비어 있어 템플릿만 써요", "No body; the template only")}
                            </div>
                          </li>
                        </ol>
                      </SectionWell>
                    </div>
                  </div>
                </WizardSection>
              )}

              {currentStep === "workspace" && (
                <WizardSection
                  title={tr("작업 폴더와 MCP", "Workspace and MCP")}
                  description={tr("스킬 목록은 setup API로 그대로 보내요.", "The skills go to the setup API as written.")}
                >
                  <div className="grid gap-4 md:grid-cols-2">
                    <TextArea
                      label={tr("스킬", "Skills")}
                      value={draft.skillsText}
                      onChange={(event) => setDraft({ ...draft, skillsText: event.target.value })}
                      placeholder="github, playwright, memory-read"
                      minRows={4}
                      maxRows={10}
                      className="md:col-span-2"
                    />
                    <SectionWell>
                      <div className="text-xs font-medium text-(--fg-dim)">
                        {tr("작업 폴더", "Workspace")}
                      </div>
                      <div className="mt-2 text-sm text-(--fg)">
                        ~/.adk/release/config/agents/{draft.agentId || "{role_id}"}
                      </div>
                    </SectionWell>
                    <SectionWell>
                      <div className="text-xs font-medium text-(--fg-dim)">
                        MCP
                      </div>
                      <div className="mt-2 text-sm text-(--fg)">
                        {parseSkills(draft.skillsText).length || tr("기본값", "Default")}
                      </div>
                    </SectionWell>
                  </div>
                </WizardSection>
              )}

              {currentStep === "cron" && (
                <WizardSection
                  title={tr("예약 작업", "Cron")}
                  description={tr("넣지 않아도 돼요. 일정은 만든 뒤 관리 화면에서 고칠 수 있어요.", "Optional. The schedule stays editable after creation.")}
                >
                  <div className="space-y-4">
                    <Checkbox
                      checked={draft.cronEnabled}
                      onChange={(event) => setDraft({ ...draft, cronEnabled: event.target.checked })}
                      label={tr("Cron 초안 넣기", "Include a cron draft")}
                    />
                    {draft.cronEnabled && field(tr("Cron 식", "Cron expression"), draft.cronSpec, (cronSpec) => setDraft({ ...draft, cronSpec }), "0 9 * * 1-5")}
                    {!validationByStep[4] && (
                      <p className="m-0 text-xs text-(--err)">
                        {tr("Cron 식은 칸이 5개 이상이어야 해요.", "A cron expression needs at least 5 fields.")}
                      </p>
                    )}
                  </div>
                </WizardSection>
              )}

              {currentStep === "preview" && (
                <WizardSection
                  title={tr("미리 보고 만들기", "Preview and create")}
                  description={tr("dry-run으로 바뀔 파일과 설정을 먼저 봐요. 실패하면 되돌린 결과를 보고 여기서 다시 해요.", "A dry-run shows the file and config changes first. On failure, check what was rolled back and retry here.")}
                >
                  <div className="grid gap-4 @3xl:grid-cols-[minmax(0,1fr)_minmax(280px,0.8fr)]">
                    <SectionWell>
                      <div className="grid gap-2 text-xs text-(--fg-dim)">
                        <div>{tr("역할 ID", "Role ID")}: <span className="text-(--fg)">{draft.agentId}</span></div>
                        <div>{tr("채널 ID", "Channel ID")}: <span className="text-(--fg)">{draft.channelId || "--"}</span></div>
                        <div>Provider: <span className="text-(--fg)">{draft.provider}</span></div>
                        <div>{tr("프롬프트", "Prompt")}: <span className="text-(--fg)">{mode === "duplicate" ? tr("원본 복사", "Copy of the source") : draft.promptTemplatePath}</span></div>
                        <div>{tr("스킬", "Skills")}: <span className="text-(--fg)">{parseSkills(draft.skillsText).join(", ") || "--"}</span></div>
                        <div>
                          {tr("충돌 검사", "Conflict check")}:{" "}
                          <span className={liveValidationError ? "text-(--err)" : "text-(--fg)"}>
                            {validating
                              ? tr("확인하는 중…", "Checking…")
                              : liveValidationError
                                ? tr("충돌 있음", "Conflict")
                                : liveValidation
                                  ? tr("충돌 없음", "No conflict")
                                  : "--"}
                          </span>
                        </div>
                      </div>
                    </SectionWell>
                    <SectionWell className="max-h-80 overflow-auto">
                      <div className="text-xs font-medium text-(--fg-muted)">
                        {tr("Dry-run 결과", "Dry-run result")}
                      </div>
                      <pre className="mt-2 whitespace-pre-wrap text-xs leading-5 text-(--fg-dim)">
                        {preview
                          ? JSON.stringify(preview, null, 2)
                          : liveValidation
                            ? JSON.stringify(liveValidation, null, 2)
                            : tr("아직 미리 본 결과가 없어요.", "No preview yet.")}
                      </pre>
                    </SectionWell>
                  </div>
                  {(preview?.errors?.length || liveValidation?.errors?.length) ? (
                    <SurfaceNotice tone="warn" className="mt-3">
                      <div className="text-xs">
                        <div className="font-semibold">{tr("Dry-run 경고", "Dry-run warnings")}</div>
                        <ul className="mt-1 list-disc pl-5">
                          {(preview?.errors ?? liveValidation?.errors ?? []).map((e, i) => (
                            <li key={i}>{e}</li>
                          ))}
                        </ul>
                      </div>
                    </SurfaceNotice>
                  ) : null}
                  {rollback ? (
                    <SurfaceNotice tone="danger" className="mt-3">
                      <div className="text-xs">
                        <div className="font-semibold">{tr("되돌린 결과", "Rollback summary")}</div>
                        {(() => {
                          const summary = summarizeRollback(rollback);
                          return (
                            <div className="mt-1 space-y-1">
                              {summary.attempted.length > 0 && (
                                <div>
                                  <span className="font-medium">{tr("시도", "Attempted")}:</span>{" "}
                                  {summary.attempted.join(", ")}
                                </div>
                              )}
                              {summary.reverted.length > 0 && (
                                <div>
                                  <span className="font-medium">{tr("복구 완료", "Reverted")}:</span>{" "}
                                  {summary.reverted.join(", ")}
                                </div>
                              )}
                              {summary.failed.length > 0 && (
                                <div className="text-(--err)">
                                  <span className="font-medium">{tr("복구 실패", "Failed to revert")}:</span>{" "}
                                  {summary.failed.join(", ")}
                                </div>
                              )}
                              {summary.attempted.length === 0 &&
                                summary.reverted.length === 0 &&
                                summary.failed.length === 0 && (
                                  <pre className="whitespace-pre-wrap text-[11px]">
                                    {JSON.stringify(rollback, null, 2)}
                                  </pre>
                                )}
                            </div>
                          );
                        })()}
                      </div>
                    </SurfaceNotice>
                  ) : null}
                </WizardSection>
              )}

              {error && (
                <SurfaceNotice tone="danger">
                  <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
                    <span className="text-sm">{error}</span>
                    {currentStep === "preview" && (
                      <Button variant="primary" size="sm" onClick={runConfirm} disabled={busy || !currentValid}>
                        {tr("다시 하기", "Retry")}
                      </Button>
                    )}
                  </div>
                </SurfaceNotice>
              )}
            </div>
          </div>
        </div>
    </AgentOverlay>
  );
}
