import { useEffect, useId, useRef, useState } from "react";
import { getAgentExecutionNode, setAgentExecutionNode } from "../../api/agentExecutionNode";
import { getClusterNodes, type ClusterNode } from "../../api/clusterNodes";
import { SurfaceActionButton, SurfaceNotice, SurfaceSubsection } from "../common/SurfacePrimitives";
import type { Translator } from "./types";

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

  return <SurfaceSubsection title={tr("우선 실행 장비", "Preferred execution device")}
    description={tr("이 에이전트의 새 세션을 우선 시작할 장비를 선택합니다.", "Choose the preferred device for this agent's new sessions.")}
    className="md:col-span-2">
    <label htmlFor={id} className="mb-1 block text-xs">{tr("우선 실행 장비", "Preferred execution device")}</label>
    <div className="flex flex-wrap items-center gap-2">
      <select id={id} value={selected} disabled={!ready || loading || saving}
        className="w-full min-w-0 rounded border px-2 py-2 text-sm sm:w-auto sm:flex-1"
        style={{ background: "var(--th-bg-surface)", borderColor: "var(--th-border)", color: "var(--th-text-primary)" }}
        onChange={(event) => { setSelected(event.target.value); setSuccess(false); }}>
        <option value="">{tr("기본 배정 정책 사용", "Use default placement policy")}</option>
        {selected && !node && <option value={selected}>{selected} — {tr("등록 정보 없음", "Not registered")}</option>}
        {nodes.map((entry) => <option key={entry.instance_id} value={entry.instance_id} disabled={!enforced}>
          {entry.hostname || entry.instance_id} · {entry.effective_role || "unknown"} · {entry.capabilities.execution_readiness?.os || "unknown"}
          {entry.status !== "online" ? ` · ${tr("오프라인", "Offline")}` : ""}
        </option>)}
      </select>
      <SurfaceActionButton disabled={!ready || loading || saving || selected === saved || (!enforced && selected !== "")}
        onClick={() => void save()}>{saving ? tr("저장 중…", "Saving…") : tr("저장", "Save")}</SurfaceActionButton>
      <SurfaceActionButton disabled={loading || saving} onClick={() => setReload((value) => value + 1)}>
        {tr("새로고침", "Refresh")}
      </SurfaceActionButton>
    </div>
    <p className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
      {tr("기존 세션은 현재 장비를 유지합니다. 새 세션은 선택한 장비가 준비되지 않았거나 실행 여유가 없으면 조건을 충족하는 다른 장비에 배정합니다. 가능한 장비가 없으면 사유를 표시합니다. 채널의 /node 지정과 필수 실행 조건은 계속 적용됩니다.",
        "Existing sessions keep their current device. New sessions use another compatible device if the preferred one is not ready or has no capacity. If no device qualifies, a reason is shown. Channel /node selections and required execution conditions still apply.")}
    </p>
    {!loading && !enforced && !error && <SurfaceNotice tone="warn" compact className="mt-2">
      {tr("현재 서버는 노드 배정을 적용하지 않습니다. 운영 설정에서 배정을 활성화한 뒤 장비를 선택할 수 있습니다.", "Node placement is not enforced on this server. Enable placement before selecting a device.")}
    </SurfaceNotice>}
    {!loading && unavailable && <SurfaceNotice tone="warn" compact className="mt-2">
      {tr("선택한 장비의 실행 준비를 확인할 수 없습니다. 해당 장비의 연결, provider 로그인 및 작업 경로를 확인하세요.", "The selected device is not ready. Check its connection, provider sign-in, and workspace.")}
    </SurfaceNotice>}
    {error && <SurfaceNotice tone="danger" compact className="mt-2">{error}</SurfaceNotice>}
    {success && <p role="status" className="mt-2 text-xs">{tr("저장했습니다. 새 세션부터 적용됩니다.", "Saved. Applies to new sessions.")}</p>}
  </SurfaceSubsection>;
}
