import { renderToStaticMarkup } from "react-dom/server";
import { expect, it } from "vitest";
import { machineResourcesSchema, type MachineResources } from "../../api/machineResources";
import { SettingsMachineResources, resourceBytes } from "./SettingsMachineResources";

const now = 1_000_000;
const gib = 1024 ** 3;
const resources: MachineResources = {
  schema: 1, observed_at_ms: now, expires_at_ms: now + 30_000, sample_interval_ms: 5_000,
  cpu: { model: "Example CPU", physical_cores: 8, logical_cores: 16, usage_percent: 42.5 },
  memory: { total_bytes: 32 * gib, used_bytes: 16 * gib, available_bytes: 16 * gib },
  disks: [{ name: "Data", mount_point: "/data", kind: "SSD", total_bytes: 1024 * gib, used_bytes: 512 * gib, available_bytes: 512 * gib }],
  gpus: [{ name: "Example GPU", usage_percent: 75, memory_used_bytes: 8 * gib, memory_total_bytes: 16 * gib, shared_memory: false }],
};
const render = (data = resources, stale = false) => renderToStaticMarkup(
  <SettingsMachineResources resources={data} now={now} stale={stale} tr={(_ko, en) => en} />,
);

it("shows CPU, GPU, memory and disk capacity with measured utilization", () => {
  const html = render();
  for (const text of ["Example CPU", "8 cores", "16 logical processors", "42.5%", "Example GPU", "75.0%", "16 GiB / 32 GiB", "8 GiB / 16 GiB", "512 GiB / 1 TiB"])
    expect(html).toContain(text);
});

it("does not label expired or disconnected measurements as live utilization", () => {
  for (const html of [render(resources, true), render({ ...resources, expires_at_ms: now }), render({ ...resources, observed_at_ms: now + 60_000 })]) {
    expect(html).toContain("Sample expired");
    expect(html).not.toContain("42.5%");
    expect(html).not.toContain("75.0%");
    expect(html).toContain("Example CPU");
  }
});

it("keeps missing GPU utilization unknown and labels shared memory without VRAM capacity", () => {
  const html = render({ ...resources, gpus: [{ name: "Integrated GPU", usage_percent: null, memory_used_bytes: gib, memory_total_bytes: null, shared_memory: true }] });
  expect(html).toContain("Shared memory used");
  expect(html).not.toContain("VRAM");
  const gpuMeter = html.match(/<div[^>]*aria-label="GPU utilization"[^>]*>/)?.[0];
  expect(gpuMeter).toBeDefined();
  expect(gpuMeter).not.toContain('role="meter"');
  expect(gpuMeter).not.toContain("aria-valuenow");
});

it("rejects invalid telemetry without treating it as zero and formats bytes consistently", () => {
  expect(machineResourcesSchema.safeParse({ ...resources, cpu: { ...resources.cpu, usage_percent: -1 } }).success).toBe(false);
  expect(resourceBytes(null)).toBe("—");
  expect(resourceBytes(Number.NaN)).toBe("—");
  expect(resourceBytes(16 * gib)).toBe("16 GiB");
});
