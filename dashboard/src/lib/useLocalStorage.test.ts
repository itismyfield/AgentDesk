import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

import {
  readLocalStorageValue,
  removeLocalStorageValue,
  subscribeToLocalStorageKey,
  writeLocalStorageValue,
} from "./useLocalStorage";

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

const eventTarget = new EventTarget();

beforeAll(() => {
  vi.stubGlobal("window", {
    localStorage: localStorageMock,
    addEventListener: eventTarget.addEventListener.bind(eventTarget),
    removeEventListener: eventTarget.removeEventListener.bind(eventTarget),
    dispatchEvent: eventTarget.dispatchEvent.bind(eventTarget),
  });
});

afterEach(() => {
  localStorageMock.clear();
  vi.clearAllMocks();
});

describe("useLocalStorage helpers", () => {
  it("resolves a lazy default only when storage is empty", () => {
    const fallbackFactory = vi.fn(() => ["default"]);

    expect(readLocalStorageValue("agentdesk.test.default", fallbackFactory)).toEqual(["default"]);
    expect(fallbackFactory).toHaveBeenCalledTimes(1);

    writeLocalStorageValue("agentdesk.test.default", ["saved"]);
    expect(readLocalStorageValue("agentdesk.test.default", fallbackFactory)).toEqual(["saved"]);
    expect(fallbackFactory).toHaveBeenCalledTimes(1);
  });

  it("stores and reads JSON values", () => {
    writeLocalStorageValue("agentdesk.test.json", {
      enabled: true,
      order: ["alpha", "beta"],
    });

    expect(storageValues["agentdesk.test.json"]).toBe(
      JSON.stringify({ enabled: true, order: ["alpha", "beta"] }),
    );
    expect(
      readLocalStorageValue("agentdesk.test.json", {
        enabled: false,
        order: [],
      }),
    ).toEqual({
      enabled: true,
      order: ["alpha", "beta"],
    });
  });

  it("falls back and warns when JSON is corrupted", () => {
    storageValues["agentdesk.test.corrupt"] = "{not-json";
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});

    expect(readLocalStorageValue("agentdesk.test.corrupt", ["safe"])).toEqual(["safe"]);
    expect(warnSpy).toHaveBeenCalledTimes(1);
  });

  it("notifies same-tab subscribers on write and remove", () => {
    const listener = vi.fn();
    const unsubscribe = subscribeToLocalStorageKey("agentdesk.test.notify", listener);

    writeLocalStorageValue("agentdesk.test.notify", "first");
    removeLocalStorageValue("agentdesk.test.notify");

    expect(listener).toHaveBeenCalledTimes(2);
    unsubscribe();
  });

  it("reads legacy raw strings for refactored string keys", () => {
    storageValues["agentdesk.test.legacy"] = "pipeline";

    expect(
      readLocalStorageValue("agentdesk.test.legacy", "general", {
        validate: (value): value is string => typeof value === "string",
        legacy: (raw) => raw,
      }),
    ).toBe("pipeline");
  });
});
