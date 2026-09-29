// @vitest-environment happy-dom

import React, { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MiniRateLimitBar } from "./MiniRateLimitBar";

describe("MiniRateLimitBar", () => {
  let container: HTMLDivElement | null = null;
  let root: Root | null = null;

  async function render(element: ReactNode) {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    await act(async () => {
      root?.render(element);
    });
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    return container;
  }

  afterEach(async () => {
    if (root) {
      await act(async () => {
        root?.unmount();
      });
      root = null;
    }
    container?.remove();
    container = null;
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("hides the projected pace percentage for 5h while keeping it for 7d", async () => {
    const nowMs = Date.UTC(2026, 5, 25, 0, 0, 0);
    vi.useFakeTimers();
    vi.setSystemTime(new Date(nowMs));
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        ok: true,
        json: async () => ({
          providers: [
            {
              provider: "claude",
              buckets: [
                {
                  name: "5h",
                  limit: 100,
                  used: 50,
                  remaining: 50,
                  reset: Math.floor((nowMs + 3_600_000) / 1000),
                },
                {
                  name: "7d",
                  limit: 100,
                  used: 25,
                  remaining: 75,
                  reset: Math.floor((nowMs + 86_400_000) / 1000),
                },
              ],
              stale: false,
            },
          ],
        }),
      })),
    );

    const target = await render(<MiniRateLimitBar isKo={true} />);
    const text = target.textContent ?? "";

    expect(text).toContain("5h");
    expect(text).toContain("50%");
    expect(text).not.toContain("50→63%");
    expect(text).toContain("7d");
    expect(text).toContain("25→29%");
  });
});
