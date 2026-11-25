import { useEffect, useId, useRef, useState } from "react";
import { getAgentExecutionNode, setAgentExecutionNode } from "../../api/agentExecutionNode";
import { getClusterNodes, type ClusterNode } from "../../api/clusterNodes";
import { Button } from "../../kit/free/button-clean";
import { KitCard, StatusDot } from "../common/KitPrimitives";
import type { Translator } from "./types";
import { nodePlatformLabel, nodeRoleLabel } from "../../lib/nodeLabels";

export function AgentExecutionNodeSelect({ agentId, provider, tr, onSaved }: {
  agentId: string; provider: string; tr: Translator; onSaved?: () => void;
}) {
  const id = useId();
  const saveGeneration = useRef(0);
  const [nodes, setNodes] = useState<ClusterNode[]>([]);
  const [selected, setSelected] = useState("");
  const [saved, setSaved] = useState("");
  const [enforced, setEnforced] = useState(false);
  const [loading, setLoading] = useState(true);
  const [ready, setReady] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    const controller = new AbortController();
    ++saveGeneration.current;
    setSaving(false);
    setLoading(true);
    setReady(false);
    setError(null);
    setSuccess(false);
    void Promise.all([getAgentExecutionNode(agentId, controller.signal), getClusterNodes(controller.signal)])
      .then(([policy, cluster]) => {
        if (controller.signal.aborted) return;
        setSelected(policy.default_node_id ?? "");
        setSaved(policy.default_node_id ?? "");
        setEnforced(policy.routing_enforced);
        setNodes(cluster.nodes);
        setReady(true);
      }).catch((failure: unknown) => {
        if (!controller.signal.aborted) setError(failure instanceof Error ? failure.message : String(failure));
      }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => { controller.abort(); ++saveGeneration.current; };
  }, [agentId, reload]);

  const save = async () => {
    const generation = ++saveGeneration.current;
    setSaving(true);
    setError(null);
    setSuccess(false);
    try {
      const policy = await setAgentExecutionNode(agentId, selected || null);
      if (generation !== saveGeneration.current) return;
      setSaved(policy.default_node_id ?? "");
      setSelected(policy.default_node_id ?? "");
      setSuccess(true);
      onSaved?.();
    } catch (failure) {
      if (generation !== saveGeneration.current) return;
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally { if (generation === saveGeneration.current) setSaving(false); }
  };
  const node = nodes.find((entry) => entry.instance_id === selected);
  const readiness = node?.execution_readiness?.providers[provider];
  const unavailable = selected !== "" && (!node || node.status !== "online" || !readiness?.eligible);

  return (
    <KitCard pad="md" className="min-w-0 md:col-span-2">
      <div className="min-w-0 space-y-3">
        <div className="min-w-0">
          <h4 className="m-0 text-sm font-semibold leading-5 text-[var(--fg)]">
            {tr("우선 실행 장비", "Preferred execution device")}
          </h4>
          <p className="m-0 mt-1 text-xs leading-5 text-[var(--fg-muted)] [overflow-wrap:anywhere]">
            {tr("Discord에서 이 에이전트의 새 세션을 우선 시작할 장비를 선택합니다.", "Choose the preferred device for this agent's new Discord sessions.")}
          </p>
        </div>
        <div>
          <label htmlFor={id} className="mb-1.5 block text-xs font-medium text-[var(--fg-dim)]">
            {tr("우선 실행 장비", "Preferred execution device")}
          </label>
          <div className="flex flex-wrap items-center gap-2">
            <select
              id={id}
              value={selected}
              disabled={!ready || loading || saving}
              className="min-h-11 w-full min-w-0 rounded-[var(--r-sm)] border border-[var(--line)] bg-[var(--bg-1)] px-2.5 py-2 text-sm text-[var(--fg)] sm:w-auto sm:flex-1"
              onChange={(event) => { setSelected(event.target.value); setSuccess(false); }}
            >
              <option value="">{tr("기본 정책 사용", "Use default policy")}</option>
              {selected && !node && <option value={selected}>{selected} — {tr("등록 정보 없음", "Not registered")}</option>}
              {nodes.map((entry) => (
                <option key={entry.instance_id} value={entry.instance_id} disabled={!enforced}>
                  {entry.hostname || entry.instance_id} · {nodeRoleLabel(entry.effective_role, tr)} · {nodePlatformLabel(entry.capabilities.execution_readiness?.os, tr)}
                  {entry.status !== "online" ? ` · ${tr("오프라인", "Offline")}` : ""}
                </option>
              ))}
            </select>
            <Button
              variant="primary"
              size="lg"
              className="min-h-11! min-w-11!"
              disabled={!ready || loading || saving || selected === saved || (!enforced && selected !== "")}
              onClick={() => void save()}
            >
              {saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}
            </Button>
            <Button
              variant="secondary"
              size="lg"
              className="min-h-11! min-w-11!"
              disabled={loading || saving}
              onClick={() => setReload((value) => value + 1)}
            >
              {tr("새로고침", "Refresh")}
            </Button>
          </div>
          <p className="m-0 mt-2 text-xs leading-5 text-[var(--fg-muted)]">
            {tr("기존 세션은 현재 장비를 유지합니다. 새 세션은 선택한 장비가 준비되지 않았거나 실행 여유가 없으면 조건을 충족하는 다른 장비에 배정합니다. 가능한 장비가 없으면 사유를 표시합니다. 채널의 /node 지정과 필수 실행 조건은 계속 적용됩니다.",
              "Existing sessions keep their current device. New sessions use another compatible device if the preferred one is not ready or has no capacity. If no device qualifies, a reason is shown. Channel /node selections and required execution conditions still apply.")}
          </p>
        </div>
        {!loading && !enforced && !error && (
          <div role="status" className="flex min-w-0 items-start gap-2.5 rounded-[var(--r-md)] border border-[color-mix(in_oklch,var(--warn)_40%,transparent)] bg-[var(--warn-soft)] px-3 py-2 text-xs leading-5 text-[var(--fg)]">
            <StatusDot tone="warn" className="mt-1.5" />
            <span className="min-w-0 flex-1">
              {tr("현재 서버는 노드 배정을 적용하지 않습니다. 운영 설정에서 배정을 활성화한 뒤 장비를 선택할 수 있습니다.", "Node placement is not enforced on this server. Enable placement before selecting a device.")}
            </span>
          </div>
        )}
        {!loading && unavailable && (
          <div role="status" className="flex min-w-0 items-start gap-2.5 rounded-[var(--r-md)] border border-[color-mix(in_oklch,var(--warn)_40%,transparent)] bg-[var(--warn-soft)] px-3 py-2 text-xs leading-5 text-[var(--fg)]">
            <StatusDot tone="warn" className="mt-1.5" />
            <span className="min-w-0 flex-1">
              {tr("선택한 장비의 실행 준비를 확인할 수 없습니다. 해당 장비의 연결, provider 로그인 및 작업 경로를 확인하세요.", "The selected device is not ready. Check its connection, provider sign-in, and workspace.")}
            </span>
          </div>
        )}
        {error && (
          <div role="alert" className="flex min-w-0 items-start gap-2.5 rounded-[var(--r-md)] border border-[color-mix(in_oklch,var(--err)_40%,transparent)] bg-[var(--err-soft)] px-3 py-2 text-xs leading-5 text-[var(--fg)]">
            <StatusDot tone="err" className="mt-1.5" />
            <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">{error}</span>
          </div>
        )}
        {success && <p role="status" className="m-0 text-xs text-[var(--ok)]">{tr("저장했습니다. 새 세션부터 적용됩니다.", "Saved. Applies to new sessions.")}</p>}
      </div>
    </KitCard>
  );
}
