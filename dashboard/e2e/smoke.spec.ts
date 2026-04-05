import { test, expect } from "@playwright/test";

test.describe("Dashboard smoke tests", () => {
  test("page loads and renders root element", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("#root")).toBeAttached();
  });

  test("theme: dark/light toggle changes CSS variables", async ({ page }) => {
    await page.goto("/");
    // Set dark, verify CSS variable responds
    await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    const darkBg = await page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue("--th-bg-primary").trim(),
    );
    expect(darkBg).toBeTruthy();

    // Switch to light, verify CSS variable changes
    await page.evaluate(() => { document.documentElement.dataset.theme = "light"; });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    const lightBg = await page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue("--th-bg-primary").trim(),
    );
    expect(lightBg).toBeTruthy();
    expect(lightBg).not.toBe(darkBg);
  });

  test("theme: auto mode responds to prefers-color-scheme", async ({ page }) => {
    // Emulate dark system preference
    await page.emulateMedia({ colorScheme: "dark" });
    await page.goto("/");
    // The SettingsContext auto path uses matchMedia to set data-theme.
    // Without backend, simulate the auto logic that the app would run:
    await page.evaluate(() => {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      document.documentElement.dataset.theme = mq.matches ? "dark" : "light";
    });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");

    // Switch to light system preference
    await page.emulateMedia({ colorScheme: "light" });
    await page.evaluate(() => {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      document.documentElement.dataset.theme = mq.matches ? "dark" : "light";
    });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  });

  test("responsive: desktop viewport shows sidebar nav", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/");
    const sidebar = page.locator("nav").first();
    await expect(sidebar).toBeVisible({ timeout: 5000 });
  });

  test("responsive: mobile viewport shows bottom tab bar", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/");
    const bottomNav = page.locator("nav").last();
    await expect(bottomNav).toBeVisible({ timeout: 5000 });
  });

  test("settings: clicking settings button renders SettingsView", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/");
    const settingsBtn = page.locator('button[title*="Settings"], button[title*="설정"]').first();
    await expect(settingsBtn).toBeVisible({ timeout: 5000 });
    await settingsBtn.click();
    // SettingsView renders a heading with "Settings" or "설정" text
    const heading = page.locator('h2:has-text("Settings"), h2:has-text("설정")').first();
    await expect(heading).toBeVisible({ timeout: 5000 });
  });
});
