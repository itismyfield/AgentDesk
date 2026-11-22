import { useCallback, useMemo } from "react";
import { useLocation, useNavigate, useSearchParams, type NavigateOptions } from "react-router";
import type { AuditProvider } from "../../api/promptAudit";
import type { Agent } from "../../types";
import { auditAgents, auditProviders, defaultAuditProvider } from "./auditModel";

export type AuditTab = "prompts" | "anchors";
export type AuditSelection = ReturnType<typeof useAuditSelection>;

/** The audit URL keys: `tab`, and for both tabs `agent` (on phones it opens the detail) and `provider` (left out when default). */
export function useAuditSelection(agents: Agent[], phone: boolean) {
  const [params, setParams] = useSearchParams();
  const location = useLocation();
  const navigate = useNavigate();
  const tab: AuditTab = params.get("tab") === "anchors" ? "anchors" : "prompts";
  const list = useMemo(() => auditAgents(agents), [agents]);
  const requested = list.find((agent) => agent.id === params.get("agent"));
  // Desktop always shows a detail, so it falls back to the first agent without touching the URL.
  const agent = requested ?? (phone ? undefined : list[0]);
  const providers = agent ? auditProviders(agent) : [];
  const fallback = agent ? defaultAuditProvider(agent) : null;
  const provider = providers.find((item) => item === params.get("provider")) ?? fallback;

  const update = useCallback((patch: Record<string, string | null>, options?: NavigateOptions) => {
    setParams((current) => {
      const next = new URLSearchParams(current);
      for (const [key, value] of Object.entries(patch)) {
        if (value) next.set(key, value);
        else next.delete(key);
      }
      return next;
    }, options);
  }, [setParams]);

  // Tab and provider swaps keep the entry's state, so back still pops to the list afterwards.
  const swap = { replace: true, state: location.state };
  const selectTab = (next: AuditTab) => update({ tab: next === "prompts" ? null : next }, swap);
  const selectProvider = (next: AuditProvider) => update({ agent: agent?.id ?? null, provider: next === fallback ? null : next }, swap);
  const selectAgent = (id: string) => {
    if (id !== requested?.id) update({ agent: id, provider: null }, { state: { auditPicked: true } });
  };
  // Back pops the entry the list pushed; a link opened straight on a detail has nothing to pop, so it swaps in the list.
  const back = () => {
    if ((location.state as { auditPicked?: boolean } | null)?.auditPicked) navigate(-1);
    else update({ agent: null, provider: null }, { replace: true });
  };

  return { tab, list, agent, providers, provider, selectTab, selectAgent, selectProvider, back };
}
