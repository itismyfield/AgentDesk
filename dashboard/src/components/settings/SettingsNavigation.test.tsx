// @vitest-environment happy-dom

import { act } from "react";
import { createRoot } from "react-dom/client";
import { describe, expect, it, vi } from "vitest";
import { SettingsPhoneCatalog } from "./SettingsNavigation";
import type { CatalogItem } from "./SettingsModel";

vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
const tr = (ko: string) => ko;

function makeItem(key: string, extra: Partial<CatalogItem> = {}): CatalogItem {
  return {
    key,
    source: "config",
    category: "workflow",
    label: `${key} 라벨`,
    description: "",
    input: "number",
    saved: "5",
    draft: null,
    baseline: "3",
    baselineSource: "hardcoded",
    resetTo: "3",
    overridden: false,
    restartNeeded: false,
    applies: "persist-live-override",
    related: [],
    dangerous: false,
    ...extra,
  };
}

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

  it("renders categories above essential settings and omits differing items already shown in essential", async () => {
    const container = document.createElement("div");
    const root = createRoot(container);
    const essentialOverridden = makeItem("max_review_rounds", { category: "review", overridden: true });
    const advancedOverridden = makeItem("maxEntryRetries", { source: "runtime", overridden: true });

    await act(async () => {
      root.render(
        <SettingsPhoneCatalog
          categories={[
            { id: "usage", label: "사용량", count: 2 },
            { id: "connections", label: "연결", count: 4 },
          ]}
          category="all"
          onCategory={vi.fn()}
          essential={[essentialOverridden]}
          differing={[essentialOverridden, advancedOverridden]}
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

    const categoryRow = container.querySelector('[data-testid="settings-category-usage"]');
    const essentialSection = container.querySelector('[data-testid="settings-tier-essential"]');
    const differingSection = container.querySelector('[data-testid="settings-differing"]');

    expect(categoryRow).not.toBeNull();
    expect(essentialSection).not.toBeNull();
    expect(differingSection).not.toBeNull();
    expect(
      (categoryRow!.compareDocumentPosition(essentialSection!) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0,
    ).toBe(true);

    const essentialRow = essentialSection?.querySelector('[data-testid="setting-row-max_review_rounds"]');
    expect(essentialRow).not.toBeNull();
    expect(essentialRow?.querySelector("svg")).not.toBeNull();
    expect(differingSection?.querySelector('[data-testid="setting-row-max_review_rounds"]')).toBeNull();
    expect(differingSection?.querySelector('[data-testid="setting-row-maxEntryRetries"]')).not.toBeNull();

    await act(async () => {
      root.render(
        <SettingsPhoneCatalog
          categories={[{ id: "usage", label: "사용량", count: 2 }]}
          category="all"
          onCategory={vi.fn()}
          essential={[essentialOverridden]}
          differing={[essentialOverridden]}
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

    expect(container.querySelector('[data-testid="settings-differing"]')).toBeNull();
    await act(async () => root.unmount());
  });
});
