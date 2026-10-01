// @ts-check
const { test, expect } = require("@playwright/test");

test("button click", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  // Expect the page to contain the counter text.
  const main = page.locator("#main");
  await expect(main).toContainText("hello axum! 0");

  // Click the increment button.
  await page.getByRole("button", { name: "Increment" }).click();

  // Expect the page to contain the updated counter text.
  await expect(main).toContainText("hello axum! 1");
});

test("svg", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  // Expect the page to contain the svg.
  const svg = page.locator("svg");

  // Expect the svg to contain the circle.
  const circle = svg.locator("circle");
  await expect(circle).toHaveAttribute("cx", "50");
  await expect(circle).toHaveAttribute("cy", "50");
  await expect(circle).toHaveAttribute("r", "40");
  await expect(circle).toHaveAttribute("stroke", "green");
  await expect(circle).toHaveAttribute("fill", "yellow");
});

test("raw attribute", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  // Expect the page to contain the div with the raw attribute.
  const div = page.locator("div.raw-attribute-div");
  await expect(div).toHaveAttribute("raw-attribute", "raw-attribute-value");
});

test("hidden attribute", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  // Expect the page to contain the div with the hidden attribute.
  const div = page.locator("div.hidden-attribute-div");
  await expect(div).toHaveAttribute("hidden", "true");
});

test("dangerous inner html", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  // Expect the page to contain the div with the dangerous inner html.
  const div = page.locator("div.dangerous-inner-html-div");
  await expect(div).toContainText("hello dangerous inner html");
});

test("input value", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  // Expect the page to contain the input with the value.
  const input = page.locator("input");
  await expect(input).toHaveValue("hello input");
});

test("style", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  // Expect the page to contain the div with the style.
  const div = page.locator("div.style-div");
  await expect(div).toHaveText("colored text");
  await expect(div).toHaveCSS("color", "rgb(255, 0, 0)");
});

test("onmounted", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  // Expect the onmounted event to be called exactly once.
  const mountedDiv = page.locator("div.onmounted-div");
  await expect(mountedDiv).toHaveText("onmounted was called 1 times");
});

test("bubbling events round-trip only when an element on their path listens", async ({ page }) => {
  await page.goto("http://127.0.0.1:3030");

  const moved = page.locator("div.path-moved");
  await expect(moved).toHaveText("moved 0 times");

  await page.evaluate(() => {
    const interpreter = window.interpreter;
    const send = interpreter.sendIpcMessage.bind(interpreter);
    window.sentEvents = [];
    interpreter.sendIpcMessage = (method, params) => {
      if (method === "user_event") {
        window.sentEvents.push(params.name);
      }
      return send(method, params);
    };
  });
  const moveOver = (selector) =>
    page.evaluate((selector) => {
      document
        .querySelector(selector)
        .dispatchEvent(new MouseEvent("mousemove", { bubbles: true, cancelable: true }));
    }, selector);

  await moveOver("div.path-quiet");
  await moveOver("div.path-listening-child");

  await expect(moved).toHaveText("moved 1 times");
  const sent = await page.evaluate(() => window.sentEvents.filter((name) => name === "mousemove"));
  expect(sent).toEqual(["mousemove"]);
});
