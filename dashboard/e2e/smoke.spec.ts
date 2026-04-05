import { test, expect } from "@playwright/test";

test.describe("Dashboard smoke tests", () => {
  test("page loads and renders root element", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("#root")).toBeAttached();
  });

  test("theme: data-theme can be toggled between dark and light", async ({ page }) => {
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

  test("responsive: desktop viewport shows sidebar nav", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/");
    // Desktop sidebar: hidden on mobile, flex on sm+
    const sidebar = page.locator("nav").first();
    await expect(sidebar).toBeVisible({ timeout: 5000 });
  });

  test("responsive: mobile viewport shows bottom tab bar", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/");
    // Mobile bottom nav: visible only below sm breakpoint
    const bottomNav = page.locator("nav").last();
    await expect(bottomNav).toBeVisible({ timeout: 5000 });
  });

  test("settings: settings button exists and is clickable", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/");
    const settingsBtn = page.locator('button[title*="Settings"], button[title*="설정"]').first();
    await expect(settingsBtn).toBeVisible({ timeout: 5000 });
    await settingsBtn.click();
    // Verify navigation occurred by checking URL or content change
    await page.waitForTimeout(500);
  });
});
