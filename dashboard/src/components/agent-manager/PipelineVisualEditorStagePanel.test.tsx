// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

import * as api from "../../api";
import type { PipelineConfigFull, PipelineStage } from "../../types";
import PipelineVisualEditor, { type PipelineEditorActions, type PipelineEditorCtx } from "./PipelineVisualEditor";
import PipelineVisualEditorStagePanel from "./PipelineVisualEditorStagePanel";

// Keep the real stage controls and save action while omitting unrelated graph UI.
vi.mock("./PipelineVisualEditorView", () => ({
  default: (props: { ctx: PipelineEditorCtx; actions: PipelineEditorActions }) => (
    <>
      <PipelineVisualEditorStagePanel {...props} />
      <button onClick={() => void props.actions.handleSave()}>Save</button>
    </>
  ),
}));

const REPO = "itismyfield/AgentDesk";
const pipeline: PipelineConfigFull = {
  name: "default", version: 1,
  states: [{ id: "ready", label: "Ready" }],
  transitions: [], gates: {}, hooks: {}, events: {}, clocks: {},
  phase_gate: { dispatch_to: "self", dispatch_type: "phase-gate" },
};

let root: Root;
let container: HTMLDivElement;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  if (!Element.prototype.scrollIntoView) {
    Element.prototype.scrollIntoView = () => {};
  }
  const storage = new Map<string, string>();
  Object.defineProperty(window, "localStorage", {
    configurable: true,
    value: { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => storage.set(key, value) },
  });
  vi.spyOn(api, "getEffectivePipeline").mockResolvedValue({ pipeline, layers: { default: true, repo: false, agent: false } });
  vi.spyOn(api, "getRepoPipeline").mockResolvedValue({ repo: REPO, pipeline_config: null });
  vi.spyOn(api, "savePipelineStages").mockResolvedValue([]);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

async function mountStage(values?: Partial<PipelineStage>) {
  vi.spyOn(api, "getPipelineStages").mockResolvedValue(values ? [{
    id: "stage-1", repo: REPO, stage_name: "qa", stage_order: 1,
    provider: null, skip_condition: null, agent_override_id: null,
    trigger_after: "review_pass", ...values,
  }] : []);
  await act(async () => root.render(
    <PipelineVisualEditor tr={(_ko, en) => en} locale="en" repo={REPO} agents={[]} />,
  ));
}

function field(label: string) {
  const lbl = [...container.querySelectorAll("label")].find((entry) => entry.textContent === label);
  const element = lbl?.closest(".aui-select-clean") ?? lbl?.parentElement;
  expect(element).toBeTruthy();
  return element!;
}

async function clickButton(text: string) {
  const button = [...container.querySelectorAll("button")].find((entry) => entry.textContent === text);
  expect(button).toBeTruthy();
  await act(async () => button!.click());
}

async function saveTriggerEdit(values: Partial<PipelineStage>) {
  expect(container.querySelector("select")).toBeNull();
  const trigger = field("Trigger").querySelector<HTMLButtonElement>('[role="combobox"]')!;
  await act(async () => trigger.click());
  const readyOption = [...document.querySelectorAll<HTMLElement>('[role="option"]')].find((entry) =>
    entry.textContent?.includes("On ready"),
  )!;
  await act(async () => readyOption.click());
  await clickButton("Save");
  expect(api.savePipelineStages).toHaveBeenCalledExactlyOnceWith(REPO, [{
    stage_name: "qa", provider: null, skip_condition: null, agent_override_id: null,
    ...values, trigger_after: "ready",
  }]);
}

it("offers no counter provider or conditional skip for a new stage", async () => {
  await mountStage();
  await clickButton("+ Stage");

  expect(container.querySelector("select")).toBeNull();
  expect(field("Trigger").querySelector('[role="combobox"]')).not.toBeNull();
  expect(field("Provider").querySelector('select, [role="combobox"]')).toBeNull();
  expect(field("Provider").textContent).toContain("Assigned agent");
  expect(field("Skip").querySelector('select, [role="combobox"]')).toBeNull();
  expect(field("Skip").textContent).toContain("Never");
});

it.each([
  { provider: "counter", agent_override_id: "existing-agent" },
  { provider: "counter", agent_override_id: "" },
  { provider: " counter ", agent_override_id: "existing-agent" },
])("shows a stored counter provider read-only and preserves it on save: %j", async (values) => {
  await mountStage(values);
  await saveTriggerEdit(values);

  expect(field("Provider").querySelector('select, [role="combobox"]')).toBeNull();
  expect(field("Provider").textContent).toContain("Counter model");
  expect(field("Agent override").querySelector('select, [role="combobox"]')).toBeNull();
  expect(field("Agent override").textContent).toContain(values.agent_override_id || "Card assignee");
});

it("shows a stored conditional skip read-only and preserves it on save", async () => {
  const values = { skip_condition: "no_rs_changes" };
  await mountStage(values);
  await saveTriggerEdit(values);

  expect(field("Skip").querySelector('select, [role="combobox"]')).toBeNull();
  expect(field("Skip").textContent).toContain("When no Rust files changed");
});

it.each([
  { provider: "codex", skip_condition: "label:hotfix" },
  { provider: " codex ", skip_condition: "  " },
  { provider: "", skip_condition: "" },
])("displays and preserves stored values verbatim: %j", async (values) => {
  await mountStage(values);
  await saveTriggerEdit(values);

  expect(field("Provider").textContent).toContain(values.provider);
  expect(field("Skip").textContent).toContain(values.skip_condition);
  expect(field("Provider").querySelector('select, [role="combobox"]')).toBeNull();
  expect(field("Skip").querySelector('select, [role="combobox"]')).toBeNull();
});
