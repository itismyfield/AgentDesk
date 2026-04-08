import { useCallback, useEffect, useMemo, useState } from "react";
import { ArrowDown, ArrowUp, Building2, Plus, Save, Trash2, UserPlus, Users } from "lucide-react";
import type { Agent, Office } from "../types";
import * as api from "../api/client";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSubsection,
} from "./common/SurfacePrimitives";

interface OfficeManagerViewProps {
  offices: Office[];
  allAgents: Agent[];
  selectedOfficeId?: string | null;
  isKo: boolean;
  onChanged: () => void;
}

interface OfficeDraft {
  name: string;
  name_ko: string;
  icon: string;
  color: string;
  description: string;
}

const OFFICE_ICONS = ["🏢", "🏠", "🏭", "🏗️", "🏛️", "🍳", "🎮", "📚", "🔬", "🎨", "🛠️", "🌐"];
const OFFICE_COLORS = [
  "#10b981", "#14b8a6", "#06b6d4", "#3b82f6", "#84cc16",
  "#f59e0b", "#f97316", "#ef4444", "#64748b", "#22c55e",
];

function makeBlankDraft(): OfficeDraft {
  return {
    name: "",
    name_ko: "",
    icon: "🏢",
    color: "#10b981",
    description: "",
  };
}

function makeDraftFromOffice(office: Office | null): OfficeDraft {
  if (!office) return makeBlankDraft();
  return {
    name: office.name ?? "",
    name_ko: office.name_ko ?? "",
    icon: office.icon ?? "🏢",
    color: office.color ?? "#10b981",
    description: office.description ?? "",
  };
}

