import { useState, useEffect, useRef, useMemo } from "react";
import { Command } from "cmdk";
import { Search } from "lucide-react";
import type { Agent, Department } from "../types";
import { SurfaceEmptyState } from "./common/SurfacePrimitives";
import AgentAvatar from "./AgentAvatar";

interface PaletteRoute {
  id: string;
  labelKo: string;
  labelEn: string;
  icon: string;
}

interface CommandPaletteProps {
  agents: Agent[];
  departments: Department[];
  isKo: boolean;
  onSelectAgent: (agent: Agent) => void;
  onNavigate: (view: string) => void;
  onClose: () => void;
  routes: PaletteRoute[];
  departmentRouteId?: string;
}

export default function CommandPalette({
  agents,
  departments,
  isKo,
  onSelectAgent,
  onNavigate,
  onClose,
  routes,
  departmentRouteId = "settings_organization",
}: CommandPaletteProps) {
  const [query, setQuery] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const tr = (ko: string, en: string) => (isKo ? ko : en);

  type ResultItem = { type: "agent"; agent: Agent } | { type: "nav"; id: string; label: string; icon: string } | { type: "dept"; dept: Department };

  const navItems = useMemo(
    () =>
      routes.map((route) => ({
      id: route.id,
      label: tr(route.labelKo, route.labelEn),
      icon: route.icon,
    })),
    [isKo, routes],
  );

  const handleSelect = (item: ResultItem) => {
    if (item.type === "nav") {
      onNavigate(item.id);
    } else if (item.type === "agent") {
      onSelectAgent(item.agent);
    } else if (item.type === "dept") {
      onNavigate(departmentRouteId);
    }
    onClose();
  };

  return (
    <div
      className="fixed inset-0 z-[100] flex items-start justify-center pt-[15vh]"
      onClick={onClose}
    >
      <div className="fixed inset-0" style={{ background: "var(--th-modal-overlay)" }} />
      <Command
        loop
        role="dialog"
        aria-modal="true"
        aria-label={tr("명령 팔레트", "Command Palette")}
        label={tr("명령 팔레트", "Command Palette")}
        className="relative w-full max-w-lg mx-4 overflow-hidden rounded-[28px] border shadow-2xl"
        style={{
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
        }}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.preventDefault();
            onClose();
          }
        }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* Search input */}
        <div className="flex items-center gap-3 px-4 py-3 border-b" style={{ borderColor: "var(--th-border)" }}>
          <Search size={18} style={{ color: "var(--th-text-muted)" }} />
          <Command.Input
            ref={inputRef}
            value={query}
            onValueChange={setQuery}
            placeholder={tr("검색... (에이전트, 메뉴, 부서)", "Search... (agents, menu, departments)")}
            className="flex-1 bg-transparent text-sm outline-none"
            style={{ color: "var(--th-text)" }}
          />
          <kbd className="text-xs px-1.5 py-0.5 rounded" style={{ background: "var(--th-bg-surface)", color: "var(--th-text-muted)" }}>
            ESC
          </kbd>
        </div>

        {/* Results */}
        <Command.List
          className="max-h-64 overflow-y-auto py-2"
          label={tr("검색 결과", "Search results")}
        >
          <Command.Empty>
            <SurfaceEmptyState className="mx-2 px-4 py-6 text-center text-sm">
              {tr("결과 없음", "No results")}
            </SurfaceEmptyState>
          </Command.Empty>

          <Command.Group heading={tr("이동", "Navigate")}>
            {navItems.map((item) => (
              <Command.Item
                key={item.id}
                value={`nav:${item.id}:${item.label}`}
                keywords={[item.id, item.label]}
                className="command-palette-item"
                onSelect={() => handleSelect({ type: "nav", ...item })}
              >
                <span className="flex w-6 items-center justify-center text-base text-center">
                  {item.icon}
                </span>
                <div className="min-w-0 flex-1">
                  <div className="truncate">{item.label}</div>
                </div>
                <span className="text-xs command-palette-item-kind">
                  {tr("이동", "Go")}
                </span>
              </Command.Item>
            ))}
          </Command.Group>

          <Command.Group heading={tr("에이전트", "Agents")}>
            {agents.map((agent) => (
              <Command.Item
                key={agent.id}
                value={`agent:${agent.id}:${agent.alias ?? ""}:${agent.name_ko}:${agent.name}`}
                keywords={[
                  agent.name,
                  agent.name_ko,
                  agent.alias ?? "",
                  agent.avatar_emoji,
                  agent.department_name_ko ?? "",
                  agent.status,
                ]}
                className="command-palette-item"
                onSelect={() => handleSelect({ type: "agent", agent })}
              >
                <span className="flex w-6 items-center justify-center text-base text-center">
                  <AgentAvatar agent={agent} agents={agents} size={22} />
                </span>
                <div className="min-w-0 flex-1">
                  <div className="truncate">
                    {agent.alias || agent.name_ko || agent.name}
                  </div>
                  <div className="text-xs command-palette-item-meta">
                    {agent.department_name_ko || ""} · {agent.status}
                  </div>
                </div>
                <span className="text-xs command-palette-item-kind">
                  {tr("에이전트", "Agent")}
                </span>
              </Command.Item>
            ))}
          </Command.Group>

          <Command.Group heading={tr("부서", "Departments")}>
            {departments.map((dept) => (
              <Command.Item
                key={dept.id}
                value={`dept:${dept.id}:${dept.name_ko}:${dept.name}`}
                keywords={[dept.name, dept.name_ko, dept.icon]}
                className="command-palette-item"
                onSelect={() => handleSelect({ type: "dept", dept })}
              >
                <span className="flex w-6 items-center justify-center text-base text-center">
                  {dept.icon}
                </span>
                <div className="min-w-0 flex-1">
                  <div className="truncate">{dept.name_ko || dept.name}</div>
                </div>
                <span className="text-xs command-palette-item-kind">
                  {tr("부서", "Dept")}
                </span>
              </Command.Item>
            ))}
          </Command.Group>
        </Command.List>
      </Command>
    </div>
  );
}
