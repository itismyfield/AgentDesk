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
          "[external](https://example.com) [same-origin](http://localhost:3000/settings) [protocol-relative](//localhost:3000/help) [anchor](#details) [evil](javascript:alert(1)) [discord](discord://discord.com/channels/1/2) [discord-message](discord://discord.com/channels/1/2/3) [discord-me](discord://discord.com/channels/@me/456) [bad-discord](discord://evil.com/alert(1)) [bad-discord-2](discord:alert(1)) [bypass-discord](DiScOrD://evil.com/alert(1)) [malicious-discord](discord://discord.com/channels/1/2/3\"onClick=\"alert(1)) [malicious-discord-2](discord://discord.com/channels/1/2?script=alert(1))"
        }
      />
    );
  });

  const links = div.querySelectorAll("a");
  const parsedHrefs = Array.from(links).map((link) => link.getAttribute("href"));

  expect(parsedHrefs).toContain("https://example.com");
  expect(parsedHrefs).toContain("http://localhost:3000/settings");
  expect(parsedHrefs).toContain("//localhost:3000/help");
  expect(parsedHrefs).toContain("#details");
  expect(parsedHrefs).not.toContain("javascript:alert(1)");

  // Authorized discord links should be preserved.
  expect(parsedHrefs).toContain("discord://discord.com/channels/1/2");
  expect(parsedHrefs).toContain("discord://discord.com/channels/1/2/3");
  expect(parsedHrefs).toContain("discord://discord.com/channels/@me/456");

  // Unauthorized discord links should be stripped.
  expect(parsedHrefs).not.toContain("discord://evil.com/alert(1)");
  expect(parsedHrefs).not.toContain("discord:alert(1)");
  expect(parsedHrefs).not.toContain("DiScOrD://evil.com/alert(1)");
  expect(parsedHrefs).not.toContain("discord://discord.com/channels/1/2/3\"onClick=\"alert(1)");
  expect(parsedHrefs).not.toContain("discord://discord.com/channels/1/2?script=alert(1)");

  for (const link of links) {
    const href = link.getAttribute("href");
    if (
      href === "https://example.com" ||
      href?.startsWith("discord:")
    ) {
      expect(link.getAttribute("target")).toBe("_blank");
      expect(link.getAttribute("rel")).toBe("noopener noreferrer");
    } else if (
      href === "http://localhost:3000/settings" ||
      href === "//localhost:3000/help" ||
      href === "#details"
    ) {
      expect(link.hasAttribute("target")).toBe(false);
      expect(link.hasAttribute("rel")).toBe(false);
    }
  }

  await act(async () => root.unmount());
});

test.each([
  ["discord://discord.com/channels/123/456", true],
  ["discord://discord.com/channels/123/456/789", true],
  ["discord://discord.com/channels/@me/456", true],
  ["DiScOrD://discord.com/channels/@me/456/789", true],
  ["discord://discord.com/channels/guild/456", false],
  ["discord://discord.com/channels/@everyone/456", false],
  ["discord://discord.com/channels/123/channel", false],
  ["discord://discord.com/channels/123/456/message", false],
  ["discord://discord.com/channels/123/456?script=alert", false],
  ["discord://discord.com/channels/123/456#fragment", false],
  ["discord://discord.com/channels/123/456%22onclick=%22alert", false],
  ["discord://discord.com/channels/123/456/789/extra", false],
  ["discord://discord.com.evil.test/channels/123/456", false],
  ["discord://discord.com@evil.test/channels/123/456", false],
])("Discord link %s is allowed: %s", async (url, allowed) => {
  const div = document.createElement("div");
  const root = createRoot(div);
  await act(async () => root.render(<MarkdownContent content={`[open](<${url}>)`} />));

  const link = div.querySelector("a");
  expect(link).not.toBeNull();
  expect(link!.getAttribute("href")).toBe(allowed ? url : "");

  await act(async () => root.unmount());
});
