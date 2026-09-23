import { describe, expect, it } from "vitest";
import type { ClusterNode } from "../../api/clusterNodes";
import { isSettingsPanel, SETTING_GROUPS } from "./SettingsModel";
import { machineApiOrigin, machineConnection, machineOnline, machineRole } from "./SettingsMachineModel";

const tr = (_ko: string, en: string) => en;
const now = Date.parse("2026-09-23T00:00:30Z");
const node: ClusterNode = {
  instance_id: "runner-example", status: "online", capabilities: {},
  last_heartbeat_at: new Date(now).toISOString(),
  forwarding_diagnostics: {
    advertised: true, configured: true, trust_validated: true,
    reachability_verified: true, expires_at_ms: now + 60_000,
  },
};
const connection = (value: ClusterNode, stale = false) => machineConnection(value, "hub-example", stale, now, 30, tr);

describe("machine connectivity", () => {
  it("does not confuse heartbeat presence with an authenticated connection", () => {
    expect(connection(node).label).toBe("Connection verified");
    expect(connection({ ...node, forwarding_diagnostics: { ...node.forwarding_diagnostics!, configured: false } }).label).toBe("Connection setup needed");
    expect(connection({ ...node, forwarding_diagnostics: { ...node.forwarding_diagnostics!, trust_validated: false } }).label).toBe("Connection unverified");
  });

  it("expires forwarding proof even when the last response said verified", () => {
    expect(connection({ ...node, forwarding_diagnostics: { ...node.forwarding_diagnostics!, expires_at_ms: now } }).label).toBe("Connection unverified");
  });

  it("does not present a failed or stale snapshot as connected", () => {
    expect(connection(node, true).label).toBe("Snapshot stale");
    expect(connection({ ...node, instance_id: "hub-example" }, true).label).toBe("Snapshot stale");
  });

  it("expires heartbeat status at the server-provided lease boundary", () => {
    const expired = { ...node, last_heartbeat_at: new Date(now - 30_000).toISOString() };
    expect(machineOnline(expired, now, 30)).toBe(false);
    expect(connection(expired).label).toBe("Offline");
    expect(machineOnline(expired, now, 60)).toBe(true);
  });

  it("uses server status when older responses have no heartbeat timestamp", () => {
    expect(machineOnline({ ...node, last_heartbeat_at: undefined }, now, 30)).toBe(true);
    expect(connection({ ...node, status: "offline" }).label).toBe("Offline");
    expect(connection({ ...node, status: undefined }).label).toBe("Status unknown");
  });

  it("identifies the serving node without requiring a remote forwarding route", () => {
    expect(connection({ ...node, instance_id: "hub-example", forwarding_diagnostics: null }).label).toBe("Current server");
  });
});

describe("machine presentation", () => {
  it("places the deep-linked machine panel immediately after general", () => {
    expect(isSettingsPanel("machine")).toBe(true);
    const general = SETTING_GROUPS.findIndex(group => group.id === "general");
    expect(SETTING_GROUPS[general + 1].id).toBe("machine");
  });

  it("shows canonical roles for both current and legacy registry values", () => {
    expect(["hub", "leader", "runner", "worker", "auto"].map(machineRole)).toEqual(["Hub", "Hub", "Runner", "Runner", null]);
  });

  it("only displays HTTP origins without credentials, query data, or paths", () => {
    expect(machineApiOrigin("https://user:example@runner.example.invalid:9443/path?token=example#secret")).toBe("https://runner.example.invalid:9443");
    expect(machineApiOrigin("javascript:alert(1)")).toBeNull();
    expect(machineApiOrigin("not a URL")).toBeNull();
    expect(machineApiOrigin(undefined)).toBeNull();
  });
});
