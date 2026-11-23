// @ts-check
const { test, expect } = require("@playwright/test");

function childShapes(locator) {
  return locator.evaluate((element) =>
    Array.from(element.children).map((child) =>
      child.className
        ? `${child.tagName.toLowerCase()}.${child.className}`
        : child.tagName.toLowerCase()
    )
  );
}

async function openHydrated(page, url) {
  const failures = [];
  page.on("pageerror", (error) => failures.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error") {
      failures.push(message.text());
    }
  });
  await page.goto(url);
  await expect(page.locator("#hydrated")).toHaveAttribute("data-hydrated", "true");
  return failures;
}

async function flipThrough(page, url, { button, target, shapes }) {
  const failures = await openHydrated(page, url);
  const container = page.locator(target);
  await expect.poll(() => childShapes(container)).toEqual(shapes[0]);
  for (let flip = 1; flip <= 4; flip += 1) {
    await page.locator(button).click();
    await expect.poll(() => childShapes(container)).toEqual(shapes[flip % 2]);
  }
  expect(failures).toEqual([]);
}

function describeSiblingPlacement(url) {
  test("a keyed row inserts after an earlier sibling empties in the same render", async ({ page }) => {
    await flipThrough(page, url, {
      button: "#flip-emptied",
      target: "#emptied",
      shapes: [["span.second"], ["span.first", "b"]],
    });
  });

  test("a keyed row inserts after an earlier sibling fills in the same render", async ({ page }) => {
    await flipThrough(page, url, {
      button: "#flip-filled",
      target: "#filled",
      shapes: [["i"], ["i", "span", "b"]],
    });
  });
}

module.exports = { describeSiblingPlacement };
