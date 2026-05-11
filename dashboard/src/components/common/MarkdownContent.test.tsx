/** @vitest-environment happy-dom */
import { test, expect } from "vitest";
import { createRoot } from "react-dom/client";
import { act } from "react";
import MarkdownContent from "./MarkdownContent";
import React from "react";

test("MarkdownContent renders links safely with target blank and noopener noreferrer", async () => {
  const div = document.createElement("div");
  const root = createRoot(div);

  await act(async () => {
    root.render(
      <MarkdownContent
        content={
          "[external](https://example.com) [same-origin](http://localhost:3000/settings) [protocol-relative](//localhost:3000/help) [anchor](#details) [evil](javascript:alert(1))"
        }
      />
    );
  });

  const links = div.querySelectorAll("a");

  for (const link of links) {
    if (link.getAttribute("href") === "https://example.com") {
      expect(link.getAttribute("target")).toBe("_blank");
      expect(link.getAttribute("rel")).toBe("noopener noreferrer");
    } else if (
      link.getAttribute("href") === "http://localhost:3000/settings" ||
      link.getAttribute("href") === "//localhost:3000/help" ||
      link.getAttribute("href") === "#details"
    ) {
      expect(link.hasAttribute("target")).toBe(false);
      expect(link.hasAttribute("rel")).toBe(false);
    } else {
      expect(link.getAttribute("href")).not.toBe("javascript:alert(1)");
    }
  }
});