export default function OfficeManagerView({
  offices,
  allAgents,
  selectedOfficeId,
  isKo,
  onChanged,
}: OfficeManagerViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const [order, setOrder] = useState<Office[]>(offices);
  const [orderDirty, setOrderDirty] = useState(false);
  const [selectedId, setSelectedId] = useState<string | null>(selectedOfficeId ?? offices[0]?.id ?? null);
  const [draft, setDraft] = useState<OfficeDraft>(() => makeDraftFromOffice(offices[0] ?? null));
  const [creating, setCreating] = useState(false);
  const [search, setSearch] = useState("");
  const [memberIds, setMemberIds] = useState<Set<string>>(new Set());
  const [membersLoading, setMembersLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [orderSaving, setOrderSaving] = useState(false);

  useEffect(() => {
    if (!orderDirty) {
      setOrder(offices);
    }
  }, [offices, orderDirty]);

  useEffect(() => {
    if (creating) return;
    if (selectedOfficeId && offices.some((office) => office.id === selectedOfficeId)) {
      setSelectedId(selectedOfficeId);
      return;
    }
    if (selectedId && offices.some((office) => office.id === selectedId)) return;
    setSelectedId(offices[0]?.id ?? null);
  }, [creating, offices, selectedId, selectedOfficeId]);

  const selectedOffice = useMemo(
    () => order.find((office) => office.id === selectedId) ?? offices.find((office) => office.id === selectedId) ?? null,
    [offices, order, selectedId],
  );

  useEffect(() => {
    if (creating) {
      setDraft(makeBlankDraft());
      setMemberIds(new Set());
      return;
    }
    setDraft(makeDraftFromOffice(selectedOffice));
  }, [creating, selectedOffice]);

  useEffect(() => {
    if (!selectedOffice || creating) {
      setMemberIds(new Set());
      return;
    }
    let cancelled = false;
    setMembersLoading(true);
    api.getAgents(selectedOffice.id).then((agents) => {
      if (!cancelled) {
        setMemberIds(new Set(agents.map((agent) => agent.id)));
      }
    }).catch(() => {
      if (!cancelled) {
        setMemberIds(new Set());
      }
    }).finally(() => {
      if (!cancelled) {
        setMembersLoading(false);
      }
    });
    return () => {
      cancelled = true;
    };
  }, [creating, selectedOffice]);

  const filteredAgents = useMemo(() => {
    const query = search.trim().toLowerCase();
    const filtered = !query
      ? allAgents
      : allAgents.filter((agent) => (
        agent.name.toLowerCase().includes(query)
        || agent.name_ko.toLowerCase().includes(query)
        || (agent.alias && agent.alias.toLowerCase().includes(query))
        || agent.avatar_emoji.includes(query)
      ));
    return [...filtered].sort((left, right) => {
      const leftAssigned = memberIds.has(left.id) ? 0 : 1;
      const rightAssigned = memberIds.has(right.id) ? 0 : 1;
      if (leftAssigned !== rightAssigned) return leftAssigned - rightAssigned;
      return (left.alias || left.name_ko || left.name).localeCompare(right.alias || right.name_ko || right.name);
    });
  }, [allAgents, memberIds, search]);

  const startCreate = () => {
    setCreating(true);
    setSelectedId(null);
  };

  const selectOffice = (officeId: string) => {
    setCreating(false);
    setSelectedId(officeId);
  };

  const moveOffice = (index: number, direction: -1 | 1) => {
    setOrder((prev) => {
      const next = [...prev];
      const target = index + direction;
      if (target < 0 || target >= next.length) return prev;
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
    setOrderDirty(true);
  };

  const saveOrder = async () => {
    setOrderSaving(true);
    try {
      for (let i = 0; i < order.length; i += 1) {
        await api.updateOffice(order[i].id, { sort_order: i });
      }
      setOrderDirty(false);
      onChanged();
    } finally {
      setOrderSaving(false);
    }
  };

  const cancelOrder = () => {
    setOrder(offices);
    setOrderDirty(false);
  };

  const saveOffice = async () => {
    setSaving(true);
    try {
      const payload = {
        name: draft.name.trim(),
        name_ko: draft.name_ko.trim() || draft.name.trim(),
        icon: draft.icon,
        color: draft.color,
        description: draft.description.trim() || null,
      };
      if (creating) {
        const created = await api.createOffice(payload);
        setCreating(false);
        setSelectedId(created.id);
      } else if (selectedOffice) {
        await api.updateOffice(selectedOffice.id, payload);
      }
      onChanged();
    } finally {
      setSaving(false);
    }
  };

  const deleteOffice = async () => {
    if (!selectedOffice) return;
    if (!window.confirm(tr("이 오피스를 삭제하시겠습니까?", "Delete this office?"))) return;
    setSaving(true);
    try {
      await api.deleteOffice(selectedOffice.id);
      setSelectedId(null);
      onChanged();
    } finally {
      setSaving(false);
    }
  };

  const toggleMember = async (agentId: string) => {
    if (!selectedOffice || creating) return;
    setSaving(true);
    const assigned = memberIds.has(agentId);
    setMemberIds((prev) => {
      const next = new Set(prev);
      if (assigned) next.delete(agentId);
      else next.add(agentId);
      return next;
    });
    try {
      if (assigned) {
        await api.removeAgentFromOffice(selectedOffice.id, agentId);
      } else {
        await api.addAgentToOffice(selectedOffice.id, agentId);
      }
      onChanged();
    } catch {
      setMemberIds((prev) => {
        const next = new Set(prev);
        if (assigned) next.add(agentId);
        else next.delete(agentId);
        return next;
      });
    } finally {
      setSaving(false);
    }
  };

  const inputStyle = {
    background: "var(--th-bg-surface)",
    border: "1px solid rgba(148,163,184,0.22)",
    color: "var(--th-text-primary)",
  } as const;

  return (
    <div
      className="mx-auto h-full max-w-6xl min-w-0 space-y-4 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <SurfaceSection
        title={tr("오피스 관리", "Offices")}
        description={tr("오피스 CRUD, 멤버 배치, 표시 순서를 한 화면에서 관리합니다.", "Manage office CRUD, memberships, and ordering from one page.")}
        badge={`${order.length} ${tr("개", "items")}`}
        actions={(
          <div className="flex flex-wrap items-center gap-2">
            {orderDirty && (
              <>
                <SurfaceActionButton tone="neutral" onClick={cancelOrder}>
                  {tr("순서 취소", "Cancel Order")}
                </SurfaceActionButton>
                <SurfaceActionButton tone="success" disabled={orderSaving} onClick={() => void saveOrder()}>
                  <span className="inline-flex items-center gap-1.5">
                    <Save size={13} />
                    {orderSaving ? tr("저장 중...", "Saving...") : tr("순서 저장", "Save Order")}
                  </span>
                </SurfaceActionButton>
              </>
            )}
            <SurfaceActionButton tone="accent" onClick={startCreate}>
              <span className="inline-flex items-center gap-1.5">
                <Plus size={13} />
                {tr("오피스 추가", "Add Office")}
              </span>
            </SurfaceActionButton>
          </div>
        )}
      >
        <div className="mt-4 flex flex-wrap gap-3">
          <SurfaceMetricPill label={tr("오피스", "Offices")} value={`${order.length} ${tr("개", "items")}`} tone="accent" />
          <SurfaceMetricPill label={tr("배치 멤버", "Assigned")} value={`${memberIds.size} ${tr("명", "agents")}`} tone={memberIds.size > 0 ? "info" : "neutral"} />
          <SurfaceMetricPill label={tr("정렬 상태", "Ordering")} value={orderDirty ? tr("변경 있음", "Unsaved changes") : tr("동기화됨", "In sync")} tone={orderDirty ? "warn" : "success"} />
        </div>
      </SurfaceSection>

      <div className="grid gap-4 xl:grid-cols-[320px_minmax(0,1fr)]">
        <SurfaceSection
          title={tr("오피스 목록", "Office List")}
          description={tr("선택한 오피스를 오른쪽에서 편집합니다.", "Select an office to edit on the right.")}
          badge={`${order.length} ${tr("개", "items")}`}
          className="h-fit"
        >
          <div className="mt-4 space-y-2">
            {order.map((office, index) => {
              const active = !creating && office.id === selectedId;
              return (
                <SurfaceCard
                  key={office.id}
                  className="flex items-center gap-3 rounded-xl border px-3 py-3 transition-colors"
                  style={{
                    borderColor: active ? `${office.color}66` : "rgba(148,163,184,0.18)",
                    background: active ? `color-mix(in srgb, ${office.color} 14%, var(--th-surface))` : "var(--th-bg-surface)",
                  }}
                >
                  <button
                    onClick={() => selectOffice(office.id)}
                    className="flex min-w-0 flex-1 items-center gap-3 text-left"
                  >
                    <div
                      className="flex h-10 w-10 items-center justify-center rounded-xl text-lg"
                      style={{ background: active ? `${office.color}22` : "rgba(148,163,184,0.12)" }}
                    >
                      {office.icon}
                    </div>
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                        {isKo ? office.name_ko || office.name : office.name}
                      </div>
                      <div className="mt-0.5 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                        {(office.agent_count ?? 0)} {tr("명", "agents")} · {(office.department_count ?? 0)} {tr("부서", "depts")}
                      </div>
                    </div>
                  </button>
                  <div className="flex flex-col gap-1">
                    <button
                      onClick={() => {
                        moveOffice(index, -1);
                      }}
                      disabled={index === 0}
                      className="rounded-md border p-1 transition-opacity hover:opacity-100 disabled:opacity-35"
                      style={{
                        color: "var(--th-text-muted)",
                        background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                        borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
                      }}
                      title={tr("위로", "Move Up")}
                    >
                      <ArrowUp size={13} />
                    </button>
                    <button
                      onClick={() => {
                        moveOffice(index, 1);
                      }}
                      disabled={index === order.length - 1}
                      className="rounded-md border p-1 transition-opacity hover:opacity-100 disabled:opacity-35"
                      style={{
                        color: "var(--th-text-muted)",
                        background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                        borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
                      }}
                      title={tr("아래로", "Move Down")}
                    >
                      <ArrowDown size={13} />
                    </button>
                  </div>
                </SurfaceCard>
              );
            })}

            {creating && (
              <SurfaceNotice tone="accent">
                <div className="text-sm font-medium">{tr("새 오피스 작성 중", "Creating a new office")}</div>
                <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  {tr("오른쪽 폼을 채워 저장하세요.", "Fill out the form on the right and save it.")}
                </div>
              </SurfaceNotice>
            )}
          </div>
        </SurfaceSection>

        <div className="space-y-4">
          <SurfaceSection
            title={creating ? tr("새 오피스", "New Office") : selectedOffice ? (isKo ? selectedOffice.name_ko || selectedOffice.name : selectedOffice.name) : tr("오피스 선택", "Select an Office")}
            description={creating
              ? tr("기본 정보부터 저장한 뒤 멤버를 배치할 수 있습니다.", "Save the basic profile first, then assign members.")
              : tr("이름, 아이콘, 설명을 수정하고 멤버를 조정합니다.", "Edit the identity, icon, description, and members for this office.")}
            badge={creating ? tr("초안", "Draft") : selectedOffice ? tr("편집 중", "Editing") : undefined}
            actions={!creating && selectedOffice ? (
              <SurfaceActionButton tone="danger" disabled={saving} onClick={() => void deleteOffice()}>
                <span className="inline-flex items-center gap-1.5">
                  <Trash2 size={13} />
                  {tr("삭제", "Delete")}
                </span>
              </SurfaceActionButton>
            ) : undefined}
            style={{
              background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, ${creating ? "var(--th-accent-primary)" : selectedOffice?.color || "var(--th-accent-info)"} 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)`,
            }}
          >
            <div className="mb-0 flex items-center gap-2">
              <Building2 size={18} style={{ color: creating ? "var(--th-accent-primary)" : selectedOffice?.color || "var(--th-accent-info)" }} />
              <span className="text-xs uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("오피스 프로필", "Office Profile")}
              </span>
            </div>

            {(creating || selectedOffice) ? (
              <div className="mt-4 grid gap-4 lg:grid-cols-[minmax(0,0.9fr)_minmax(0,1.1fr)]">
                <SurfaceSubsection
                  title={tr("기본 정보", "Identity")}
                  description={tr("이름과 설명을 먼저 정리합니다.", "Define the office name and description first.")}
                >
                  <div className="space-y-3">
                  <label className="block">
                    <span className="mb-1 block text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                      {tr("영문 이름", "Name")}
                    </span>
                    <input
                      type="text"
                      value={draft.name}
                      onChange={(event) => setDraft((prev) => ({ ...prev, name: event.target.value }))}
                      className="w-full rounded-xl px-3 py-2 text-sm"
                      style={inputStyle}
                    />
                  </label>

                  <label className="block">
                    <span className="mb-1 block text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                      {tr("표시 이름", "Display Name")}
                    </span>
                    <input
                      type="text"
                      value={draft.name_ko}
                      onChange={(event) => setDraft((prev) => ({ ...prev, name_ko: event.target.value }))}
                      className="w-full rounded-xl px-3 py-2 text-sm"
                      style={inputStyle}
                    />
                  </label>

                  <label className="block">
                    <span className="mb-1 block text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                      {tr("설명", "Description")}
                    </span>
                    <textarea
                      value={draft.description}
                      onChange={(event) => setDraft((prev) => ({ ...prev, description: event.target.value }))}
                      rows={4}
                      className="w-full rounded-xl px-3 py-2 text-sm"
                      style={inputStyle}
                    />
                  </label>
                  </div>
                </SurfaceSubsection>

                <SurfaceSubsection
                  title={tr("표현", "Appearance")}
                  description={tr("아이콘, 대표 색상, 미리보기를 한 곳에서 조정합니다.", "Adjust icon, accent color, and preview together.")}
                >
                  <div className="space-y-3">
                  <div>
                    <div className="mb-1 text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                      {tr("아이콘", "Icon")}
                    </div>
                    <div className="flex flex-wrap gap-2">
                      {OFFICE_ICONS.map((icon) => (
                        <button
                          key={icon}
                          onClick={() => setDraft((prev) => ({ ...prev, icon }))}
                          className="flex h-10 w-10 items-center justify-center rounded-xl text-lg transition-colors"
                          style={{
                            border: draft.icon === icon
                              ? "1px solid color-mix(in srgb, var(--th-accent-primary) 40%, transparent)"
                              : "1px solid rgba(148,163,184,0.18)",
                            background: draft.icon === icon
                              ? "color-mix(in srgb, var(--th-accent-primary-soft) 76%, transparent)"
                              : "var(--th-bg-surface)",
                          }}
                        >
                          {icon}
                        </button>
                      ))}
                    </div>
                  </div>

                  <div>
                    <div className="mb-1 text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                      {tr("대표 색상", "Accent Color")}
                    </div>
                    <div className="flex flex-wrap gap-2">
                      {OFFICE_COLORS.map((color) => (
                        <button
                          key={color}
                          onClick={() => setDraft((prev) => ({ ...prev, color }))}
                          className="h-9 w-9 rounded-full border-2 transition-transform hover:scale-105"
                          style={{
                            background: color,
                            borderColor: draft.color === color ? "#ffffff" : "rgba(255,255,255,0.28)",
                          }}
                        />
                      ))}
                    </div>
                  </div>

                  <div
                    className="rounded-2xl border p-4"
                    style={{ borderColor: `${draft.color}55`, background: `color-mix(in srgb, ${draft.color} 14%, var(--th-bg-surface))` }}
                  >
                    <div className="flex items-center gap-3">
                      <div
                        className="flex h-12 w-12 items-center justify-center rounded-2xl text-2xl"
                        style={{ background: `${draft.color}22` }}
                      >
                        {draft.icon}
                      </div>
                      <div className="min-w-0">
                        <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                          {draft.name_ko.trim() || draft.name.trim() || tr("오피스 이름", "Office Name")}
                        </div>
                        <div className="truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {draft.name.trim() || tr("영문 이름이 여기에 표시됩니다.", "English name appears here.")}
                        </div>
                      </div>
                    </div>
                  </div>
                  </div>
                </SurfaceSubsection>
              </div>
            ) : (
              <SurfaceEmptyState className="mt-4 px-4 py-8 text-center">
                {tr("왼쪽에서 오피스를 선택하거나 새 오피스를 추가하세요.", "Choose an office from the left or create a new one.")}
              </SurfaceEmptyState>
            )}

            {(creating || selectedOffice) && (
              <div className="mt-4 flex flex-wrap items-center justify-end gap-2">
                {creating && (
                  <SurfaceActionButton
                    onClick={() => {
                      setCreating(false);
                      setSelectedId(offices[0]?.id ?? null);
                    }}
                  >
                    {tr("취소", "Cancel")}
                  </SurfaceActionButton>
                )}
                <SurfaceActionButton
                  onClick={() => void saveOffice()}
                  disabled={saving || !draft.name.trim()}
                  tone="accent"
                  className="text-sm"
                >
                  <span className="inline-flex items-center gap-1.5">
                    <Save size={14} />
                    {saving ? tr("저장 중...", "Saving...") : tr("오피스 저장", "Save Office")}
                  </span>
                </SurfaceActionButton>
              </div>
            )}
          </SurfaceSection>

          {!creating && selectedOffice && (
            <SurfaceSection
              title={tr("오피스 멤버", "Office Members")}
              description={tr("현재 오피스에 배치할 에이전트를 토글합니다.", "Toggle which agents should belong to this office.")}
              badge={`${memberIds.size} ${tr("명 배치됨", "assigned")}`}
              style={{
                background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, ${selectedOffice.color} 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)`,
              }}
            >
              <div className="mb-0 flex items-center gap-2">
                <Users size={18} style={{ color: selectedOffice.color }} />
                <span className="text-xs uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                  {tr("멤버 배치", "Membership")}
                </span>
              </div>

              <div className="mt-4 flex flex-wrap items-center gap-2">
                <input
                  type="text"
                  value={search}
                  onChange={(event) => setSearch(event.target.value)}
                  placeholder={tr("에이전트 검색", "Search agents")}
                  className="min-w-[220px] flex-1 rounded-xl px-3 py-2 text-sm"
                  style={inputStyle}
                />
                <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {membersLoading ? tr("멤버 로딩 중...", "Loading members...") : `${filteredAgents.length} ${tr("명 표시", "shown")}`}
                </span>
              </div>

              {filteredAgents.length > 0 ? (
                <div className="mt-4 grid gap-3 md:grid-cols-2">
                  {filteredAgents.map((agent) => {
                    const assigned = memberIds.has(agent.id);
                    return (
                      <SurfaceCard
                        key={agent.id}
                        onClick={() => void toggleMember(agent.id)}
                        className="cursor-pointer rounded-2xl border px-3 py-3 text-left transition-colors disabled:opacity-60"
                        style={{
                          borderColor: assigned ? `${selectedOffice.color}55` : "rgba(148,163,184,0.18)",
                          background: assigned ? `color-mix(in srgb, ${selectedOffice.color} 12%, var(--th-bg-surface))` : "var(--th-bg-surface)",
                        }}
                      >
                        <div className="flex items-center gap-3">
                          <div className="flex h-11 w-11 items-center justify-center rounded-2xl text-xl" style={{ background: "rgba(148,163,184,0.12)" }}>
                            {agent.avatar_emoji}
                          </div>
                          <div className="min-w-0 flex-1">
                            <div className="truncate text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                              {agent.alias || agent.name_ko || agent.name}
                            </div>
                            <div className="truncate text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                              {agent.department_name_ko || agent.department_name || tr("미배정", "Unassigned")}
                            </div>
                          </div>
                          <span
                            className="inline-flex items-center gap-1 rounded-full px-2.5 py-1 text-[11px] font-medium"
                            style={{
                              background: assigned ? `${selectedOffice.color}22` : "rgba(148,163,184,0.12)",
                              color: assigned ? selectedOffice.color : "var(--th-text-muted)",
                            }}
                          >
                            <UserPlus size={12} />
                            {assigned ? tr("배치됨", "Assigned") : tr("추가", "Add")}
                          </span>
                        </div>
                      </SurfaceCard>
                    );
                  })}
                </div>
              ) : (
                <SurfaceEmptyState className="mt-4">
                  {tr("조건에 맞는 에이전트가 없습니다.", "No agents match this filter.")}
                </SurfaceEmptyState>
              )}
            </SurfaceSection>
          )}
        </div>
      </div>
    </div>
  );
}
