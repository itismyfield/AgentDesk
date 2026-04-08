import { useState, useCallback, useMemo, type DragEvent } from "react";
import type { Agent, Department, DispatchedSession } from "../types";
import type { UiLanguage } from "../i18n";
import { localeName } from "../i18n";
import * as api from "../api";
import { buildSpriteMap } from "./AgentAvatar";
import { pickRandomSpritePair } from "./agent-manager/utils";
import { BLANK, ICON_SPRITE_POOL } from "./agent-manager/constants";
import type { FormData } from "./agent-manager/types";
import AgentsTab from "./agent-manager/AgentsTab";
import DepartmentsTab from "./agent-manager/DepartmentsTab";
import AgentFormModal from "./agent-manager/AgentFormModal";
import DepartmentFormModal from "./agent-manager/DepartmentFormModal";
import { SessionPanel } from "./session-panel/SessionPanel";

interface AgentManagerViewProps {
  agents: Agent[];
  departments: Department[];
  language: UiLanguage;
  officeId?: string | null;
  onAgentsChange: () => void;
  onDepartmentsChange: () => void;
  sessions?: DispatchedSession[];
  onAssign?: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
  activeTab?: Tab;
  onTabChange?: (tab: Tab) => void;
  showHeader?: boolean;
  showTabBar?: boolean;
  title?: string;
  subtitle?: string;
}

type Tab = "agents" | "departments" | "dispatch";

