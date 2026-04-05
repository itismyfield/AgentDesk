import { useState, useEffect, useRef, useCallback } from "react";
import type { ChatMessage, DiscordBinding } from "../api/client";
import type { Notification } from "./NotificationCenter";
import type { Agent, AuditLogEntry, Department, MessageType } from "../types";
import { getAgentWarnings, getAgentWorkSummary } from "../agent-insights";
import * as api from "../api";
import { AlertTriangle, Building2, Megaphone, Send, Users } from "lucide-react";

type ChatMode = "agent" | "department" | "all";
type ChatMessageFilter = "all" | MessageType;

interface ChatViewProps {
  agents: Agent[];
  departments: Department[];
  notifications: Notification[];
  auditLogs: AuditLogEntry[];
  isKo: boolean;
  wsRef: React.RefObject<WebSocket | null>;
  onMessageSent?: () => void;
}

const FEED_FILTERS: Array<{ value: ChatMessageFilter; ko: string; en: string }> = [
  { value: "all", ko: "전체 유형", en: "All types" },
  { value: "chat", ko: "채팅", en: "Chat" },
  { value: "directive", ko: "지시", en: "Directive" },
  { value: "announcement", ko: "공지", en: "Announcement" },
  { value: "report", ko: "보고", en: "Report" },
  { value: "status_update", ko: "상태", en: "Status" },
];

const COMPOSE_TYPES: Array<{ value: MessageType; ko: string; en: string }> = [
  { value: "directive", ko: "지시", en: "Directive" },
  { value: "announcement", ko: "공지", en: "Announcement" },
  { value: "chat", ko: "채팅", en: "Chat" },
];

function getMessageTypeTone(messageType: string): { bg: string; color: string } {
  switch (messageType) {
    case "directive":
      return { bg: "rgba(245, 158, 11, 0.18)", color: "#f59e0b" };
    case "announcement":
      return { bg: "rgba(99, 102, 241, 0.18)", color: "#818cf8" };
    case "report":
      return { bg: "rgba(16, 185, 129, 0.18)", color: "#34d399" };
    case "status_update":
      return { bg: "rgba(56, 189, 248, 0.18)", color: "#38bdf8" };
    default:
      return { bg: "rgba(148, 163, 184, 0.18)", color: "#cbd5e1" };
  }
}

function getTargetLabel(msg: ChatMessage, isKo: boolean): string {
  if (msg.receiver_type === "all") return isKo ? "전체 공지" : "Broadcast";
  if (msg.receiver_type === "department") {
    return msg.receiver_name_ko || msg.receiver_name || (isKo ? "부서" : "Department");
  }
  return msg.receiver_name_ko || msg.receiver_name || (isKo ? "에이전트" : "Agent");
}

function getMessageTypeLabel(type: string, isKo: boolean): string {
  const hit = FEED_FILTERS.find((item) => item.value === type);
  if (hit) return isKo ? hit.ko : hit.en;
  return type;
}

function getActivitySourceLabel(agent: Agent, isKo: boolean): string {
  if (agent.activity_source === "agentdesk") return "AgentDesk";
  return isKo ? "기본" : "Default";
}

function getBindingSourceLabel(source: string | undefined, isKo: boolean): string {
  switch (source) {
    case "role-map":
      return "RoleMap";
    case "primary":
      return isKo ? "기본" : "Primary";
    case "alt":
      return "Alt";
    case "codex":
      return "Codex";
    default:
      return isKo ? "채널" : "Channel";
  }
}

function getBindingOptionLabel(binding: DiscordBinding, isKo: boolean): string {
  const source = getBindingSourceLabel(binding.source, isKo);
  const channel = binding.channelName ? `#${binding.channelName}` : binding.channelId;
  return `${source} · ${channel}`;
}

function matchesAuditTarget(
  log: AuditLogEntry,
  mode: ChatMode,
  selectedAgent: string | null,
  selectedDepartment: string | null,
): boolean {
  if (mode === "agent" && selectedAgent) {
    return (
      log.entity_id === selectedAgent ||
      log.metadata?.agent_id === selectedAgent ||
      log.metadata?.receiver_id === selectedAgent
    );
  }

  if (mode === "department" && selectedDepartment) {
    return (
      log.entity_id === selectedDepartment ||
      log.metadata?.department_id === selectedDepartment ||
      log.metadata?.receiver_id === selectedDepartment
    );
  }

  return true;
}

