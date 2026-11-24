import { zodResolver } from "@hookform/resolvers/zod";
import { useEffect } from "react";
import { useForm } from "react-hook-form";
import { X } from "lucide-react";
import { z } from "zod";
import { catalogLabel, useProviderCatalog } from "../../api/providers";
import type { Department } from "../../types";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import { Checkbox } from "../../kit/free/checkbox-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { Select } from "../../kit/free/select-clean";
import { TextArea } from "../../kit/free/text-area-clean";
import { TextField } from "../../kit/free/text-field-clean";
import AgentPersona from "../AgentPersona";
import EmojiPicker from "./EmojiPicker";
import AgentPromptEditor from "./AgentPromptEditor";
import { AgentOverlay } from "./AgentOverlay";
import type { FormData } from "./types";

const agentFormSchema = z.object({
  name: z.string().trim().min(1, "required"),
  name_ko: z.string(),
  name_ja: z.string(),
  name_zh: z.string(),
  department_id: z.string(),
  cli_provider: z.string().trim().min(1, "required"),
  avatar_emoji: z.string().trim().min(1, "required"),
  personality: z.string(),
  prompt_content: z.string(),
  auto_commit: z.boolean(),
});

type AgentFormValues = z.infer<typeof agentFormSchema>;
type TextKey = "name" | "name_ko" | "name_ja" | "name_zh" | "personality";

