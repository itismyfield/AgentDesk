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
  expect((html.match(/<path /g) ?? []).length).toBe(3); // baseline plus two separate trends
});

it("leaves a gap after expired samples and preserves a fixed fifteen-minute time axis", () => {
  const html = renderToStaticMarkup(<MachineSparkline color="cpu" label="CPU" stale={false}
    tr={(_ko, en) => en} now={900_000} values={[
      { at: 0, expiresAt: 30_000, value: 10 }, { at: 10_000, expiresAt: 40_000, value: 20 },
      { at: 890_000, expiresAt: 920_000, value: 30 }, { at: 900_000, expiresAt: 930_000, value: 40 },
    ]} />);
  expect((html.match(/<path /g) ?? []).length).toBe(3);
  expect(html).toContain("M98.89,");
  expect(html).toContain("L1.11,");
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