export default function AgentManagerView({
  agents,
  departments,
  language,
  officeId,
  onAgentsChange,
  onDepartmentsChange,
  sessions,
  onAssign,
  activeTab,
  onTabChange,
  showHeader = true,
  showTabBar = true,
  title,
  subtitle,
}: AgentManagerViewProps) {
  const locale = language;
  const isKo = locale.startsWith("ko");
  const tr = useCallback(
    (ko: string, en: string) => (isKo ? ko : en),
    [isKo],
  );

  // ── Tab state ──
  const [internalTab, setInternalTab] = useState<Tab>("agents");
  const tab = activeTab ?? internalTab;
  const handleTabChange = useCallback((nextTab: Tab) => {
    if (activeTab === undefined) {
      setInternalTab(nextTab);
    }
    onTabChange?.(nextTab);
  }, [activeTab, onTabChange]);

  // ── Agent tab state ──
  const [deptTab, setDeptTab] = useState("all");
  const [statusFilter, setStatusFilter] = useState<string>("all");
  const [search, setSearch] = useState("");
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  // ── Agent modal state ──
  const [agentModal, setAgentModal] = useState<{ open: boolean; editAgent: Agent | null }>({ open: false, editAgent: null });
  const [form, setForm] = useState<FormData>(BLANK);

  // ── Department modal state ──
  const [deptModal, setDeptModal] = useState<{ open: boolean; editDept: Department | null }>({ open: false, editDept: null });

  // ── Department ordering state ──
  const [deptOrder, setDeptOrder] = useState<Department[]>(departments);
  const [deptOrderDirty, setDeptOrderDirty] = useState(false);
  const [reorderSaving, setReorderSaving] = useState(false);
  const [draggingDeptId, setDraggingDeptId] = useState<string | null>(null);
  const [dragOverDeptId, setDragOverDeptId] = useState<string | null>(null);
  const [dragOverPosition, setDragOverPosition] = useState<"before" | "after" | null>(null);

  // Sync deptOrder when departments prop changes
  if (!deptOrderDirty && JSON.stringify(deptOrder.map(d => d.id)) !== JSON.stringify(departments.map(d => d.id))) {
    setDeptOrder(departments);
  }

  // ── Derived data ──
  const spriteMap = useMemo(() => buildSpriteMap(agents), [agents]);
  const randomIconSprites = useMemo(() => ({ total: pickRandomSpritePair(ICON_SPRITE_POOL) }), []);

  const sortedAgents = useMemo(() => {
    let filtered = agents;
    if (deptTab !== "all") {
      filtered = filtered.filter((a) => a.department_id === deptTab);
    }
    if (statusFilter !== "all") {
      filtered = filtered.filter((a) => a.status === statusFilter);
    }
    if (search.trim()) {
      const q = search.toLowerCase();
      filtered = filtered.filter(
        (a) =>
          a.name.toLowerCase().includes(q) ||
          a.name_ko.toLowerCase().includes(q) ||
          (a.alias && a.alias.toLowerCase().includes(q)) ||
          a.avatar_emoji.includes(q),
      );
    }
    return [...filtered].sort((a, b) => {
      const statusOrder = { working: 0, idle: 1, break: 2, offline: 3 };
      const sa = statusOrder[a.status] ?? 4;
      const sb = statusOrder[b.status] ?? 4;
      if (sa !== sb) return sa - sb;
      return a.name.localeCompare(b.name);
    });
  }, [agents, deptTab, statusFilter, search]);

  // ── Agent CRUD ──
  const openCreateAgent = useCallback(() => {
    setForm(BLANK);
    setAgentModal({ open: true, editAgent: null });
  }, []);

  const openEditAgent = useCallback((agent: Agent) => {
    setForm({
      name: agent.name,
      name_ko: agent.name_ko ?? "",
      name_ja: agent.name_ja ?? "",
      name_zh: agent.name_zh ?? "",
      department_id: agent.department_id ?? "",
      cli_provider: agent.cli_provider ?? "claude",
      avatar_emoji: agent.avatar_emoji ?? "🤖",
      sprite_number: agent.sprite_number ?? null,
      personality: agent.personality ?? "",
    });
    setAgentModal({ open: true, editAgent: agent });
  }, []);

  const handleSaveAgent = useCallback(async () => {
    setSaving(true);
    try {
      const payload: Record<string, unknown> = {
        name: form.name.trim(),
        name_ko: form.name_ko.trim() || form.name.trim(),
        name_ja: form.name_ja.trim() || undefined,
        name_zh: form.name_zh.trim() || undefined,
        department_id: form.department_id || null,
        cli_provider: form.cli_provider,
        avatar_emoji: form.avatar_emoji,
        sprite_number: form.sprite_number,
        personality: form.personality.trim() || null,
      };
      if (!agentModal.editAgent && officeId) {
        payload.office_id = officeId;
      }
      if (agentModal.editAgent) {
        await api.updateAgent(agentModal.editAgent.id, payload);
      } else {
        await api.createAgent(payload);
      }
      setAgentModal({ open: false, editAgent: null });
      onAgentsChange();
    } catch (e) {
      console.error("Agent save failed:", e);
    } finally {
      setSaving(false);
    }
  }, [form, agentModal.editAgent, onAgentsChange]);

  const handleDeleteAgent = useCallback(async (id: string) => {
    setSaving(true);
    try {
      await api.deleteAgent(id);
      setConfirmDeleteId(null);
      onAgentsChange();
    } catch (e) {
      console.error("Agent delete failed:", e);
    } finally {
      setSaving(false);
    }
  }, [onAgentsChange]);

  // ── Department editing ──
  const openCreateDept = useCallback(() => {
    setDeptModal({ open: true, editDept: null });
  }, []);

  const openEditDept = useCallback((dept: Department) => {
    setDeptModal({ open: true, editDept: dept });
  }, []);

  // ── Department ordering ──
  const handleMoveDept = useCallback((index: number, direction: -1 | 1) => {
    setDeptOrder((prev) => {
      const next = [...prev];
      const target = index + direction;
      if (target < 0 || target >= next.length) return prev;
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
    setDeptOrderDirty(true);
  }, []);

  const handleSaveOrder = useCallback(async () => {
    setReorderSaving(true);
    try {
      for (let i = 0; i < deptOrder.length; i++) {
        await api.updateDepartment(deptOrder[i].id, { sort_order: i });
      }
      setDeptOrderDirty(false);
      onDepartmentsChange();
    } catch (e) {
      console.error("Order save failed:", e);
    } finally {
      setReorderSaving(false);
    }
  }, [deptOrder, onDepartmentsChange]);

  const handleCancelOrder = useCallback(() => {
    setDeptOrder(departments);
    setDeptOrderDirty(false);
  }, [departments]);

  // Drag & drop handlers
  const handleDragStart = useCallback((deptId: string, e: DragEvent<HTMLDivElement>) => {
    setDraggingDeptId(deptId);
    e.dataTransfer.effectAllowed = "move";
  }, []);

  const handleDragOver = useCallback((deptId: string, e: DragEvent<HTMLDivElement>) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    const rect = e.currentTarget.getBoundingClientRect();
    const midY = rect.top + rect.height / 2;
    setDragOverDeptId(deptId);
    setDragOverPosition(e.clientY < midY ? "before" : "after");
  }, []);

  const handleDrop = useCallback((targetId: string, _e: DragEvent<HTMLDivElement>) => {
    if (!draggingDeptId || draggingDeptId === targetId) {
      setDraggingDeptId(null);
      setDragOverDeptId(null);
      setDragOverPosition(null);
      return;
    }
    setDeptOrder((prev) => {
      const next = prev.filter((d) => d.id !== draggingDeptId);
      const targetIndex = next.findIndex((d) => d.id === targetId);
      const insertAt = dragOverPosition === "after" ? targetIndex + 1 : targetIndex;
      const dragged = prev.find((d) => d.id === draggingDeptId);
      if (dragged) next.splice(insertAt, 0, dragged);
      return next;
    });
    setDeptOrderDirty(true);
    setDraggingDeptId(null);
    setDragOverDeptId(null);
    setDragOverPosition(null);
  }, [draggingDeptId, dragOverPosition]);

  const handleDragEnd = useCallback(() => {
    setDraggingDeptId(null);
    setDragOverDeptId(null);
    setDragOverPosition(null);
  }, []);

  const defaultTitle =
    tab === "departments"
      ? tr("부서 관리", "Departments")
      : tab === "dispatch"
        ? tr("파견 세션", "Dispatch Sessions")
        : tr("직원 관리", "Agent Manager");
  const resolvedTitle = title ?? defaultTitle;
  const resolvedSubtitle = subtitle
    ?? (tab === "departments"
      ? tr("부서 순서, 역할, 테마를 관리합니다.", "Manage department order, roles, and themes.")
      : tab === "dispatch"
        ? tr("감지된 AgentDesk 세션을 오피스에 배치합니다.", "Assign detected AgentDesk sessions into the office.")
        : tr("에이전트 프로필, 스킬, 소속을 관리합니다.", "Manage agent profiles, skills, and office membership."));

  return (
    <div
      className="mx-auto h-full max-w-5xl min-w-0 space-y-4 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      {showHeader && (
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <h1 className="text-xl font-bold" style={{ color: "var(--th-text-heading)" }}>
              {resolvedTitle}
            </h1>
            {resolvedSubtitle && (
              <p className="mt-1 text-sm" style={{ color: "var(--th-text-muted)" }}>
                {resolvedSubtitle}
              </p>
            )}
          </div>
          <div className="flex items-center gap-2">
            {(showTabBar || tab === "departments") && tab !== "dispatch" && (
              <button
                onClick={openCreateDept}
                className="rounded-lg border px-3 py-1.5 text-xs font-medium transition-opacity hover:opacity-100"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
                  background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                  color: "var(--th-text-secondary)",
                }}
              >
                + {tr("부서 추가", "Add Dept")}
              </button>
            )}
            {(showTabBar || tab === "agents") && tab !== "departments" && tab !== "dispatch" && (
              <button
                onClick={openCreateAgent}
                className="px-3 py-1.5 rounded-lg text-xs font-medium bg-blue-600 hover:bg-blue-500 text-white transition-all"
              >
                + {tr("직원 채용", "Hire Agent")}
              </button>
            )}
          </div>
        </div>
      )}

      {showTabBar && (
        <div className="flex gap-1" style={{ borderBottom: "1px solid var(--th-card-border)" }}>
          <button
            onClick={() => handleTabChange("agents")}
            className={`px-4 py-2 text-sm font-medium transition-colors ${
              tab === "agents" ? "text-blue-400 border-b-2 border-blue-400" : ""
            }`}
            style={tab !== "agents" ? { color: "var(--th-text-muted)" } : undefined}
          >
            {tr("직원", "Agents")} ({agents.length})
          </button>
          <button
            onClick={() => handleTabChange("departments")}
            className={`px-4 py-2 text-sm font-medium transition-colors ${
              tab === "departments" ? "text-blue-400 border-b-2 border-blue-400" : ""
            }`}
            style={tab !== "departments" ? { color: "var(--th-text-muted)" } : undefined}
          >
            {tr("부서", "Departments")} ({departments.length})
          </button>
          {sessions && onAssign && (
            <button
              onClick={() => handleTabChange("dispatch")}
              className={`px-4 py-2 text-sm font-medium transition-colors ${
                tab === "dispatch" ? "text-blue-400 border-b-2 border-blue-400" : ""
              }`}
              style={tab !== "dispatch" ? { color: "var(--th-text-muted)" } : undefined}
            >
              {tr("파견", "Dispatch")} ({sessions.length})
            </button>
          )}
        </div>
      )}

      {/* Tab content */}
      {tab === "dispatch" && sessions && onAssign ? (
        <SessionPanel
          sessions={sessions}
          departments={departments}
          agents={agents}
          onAssign={onAssign}
        />
      ) : tab === "agents" ? (
        <div className="space-y-4">
          <AgentsTab
            tr={tr}
            locale={locale}
            isKo={isKo}
            agents={agents}
            departments={departments}
            deptTab={deptTab}
            setDeptTab={setDeptTab}
            search={search}
            setSearch={setSearch}
            statusFilter={statusFilter}
            setStatusFilter={setStatusFilter}
            sortedAgents={sortedAgents}
            spriteMap={spriteMap}
            confirmDeleteId={confirmDeleteId}
            setConfirmDeleteId={setConfirmDeleteId}
            onEditAgent={openEditAgent}
            onEditDepartment={openEditDept}
            onDeleteAgent={handleDeleteAgent}
            saving={saving}
            randomIconSprites={randomIconSprites}
          />
        </div>
      ) : (
        <DepartmentsTab
          tr={tr}
          locale={locale}
          agents={agents}
          departments={departments}
          deptOrder={deptOrder}
          deptOrderDirty={deptOrderDirty}
          reorderSaving={reorderSaving}
          draggingDeptId={draggingDeptId}
          dragOverDeptId={dragOverDeptId}
          dragOverPosition={dragOverPosition}
          onSaveOrder={handleSaveOrder}
          onCancelOrder={handleCancelOrder}
          onMoveDept={handleMoveDept}
          onEditDept={openEditDept}
          onDragStart={handleDragStart}
          onDragOver={handleDragOver}
          onDrop={handleDrop}
          onDragEnd={handleDragEnd}
        />
      )}

      {/* Agent create/edit modal */}
      {agentModal.open && (
        <AgentFormModal
          isKo={isKo}
          locale={locale}
          tr={tr}
          form={form}
          setForm={setForm}
          departments={departments}
          isEdit={!!agentModal.editAgent}
          saving={saving}
          onSave={handleSaveAgent}
          onClose={() => setAgentModal({ open: false, editAgent: null })}
        />
      )}

      {/* Department modal */}
      {deptModal.open && (
        <DepartmentFormModal
          locale={locale}
          tr={tr}
          department={deptModal.editDept}
          departments={departments}
          officeId={officeId}
          onSave={() => {
            setDeptModal({ open: false, editDept: null });
            onDepartmentsChange();
          }}
          onClose={() => setDeptModal({ open: false, editDept: null })}
        />
      )}
    </div>
  );
}
