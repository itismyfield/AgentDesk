// @vitest-environment happy-dom
import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { ApiRequestError } from "../../api/httpClient";
import { createServerCampaign, editServerCampaignDefinition, getServerCampaign } from "../../api/serverCampaigns";
import { serverCampaignFixtures as fixtures } from "../../api/serverCampaignsFixtures";
import ServerCampaignForm, { createDraft, editDraft, type DefinitionDraft } from "./ServerCampaignForm";

vi.mock("../../api/serverCampaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/serverCampaigns")>()),
  createServerCampaign: vi.fn(),
  editServerCampaignDefinition: vi.fn(),
  getServerCampaign: vi.fn(),
}));
const create = vi.mocked(createServerCampaign);
const edit = vi.mocked(editServerCampaignDefinition);
const read = vi.mocked(getServerCampaign);
const tr = (_ko: string, en: string) => en;
const result = { definition_revision: 1, run_id: null, control_version: null, event_sequence: 1, control: "prepared" as const };
let container: HTMLDivElement;
let root: Root;
let saved: ReturnType<typeof vi.fn<(id: string, result: unknown) => void>>;
let conflicted: ReturnType<typeof vi.fn<() => void>>;

let replaceDraft: (draft: DefinitionDraft) => void = () => {};
function Harness({ initial }: { initial: DefinitionDraft }) {
  const [draft, setDraft] = useState<DefinitionDraft | null>(initial);
  replaceDraft = setDraft;
  return draft ? <ServerCampaignForm draft={draft} onDraftChange={(update) => setDraft((current) => current && update(current))}
    tr={tr} onSaved={saved} onConflict={conflicted} onCancel={() => setDraft(null)} disabled={false} /> : <p>closed</p>;
}
async function mount(initial: DefinitionDraft) {
  await act(async () => root.render(<Harness initial={initial} />));
}
function field(label: string, index = 0) {
  return [...container.querySelectorAll<HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement>(`[aria-label="${label}"]`)][index];
}
async function type(element: HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement, value: string) {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(element), "value")!.set!;
    setter.call(element, value);
    element.dispatchEvent(new Event(element instanceof HTMLSelectElement ? "change" : "input", { bubbles: true }));
  });
}
async function press(text: string) {
  const button = [...container.querySelectorAll("button")].find((candidate) => candidate.textContent === text);
  if (!button) throw new Error(`no button ${text}`);
  await act(async () => { button.click(); });
}
async function check(label: string) {
  await act(async () => { container.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`)!.click(); });
}
const rejection = (status: number, context: Record<string, unknown>) =>
  new ApiRequestError("server campaign command rejected", { status, body: { error: "rejected", code: "x", context } });

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  saved = vi.fn<(id: string, result: unknown) => void>();
  conflicted = vi.fn<() => void>();
  create.mockReset();
  edit.mockReset();
  read.mockReset();
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

it("creates a two-task definition with acceptance and a dependency from the minimal form", async () => {
  create.mockResolvedValueOnce(result);
  await mount(createDraft(null, () => "key-1"));
  await type(field("Campaign ID"), "release-2");
  await type(field("Name"), "Release two");
  await type(field("Goal"), "Ship it");
  await press("Add task");
  await type(field("Task ID"), "A");
  await type(field("Task title"), "Build");
  await type(field("Issue link"), "https://github.com/itismyfield/AgentDesk/issues/1");
  await press("Add acceptance");
  await type(field("Acceptance ID"), "main");
  await type(field("Acceptance description"), "In main");
  await type(field("Verification"), "main");
  await press("Add task");
  await type(field("Task ID", 1), "B");
  await type(field("Task title", 1), "Follow-up");
  await type(field("Issue link", 1), "https://github.com/itismyfield/AgentDesk/issues/2");
  await type(field("Group", 1), "later");
  await check("B depends on A");
  await press("Create");
  expect(create).toHaveBeenCalledWith("release-2", {
    repository: "adk", title: "Release two", goal: "Ship it", profile_version: "v1",
    nodes: [
      { id: "A", title: "Build", issue_url: "https://github.com/itismyfield/AgentDesk/issues/1", acceptance: [{ id: "main", description: "In main", verification: "main" }], dependencies: [], group: null },
      { id: "B", title: "Follow-up", issue_url: "https://github.com/itismyfield/AgentDesk/issues/2", acceptance: [], dependencies: ["A"], group: "later" },
    ],
  }, "key-1");
  expect(saved).toHaveBeenCalledWith("release-2", result);
});

it("keeps the draft and names the server's cycle and empty acceptance refusals", async () => {
  create
    .mockRejectedValueOnce(rejection(400, { rejection: "invalid_definition", target: "release", definition_error: { code: "cycle", target: "A", path: ["A", "B", "A"] } }))
    .mockRejectedValueOnce(rejection(400, { rejection: "invalid_definition", target: "release", definition_error: { code: "empty_acceptance", target: "B", path: [] } }));
  let next = 0;
  const draft = createDraft(fixtures.prepared.definition, () => `key-${++next}`);
  draft.id = "release-3";
  await mount(draft);
  await press("Create");
  expect(container.textContent).toContain("The dependencies form a cycle · A → B → A");
  await press("Create");
  expect(container.textContent).toContain("A task has no acceptance condition · B");
  expect(field("Name").value).toBe("Release");
  expect(saved).not.toHaveBeenCalled();
  // The first refusal was decided under its key, so the same body goes out again under a new one.
  expect(create.mock.calls.map((call) => call[2])).toEqual(["key-1", "key-2"]);
});

function revisionEight() {
  const latest = structuredClone(fixtures.prepared);
  latest.view.definition_revision = 8;
  latest.definition.goal = "Changed elsewhere";
  latest.definition.nodes[1].group = "group-from-r8";
  latest.definition.nodes.push({
    id: "D", title: "Deploy service", issue_url: "https://github.com/itismyfield/AgentDesk/issues/7441", group: "rollout-r8", dependencies: ["A"],
    acceptance: [{ id: "AC-D1", description: "Production deployment verified", verification: "deployment" }],
  });
  return latest;
}
const addedD = "Deploy service\nissue_url: https://github.com/itismyfield/AgentDesk/issues/7441\ngroup: rollout-r8\ndependencies: A\nacceptance.AC-D1: Production deployment verified (deployment)";
function latestValues(scope = '[aria-label="Values in revision 8"]') {
  return [...container.querySelectorAll(`${scope} > div`)].map((row) => [row.querySelector("dt")?.textContent, row.querySelector("dd")?.textContent]);
}

it("keeps the draft on a 409 and shows the newer definition's changed values next to it", async () => {
  edit.mockRejectedValueOnce(rejection(409, { rejection: "stale_definition", target: "release" })).mockResolvedValueOnce(result);
  read.mockResolvedValueOnce(revisionEight());
  let next = 0;
  await mount(editDraft(fixtures.prepared, () => `key-${++next}`));
  await type(field("Name"), "My new name");
  await press("Save");
  expect(container.textContent).toContain("This definition changed (revision 8). Your draft is kept.");
  expect(container.textContent).toContain("Changed elsewhere: goal, B.group, D (added)");
  expect(latestValues()).toEqual([["goal", "Changed elsewhere"], ["B.group", "group-from-r8"], ["D (added)", addedD]]);
  // The whole newer definition stays readable, not only what changed.
  expect(latestValues(".campaign-server-latest-definition dl").map(([id]) => id)).toEqual(["A", "B", "C", "D"]);
  expect(latestValues(".campaign-server-latest-definition dl")[3][1]).toBe(addedD);
  expect(container.querySelectorAll('[aria-label="Task ID"]')).toHaveLength(3);
  expect(conflicted).toHaveBeenCalledOnce();
  expect(field("Name").value).toBe("My new name");
  expect(field("Goal").value).toBe("Integrate and verify the release");
  expect(field("Group", 1).value).toBe("release");
  // Saving over the newer revision is an explicit choice; the draft is still the user's.
  expect(edit).toHaveBeenCalledOnce();
  await press("Save over revision 8");
  expect(edit.mock.calls[1].slice(1, 3)).toEqual([8, { ...fixtures.prepared.definition, title: "My new name" }]);
});

it("measures the next conflict from the revision the user saved over", async () => {
  const stale = rejection(409, { rejection: "stale_definition", target: "release" });
  edit.mockRejectedValueOnce(stale).mockRejectedValueOnce(stale);
  const r8 = revisionEight();
  const r9 = structuredClone(r8);
  r9.view.definition_revision = 9;
  r9.definition.title = "Renamed in r9";
  read.mockResolvedValueOnce(r8).mockResolvedValueOnce(r9);
  await mount(editDraft(fixtures.prepared, () => "key"));
  await press("Save");
  await press("Save over revision 8");
  expect(latestValues('[aria-label="Values in revision 9"]')).toEqual([["title", "Renamed in r9"]]);
});

it("shows the server's own text for a refusal that carries no decision code", async () => {
  create.mockRejectedValueOnce(new ApiRequestError("invalid campaign ID", { status: 400, body: { error: "invalid campaign ID", code: "validation", context: {} } }));
  await mount(createDraft(fixtures.prepared.definition, () => "k"));
  await press("Create");
  expect(container.textContent).toContain("invalid campaign ID");
  expect(container.textContent).not.toContain("HTTP 400");
});

it.each([
  ["no answer", () => new TypeError("Failed to fetch"), "No answer arrived."],
  ["a 409", () => rejection(409, { rejection: "stale_definition", target: "release" }), "This definition changed (revision 8)."],
])("keeps what was typed while a save waited for %s", async (_answer, failure, notice) => {
  let answer: (error: unknown) => void = () => {};
  edit.mockImplementationOnce(() => new Promise((_resolve, reject) => { answer = reject; }));
  read.mockResolvedValueOnce(revisionEight());
  await mount(editDraft(fixtures.prepared, () => "key-1"));
  await type(field("Name"), "Sent name");
  await press("Save");
  await type(field("Name"), "Typed while waiting");
  await type(field("Acceptance description", 2), "Added while waiting");
  await act(async () => answer(failure()));
  expect(container.textContent).toContain(notice);
  expect(field("Name").value).toBe("Typed while waiting");
  expect(field("Acceptance description", 2).value).toBe("Added while waiting");
});

it("keeps edits made while a save was in flight as a draft on the saved revision", async () => {
  let answer: (value: typeof result) => void = () => {};
  edit.mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; })).mockResolvedValueOnce({ ...result, definition_revision: 9 });
  let next = 0;
  await mount(editDraft(fixtures.prepared, () => `key-${++next}`));
  await type(field("Name"), "Sent name");
  await press("Save");
  await type(field("Name"), "Typed while waiting");
  await act(async () => answer({ ...result, definition_revision: 8 }));
  expect(saved).toHaveBeenCalledOnce();
  expect(container.textContent).toContain("Saved as revision 8. Changes made since then are not saved yet.");
  expect(field("Name").value).toBe("Typed while waiting");
  // The next save builds on the saved revision; with nothing typed after it, the draft closes.
  await press("Save");
  expect(edit.mock.calls[1].slice(1, 3)).toEqual([8, { ...fixtures.prepared.definition, title: "Typed while waiting" }]);
  expect(container.textContent).toBe("closed");
});

it("resends an unanswered save with the same key and a changed draft with a new one", async () => {
  edit.mockRejectedValueOnce(new TypeError("Failed to fetch")).mockRejectedValueOnce(new TypeError("Failed to fetch")).mockResolvedValueOnce(result);
  let next = 0;
  await mount(editDraft(fixtures.prepared, () => `key-${++next}`));
  await press("Save");
  expect(container.textContent).toContain("No answer arrived. Saving again with the same content cannot apply it twice.");
  await press("Save");
  await type(field("Name"), "Another name");
  await press("Save");
  expect(edit.mock.calls.map((call) => call[4])).toEqual(["key-1", "key-1", "key-2"]);
  expect(edit.mock.calls[2][1]).toBe(7);
});

it("marks the change reason when the server asks for it and sends the next save under a new key", async () => {
  edit.mockRejectedValueOnce(rejection(400, { rejection: "missing_change_reason", target: null })).mockResolvedValueOnce(result);
  let next = 0;
  await mount(editDraft(fixtures.prepared, () => `key-${++next}`));
  await type(field("Acceptance description"), "Included in main, verified");
  await press("Save");
  expect(container.textContent).toContain("Changing requirements or acceptance needs a reference link and an explanation");
  expect(field("Change reference").getAttribute("aria-invalid")).toBe("true");
  await type(field("Change reference"), "https://github.com/itismyfield/AgentDesk/issues/7438#c1");
  await type(field("Change explanation"), "Clarified");
  await press("Save");
  expect(edit.mock.calls[0][3]).toBeNull();
  expect(edit.mock.calls[1][3]).toEqual({ reference: "https://github.com/itismyfield/AgentDesk/issues/7438#c1", explanation: "Clarified" });
  expect(edit.mock.calls.map((call) => call[4])).toEqual(["key-1", "key-2"]);
  expect(saved).toHaveBeenCalledOnce();
});

it("does not pre-validate the definition in the browser", async () => {
  create.mockRejectedValueOnce(rejection(400, { rejection: "invalid_definition", target: null, definition_error: { code: "empty_acceptance", target: "task-1", path: [] } }));
  await mount(createDraft(null, () => "k"));
  await press("Add task");
  await press("Create");
  expect(create).toHaveBeenCalledOnce();
  expect(container.textContent).toContain("A task has no acceptance condition · task-1");
});

it("applies a save's answer only to the draft it was sent from", async () => {
  let fail: (reason: unknown) => void = () => {};
  edit.mockImplementationOnce(() => new Promise((_, reject) => { fail = reject; }));
  await mount(editDraft(fixtures.prepared, () => "key-1"));
  await press("Save");
  // The panel replaced the draft (for example, it was discarded and reopened) before the answer came.
  await act(async () => replaceDraft(editDraft(fixtures.prepared, () => "key-2")));
  await act(async () => fail(new TypeError("Failed to fetch")));
  expect(container.textContent).not.toContain("No answer arrived.");
});

it("shows a dependency a rename left behind so the user can point it at the renamed task", async () => {
  create.mockResolvedValue(result);
  await mount(createDraft(null, () => "k"));
  await type(field("Campaign ID"), "release-4");
  await press("Add task");
  await press("Add task");
  await check("task-2 depends on task-1");
  await type(field("Task ID"), "build");
  const box = (label: string) => container.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`);
  expect(box("task-2 depends on task-1 (no such task)")?.checked).toBe(true);
  expect(box("task-2 depends on build")?.checked).toBe(false);
  await check("task-2 depends on task-1 (no such task)");
  await check("task-2 depends on build");
  await press("Create");
  expect(create.mock.calls[0][1].nodes.map((node) => [node.id, node.dependencies])).toEqual([["build", []], ["task-2", ["build"]]]);
});

