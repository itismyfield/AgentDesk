// @vitest-environment happy-dom

import { act } from "react";
import { createRoot } from "react-dom/client";
import { describe, expect, it, vi } from "vitest";
import { SettingsPhoneCatalog } from "./SettingsNavigation";

vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
const tr = (ko: string) => ko;

describe("SettingsPhoneCatalog", () => {
  it("suppresses misleading 0 counts on category rows while config and runtime are still loading", async () => {
    const container = document.createElement("div");
    const root = createRoot(container);
    const categories = [
      { id: "workflow" as const, label: "작업 흐름", count: 0 },
      { id: "connections" as const, label: "연결", count: 0 },
    ];

    await act(async () => {
      root.render(
        <SettingsPhoneCatalog
          categories={categories}
          category="all"
          onCategory={vi.fn()}
          differing={[]}
          rows={[]}
          searching={false}
          loading
          notice={null}
          blocks={null}
          onOpen={vi.fn()}
          tr={tr}
        />,
      );
    });

    const workflowRow = container.querySelector('[data-testid="settings-category-workflow"]');
    expect(workflowRow?.textContent).toBe("작업 흐름");

    await act(async () => {
      root.render(
        <SettingsPhoneCatalog
          categories={[{ id: "workflow", label: "작업 흐름", count: 7 }]}
          category="all"
          onCategory={vi.fn()}
          differing={[]}
          rows={[]}
          searching={false}
          loading={false}
          notice={null}
          blocks={null}
          onOpen={vi.fn()}
          tr={tr}
        />,
      );
    });

    expect(container.querySelector('[data-testid="settings-category-workflow"]')?.textContent).toContain("7");
    await act(async () => root.unmount());
  });
});
