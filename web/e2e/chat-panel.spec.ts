import { expect, test } from "@playwright/test";

// The chat panel UI, with the OpenCode service mocked: this spec checks the
// wiring, not the LLM. Real sessions are exercised manually. The model is
// pinned server-side, so the panel never sends one.

test("the chat panel sends a message and shows the reply", async ({ page }) => {
  let sent: { text?: string; model?: unknown } = {};
  await page.route("**/chat", (route) => {
    sent = route.request().postDataJSON();
    return route.fulfill({ json: { reply: "Hello from the canvas" } });
  });

  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.getByTitle("Chat", { exact: true }).click();
  const panel = page.getByRole("dialog", { name: "Canvas chat" });
  await expect(panel.locator(".chat-model-label")).toHaveText(
    "DeepSeek 4.1 Flash",
  );
  await panel.getByPlaceholder("Talk to the agent…").fill("hi");
  await panel.getByRole("button", { name: "Send" }).click();

  await expect(panel.locator(".chat-bubble.user")).toHaveText("hi");
  await expect(panel.locator(".chat-bubble.assistant")).toHaveText(
    "Hello from the canvas",
  );
  expect(sent.text).toBe("hi");
  expect(sent.model).toBeUndefined();
});

test("chat failures are visible in the panel", async ({ page }) => {
  await page.route("**/chat", (route) =>
    route.fulfill({
      status: 502,
      json: { error: "This model is not available in your country" },
    }),
  );

  await page.goto("/");
  await page.waitForFunction(() => window.__rsReady === true);
  await page.getByTitle("Chat", { exact: true }).click();
  const panel = page.getByRole("dialog", { name: "Canvas chat" });
  await panel.getByPlaceholder("Talk to the agent…").fill("hi");
  await panel.getByRole("button", { name: "Send" }).click();

  await expect(panel.locator(".chat-bubble.error")).toContainText(
    "not available in your country",
  );
  await expect(panel.getByRole("button", { name: "Send" })).toBeEnabled();
});
