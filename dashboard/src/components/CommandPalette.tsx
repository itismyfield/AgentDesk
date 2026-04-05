import { useState, useEffect, useRef, useMemo } from "react";
import { Search } from "lucide-react";
import type { Agent, Department } from "../types";
import { PALETTE_ROUTES } from "../app/routes";

interface CommandPaletteProps {
  agents: Agent[];
  departments: Department[];
  isKo: boolean;
  onSelectAgent: (agent: Agent) => void;
  onNavigate: (view: string) => void;
  onClose: () => void;
}

export default function CommandPalette({
  agents,
  departments,
  isKo,
  onSelectAgent,
  onNavigate,
  onClose,
}: CommandPaletteProps) {
  const [query, setQuery] = useState("");
  const [selectedIndex, setSelectedIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const tr = (ko: string, en: string) => (isKo ? ko : en);

  type ResultItem = { type: "agent"; agent: Agent } | { type: "nav"; id: string; label: string; icon: string } | { type: "dept"; dept: Department };

  const results = useMemo(() => {
    const items: ResultItem[] = [];
    const q = query.toLowerCase().trim();

    // Navigation items from route registry
    const navs = PALETTE_ROUTES.map((r) => ({
      id: r.id,
      label: tr(r.labelKo, r.labelEn),
      icon: r.icon,
    }));

    if (!q) {
      items.push(...navs.map((n) => ({ type: "nav" as const, ...n })));
      items.push(...agents.slice(0, 8).map((a) => ({ type: "agent" as const, agent: a })));
      return items;
    }

    // Filter navs
    for (const n of navs) {
      if (n.label.toLowerCase().includes(q) || n.id.includes(q)) {
        items.push({ type: "nav", ...n });
      }
    }

    // Filter agents
    for (const a of agents) {
      if (
        a.name.toLowerCase().includes(q) ||
        a.name_ko.toLowerCase().includes(q) ||
        (a.alias && a.alias.toLowerCase().includes(q)) ||
        a.avatar_emoji.includes(q)
      ) {
        items.push({ type: "agent", agent: a });
      }
    }

    // Filter departments
    for (const d of departments) {
      if (d.name.toLowerCase().includes(q) || d.name_ko.toLowerCase().includes(q)) {
        items.push({ type: "dept", dept: d });
      }
    }

    return items.slice(0, 12);
  }, [query, agents, departments, isKo]);

  useEffect(() => {
    setSelectedIndex(0);
  }, [query]);

  const handleSelect = (item: ResultItem) => {
    if (item.type === "nav") {
      onNavigate(item.id);
    } else if (item.type === "agent") {
      onSelectAgent(item.agent);
    } else if (item.type === "dept") {
      onNavigate("agents");
    }
    onClose();
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setSelectedIndex((i) => Math.min(i + 1, results.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSelectedIndex((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (results[selectedIndex]) handleSelect(results[selectedIndex]);
    } else if (e.key === "Escape") {
      onClose();
    }
  };

  return (
    <div
      className="fixed inset-0 z-[100] flex items-start justify-center pt-[15vh]"
      onClick={onClose}
    >
      <div className="fixed inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        role="dialog"
        aria-modal="true"
        aria-label={tr("명령 팔레트", "Command Palette")}
        className="relative w-full max-w-lg mx-4 rounded-2xl overflow-hidden shadow-2xl"
        style={{ background: "var(--th-surface)", border: "1px solid var(--th-border)" }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* Search input */}
        <div className="flex items-center gap-3 px-4 py-3 border-b" style={{ borderColor: "var(--th-border)" }}>
          <Search size={18} style={{ color: "var(--th-text-muted)" }} />
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={tr("검색... (에이전트, 메뉴, 부서)", "Search... (agents, menu, departments)")}
            className="flex-1 bg-transparent text-sm outline-none"
            style={{ color: "var(--th-text)" }}
          />
          <kbd className="text-xs px-1.5 py-0.5 rounded" style={{ background: "var(--th-bg-surface)", color: "var(--th-text-muted)" }}>
            ESC
          </kbd>
        </div>

        {/* Results */}
        <div className="max-h-64 overflow-y-auto py-2">
          {results.map((item, i) => (
            <button
              key={item.type === "agent" ? item.agent.id : item.type === "nav" ? item.id : item.dept.id}
              onClick={() => handleSelect(item)}
              className={`w-full flex items-center gap-3 px-4 py-2 text-left text-sm transition-colors ${
                i === selectedIndex ? "bg-indigo-600/20" : "hover:bg-surface-subtle"
              }`}
              style={{ color: "var(--th-text)" }}
            >
              <span className="text-base w-6 text-center">
                {item.type === "agent" ? item.agent.avatar_emoji
                  : item.type === "nav" ? item.icon
                  : item.dept.icon}
              </span>
              <div className="flex-1 min-w-0">
                <div className="truncate">
                  {item.type === "agent"
                    ? (item.agent.alias || item.agent.name_ko || item.agent.name)
                    : item.type === "nav"
                    ? item.label
                    : (item.dept.name_ko || item.dept.name)}
                </div>
                {item.type === "agent" && (
                  <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {item.agent.department_name_ko || ""} · {item.agent.status}
                  </div>
                )}
              </div>
              <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {item.type === "agent" ? tr("에이전트", "Agent")
                  : item.type === "nav" ? tr("이동", "Go")
                  : tr("부서", "Dept")}
              </span>
            </button>
          ))}
          {results.length === 0 && (
            <div className="px-4 py-6 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
              {tr("결과 없음", "No results")}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
