// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useLocalStorage } from "./useLocalStorage";

interface PersistedObjectStore {
  version: 2;
  entries: Record<string, { updatedAtMs: number }>;
}

const STORAGE_KEY = "agentdesk.test.object-snapshot";
const storageValues: Record<string, string> = {};

const localStorageMock = {
  getItem: vi.fn((key: string) => storageValues[key] ?? null),
  setItem: vi.fn((key: string, value: string) => {
    storageValues[key] = value;
  }),
  removeItem: vi.fn((key: string) => {
    delete storageValues[key];
  }),
  clear: vi.fn(() => {
    Object.keys(storageValues).forEach((key) => delete storageValues[key]);
  }),
};

function HookProbe({
  onValue,
}: {
  onValue: (value: PersistedObjectStore) => void;
}) {
  const [value] = useLocalStorage<PersistedObjectStore>(STORAGE_KEY, {
    version: 2,
    entries: {},
  });
  onValue(value);
  return null;
}

describe("useLocalStorage hook", () => {
  let container: HTMLDivElement | null = null;
  let root: Root | null = null;

  afterEach(async () => {
    if (root) {
      await act(async () => {
        root?.unmount();
      });
      root = null;
    }
    if (container) {
      container.remove();
      container = null;
    }
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: localStorageMock,
    });
    localStorageMock.clear();
    vi.restoreAllMocks();
  });

  it("keeps object snapshots stable across rerenders when storage bytes are unchanged", async () => {
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: localStorageMock,
    });
    window.localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        version: 2,
        entries: {
          alpha: { updatedAtMs: 1 },
        },
      }),
    );
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    const snapshots: PersistedObjectStore[] = [];
    const consoleErrorSpy = vi
      .spyOn(console, "error")
      .mockImplementation(() => {});

    await act(async () => {
      root?.render(
        <HookProbe
          onValue={(value) => {
            snapshots.push(value);
          }}
        />,
      );
    });

    const firstSnapshot = snapshots.at(-1);
    expect(firstSnapshot).toEqual({
      version: 2,
      entries: {
        alpha: { updatedAtMs: 1 },
      },
    });

    await act(async () => {
      root?.render(
        <HookProbe
          onValue={(value) => {
            snapshots.push(value);
          }}
        />,
      );
    });

    expect(snapshots.at(-1)).toBe(firstSnapshot);
    expect(
      consoleErrorSpy.mock.calls.some((call) =>
        call.some((entry) =>
          String(entry).includes("Maximum update depth exceeded"),
        ),
      ),
    ).toBe(false);
  });
});
