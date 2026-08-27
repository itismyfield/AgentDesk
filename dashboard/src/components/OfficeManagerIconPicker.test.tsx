// @vitest-environment happy-dom

import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Office } from "../types";
import OfficeManagerModal from "./OfficeManagerModal";
import OfficeManagerView from "./OfficeManagerView";

vi.mock("../api/client", () => ({
  addAgentToOffice: vi.fn().mockResolvedValue(undefined),
  createOffice: vi.fn().mockResolvedValue({ id: "office-created" }),
  deleteOffice: vi.fn().mockResolvedValue(undefined),
  getAgents: vi.fn().mockResolvedValue([]),
  removeAgentFromOffice: vi.fn().mockResolvedValue(undefined),
  updateOffice: vi.fn().mockResolvedValue(undefined),
}));

const office: Office = {
  id: "office-1",
  name: "Studio",
  name_ko: "스튜디오",
  icon: "🎮",
  color: "#3b82f6",
  description: null,
  sort_order: 0,
  created_at: 1710000000000,
};

function queryIconButton(container: HTMLElement, label: string): HTMLButtonElement {
  const button = container.querySelector<HTMLButtonElement>(
    `button[aria-label="${label}"]`,
  );
  expect(button).not.toBeNull();
  return button as HTMLButtonElement;
}

describe("Office manager icon picker accessibility", () => {
  let container: HTMLDivElement | null = null;
  let root: Root | null = null;

  async function render(element: ReactNode) {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root?.render(element);
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
    vi.clearAllMocks();
  });

  it("renders view icon buttons as non-submit toggle buttons with localized labels", async () => {
    const target = await render(
      <OfficeManagerView
        offices={[office]}
        allAgents={[]}
        selectedOfficeId={office.id}
        isKo={false}
        onChanged={() => {}}
      />,
    );

    const selectedIcon = queryIconButton(target, "Icon 🎮");
    expect(selectedIcon.getAttribute("type")).toBe("button");
    expect(selectedIcon.getAttribute("role")).toBe("radio");
    expect(selectedIcon.getAttribute("aria-checked")).toBe("true");

    const unselectedIcon = queryIconButton(target, "Icon 🏢");
    expect(unselectedIcon.getAttribute("type")).toBe("button");
    expect(unselectedIcon.getAttribute("role")).toBe("radio");
    expect(unselectedIcon.getAttribute("aria-checked")).toBe("false");
  });

  it("renders modal create icon buttons as non-submit toggle buttons with Korean labels", async () => {
    const target = await render(
      <OfficeManagerModal
        offices={[]}
        allAgents={[]}
        isKo
        onClose={() => {}}
        onChanged={() => {}}
      />,
    );

    const addButton = Array.from(target.querySelectorAll("button")).find(
      (button) => button.textContent?.includes("오피스 추가"),
    );
    expect(addButton).toBeDefined();

    await act(async () => {
      addButton?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    const defaultIcon = queryIconButton(target, "아이콘 🏢");
    expect(defaultIcon.getAttribute("type")).toBe("button");
    expect(defaultIcon.getAttribute("role")).toBe("radio");
    expect(defaultIcon.getAttribute("aria-checked")).toBe("true");

    const radioGroups = Array.from(
      target.querySelectorAll<HTMLElement>('[role="radiogroup"][aria-labelledby]'),
    );
    expect(radioGroups).toHaveLength(2);
    expect(
      radioGroups.map((group) => {
        const labelId = group.getAttribute("aria-labelledby");
        const label = labelId
          ? target.querySelector<HTMLElement>(`[id="${labelId}"]`)
          : null;
        return { tagName: label?.tagName, text: label?.textContent };
      }),
    ).toEqual([
      { tagName: "DIV", text: "아이콘" },
      { tagName: "DIV", text: "색상" },
    ]);
  });

  it("contains focus across modal views and restores the opener", async () => {
    const opener = document.createElement("button");
    document.body.appendChild(opener);
    opener.focus();
    const onClose = vi.fn();

    const target = await render(
      <OfficeManagerModal
        offices={[]}
        allAgents={[]}
        isKo={false}
        onClose={onClose}
        onChanged={() => {}}
      />,
    );

    const dialog = target.querySelector<HTMLElement>('[role="dialog"]');
    expect(dialog).not.toBeNull();
    expect(dialog?.contains(document.activeElement)).toBe(true);

    const addButton = Array.from(target.querySelectorAll("button")).find(
      (button) => button.textContent?.includes("Add Office"),
    );
    expect(addButton).toBeDefined();

    await act(async () => {
      addButton?.focus();
      addButton?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(target.textContent).toContain("New Office");
    expect(document.activeElement).toBe(dialog);

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    });
    expect(onClose).toHaveBeenCalledOnce();

    await act(async () => {
      root?.unmount();
    });
    root = null;
    expect(document.activeElement).toBe(opener);
    opener.remove();
  });
});