export default function ChatView({
  agents,
  departments,
  notifications,
  auditLogs,
  isKo,
  wsRef,
  onMessageSent,
}: ChatViewProps) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [selectedAgent, setSelectedAgent] = useState<string | null>(null);
  const [selectedDepartment, setSelectedDepartment] = useState<string | null>(null);
  const [mode, setMode] = useState<ChatMode>("all");
  const [messageTypeFilter, setMessageTypeFilter] = useState<ChatMessageFilter>("all");
  const [composeType, setComposeType] = useState<MessageType>("announcement");
  const [discordBindings, setDiscordBindings] = useState<DiscordBinding[]>([]);
  const [selectedAgentTarget, setSelectedAgentTarget] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const locale = isKo ? "ko-KR" : "en-US";
  const selectedAgentObj = agents.find((agent) => agent.id === selectedAgent) ?? null;
  const selectedDepartmentObj = departments.find((department) => department.id === selectedDepartment) ?? null;
  const agentRouteBindings = discordBindings.reduce<Record<string, DiscordBinding[]>>((acc, binding) => {
    if (!acc[binding.agentId]) acc[binding.agentId] = [];
    acc[binding.agentId].push(binding);
    return acc;
  }, {});
  const routableAgents = agents.filter((agent) => (agentRouteBindings[agent.id]?.length ?? 0) > 0);
  const selectedAgentBindings = selectedAgent ? agentRouteBindings[selectedAgent] ?? [] : [];
  const selectedAgentTargetBinding = selectedAgentBindings.find((binding) => binding.channelId === selectedAgentTarget) ?? null;

  const matchesCurrentFeed = useCallback((message: ChatMessage) => {
    if (messageTypeFilter !== "all" && message.message_type !== messageTypeFilter) return false;

    if (mode === "all") return true;

    if (mode === "agent" && selectedAgent) {
      return (
        (message.receiver_type === "agent" && message.receiver_id === selectedAgent) ||
        message.receiver_type === "all" ||
        (message.sender_type === "agent" && message.sender_id === selectedAgent)
      );
    }

    if (mode === "department" && selectedDepartment) {
      const senderDepartmentId =
        message.sender_type === "agent"
          ? agents.find((agent) => agent.id === message.sender_id)?.department_id
          : null;

      return (
        (message.receiver_type === "department" && message.receiver_id === selectedDepartment) ||
        message.receiver_type === "all" ||
        senderDepartmentId === selectedDepartment
      );
    }

    return false;
  }, [agents, messageTypeFilter, mode, selectedAgent, selectedDepartment]);

  const loadMessages = useCallback(async () => {
    if (mode === "agent" && !selectedAgent) {
      setMessages([]);
      return;
    }
    if (mode === "department" && !selectedDepartment) {
      setMessages([]);
      return;
    }

    try {
      const opts: Parameters<typeof api.getMessages>[0] = {
        limit: 150,
        messageType: messageTypeFilter,
      };
      if (mode === "agent" && selectedAgent) {
        opts.receiverType = "agent";
        opts.receiverId = selectedAgent;
      } else if (mode === "department" && selectedDepartment) {
        opts.receiverType = "department";
        opts.receiverId = selectedDepartment;
      }
      const data = await api.getMessages(opts);
      setMessages(data.messages);
    } catch {
      // ignore
    }
  }, [messageTypeFilter, mode, selectedAgent, selectedDepartment]);

  useEffect(() => {
    loadMessages();
  }, [loadMessages]);

  useEffect(() => {
    api.getDiscordBindings()
      .then((bindings) => setDiscordBindings(bindings))
      .catch(() => setDiscordBindings([]));
  }, []);

  useEffect(() => {
    if (mode === "all") {
      setComposeType("announcement");
      return;
    }
    setComposeType("directive");
  }, [mode]);

  useEffect(() => {
    if (mode !== "agent") {
      setSelectedAgentTarget(null);
      return;
    }
    if (!selectedAgent) {
      setSelectedAgentTarget(null);
      return;
    }
    const routes = agentRouteBindings[selectedAgent] ?? [];
    if (routes.length === 0) {
      setSelectedAgentTarget(null);
      return;
    }
    setSelectedAgentTarget((prev) =>
      prev && routes.some((binding) => binding.channelId === prev) ? prev : routes[0].channelId,
    );
  }, [agentRouteBindings, mode, selectedAgent]);

  useEffect(() => {
    if (mode !== "agent" || !selectedAgent) return;
    if (routableAgents.some((agent) => agent.id === selectedAgent)) return;
    setSelectedAgent(null);
    setSelectedAgentTarget(null);
  }, [mode, routableAgents, selectedAgent]);

  useEffect(() => {
    if (!scrollRef.current) return;
    scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
  }, [messages]);

  useEffect(() => {
    const handler = (event: MessageEvent) => {
      try {
        const data = JSON.parse(event.data);
        if (data.type !== "new_message") return;
        const incoming = data.payload as ChatMessage;
        if (!matchesCurrentFeed(incoming)) return;
        setMessages((prev) => {
          const existingIndex = prev.findIndex((message) => message.id === incoming.id);
          if (existingIndex >= 0) {
            return prev.map((message) => (message.id === incoming.id ? incoming : message));
          }
          return [...prev, incoming].slice(-150);
        });
      } catch {
        // ignore
      }
    };

    const ws = wsRef.current;
    if (!ws) return;
    ws.addEventListener("message", handler);
    return () => ws.removeEventListener("message", handler);
  }, [matchesCurrentFeed, wsRef]);

  const handleSend = async () => {
    if (!input.trim() || sending) return;
    if (mode === "agent" && !selectedAgent) return;
    if (mode === "department" && !selectedDepartment) return;

    setSending(true);
    try {
      const sent = await api.sendMessage({
        sender_type: "ceo",
        receiver_type: mode === "all" ? "all" : mode === "agent" ? "agent" : "department",
        receiver_id: mode === "agent" ? selectedAgent : mode === "department" ? selectedDepartment : null,
        discord_target: mode === "agent" ? selectedAgentTarget : null,
        content: input.trim(),
        message_type: composeType,
      });

      setMessages((prev) => {
        const existingIndex = prev.findIndex((message) => message.id === sent.id);
        if (existingIndex >= 0) {
          return prev.map((message) => (message.id === sent.id ? sent : message));
        }
        return [...prev, sent].slice(-150);
      });
      setInput("");
      onMessageSent?.();
    } catch (error) {
      console.error("Send failed:", error);
    } finally {
      setSending(false);
    }
  };

  const departmentAgents = selectedDepartment
    ? agents.filter((agent) => agent.department_id === selectedDepartment)
    : [];
  const workingDepartmentAgents = departmentAgents.filter((agent) => agent.status === "working");
  const workingDepartmentWork = workingDepartmentAgents
    .map((agent) => ({
      id: agent.id,
      label: agent.alias || agent.name_ko || agent.name,
      summary: getAgentWorkSummary(agent),
    }))
    .filter((entry) => entry.summary)
    .slice(0, 3);

  const selectedAgentWarnings = selectedAgentObj
    ? getAgentWarnings(selectedAgentObj, {
      hasDiscordBindings: selectedAgentBindings.length > 0,
    })
    : [];

  const selectedAgentRouteCount = selectedAgentBindings.length;

  const relevantNotifications = notifications.slice(0, 5);
  const relevantAuditLogs = auditLogs
    .filter((log) => matchesAuditTarget(log, mode, selectedAgent, selectedDepartment))
    .slice(0, 5);

  const allAgentsWithRoute = routableAgents.length;
  const allWorkingAgents = agents.filter((agent) => agent.status === "working").length;
  const allWarningAgents = agents.filter((agent) =>
    getAgentWarnings(agent, {
      hasDiscordBindings: (agentRouteBindings[agent.id]?.length ?? 0) > 0,
    }).length > 0,
  ).length;

  const inputDisabled =
    sending ||
    (mode === "agent" && !selectedAgent) ||
    (mode === "agent" && !selectedAgentTarget) ||
    (mode === "department" && !selectedDepartment);

  const placeholder =
    mode === "all"
      ? tr("전체 공지 메시지...", "Broadcast message...")
      : mode === "agent"
        ? selectedAgentObj
          ? tr(
            `${selectedAgentObj.alias || selectedAgentObj.name_ko || "에이전트"}에게 메시지...`,
            `Message to ${selectedAgentObj.name || "agent"}...`,
          )
          : tr("에이전트를 선택하세요", "Select an agent")
        : selectedDepartmentObj
          ? tr(
            `${selectedDepartmentObj.name_ko || "부서"}에 메시지...`,
            `Message to ${selectedDepartmentObj.name || "department"}...`,
          )
          : tr("부서를 선택하세요", "Select a department");

  return (
    <div className="flex-1 min-h-0 flex flex-col overflow-hidden">
      <div
        className="px-4 py-3 border-b shrink-0"
        style={{ borderColor: "var(--th-card-border)", background: "var(--th-surface)" }}
      >
        <div className="flex flex-wrap items-center gap-2">
          <button
            onClick={() => {
              setMode("all");
              setSelectedAgent(null);
              setSelectedDepartment(null);
            }}
            className={`px-2.5 py-1 rounded-lg text-xs font-medium transition-colors ${mode === "all" ? "bg-indigo-600 text-white" : ""}`}
            style={mode !== "all" ? { color: "var(--th-text-muted)" } : undefined}
          >
            <Megaphone size={12} className="inline mr-1" />
            {tr("전체", "All")}
          </button>
          <button
            onClick={() => {
              setMode("agent");
              setSelectedDepartment(null);
            }}
            className={`px-2.5 py-1 rounded-lg text-xs font-medium transition-colors ${mode === "agent" ? "bg-indigo-600 text-white" : ""}`}
            style={mode !== "agent" ? { color: "var(--th-text-muted)" } : undefined}
          >
            <Users size={12} className="inline mr-1" />
            {tr("1:1", "DM")}
          </button>
          <button
            onClick={() => {
              setMode("department");
              setSelectedAgent(null);
            }}
            className={`px-2.5 py-1 rounded-lg text-xs font-medium transition-colors ${mode === "department" ? "bg-indigo-600 text-white" : ""}`}
            style={mode !== "department" ? { color: "var(--th-text-muted)" } : undefined}
          >
            <Building2 size={12} className="inline mr-1" />
            {tr("부서", "Dept")}
          </button>

          {mode === "agent" && (
            <select
              value={selectedAgent || ""}
              onChange={(event) => setSelectedAgent(event.target.value || null)}
              className="px-2 py-1 rounded-lg text-xs bg-transparent border"
              style={{ borderColor: "var(--th-input-border)", color: "var(--th-text)" }}
            >
              <option value="">{tr("에이전트 선택", "Select Agent")}</option>
              {routableAgents.map((agent) => (
                <option key={agent.id} value={agent.id}>
                  {agent.avatar_emoji} {agent.alias || agent.name_ko || agent.name}
                </option>
              ))}
            </select>
          )}

          {mode === "agent" && selectedAgentBindings.length > 1 && (
            <select
              value={selectedAgentTarget || ""}
              onChange={(event) => setSelectedAgentTarget(event.target.value || null)}
              className="px-2 py-1 rounded-lg text-xs bg-transparent border"
              style={{ borderColor: "var(--th-input-border)", color: "var(--th-text)" }}
            >
              {selectedAgentBindings.map((binding) => (
                <option key={`${binding.agentId}:${binding.channelId}:${binding.source || "channel"}`} value={binding.channelId}>
                  {getBindingOptionLabel(binding, isKo)}
                </option>
              ))}
            </select>
          )}

          {mode === "department" && (
            <select
              value={selectedDepartment || ""}
              onChange={(event) => setSelectedDepartment(event.target.value || null)}
              className="px-2 py-1 rounded-lg text-xs bg-transparent border"
              style={{ borderColor: "var(--th-input-border)", color: "var(--th-text)" }}
            >
              <option value="">{tr("부서 선택", "Select Department")}</option>
              {departments.map((department) => (
                <option key={department.id} value={department.id}>
                  {department.icon} {department.name_ko || department.name}
                </option>
              ))}
            </select>
          )}

          <div className="flex-1" />

          <select
            value={messageTypeFilter}
            onChange={(event) => setMessageTypeFilter(event.target.value as ChatMessageFilter)}
            className="px-2 py-1 rounded-lg text-xs bg-transparent border"
            style={{ borderColor: "var(--th-input-border)", color: "var(--th-text)" }}
          >
            {FEED_FILTERS.map((item) => (
              <option key={item.value} value={item.value}>
                {isKo ? item.ko : item.en}
              </option>
            ))}
          </select>

          <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {messages.length} {tr("메시지", "messages")}
          </span>
        </div>

        <div className="grid gap-3 mt-3 md:grid-cols-3">
          <div
            className="rounded-2xl border px-3 py-3"
            style={{ borderColor: "var(--th-card-border)", background: "var(--th-bg-surface)" }}
          >
            <div className="text-xs font-semibold mb-2" style={{ color: "var(--th-text-muted)" }}>
              {tr("현재 대상", "Current target")}
            </div>

            {mode === "all" && (
              <div className="space-y-2 text-xs">
                <div className="font-medium" style={{ color: "var(--th-text)" }}>
                  {tr("전체 브로드캐스트 피드", "Global broadcast feed")}
                </div>
                <div style={{ color: "var(--th-text-muted)" }}>
                  {tr(`에이전트 ${agents.length}명 · 작업중 ${allWorkingAgents}명`, `${agents.length} agents · ${allWorkingAgents} working`)}
                </div>
                <div style={{ color: "var(--th-text-muted)" }}>
                  {tr(`Discord 라우트 ${allAgentsWithRoute}명 · 경고 ${allWarningAgents}명`, `${allAgentsWithRoute} routed · ${allWarningAgents} warnings`)}
                </div>
              </div>
            )}

            {mode === "agent" && (
              <div className="space-y-2 text-xs">
                {selectedAgentObj ? (
                  <>
                    <div className="font-medium flex items-center gap-2" style={{ color: "var(--th-text)" }}>
                      <span>{selectedAgentObj.avatar_emoji}</span>
                      <span>{selectedAgentObj.alias || selectedAgentObj.name_ko || selectedAgentObj.name}</span>
                    </div>
                    <div style={{ color: "var(--th-text-muted)" }}>
                      {tr("상태", "Status")}: {selectedAgentObj.status}
                      {" · "}
                      {getActivitySourceLabel(selectedAgentObj, isKo)}
                    </div>
                    <div style={{ color: "var(--th-text-muted)" }}>
                      {tr("부서", "Department")}: {selectedAgentObj.department_name_ko || selectedAgentObj.department_name || tr("없음", "None")}
                    </div>
                    <div style={{ color: "var(--th-text-muted)" }}>
                      {tr("현재 작업", "Current work")}: {getAgentWorkSummary(selectedAgentObj) || tr("정보 없음", "No detail")}
                    </div>
                    <div style={{ color: "var(--th-text-muted)" }}>
                      {tr("Discord 라우트", "Discord routes")}: {selectedAgentRouteCount}
                    </div>
                    {selectedAgentTargetBinding && (
                      <div style={{ color: "var(--th-text-muted)" }}>
                        {tr("전송 채널", "Send route")}: {getBindingOptionLabel(selectedAgentTargetBinding, isKo)}
                      </div>
                    )}
                    {selectedAgentWarnings.length > 0 && (
                      <div className="flex flex-wrap gap-1 pt-1">
                        {selectedAgentWarnings.map((warning) => (
                          <span
                            key={warning.code}
                            className="inline-flex items-center gap-1 text-xs px-2 py-0.5 rounded-full"
                            style={{
                              background: warning.severity === "error" ? "rgba(248,113,113,0.14)" : "rgba(251,191,36,0.14)",
                              color: warning.severity === "error" ? "#f87171" : "#fbbf24",
                            }}
                          >
                            <AlertTriangle size={10} />
                            {isKo ? warning.ko : warning.en}
                          </span>
                        ))}
                      </div>
                    )}
                  </>
                ) : (
                  <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr("Discord 라우트가 있는 에이전트를 선택하면 상태와 현재 작업을 보여줍니다.", "Select a routed agent to inspect live status and work.")}
                  </div>
                )}
              </div>
            )}

            {mode === "department" && (
              <div className="space-y-2 text-xs">
                {selectedDepartmentObj ? (
                  <>
                    <div className="font-medium flex items-center gap-2" style={{ color: "var(--th-text)" }}>
                      <span>{selectedDepartmentObj.icon}</span>
                      <span>{selectedDepartmentObj.name_ko || selectedDepartmentObj.name}</span>
                    </div>
                    <div style={{ color: "var(--th-text-muted)" }}>
                      {tr(`소속 ${departmentAgents.length}명 · 작업중 ${workingDepartmentAgents.length}명`, `${departmentAgents.length} members · ${workingDepartmentAgents.length} working`)}
                    </div>
                    {workingDepartmentWork.length > 0 ? (
                      <div className="space-y-1">
                        {workingDepartmentWork.map((entry) => (
                          <div key={entry.id} style={{ color: "var(--th-text-muted)" }}>
                            {entry.label}: {entry.summary}
                          </div>
                        ))}
                      </div>
                    ) : (
                      <div style={{ color: "var(--th-text-muted)" }}>
                        {tr("현재 노출된 작업 요약이 없습니다.", "No visible work summary for this department.")}
                      </div>
                    )}
                  </>
                ) : (
                  <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr("부서를 선택하면 현재 가동 인원과 작업 요약을 보여줍니다.", "Select a department to inspect active members and work.")}
                  </div>
                )}
              </div>
            )}
          </div>

          <div
            className="rounded-2xl border px-3 py-3"
            style={{ borderColor: "var(--th-card-border)", background: "var(--th-bg-surface)" }}
          >
            <div className="text-xs font-semibold mb-2" style={{ color: "var(--th-text-muted)" }}>
              {tr("최근 이벤트", "Recent events")}
            </div>
            {relevantNotifications.length === 0 ? (
              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {tr("최근 이벤트가 없습니다.", "No recent runtime events.")}
              </div>
            ) : (
              <div className="space-y-2">
                {relevantNotifications.map((notification) => (
                  <div key={notification.id} className="text-xs">
                    <div style={{ color: "var(--th-text)" }}>{notification.message}</div>
                    <div style={{ color: "var(--th-text-muted)" }}>
                      {new Date(notification.ts).toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit" })}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>

          <div
            className="rounded-2xl border px-3 py-3"
            style={{ borderColor: "var(--th-card-border)", background: "var(--th-bg-surface)" }}
          >
            <div className="text-xs font-semibold mb-2" style={{ color: "var(--th-text-muted)" }}>
              {tr("최근 변경", "Recent changes")}
            </div>
            {relevantAuditLogs.length === 0 ? (
              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {tr("관련 변경 기록이 없습니다.", "No related audit entries.")}
              </div>
            ) : (
              <div className="space-y-2">
                {relevantAuditLogs.map((log) => (
                  <div key={log.id} className="text-xs">
                    <div style={{ color: "var(--th-text)" }}>{log.summary}</div>
                    <div style={{ color: "var(--th-text-muted)" }}>
                      {new Date(log.created_at).toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit" })}
                      {" · "}
                      {log.actor}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      </div>

      <div
        ref={scrollRef}
        className="flex-1 overflow-y-auto px-4 py-3 space-y-2 min-h-0"
      >
        {messages.length === 0 && (
          <div className="text-center py-16" style={{ color: "var(--th-text-muted)" }}>
            <div className="text-4xl mb-2">💬</div>
            <div className="text-sm">{tr("표시할 메시지가 없습니다", "No messages in this feed")}</div>
            <div className="text-xs mt-1">
              {tr("대상을 선택하거나 필터를 조정해보세요.", "Select a target or adjust the filters.")}
            </div>
          </div>
        )}

        {messages.map((msg) => {
          const isCeo = msg.sender_type === "ceo";
          const isSystem = msg.sender_type === "system";
          const senderAgent = !isCeo && !isSystem
            ? agents.find((agent) => agent.id === msg.sender_id)
            : null;
          const tone = getMessageTypeTone(msg.message_type);

          if (isSystem) {
            return (
              <div key={msg.id} className="text-center">
                <span
                  className="inline-block text-xs px-2 py-0.5 rounded-full"
                  style={{ background: "var(--th-bg-surface)", color: "var(--th-text-muted)" }}
                >
                  {msg.content}
                </span>
              </div>
            );
          }

          return (
            <div key={msg.id} className={`flex gap-2 ${isCeo ? "flex-row-reverse" : ""}`}>
              <div
                className="w-8 h-8 rounded-full flex items-center justify-center text-sm shrink-0"
                style={{ background: isCeo ? "#6366f1" : "var(--th-bg-surface)" }}
              >
                {isCeo ? "👑" : (senderAgent?.avatar_emoji || msg.sender_avatar || "🤖")}
              </div>

              <div className={`max-w-[78%] ${isCeo ? "text-right" : ""}`}>
                <div className="text-xs mb-1 flex flex-wrap gap-1 items-center" style={{ color: "var(--th-text-muted)" }}>
                  <span>
                    {isCeo
                      ? "CEO"
                      : (senderAgent?.alias || msg.sender_name_ko || msg.sender_name || "Agent")}
                  </span>
                  <span>
                    {new Date(msg.created_at).toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit" })}
                  </span>
                  <span
                    className="inline-flex items-center px-1.5 py-0.5 rounded-full"
                    style={{ background: tone.bg, color: tone.color }}
                  >
                    {getMessageTypeLabel(msg.message_type, isKo)}
                  </span>
                  <span
                    className="inline-flex items-center px-1.5 py-0.5 rounded-full"
                    style={{ background: "rgba(148,163,184,0.12)", color: "var(--th-text-muted)" }}
                  >
                    {getTargetLabel(msg, isKo)}
                  </span>
                </div>

                <div
                  className="px-3 py-2 rounded-2xl text-sm whitespace-pre-wrap break-words"
                  style={{
                    background: isCeo ? "#4f46e5" : "var(--th-bg-surface)",
                    color: isCeo ? "#fff" : "var(--th-text)",
                    borderRadius: isCeo ? "18px 18px 4px 18px" : "18px 18px 18px 4px",
                  }}
                >
                  {msg.content}
                </div>
              </div>
            </div>
          );
        })}
      </div>

      <div
        className="px-4 py-3 border-t shrink-0"
        style={{ borderColor: "var(--th-card-border)", background: "var(--th-surface)" }}
      >
        <div className="flex flex-wrap gap-2 items-center mb-2">
          <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {tr("보내는 유형", "Send as")}
          </span>
          <select
            value={composeType}
            onChange={(event) => setComposeType(event.target.value as MessageType)}
            className="px-2 py-1 rounded-lg text-xs bg-transparent border"
            style={{ borderColor: "var(--th-input-border)", color: "var(--th-text)" }}
          >
            {COMPOSE_TYPES.map((item) => (
              <option key={item.value} value={item.value}>
                {isKo ? item.ko : item.en}
              </option>
            ))}
          </select>
        </div>

        <div className="flex gap-2 items-end">
          <textarea
            value={input}
            onChange={(event) => setInput(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                handleSend();
              }
            }}
            placeholder={placeholder}
            className="flex-1 px-3 py-2 rounded-xl text-base resize-none bg-transparent border"
            style={{
              borderColor: "var(--th-input-border)",
              color: "var(--th-text)",
              maxHeight: "120px",
              minHeight: "40px",
            }}
            rows={1}
            disabled={inputDisabled}
          />
          <button
            onClick={handleSend}
            disabled={!input.trim() || inputDisabled}
            className="p-2.5 rounded-xl bg-indigo-600 text-white disabled:opacity-40 transition-opacity hover:bg-indigo-500"
          >
            <Send size={16} />
          </button>
        </div>
      </div>
    </div>
  );
}
