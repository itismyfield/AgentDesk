import { useCallback, useMemo, useState } from "react";
import type { Agent, DispatchedSession } from "../../types";
import type { UiLanguage } from "../../i18n";
import * as api from "../../api";
import { BLANK } from "./constants";
import type { FormData } from "./types";
import {
  classifyRoster,
  filterAndSortAgents,
  isStandIn,
  type AgentSortMode,
  type AgentStatusFilter,
} from "./agent-list-controls";

export type { AgentSortMode };

export type AgentManagerTab = "agents" | "skills" | "dispatch";

interface UseAgentManagerControllerParams {
  agents: Agent[];
  language: UiLanguage;
  onAgentsChange: () => void;
  /** Every workspace session; states come from them as on home. */
  sessions: DispatchedSession[];
  sessionsFailed: boolean;
  /** Keeps the shell's tab in step; the page reads its tab from the URL. */
  onTabChange?: (tab: AgentManagerTab) => void;
}

export function useAgentManagerController({
  agents,
  language,
  onAgentsChange,
  sessions,
  sessionsFailed,
  onTabChange,
}: UseAgentManagerControllerParams) {
  const locale = language;
  const isKo = locale.startsWith("ko");
  const tr = useCallback(
    (ko: string, en: string) => (isKo ? ko : en),
    [isKo],
  );

  const [statusFilter, setStatusFilter] = useState<AgentStatusFilter>("all");
  const [sortMode, setSortMode] = useState<AgentSortMode>("status");
  const [search, setSearch] = useState("");
  const [includeTests, setIncludeTests] = useState(false);
  const [saving, setSaving] = useState(false);
  const [agentModal, setAgentModal] = useState<{
    open: boolean;
    editAgent: Agent | null;
  }>({ open: false, editAgent: null });
  const [setupWizard, setSetupWizard] = useState<{
    open: boolean;
    mode: "create" | "duplicate";
    sourceAgent: Agent | null;
  }>({ open: false, mode: "create", sourceAgent: null });
  const [form, setForm] = useState<FormData>(BLANK);

  const handleTabChange = useCallback((nextTab: AgentManagerTab) => {
    onTabChange?.(nextTab);
  }, [onTabChange]);

  const entries = useMemo(() => classifyRoster(agents, sessions, sessionsFailed), [agents, sessions, sessionsFailed]);
  const sortedEntries = useMemo(() => {
    const rows = entries.filter((entry) => !isStandIn(entry.agent));
    return filterAndSortAgents(rows, { statusFilter, search, includeTests }, sortMode);
  }, [entries, includeTests, search, sortMode, statusFilter]);

  const openCreateAgent = useCallback(() => {
    setSetupWizard({ open: true, mode: "create", sourceAgent: null });
  }, []);

  const openEditAgent = useCallback((agent: Agent) => {
    const applyAgentToForm = (target: Agent) => setForm({
      ...BLANK,
      name: target.name,
      name_ko: target.name_ko ?? "",
      name_ja: target.name_ja ?? "",
      name_zh: target.name_zh ?? "",
      cli_provider: target.cli_provider ?? "claude",
      avatar_emoji: target.avatar_emoji ?? "🤖",
      personality: target.personality ?? "",
      prompt_content: target.prompt_content ?? "",
      auto_commit: false,
    });
    applyAgentToForm(agent);
    setAgentModal({ open: true, editAgent: agent });
    api.getAgent(agent.id)
      .then((detail) => {
        applyAgentToForm(detail);
        setAgentModal((current) =>
          current.open && current.editAgent?.id === agent.id
            ? { open: true, editAgent: detail }
            : current,
        );
      })
      .catch((error) => {
        console.error("Agent detail load failed:", error);
      });
  }, []);

  const openDuplicateAgent = useCallback((agent: Agent) => {
    setSetupWizard({ open: true, mode: "duplicate", sourceAgent: agent });
    api.getAgent(agent.id)
      .then((detail) => {
        setSetupWizard((current) =>
          current.open && current.mode === "duplicate" && current.sourceAgent?.id === agent.id
            ? { open: true, mode: "duplicate", sourceAgent: detail }
            : current,
        );
      })
      .catch((error) => {
        console.error("Agent detail load failed:", error);
      });
  }, []);

  const handleSaveAgent = useCallback(async (values: FormData) => {
    setSaving(true);
    try {
      const payload: Record<string, unknown> = {
        name: values.name.trim(),
        name_ko: values.name_ko.trim() || values.name.trim(),
        name_ja: values.name_ja.trim() || undefined,
        name_zh: values.name_zh.trim() || undefined,
        cli_provider: values.cli_provider,
        avatar_emoji: values.avatar_emoji,
        personality: values.personality.trim() || null,
        prompt_content: values.prompt_content,
        auto_commit: values.auto_commit,
      };

      if (agentModal.editAgent) {
        await api.updateAgent(agentModal.editAgent.id, payload);
      } else {
        await api.createAgent(payload);
      }

      setAgentModal({ open: false, editAgent: null });
      onAgentsChange();
    } catch (error) {
      console.error("Agent save failed:", error);
    } finally {
      setSaving(false);
    }
  }, [agentModal.editAgent, onAgentsChange]);

  const handleDeleteAgent = useCallback(async (id: string) => {
    setSaving(true);
    try {
      await api.deleteAgent(id);
      onAgentsChange();
    } catch (error) {
      console.error("Agent delete failed:", error);
    } finally {
      setSaving(false);
    }
  }, [onAgentsChange]);

  return {
    form,
    agentModal,
    handleDeleteAgent,
    handleSaveAgent,
    handleTabChange,
    isKo,
    locale,
    entries,
    includeTests,
    openCreateAgent,
    openEditAgent,
    openDuplicateAgent,
    saving,
    search,
    setAgentModal,
    setForm,
    setIncludeTests,
    setSearch,
    setSortMode,
    setStatusFilter,
    setupWizard,
    setSetupWizard,
    sortMode,
    sortedEntries,
    statusFilter,
    tr,
  };
}
