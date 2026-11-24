import { useState, type BaseSyntheticEvent } from "react";
import { X } from "lucide-react";
import type { Department } from "../../types";
import * as api from "../../api";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { Cap } from "../common/KitPrimitives";
import { AgentOverlay } from "./AgentOverlay";
import { DEPT_COLORS } from "./constants";
import EmojiPicker from "./EmojiPicker";
import type { Translator } from "./types";

/**
 * Department create/edit window. The server stores only `name` on create and on PATCH, so the icon, color, Korean name
 * and description show read-only until it takes them; delete says how many agents lose their department.
 */
export default function DepartmentFormModal({
  locale,
  tr,
  department,
  memberCount,
  onSave,
  onClose,
}: {
  locale: string;
  tr: Translator;
  department: Department | null;
  /** Agents whose department this is; deleting leaves them unassigned. */
  memberCount: number;
  onSave: () => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(department?.name ?? "");
  const [touched, setTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const trimmed = name.trim();
  const unchanged = department !== null && trimmed === department.name;
  const title = department ? tr("부서 편집", "Edit department") : tr("새 부서", "New department");

  const run = async (action: () => Promise<unknown>, failure: string) => {
    setBusy(true);
    setError(null);
    try {
      await action();
      onSave();
    } catch (cause) {
      console.error("Department change failed:", cause);
      setError(failure);
    } finally {
      setBusy(false);
    }
  };
  const submit = (event?: BaseSyntheticEvent) => {
    event?.preventDefault();
    setTouched(true);
    if (!trimmed || unchanged || busy) return;
    void run(
      () => (department ? api.updateDepartment(department.id, { name: trimmed }) : api.createDepartment({ name: trimmed })),
      tr("저장하지 못했어요. 잠시 뒤 다시 해 보세요.", "Could not save. Try again in a moment."),
    );
  };
  const remove = () => {
    if (!department) return;
    void run(() => api.deleteDepartment(department.id), tr("지우지 못했어요. 잠시 뒤 다시 해 보세요.", "Could not delete. Try again in a moment."));
  };

  const footer = confirmDelete && department ? (
    <div className="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-center">
      <span className="min-w-0 flex-1 text-sm text-(--fg)">
        {memberCount > 0
          ? tr(`이 부서를 지울까요? 소속 에이전트 ${memberCount}명은 미배정이 돼요.`, `Delete this department? Its ${memberCount} agents become unassigned.`)
          : tr("이 부서를 지울까요?", "Delete this department?")}
      </span>
      <div className="flex flex-col-reverse gap-2 sm:flex-row">
        <Button variant="secondary" size="md" onClick={() => setConfirmDelete(false)} disabled={busy}>
          {tr("취소", "Cancel")}
        </Button>
        <Button variant="destructive" size="md" onClick={remove} loading={busy}>
          {tr("삭제", "Delete")}
        </Button>
      </div>
    </div>
  ) : (
    <div className="flex flex-col-reverse gap-2 sm:flex-row sm:items-center">
      {/* Kit buttons own their margins, so the wrapper pushes the delete button to the start. */}
      {department ? (
        <span className="flex flex-col sm:mr-auto">
          <Button variant="ghost" size="md" onClick={() => setConfirmDelete(true)} disabled={busy}>
            {tr("부서 삭제", "Delete department")}
          </Button>
        </span>
      ) : null}
      <Button variant="secondary" size="md" onClick={onClose}>
        {tr("취소", "Cancel")}
      </Button>
      <Button type="submit" variant="primary" size="md" loading={busy} disabled={!trimmed || unchanged}>
        {department ? tr("이름 저장", "Save name") : tr("부서 추가", "Add department")}
      </Button>
    </div>
  );

  return (
    <AgentOverlay
      as="form"
      variant="sheet"
      ariaLabelledBy="dept-modal-title"
      onClose={onClose}
      onSubmit={submit}
      widthClassName="sm:max-w-xl"
      testId="department-form-overlay"
      header={(
        <div className="flex items-center gap-3">
          <span aria-hidden className="size-3 shrink-0 rounded-full" style={{ background: department?.color || "var(--fg-faint)" }} />
          <div className="min-w-0 flex-1">
            <h2 id="dept-modal-title" className="m-0 text-[15px] leading-6 font-semibold text-(--fg)">{title}</h2>
            {department ? <p className="m-0 truncate font-mono text-xs leading-4 text-(--fg-muted)">{department.id}</p> : null}
          </div>
          <IconButton icon={<X size={15} />} label={tr("닫기", "Close")} onClick={onClose} />
        </div>
      )}
      footer={(
        <div className="flex min-w-0 flex-col gap-2">
          {error ? <p role="alert" className="m-0 text-sm text-(--err)">{error}</p> : null}
          {footer}
        </div>
      )}
    >
      <div className="flex min-w-0 flex-col gap-5">
        <TextField
          id="department-name"
          data-autofocus
          label={<>{tr("영문 이름", "Name")} <span className="text-(--err)">*</span></>}
          placeholder="Platform"
          value={name}
          onChange={(event) => setName(event.target.value)}
          error={touched && !trimmed ? tr("이름을 적어 주세요.", "Enter a name.") : undefined}
          hint={locale.startsWith("ko") && department?.name_ko ? tr("한국어 화면에는 아래 한글 이름이 먼저 보여요.", "Korean screens show the Korean name below first.") : undefined}
        />
        {department ? (
          <fieldset disabled data-testid="department-readonly" className="m-0 flex min-w-0 flex-col gap-4 rounded-(--r-md) border border-(--line-soft) p-3 sm:p-4">
            <legend className="px-1 text-xs leading-4 font-medium text-(--fg-muted)">{tr("보기만 하는 항목", "Read-only")}</legend>
            <Cap>{tr("아이콘·색·한글 이름·설명은 지금 서버가 저장하지 않아요. 서버가 받게 되면 여기에서 바꿀 수 있어요.", "The server does not store the icon, color, Korean name or description yet. They become editable here once it does.")}</Cap>
            <div className="grid min-w-0 grid-cols-[auto_minmax(0,1fr)] items-start gap-3">
              <div className="pointer-events-none">
                <span className="mb-1.5 block text-[13px] font-medium text-(--fg)">{tr("아이콘", "Icon")}</span>
                <EmojiPicker id="department-icon" value={department.icon ?? ""} onChange={() => undefined} aria-label={tr(`아이콘 ${department.icon ?? ""}`, `Icon ${department.icon ?? ""}`)} />
              </div>
              <TextField id="department-name-ko" label={tr("한글 이름", "Korean name")} value={department.name_ko ?? ""} readOnly />
            </div>
            <div className="flex min-w-0 flex-col gap-1.5">
              <span className="text-[13px] font-medium text-(--fg)">{tr("테마 색", "Theme color")}</span>
              <div role="img" aria-label={tr(`테마 색 ${department.color || "없음"}`, `Theme color ${department.color || "none"}`)} className="flex flex-wrap gap-2">
                {DEPT_COLORS.map((color) => (
                  <span
                    key={color}
                    className="size-7 rounded-full"
                    style={{ background: color, outline: color === department.color ? `2px solid ${color}` : "none", outlineOffset: 2 }}
                  />
                ))}
              </div>
            </div>
            <TextField id="department-description" label={tr("설명", "Description")} value={department.description ?? ""} readOnly />
          </fieldset>
        ) : (
          <Cap>{tr("새 부서는 이름만 저장돼요. 아이콘·색·한글 이름은 아직 서버가 받지 않아요.", "A new department keeps only its name. The server does not take an icon, color or Korean name yet.")}</Cap>
        )}
      </div>
    </AgentOverlay>
  );
}