export default function AgentFormModal({
  locale,
  tr,
  form,
  departments,
  isEdit,
  agentId,
  saving,
  onSave,
  onClose,
}: {
  isKo: boolean;
  locale: string;
  tr: (ko: string, en: string) => string;
  form: FormData;
  departments: Department[];
  isEdit: boolean;
  /** The edited agent's id, so the preview draws the same avatar as the lists. */
  agentId?: string;
  saving: boolean;
  onSave: (values: FormData) => void | Promise<void>;
  onClose: () => void;
}) {
  const {
    handleSubmit,
    reset,
    setValue,
    watch,
    formState: { errors },
  } = useForm<AgentFormValues>({
    resolver: zodResolver(agentFormSchema),
    defaultValues: form,
    mode: "onChange",
  });
  const formValues = watch();
  const providerCatalog = useProviderCatalog(formValues.cli_provider);

  useEffect(() => {
    reset(form);
  }, [form, reset]);

  const text = (key: TextKey) => ({
    value: formValues[key],
    onChange: (event: { target: { value: string } }) =>
      setValue(key, event.target.value, { shouldDirty: true, shouldValidate: true }),
  });
  const handleSave = handleSubmit(async (values) => {
    await onSave({
      ...values,
      cli_provider: values.cli_provider as FormData["cli_provider"],
    });
  });
  const title = isEdit ? tr("에이전트 수정", "Edit agent") : tr("새 에이전트", "New agent");

  return (
    <AgentOverlay
      as="form"
      variant="sheet"
      ariaLabelledBy="agent-modal-title"
      onClose={onClose}
      onSubmit={handleSave}
      widthClassName="sm:max-w-4xl"
      testId="agent-form-overlay"
      header={
        <div className="flex items-center gap-3">
          <AgentPersona
            agent={{ id: agentId ?? (formValues.name || "new-agent"), name: formValues.name || "Agent" }}
            state="idle"
            size={36}
          />
          <div className="min-w-0 flex-1">
            <h2 id="agent-modal-title" className="m-0 text-[15px] font-semibold leading-6 text-(--fg)">
              {title}
            </h2>
            {formValues.name.trim() ? (
              <p className="m-0 truncate text-xs leading-4 text-(--fg-muted)">{formValues.name.trim()}</p>
            ) : null}
          </div>
          <IconButton icon={<X size={15} />} label={tr("닫기", "Close")} onClick={onClose} />
        </div>
      }
      footer={
        <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
          <Button variant="secondary" size="md" onClick={onClose}>
            {tr("취소", "Cancel")}
          </Button>
          <Button type="submit" variant="primary" size="md" loading={saving} disabled={saving || !formValues.name.trim()}>
            {saving ? tr("저장하는 중…", "Saving…") : isEdit ? tr("저장", "Save") : tr("에이전트 추가", "Add agent")}
          </Button>
        </div>
      }
    >
      <div className="grid min-w-0 grid-cols-1 gap-6 sm:grid-cols-2">
        <section className="flex min-w-0 flex-col gap-4" aria-labelledby="agent-form-basic">
          <h3 id="agent-form-basic" className="m-0 text-xs font-medium text-(--fg-muted)">
            {tr("기본 정보", "Basics")}
          </h3>
          <TextField
            id="agent-name"
            label={<>{tr("영문 이름", "Name")} <span className="text-(--err)">*</span></>}
            placeholder="DORO"
            data-autofocus
            error={errors.name ? tr("영문 이름을 적어 주세요.", "Enter a name.") : undefined}
            {...text("name")}
          />
          {locale.startsWith("ko") && (
            <TextField id="agent-name-ko" label={tr("한글 이름", "Korean name")} placeholder="도로롱" {...text("name_ko")} />
          )}
          {locale.startsWith("ja") && (
            <TextField
              id="agent-name-ja"
              label={tr("일본어 이름", "Japanese name")}
              placeholder="ドロロン"
              {...text("name_ja")}
            />
          )}
          {locale.startsWith("zh") && (
            <TextField
              id="agent-name-zh"
              label={tr("중국어 이름", "Chinese name")}
              placeholder="多罗隆"
              {...text("name_zh")}
            />
          )}
          <div className="grid min-w-0 grid-cols-[auto_minmax(0,1fr)] items-end gap-3">
            <div>
              <label htmlFor="agent-emoji" className="mb-1.5 block text-[13px] font-medium text-(--fg)">
                {tr("아이콘", "Icon")}
              </label>
              <EmojiPicker
                id="agent-emoji"
                value={formValues.avatar_emoji}
                onChange={(emoji) => setValue("avatar_emoji", emoji, { shouldDirty: true, shouldValidate: true })}
              />
            </div>
            <Select
              id="agent-department"
              label={tr("소속 부서", "Department")}
              value={formValues.department_id}
              onChange={(value) => setValue("department_id", value, { shouldDirty: true })}
              options={[
                { value: "", label: tr("미배정", "Unassigned") },
                ...departments.map((department) => ({
                  value: department.id,
                  label: [department.icon, localeName(locale, department)].filter(Boolean).join(" "),
                })),
              ]}
            />
          </div>
          <Select
            id="agent-provider"
            label={tr("메인 Provider", "Main provider")}
            value={formValues.cli_provider}
            onChange={(value) => setValue("cli_provider", value, { shouldDirty: true, shouldValidate: true })}
            disabled={providerCatalog.loading}
            hint={
              providerCatalog.loading
                ? tr("Provider 목록을 불러오는 중이에요.", "Loading providers…")
                : providerCatalog.error
                  ? tr("목록을 불러오지 못해 기본 목록을 보여 줘요.", "Could not load the list; showing the built-in one.")
                  : undefined
            }
            options={providerCatalog.selectableIds.map((provider) => ({
              value: provider,
              label: catalogLabel(providerCatalog.entries, provider),
            }))}
          />
        </section>

        <section className="flex min-w-0 flex-col gap-4" aria-labelledby="agent-form-details">
          <h3 id="agent-form-details" className="m-0 text-xs font-medium text-(--fg-muted)">
            {tr("성격과 프롬프트", "Personality and prompt")}
          </h3>
          <TextArea
            id="agent-personality"
            label={tr("성격", "Personality")}
            placeholder={tr("전문 분야나 성격을 적어 주세요", "Expertise or personality")}
            minRows={5}
            maxRows={12}
            {...text("personality")}
          />
          {isEdit && (
            <div className="flex min-w-0 flex-col gap-1.5">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <span className="text-[13px] font-medium text-(--fg)">prompt.md</span>
                <Checkbox
                  checked={formValues.auto_commit}
                  onChange={(event) => setValue("auto_commit", event.target.checked, { shouldDirty: true })}
                  label={tr("자동 커밋", "Auto commit")}
                />
              </div>
              <AgentPromptEditor
                label="prompt.md"
                value={formValues.prompt_content}
                onChange={(prompt_content) => setValue("prompt_content", prompt_content, { shouldDirty: true })}
                minHeight={300}
              />
            </div>
          )}
        </section>
      </div>
    </AgentOverlay>
  );
}
