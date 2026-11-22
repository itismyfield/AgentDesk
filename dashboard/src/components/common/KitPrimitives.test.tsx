// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, expect, it } from "vitest";
import { KeyValue, RowButton, RowLink } from "./KitPrimitives";

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

it("marks selected RowLink and RowButton with aria-current unless the caller sets one, and merges spacing overrides", async () => {
  await act(async () => root.render(<MemoryRouter>
    <RowLink to="/a" selected className="gap-1.5 py-2.5">A</RowLink>
    <RowLink to="/b">B</RowLink>
    <RowLink to="/c" selected aria-current="page">C</RowLink>
    <RowButton selected>D</RowButton>
    <RowButton>E</RowButton>
    <RowButton selected aria-current={undefined}>F</RowButton>
  </MemoryRouter>));
  const links = container.querySelectorAll("a");
  const buttons = container.querySelectorAll("button");
  expect([links[0].getAttribute("aria-current"), links[1].getAttribute("aria-current"), links[2].getAttribute("aria-current")]).toEqual(["true", null, "page"]);
  expect([...buttons].map((button) => button.getAttribute("aria-current"))).toEqual(["true", null, null]);
  expect(links[0].className.split(/\s+/)).toEqual(expect.arrayContaining(["gap-1.5", "py-2.5"]));
  expect(links[0].className.split(/\s+/)).not.toEqual(expect.arrayContaining(["gap-3", "py-3"]));
});

it("keeps dl > div children to DT and DD when KeyValue renders meta in dl mode", async () => {
  await act(async () => root.render(<dl><KeyValue dl label="Claude" meta={<span>12:00</span>}>83%</KeyValue></dl>));
  const group = container.querySelector("dl > div")!;
  expect([...group.children].map((child) => child.tagName)).toEqual(["DT", "DD"]);
  expect(group.querySelector("dd")?.textContent).toBe("12:0083%");
});
