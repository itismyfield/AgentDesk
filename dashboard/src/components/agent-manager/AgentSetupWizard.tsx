import { X } from "lucide-react";
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

  const title = mode === "duplicate" ? tr("에이전트 복제", "Duplicate Agent") : tr("에이전트 생성", "Create Agent");

  return (
    <AgentOverlay
      variant="sheet"
      ariaLabel={tr("에이전트 설정 마법사", "Agent setup wizard")}
      onClose={onClose}
      widthClassName="sm:max-w-5xl"
      flushBody
      testId="agent-setup-wizard"
      header={
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <div className="text-xs font-medium text-(--fg-muted)">
              {mode === "duplicate"
                ? tr("복제 설정", "Duplicate Setup")
                : tr("온보딩 설정", "Onboarding Setup")}
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
                  {busy ? tr("확인 중...", "Checking...") : tr("미리 보기", "Preview")}
                </Button>
                <Button variant="primary" size="md" onClick={runConfirm} disabled={!currentValid || busy}>
                  {busy ? tr("처리 중...", "Working...") : tr("확정", "Confirm")}
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
                    <span className={validationByStep[index] ? "text-(--ok)" : "text-(--fg-faint)"}>
                      {validationByStep[index] ? "OK" : "--"}
                    </span>
                  </button>
                </li>
              ))}
            </ol>
          </nav>

          <div className="min-w-0 p-4 sm:p-5">
            <div className="space-y-4">
              {currentStep === "role" && (
                <WizardSection
                  title={tr("역할 정의", "Role Definition")}
                  description={tr("role_id와 표시 이름은 생성 후 관리 화면에서 계속 편집할 수 있습니다.", "Role ID and display names remain editable after creation.")}
                >
                  <div className="grid gap-4 md:grid-cols-2">
                    {field("role_id", draft.agentId, (agentId) => setDraft({ ...draft, agentId }), "adk-researcher")}
                    {field(tr("표시 이름", "Display Name"), draft.name, (name) => setDraft({ ...draft, name }), "Researcher")}
                    {field(tr("한글 이름", "Korean Name"), draft.nameKo, (nameKo) => setDraft({ ...draft, nameKo }), "리서처")}
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
                      {tr("role_id는 영문/숫자/_/- 2자 이상이고 이름이 필요합니다.", "role_id needs 2+ letters/numbers/_/- and a name.")}
                    </p>
                  )}
                </WizardSection>
              )}

              {currentStep === "discord" && (
                <WizardSection
                  title={tr("Discord 채널", "Discord Channel")}
                  description={tr("신규 역할이 연결될 채널 ID를 지정합니다.", "Choose the Discord channel ID for this role.")}
                >
                  <div className="grid gap-4 md:grid-cols-2">
                    {field("channel_id", draft.channelId, (channelId) => setDraft({ ...draft, channelId }), "123456789012345678")}
                    <Select
                      label="Provider"
                      value={draft.provider}
                      onChange={(provider) => {
                        providerTouchedRef.current = true;
                        setDraft({ ...draft, provider: provider as CliProvider });
                      }}
                      disabled={providerCatalog.loading}
                      hint={
                        providerCatalog.loading
                          ? tr("프로바이더 목록을 불러오는 중...", "Loading providers...")
                          : providerCatalog.error
                            ? tr("카탈로그를 못 불러 로컬 목록을 씁니다.", "Catalog unavailable; using local list.")
                            : providerAuto && !providerTouchedRef.current
                              ? `${tr("자동 추천", "Auto-detected")}: ${providerAuto}`
                              : undefined
                      }
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
                      {tr("Discord channel_id는 숫자 ID여야 합니다.", "Discord channel_id must be a numeric ID.")}
                    </p>
                  )}
                </WizardSection>
              )}

              {currentStep === "prompt" && (
                <WizardSection
                  title={tr("Role prompt", "Role Prompt")}
                  description={tr("템플릿을 복사한 뒤, 필요하면 최종 프롬프트 파일을 다시 씁니다.", "The template is copied first, then the final prompt file can be rewritten.")}
                >
                  <div className="space-y-4">
                    {mode !== "duplicate" && field(
                      "prompt_template_path",
                      draft.promptTemplatePath,
                      (promptTemplatePath) => setDraft({ ...draft, promptTemplatePath }),
                      "~/.adk/release/config/agents/_shared.prompt.md",
                    )}
                    <div className="grid gap-4 lg:grid-cols-2">
                      <AgentPromptEditor
                        label={tr("프롬프트 본문", "Prompt content")}
                        value={draft.promptContent}
                        onChange={(promptContent) => setDraft({ ...draft, promptContent })}
                        minHeight={320}
                      />
                      <SectionWell>
                        <div className="text-xs font-medium text-(--fg-muted)">
                          {tr("_shared 병합 미리보기", "_shared merge preview")}
                        </div>
                        <p className="mt-2 text-xs text-(--fg-dim)">
                          {tr(
                            "런타임은 아래 순서로 파일을 합쳐 최종 역할 프롬프트를 만듭니다.",
                            "The runtime combines the files below in order to build the final role prompt.",
                          )}
                        </p>
                        <ol className="mt-3 space-y-2 text-xs text-(--fg)">
                          <li>
                            <span className="font-mono text-(--accent)">1.</span>{" "}
                            <span className="font-mono">{draft.promptTemplatePath || "_shared.prompt.md"}</span>
                            <div className="text-[11px] text-(--fg-muted)">
                              {tr("조직 공용 헤더 (설치 시 복사)", "org-wide shared header (copied on setup)")}
                            </div>
                          </li>
                          <li>
                            <span className="font-mono text-(--accent)">2.</span>{" "}
                            <span className="font-mono">agents/{draft.agentId || "{role_id}"}.prompt.md</span>
                            <div className="text-[11px] text-(--fg-muted)">
                              {draft.promptContent.trim()
                                ? tr(
                                    `사용자 프롬프트 (${draft.promptContent.trim().length} 자)`,
                                    `user prompt (${draft.promptContent.trim().length} chars)`,
                                  )
                                : tr("빈 상태 — 템플릿만 사용", "empty — template only")}
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
                  title={tr("작업 폴더와 MCP", "Workspace + MCP")}
                  description={tr("스킬은 setup API에 그대로 전달됩니다.", "Skills are passed through to the setup API.")}
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
                        {tr("작업공간", "Workspace")}
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
                  description={tr("선택 항목입니다. 일정 연결은 관리 화면에서 이어서 조정합니다.", "Optional. Schedule wiring can be adjusted from management later.")}
                >
                  <div className="space-y-4">
                    <Checkbox
                      checked={draft.cronEnabled}
                      onChange={(event) => setDraft({ ...draft, cronEnabled: event.target.checked })}
                      label={tr("Cron 초안 포함", "Include cron draft")}
                    />
                    {draft.cronEnabled && field("cron", draft.cronSpec, (cronSpec) => setDraft({ ...draft, cronSpec }), "0 9 * * 1-5")}
                    {!validationByStep[4] && (
                      <p className="m-0 text-xs text-(--err)">
                        {tr("Cron 표현식은 최소 5개 필드가 필요합니다.", "Cron expression needs at least 5 fields.")}
                      </p>
                    )}
                  </div>
                </WizardSection>
              )}

              {currentStep === "preview" && (
                <WizardSection
                  title={tr("미리 보고 확정", "Preview + Confirm")}
                  description={tr("dry-run으로 파일/설정 변경을 확인한 뒤 생성합니다. 실패 시 setup rollback 결과를 확인하고 같은 화면에서 재시도합니다.", "Dry-run the file/config changes before creation. On failure, review rollback output and retry here.")}
                >
                  <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(280px,0.8fr)]">
                    <SectionWell>
                      <div className="grid gap-2 text-xs text-(--fg-dim)">
                        <div>role_id: <span className="text-(--fg)">{draft.agentId}</span></div>
                        <div>channel_id: <span className="text-(--fg)">{draft.channelId || "--"}</span></div>
                        <div>provider: <span className="text-(--fg)">{draft.provider}</span></div>
                        <div>prompt: <span className="text-(--fg)">{mode === "duplicate" ? tr("원본 복사", "copy source") : draft.promptTemplatePath}</span></div>
                        <div>skills: <span className="text-(--fg)">{parseSkills(draft.skillsText).join(", ") || "--"}</span></div>
                        <div>
                          {tr("실시간 검증", "Live validation")}:{" "}
                          <span className={liveValidationError ? "text-(--err)" : "text-(--fg)"}>
                            {validating
                              ? tr("확인 중...", "checking...")
                              : liveValidationError
                                ? tr("충돌 있음", "conflict")
                                : liveValidation
                                  ? tr("충돌 없음", "no conflict")
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
                            : tr("아직 preview가 없습니다.", "No preview yet.")}
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
                        <div className="font-semibold">{tr("Rollback 결과", "Rollback summary")}</div>
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
                        {tr("재시도", "Retry")}
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
