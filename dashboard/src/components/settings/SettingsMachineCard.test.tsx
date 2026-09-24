import { renderToStaticMarkup } from "react-dom/server";
import { expect, it, vi } from "vitest";
import type { ClusterNode } from "../../api/clusterNodes";
import { SettingsMachineCard } from "./SettingsMachineCard";

vi.mock("./useMachineResourceHistory", () => ({
  useMachineResourceHistory: () => ({ data: [] }),
}));

const now = Date.parse("2026-09-23T00:00:30Z");
const node: ClusterNode = {
  instance_id: "runner-example", hostname: "Build runner", effective_role: "worker", role: "runner", status: "online",
  active_session_count: 0, execution_active: 1, execution_occupied: 1,
  capabilities: {
    execution_capacity: { version: 1, slots: 2 },
    execution_readiness: { os: "linux", arch: "aarch64", runtime_profile: "runner", observed_at_ms: now, expires_at_ms: now + 60_000, backends: ["process"],
      providers: { codex: { cli_installed: true, cli_usable: true }, antigravity: { cli_installed: false, cli_usable: false } } },
  },
  execution_readiness: { providers: { codex: { eligible: true, reasons: [] } } },
};
const render = (value = node, stale = false, sessionCountsUnavailable = false) => renderToStaticMarkup(
  <SettingsMachineCard node={value} now={now} stale={stale} sessionCountsUnavailable={sessionCountsUnavailable} tr={(_ko, en) => en} />,
);

it("shows platform, canonical role and capacity from the selected device", () => {
  const html = render();
  expect(html).toContain("Runner");
  expect(html).not.toContain(">worker<");
  expect(html).toContain("Linux / aarch64");
  expect(html).toContain("Ready for new work");
});

it("shows installed CLIs even without credentials and hides uninstalled providers and removed copy", () => {
  const html = render({ ...node, execution_readiness: { providers: {
    codex: { eligible: false, reasons: ["provider_credentials_missing"] },
    antigravity: { eligible: false, reasons: ["provider_cli_unavailable"] },
  } } });
  expect(html).toContain("codex");
  expect(html).toContain("Local credentials missing");
  expect(html).not.toContain("antigravity");
  expect(html).not.toContain("Configured role");
  expect(html).not.toContain("The address is advertised");
  expect(html).not.toContain("remaining quota");
});

it("never offers healthy execution readiness on offline, expired, stale, or full nodes", () => {
  for (const html of [
    render({ ...node, status: "offline" }), render(node, true),
    render({ ...node, capabilities: { ...node.capabilities, execution_readiness: { ...node.capabilities.execution_readiness!, expires_at_ms: now } } }),
    render({ ...node, execution_occupied: 2 }),
  ]) expect(html).not.toContain("Ready for new work");
  expect(render({ ...node, execution_occupied: 2 })).toContain("Waiting for capacity");
});

it("keeps missing capacity and failed session summaries unknown", () => {
  expect(render({ ...node, execution_occupied: null })).toContain("Capacity unknown");
  expect(render(node, false, true)).toMatch(/Active sessions<\/dt><dd[^>]*>Unknown<\/dd>/);
});
