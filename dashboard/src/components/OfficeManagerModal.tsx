import { useState, useCallback } from "react";
import { X, Plus, Trash2, UserPlus, UserMinus, Settings2 } from "lucide-react";
import type { Office, Agent } from "../types";
import AgentAvatar from "./AgentAvatar";
import {
  OFFICE_COLORS,
  OFFICE_ICONS,
  useOfficeManager,
} from "./office-manager/useOfficeManager";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
  SurfaceSubsection,
} from "./common/SurfacePrimitives";

interface OfficeManagerModalProps {
  offices: Office[];
  allAgents: Agent[];
  isKo: boolean;
  onClose: () => void;
  onChanged: () => void;
}

type ModalView = "list" | "edit" | "agents";

export default function OfficeManagerModal({
  offices,
  allAgents,
  isKo,
  onClose,
  onChanged,
}: OfficeManagerModalProps) {
  const [view, setView] = useState<ModalView>("list");
  const [editOffice, setEditOffice] = useState<Office | null>(null);
  const [agentsOffice, setAgentsOffice] = useState<Office | null>(null);
  const {
    deleteOffice,
    draft,
    loadMembers,
    memberIds,
    resetDraft,
    saveOffice,
    saving,
    setDraft,
    toggleMember,
  } = useOfficeManager({ allAgents, onChanged });

  const tr = useCallback(
    (ko: string, en: string) => (isKo ? ko : en),
    [isKo],
  );

  const openCreate = () => {
    setEditOffice(null);
    resetDraft(null);
    setView("edit");
  };

  const openEdit = (o: Office) => {
    setEditOffice(o);
    resetDraft(o);
    setView("edit");
  };

  const openAgents = (o: Office) => {
    setAgentsOffice(o);
    loadMembers(o.id);
    setView("agents");
  };

  const handleSave = async () => {
    try {
      await saveOffice({ creating: !editOffice, office: editOffice });
      setView("list");
    } catch (e) {
      console.error("Office save failed:", e);
    }
  };

  const handleDelete = async (id: string) => {
    if (!confirm(tr("이 오피스를 삭제하시겠습니까?", "Delete this office?")))
      return;
    const office = offices.find((item) => item.id === id);
    if (!office) return;
    try {
      await deleteOffice(office);
    } catch (e) {
      console.error("Office delete failed:", e);
    }
  };

  const toggleAgent = async (agentId: string) => {
    if (!agentsOffice) return;
    try {
      await toggleMember(agentsOffice, agentId);
    } catch (e) {
      console.error("Toggle agent failed:", e);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center"
      style={{ background: "var(--th-modal-overlay)" }}
      onClick={(e) => e.target === e.currentTarget && onClose()}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="office-manager-title"
        className="mx-4 flex max-h-[84vh] w-full max-w-2xl flex-col rounded-[28px] border"
        style={{
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        }}
      >
        {/* Header */}
        <div
          className="flex items-center justify-between p-4"
          style={{ borderBottom: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)" }}
        >
          <h2
            id="office-manager-title"
            className="text-lg font-bold"
            style={{ color: "var(--th-text-heading)" }}
          >
            {view === "list" && tr("오피스 관리", "Manage Offices")}
            {view === "edit" &&
              (editOffice
                ? tr("오피스 편집", "Edit Office")
                : tr("새 오피스", "New Office"))}
            {view === "agents" &&
              `${agentsOffice?.icon ?? ""} ${isKo ? agentsOffice?.name_ko : agentsOffice?.name} — ${tr("멤버 관리", "Manage Members")}`}
          </h2>
          <SurfaceActionButton tone="neutral" compact onClick={onClose} aria-label={tr("닫기", "Close")}>
            <X size={16} />
          </SurfaceActionButton>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto p-4">
          {/* ── LIST VIEW ── */}
          {view === "list" && (
            <div className="space-y-2">
              <SurfaceNotice tone="info">
                <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                  {tr("오피스 목록", "Office List")}
                </div>
                <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {tr("오피스 프로필 편집과 멤버 관리를 여기서 바로 이동합니다.", "Move between office profile editing and member management from here.")}
                </div>
              </SurfaceNotice>
              {offices.map((o) => (
                <SurfaceCard
                  key={o.id}
                  className="flex items-center gap-3 p-3 rounded-lg"
                  style={{
                    background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
                    border: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
                  }}
                >
                  <span className="text-xl">{o.icon}</span>
                  <div className="flex-1 min-w-0">
                    <div
                      className="font-medium text-sm truncate"
                      style={{ color: "var(--th-text-primary)" }}
                    >
                      {isKo ? o.name_ko || o.name : o.name}
                    </div>
                    <div
                      className="text-xs"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {o.agent_count ?? 0} {tr("명", "agents")} · {o.department_count ?? 0} {tr("부서", "depts")}
                    </div>
                  </div>
                  <div className="flex items-center gap-1">
                    <SurfaceActionButton
                      onClick={() => openAgents(o)}
                      tone="neutral"
                      compact
                      title={tr("멤버 관리", "Manage Members")}
                    >
                      <Settings2
                        size={14}
                        style={{ color: "var(--th-text-secondary)" }}
                      />
                    </SurfaceActionButton>
                    <SurfaceActionButton
                      onClick={() => openEdit(o)}
                      tone="neutral"
                      compact
                    >
                      {tr("편집", "Edit")}
                    </SurfaceActionButton>
                    <SurfaceActionButton
                      onClick={() => handleDelete(o.id)}
                      tone="danger"
                      compact
                    >
                      <Trash2 size={14} className="text-red-400" />
                    </SurfaceActionButton>
                  </div>
                </SurfaceCard>
              ))}

              {offices.length === 0 && (
                <SurfaceEmptyState className="py-8 text-center text-sm">
                  {tr("오피스가 없습니다", "No offices yet")}
                </SurfaceEmptyState>
              )}

              <SurfaceActionButton
                onClick={openCreate}
                tone="accent"
                className="w-full border-dashed"
                style={{
                  borderStyle: "dashed",
                }}
              >
                <Plus size={16} />
                {tr("오피스 추가", "Add Office")}
              </SurfaceActionButton>
            </div>
          )}

          {/* ── EDIT VIEW ── */}
          {view === "edit" && (
            <div className="grid gap-4 md:grid-cols-2">
              <SurfaceSubsection
                title={tr("기본 정보", "Identity")}
                description={tr("오피스 이름과 설명을 정리합니다.", "Define office naming and description.")}
              >
                <div className="space-y-4">
                  <div>
                <label
                  htmlFor="office-name-en"
                  className="block text-xs font-medium mb-1"
                  style={{ color: "var(--th-text-secondary)" }}
                >
                  {tr("이름 (영문)", "Name (EN)")}
                </label>
                <input
                  id="office-name-en"
                  value={draft.name}
                  onChange={(e) => setDraft((prev) => ({ ...prev, name: e.target.value }))}
                  className="w-full px-3 py-2 rounded-lg text-sm"
                  style={{
                    background: "var(--th-input-bg)",
                    border: "1px solid var(--th-input-border)",
                    color: "var(--th-text-primary)",
                  }}
                  placeholder="e.g. CookingHeart"
                />
              </div>
                  <div>
                    <label
                      htmlFor="office-name-ko"
                      className="block text-xs font-medium mb-1"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("이름 (한국어)", "Name (KO)")}
                    </label>
                    <input
                      id="office-name-ko"
                      value={draft.name_ko}
                      onChange={(e) => setDraft((prev) => ({ ...prev, name_ko: e.target.value }))}
                      className="w-full px-3 py-2 rounded-lg text-sm"
                      style={{
                        background: "var(--th-input-bg)",
                        border: "1px solid var(--th-input-border)",
                        color: "var(--th-text-primary)",
                      }}
                      placeholder="e.g. 쿠킹하트"
                    />
                  </div>
                  <div>
                    <label
                      className="block text-xs font-medium mb-1"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("설명", "Description")}
                    </label>
                    <textarea
                      value={draft.description}
                      onChange={(e) => setDraft((prev) => ({ ...prev, description: e.target.value }))}
                      className="w-full px-3 py-2 rounded-lg text-sm resize-none"
                      rows={3}
                      style={{
                        background: "var(--th-input-bg)",
                        border: "1px solid var(--th-input-border)",
                        color: "var(--th-text-primary)",
                      }}
                    />
                  </div>
                </div>
              </SurfaceSubsection>

              <SurfaceSubsection
                title={tr("표현", "Appearance")}
                description={tr("아이콘과 대표 색상을 선택합니다.", "Choose the office icon and accent color.")}
              >
                <div className="space-y-4">
                  <div>
                    <label
                      className="block text-xs font-medium mb-1"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("아이콘", "Icon")}
                    </label>
                    <div className="flex gap-1.5 flex-wrap" role="radiogroup" aria-label={tr("아이콘", "Icon")}>
                      {OFFICE_ICONS.map((ic, idx) => (
                        <button
                          key={ic}
                          type="button"
                          role="radio"
                          aria-label={tr(`아이콘 ${ic}`, `Icon ${ic}`)}
                          aria-checked={draft.icon === ic}
                          tabIndex={draft.icon === ic || (!OFFICE_ICONS.includes(draft.icon) && idx === 0) ? 0 : -1}
                          onKeyDown={(e) => {
                            let nextIdx = idx;
                            if (e.key === "ArrowRight" || e.key === "ArrowDown") {
                              e.preventDefault();
                              nextIdx = (idx + 1) % OFFICE_ICONS.length;
                            } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
                              e.preventDefault();
                              nextIdx = (idx - 1 + OFFICE_ICONS.length) % OFFICE_ICONS.length;
                            } else if (e.key === "Home") {
                              e.preventDefault();
                              nextIdx = 0;
                            } else if (e.key === "End") {
                              e.preventDefault();
                              nextIdx = OFFICE_ICONS.length - 1;
                            }
                            if (nextIdx !== idx) {
                              setDraft((prev) => ({ ...prev, icon: OFFICE_ICONS[nextIdx] }));
                              const nextButton = e.currentTarget.parentElement?.children[nextIdx] as HTMLButtonElement | undefined;
                              nextButton?.focus();
                            }
                          }}
                          onClick={() => setDraft((prev) => ({ ...prev, icon: ic }))}
                          className="flex h-8 w-8 items-center justify-center rounded text-base transition-all"
                          style={{
                            color: "var(--th-text-heading)",
                            border: draft.icon === ic
                              ? `1px solid ${draft.color}`
                              : "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
                            background: draft.icon === ic
                              ? `color-mix(in srgb, ${draft.color} 16%, var(--th-bg-surface) 84%)`
                              : "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
                            boxShadow: draft.icon === ic ? `0 0 0 1px ${draft.color}55` : "none",
                          }}
                        >
                          {ic}
                        </button>
                      ))}
                    </div>
                  </div>
                  <div>
                    <label
                      className="block text-xs font-medium mb-1"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("색상", "Color")}
                    </label>
                    <div className="flex gap-1.5 flex-wrap" role="radiogroup" aria-label={tr("색상", "Color")}>
                      {OFFICE_COLORS.map((c, idx) => (
                        <button
                          key={c}
                          type="button"
                          role="radio"
                          aria-label={tr(`색상 ${c}`, `Color ${c}`)}
                          aria-checked={draft.color === c}
                          tabIndex={draft.color === c || (!OFFICE_COLORS.includes(draft.color) && idx === 0) ? 0 : -1}
                          onKeyDown={(e) => {
                            let nextIdx = idx;
                            if (e.key === "ArrowRight" || e.key === "ArrowDown") {
                              e.preventDefault();
                              nextIdx = (idx + 1) % OFFICE_COLORS.length;
                            } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
                              e.preventDefault();
                              nextIdx = (idx - 1 + OFFICE_COLORS.length) % OFFICE_COLORS.length;
                            } else if (e.key === "Home") {
                              e.preventDefault();
                              nextIdx = 0;
                            } else if (e.key === "End") {
                              e.preventDefault();
                              nextIdx = OFFICE_COLORS.length - 1;
                            }
                            if (nextIdx !== idx) {
                              setDraft((prev) => ({ ...prev, color: OFFICE_COLORS[nextIdx] }));
                              const nextButton = e.currentTarget.parentElement?.children[nextIdx] as HTMLButtonElement | undefined;
                              nextButton?.focus();
                            }
                          }}
                          onClick={() => setDraft((prev) => ({ ...prev, color: c }))}
                          className={`w-7 h-7 rounded-full transition-all ${
                            draft.color === c
                              ? "ring-2 ring-offset-2 ring-offset-gray-900 ring-white"
                              : ""
                          }`}
                          style={{ background: c }}
                        />
                      ))}
                    </div>
                  </div>
                  <SurfaceCard
                    style={{
                      borderColor: `${draft.color}55`,
                      background: `color-mix(in srgb, ${draft.color} 12%, var(--th-card-bg) 88%)`,
                    }}
                  >
                    <div className="flex items-center gap-3">
                      <div className="flex h-11 w-11 items-center justify-center rounded-2xl text-2xl" style={{ background: `${draft.color}22` }}>
                        {draft.icon}
                      </div>
                      <div className="min-w-0">
                        <div className="truncate text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                          {draft.name_ko.trim() || draft.name.trim() || tr("오피스 이름", "Office Name")}
                        </div>
                        <div className="truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {draft.name.trim() || tr("영문 이름 미입력", "No English name yet")}
                        </div>
                      </div>
                    </div>
                  </SurfaceCard>
                </div>
              </SurfaceSubsection>
            </div>
          )}

          {/* ── AGENTS VIEW ── */}
          {view === "agents" && agentsOffice && (
            <div className="space-y-1">
              <SurfaceNotice tone="accent" compact>
                <div>
                  <div className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                    {tr("멤버 토글", "Toggle Membership")}
                  </div>
                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {tr("오피스에 포함할 에이전트를 선택하세요.", "Choose which agents belong to this office.")}
                  </div>
                </div>
              </SurfaceNotice>
              {allAgents.map((a) => {
                const inOffice = memberIds.has(a.id);
                return (
                  <SurfaceCard
                    key={a.id}
                    role="switch"
                    aria-checked={inOffice}
                    aria-label={tr(`${a.name_ko || a.name} - 오피스 멤버 토글`, `Toggle ${a.name} in office members`)}
                    tabIndex={0}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        toggleAgent(a.id);
                      }
                    }}
                    onClick={() => toggleAgent(a.id)}
                    className="w-full cursor-pointer p-2.5 text-left transition-all focus:outline-none focus:ring-2 focus:ring-[var(--th-accent-primary)] focus:ring-offset-2 focus:ring-offset-[var(--th-bg-surface)]"
                    style={{
                      background: inOffice
                        ? "color-mix(in srgb, var(--th-accent-primary-soft) 22%, var(--th-bg-surface) 78%)"
                        : "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                      border: inOffice
                        ? "1px solid color-mix(in srgb, var(--th-accent-primary) 28%, transparent)"
                        : "1px solid color-mix(in srgb, var(--th-border) 66%, transparent)",
                    }}
                  >
                    <div className="flex items-center gap-3">
                      <AgentAvatar agent={a} agents={allAgents} size={24} rounded="xl" />
                      <div className="flex-1 min-w-0">
                        <div
                          className="text-sm truncate"
                          style={{ color: "var(--th-text-primary)" }}
                        >
                          {isKo ? a.name_ko || a.name : a.name}
                        </div>
                      </div>
                      {inOffice ? (
                        <UserMinus size={14} className="text-red-400 shrink-0" />
                      ) : (
                        <UserPlus
                          size={14}
                          className="shrink-0"
                          style={{ color: "var(--th-text-muted)" }}
                        />
                      )}
                    </div>
                  </SurfaceCard>
                );
              })}
              {allAgents.length === 0 && (
                <SurfaceEmptyState className="py-8 text-center text-sm">
                  {tr("등록된 에이전트가 없습니다", "No agents registered")}
                </SurfaceEmptyState>
              )}
            </div>
          )}
        </div>

        {/* Footer */}
        <div
          className="flex items-center justify-end gap-2 p-4"
          style={{ borderTop: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)" }}
        >
          {view !== "list" && (
            <SurfaceActionButton tone="neutral" onClick={() => setView("list")}>
              {tr("뒤로", "Back")}
            </SurfaceActionButton>
          )}
          {view === "edit" && (
            <SurfaceActionButton
              onClick={handleSave}
              disabled={saving || !draft.name.trim()}
              tone="accent"
            >
              {saving
                ? tr("저장 중...", "Saving...")
                : editOffice
                  ? tr("저장", "Save")
                  : tr("생성", "Create")}
            </SurfaceActionButton>
          )}
          {view === "list" && (
            <SurfaceActionButton tone="neutral" onClick={onClose}>
              {tr("닫기", "Close")}
            </SurfaceActionButton>
          )}
          {view === "agents" && (
            <SurfaceActionButton tone="accent" onClick={onClose}>
              {tr("완료", "Done")}
            </SurfaceActionButton>
          )}
        </div>
      </div>
    </div>
  );
}
