import { renderToStaticMarkup } from "react-dom/server";
import { expect, it } from "vitest";
import { MachineSparkline, recentResources } from "./MachineSparkline";
import type { MachineResources } from "../../api/machineResources";

it("keeps missing network readings as gaps instead of inventing a connected trend", () => {
  const html = renderToStaticMarkup(<MachineSparkline color="network" label="Ethernet" stale={false}
    tr={(_ko, en) => en} now={5} values={[
      { at: 1, value: 10 }, { at: 2, value: 20 }, { at: 3, value: null },
      { at: 4, value: 30 }, { at: 5, value: 40 },
    ]} />);
  expect((html.match(/data-layer="line"/g) ?? []).length).toBe(2);
  expect((html.match(/data-layer="area"/g) ?? []).length).toBe(2);
});

it("leaves a gap after expired samples and preserves a fixed fifteen-minute time axis", () => {
  const html = renderToStaticMarkup(<MachineSparkline color="cpu" label="CPU" stale={false}
    tr={(_ko, en) => en} now={900_000} values={[
      { at: 0, expiresAt: 30_000, value: 10 }, { at: 10_000, expiresAt: 40_000, value: 20 },
      { at: 890_000, expiresAt: 920_000, value: 30 }, { at: 900_000, expiresAt: 930_000, value: 40 },
    ]} />);
  expect((html.match(/data-layer="line"/g) ?? []).length).toBe(2);
  expect((html.match(/data-layer="area"/g) ?? []).length).toBe(2);
  expect(html).toContain("M98.89,");
  expect(html).toContain("L1.11,");
  expect(html).toContain("L1.11,32 L0.00,32 Z");
  expect(html).toContain("L100.00,32 L98.89,32 Z");
});

it("deduplicates live and saved samples and excludes expired timeline windows", () => {
  const sample = { observed_at_ms: 1_000_000 } as MachineResources;
  const old = { observed_at_ms: 1_000_000 - 16 * 60_000 } as MachineResources;
  const newer = { observed_at_ms: 1_000_010 } as MachineResources;
  expect(recentResources([old, sample], sample, 1_000_000)).toEqual([sample]);
  expect(recentResources([sample], newer, 1_000_000)).toEqual([sample, newer]);
});

it("retains the full fifteen minutes at the five-second sampling cadence", () => {
  const now = 1_000_000;
  const samples = Array.from({ length: 181 }, (_, index) => ({
    observed_at_ms: now - 900_000 + index * 5_000,
  }) as MachineResources);
  expect(recentResources(samples, samples.at(-1), now)).toEqual(samples);
});