it("lists a dependency on a task that no longer exists so it can be removed", async () => {
  const copied = structuredClone(fixtures.prepared.definition);
  copied.nodes[1].dependencies = [...copied.nodes[1].dependencies, "gone"];
  await mount(createDraft(copied, () => "k"));
  const orphan = container.querySelector<HTMLInputElement>(`input[aria-label="${copied.nodes[1].id} depends on gone (no such task)"]`)!;
  expect(orphan.checked).toBe(true);
  await act(async () => { orphan.click(); });
  expect(container.querySelector(`input[aria-label="${copied.nodes[1].id} depends on gone (no such task)"]`)).toBeNull();
});

it("never moves a dependency onto another task while task IDs are retyped", async () => {
  await mount(createDraft(null, () => "k"));
  for (let added = 0; added < 3; added++) await press("Add task");
  await check("task-3 depends on task-1");
  // Typing "task-20" over "task-1" passes through "task-2", the other task's ID;
  // then the other task is backspaced through the same prefixes to "task-5".
  for (const step of ["task-", "task-2", "task-20"]) await type(field("Task ID"), step);
  for (const step of ["task-", "task-5"]) await type(field("Task ID", 1), step);
  const box = (label: string) => container.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`);
  for (const other of ["task-2", "task-20", "task-5", "task-"]) expect(box(`task-3 depends on ${other}`)?.checked ?? false, other).toBe(false);
  expect(box("task-3 depends on task-1 (no such task)")?.checked).toBe(true);
  // Each task and each leftover dependency is listed exactly once, whatever IDs passed through.
  const rows = [...container.querySelectorAll("fieldset.campaign-server-task")[2].querySelectorAll<HTMLInputElement>('input[type="checkbox"]')].map((input) => input.getAttribute("aria-label"));
  expect(rows).toEqual(["task-3 depends on task-20", "task-3 depends on task-5", "task-3 depends on task-1 (no such task)"]);
});

it("lists a dependency on the task's own ID so it can be removed", async () => {
  create.mockResolvedValue(result);
  await mount(createDraft(null, () => "k"));
  await type(field("Campaign ID"), "self");
  await press("Add task");
  await press("Add task");
  await check("task-2 depends on task-1");
  await type(field("Task ID"), "build");
  await type(field("Task ID", 1), "task-1");
  await check("task-1 depends on task-1 (this task)");
  await press("Create");
  expect(create.mock.calls[0][1].nodes.map((node) => [node.id, node.dependencies])).toEqual([["build", []], ["task-1", []]]);
});

it("keeps a dependency another task still satisfies when a task with the same ID is removed", async () => {
  await mount(createDraft(null, () => "k"));
  for (let added = 0; added < 3; added++) await press("Add task");
  await type(field("Task ID", 1), "task-1");
  await check("task-3 depends on task-1");
  const removeSecond = [...container.querySelectorAll("fieldset.campaign-server-task")[1].querySelectorAll("button")].find((button) => button.textContent === "Remove task")!;
  await act(async () => { removeSecond.click(); });
  expect(container.querySelector<HTMLInputElement>('input[aria-label="task-3 depends on task-1"]')?.checked).toBe(true);
});

it("fixes only the IDs of tasks from the saved definition, not a new task typed to one of them", async () => {
  await mount(editDraft(fixtures.prepared, () => "k"));
  await press("Add task");
  const added = () => field("Task ID", fixtures.prepared.definition.nodes.length) as HTMLInputElement;
  await type(added(), "A");
  expect(added().readOnly).toBe(false);
  await type(added(), "AB");
  expect(added().value).toBe("AB");
  expect((field("Task ID", 0) as HTMLInputElement).readOnly).toBe(true);
});

it("never gives a new task an ID that a leftover dependency still names", async () => {
  await mount(createDraft(null, () => "k"));
  for (let added = 0; added < 4; added++) await press("Add task");
  await check("task-1 depends on task-4");
  await type(field("Task ID", 3), "z");
  const removeSecond = [...container.querySelectorAll("fieldset.campaign-server-task")[1].querySelectorAll("button")].find((button) => button.textContent === "Remove task")!;
  await act(async () => { removeSecond.click(); });
  await press("Add task");
  expect((field("Task ID", 3) as HTMLInputElement).value).not.toBe("task-4");
  expect(container.querySelector<HTMLInputElement>('input[aria-label="task-1 depends on task-4 (no such task)"]')?.checked).toBe(true);
});

it("fixes the task IDs a save sent once it succeeds, but not a task typed to one of them meanwhile", async () => {
  let answer: (value: typeof result) => void = () => {};
  create.mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }));
  await mount(createDraft(null, () => "k"));
  await type(field("Campaign ID"), "c");
  await press("Add task");
  await press("Create");
  await press("Add task");
  await type(field("Task ID", 1), "task-1");
  await act(async () => answer(result));
  expect(container.textContent).toContain("Saved as revision 1.");
  expect((field("Task ID", 0) as HTMLInputElement).readOnly).toBe(true);
  expect((field("Task ID", 1) as HTMLInputElement).readOnly).toBe(false);
});
