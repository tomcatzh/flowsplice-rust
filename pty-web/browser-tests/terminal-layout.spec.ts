import { test as base, expect, type Page } from "@playwright/test";

// Actual production xterm and FitAddon run in both engines. Only the native
// transport is replaced; every response uses the same batched bridge as iOS.
const test = base.extend<{ checkedTransport: void }>({
  checkedTransport: [async ({ page }, use) => {
    const errors: string[] = [];
    page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
    page.on("pageerror", error => errors.push(error.message));
    await page.addInitScript(() => {
      const w = window as any;
      w.testActions = [];
      w.FlowSpliceNative = { send: (json: string) => w.testActions.push(JSON.parse(json)) };
    });
    await page.goto("/");
    await page.waitForFunction(() => !!(window as any).flowsplice);
    await use();
    expect.soft(errors, "No transient terminal exception, even if a later retry succeeds").toEqual([]);
    expect(await page.evaluate(() => (window as any).testActions.filter((a: any) => ["disconnect", "disconnect_home"].includes(a.op))), "No unexpected Home teardown").toEqual([]);
  }, { auto: true }],
});
const homes = [
  { id: "mac", name: "Mac fixture", platform: "macos", relay: "127.0.0.1:7000", wanted: true },
  { id: "vps", name: "VPS fixture", platform: "linux", relay: "127.0.0.2:7000", wanted: true },
];
const session = (id: string) => ({ id, name: id, created_at_unix_secs: 10, writer: null, connection_count: 0 });
const protocol = (message: any, home_id = "mac") => ({ type: "protocol", home_id, message });
const response = (result: any, home = "mac", request_id = "fixture") => protocol({ type: "response", request_id, result }, home);
const output = (id: string, text: string, home = "mac") => protocol({ type: "output", attachment_id: `a-${home}-${id}`, data: Array.from(new TextEncoder().encode(text)) }, home);
const attached = (id: string, home = "mac", mode = "read_write") => response({ status: "attached", session: session(id), attachment_id: `a-${home}-${id}`, mode, writer_epoch: 1 }, home);
async function batch(page: Page, events: any[]) {
  await page.evaluate(events => (window as any).flowsplice.receiveBatch(events), events);
}
async function setup(page: Page, workspace: any = null) {
  await batch(page, [
    { type: "workspace", value: workspace }, { type: "platform", platform: "ios" },
    { type: "identity", installed: true, connected: true, busy: false }, { type: "homes", homes },
    ...homes.flatMap(h => [
      { type: "state", home_id: h.id, installed: true, connected: true, busy: false },
      protocol({ type: "hello", can_write: true }, h.id),
      response({ status: "session_details", sessions: [session("one"), session("two")] }, h.id),
    ]),
  ]);
}
async function management(page: Page, home = "mac") {
  await page.locator("#manage").click();
  await page.locator(`#home-nav button[data-home-id="${home}"]`).click();
}
async function open(page: Page, index = 0) {
  await page.locator("#list").getByRole("button", { name: "打开", exact: true }).nth(index).click();
}
async function complete(page: Page, id: string, home = "mac", mode = "read_write", text = `READY-${home}-${id}`) {
  const operation = await page.evaluate(() => (window as any).testActions.findLast((a: any) => ["join", "new_named"].includes(a.operation?.op))?.operation);
  await batch(page, [{ type: "submitted", home_id: home, request_id: "fixture", operation }, attached(id, home, mode), output(id, text, home)]);
}
async function healthy(page: Page) {
  await expect(page.locator("#terminal-view")).toBeVisible();
  await expect(page.locator("#terminal-notice")).toBeHidden();
  await expect(page.locator("#panes > .terminal:not([hidden]) .xterm-screen")).toBeVisible();
  const sizes = await page.evaluate(() => (window as any).testActions.filter((a: any) => a.operation?.op === "resize").map((a: any) => a.operation));
  expect(sizes.length).toBeGreaterThan(0);
  for (const size of sizes) {
    expect(Number.isInteger(size.columns) && size.columns >= 2 && size.columns <= 512).toBe(true);
    expect(Number.isInteger(size.rows) && size.rows >= 1 && size.rows <= 256).toBe(true);
  }
}
async function terminalText(page: Page, text: string) {
  await expect(page.locator("#panes > .terminal:not([hidden]) .xterm-rows")).toContainText(text);
}

test("Open batches submitted, attached and output while management is visible", async ({ page }) => {
  await setup(page);
  await page.locator('#home-cards button[data-home-id="mac"]').click();
  await open(page);
  await complete(page, "one");
  await healthy(page);
  await terminalText(page, "READY-mac-one");
});

test("named creation uses real dialog and opens in the same native batch", async ({ page }) => {
  await setup(page);
  await page.locator('#home-cards button[data-home-id="mac"]').click();
  await page.locator("#new").click();
  await page.locator("#session-name").fill("Daily work");
  await page.locator("#confirm-new").click();
  expect(await page.evaluate(() => (window as any).testActions.some((a: any) => a.operation?.op === "new_named" && a.operation.name === "Daily work"))).toBe(true);
  await complete(page, "created");
  await healthy(page);
  await terminalText(page, "READY-mac-created");
});

test("second tab, hidden output and Home return retain both terminals", async ({ page }) => {
  await setup(page);
  await page.locator('#home-cards button[data-home-id="mac"]').click();
  await open(page); await complete(page, "one");
  await management(page); await open(page); await complete(page, "two");
  await batch(page, [output("one", "\r\nINACTIVE-PRESERVED")]);
  await page.locator('#tabs button[data-session-id="one"]').click();
  await healthy(page); await terminalText(page, "INACTIVE-PRESERVED");
  await page.locator("#manage").click();
  await page.locator('#tabs button[data-session-id="two"]').click();
  await healthy(page); await terminalText(page, "READY-mac-two");
});

test("startup restores multiple Homes and buffers inactive output before first display", async ({ page }) => {
  await setup(page, { version: 1, classMode: false, identityWanted: false, homes,
    tabs: [{ home: "mac", session: "one", name: "one", mode: "read_write", deleted: false }, { home: "vps", session: "two", name: "two", mode: "read_only", deleted: false }],
    active: { home: "mac", session: "one" }, selected: "mac", page: "terminal" });
  await batch(page, [attached("two", "vps", "read_only"), output("two", "HIDDEN-VPS-OUTPUT", "vps"), attached("one"), output("one", "RESTORED-MAC")]);
  await healthy(page); await terminalText(page, "RESTORED-MAC");
  await page.locator('#tabs button[data-home-id="vps"]').click();
  await healthy(page); await terminalText(page, "HIDDEN-VPS-OUTPUT");
  await expect(page.locator("#mode-label")).toHaveText("只读");
});

test("separate output batch, background transitions and portrait landscape layout", async ({ page }) => {
  await setup(page);
  await page.locator('#home-cards button[data-home-id="mac"]').click(); await open(page);
  await batch(page, [attached("one")]);
  await healthy(page);
  await batch(page, [{ type: "lifecycle", active: false }, output("one", "BACKGROUND-OUTPUT")]);
  await page.setViewportSize({ width: 1133, height: 744 });
  await batch(page, [{ type: "lifecycle", active: true }]);
  await healthy(page); await terminalText(page, "BACKGROUND-OUTPUT");
  await page.setViewportSize({ width: 390, height: 844 });
  await batch(page, [output("one", "\r\nPHONE-OUTPUT")]);
  await healthy(page); await terminalText(page, "PHONE-OUTPUT");
});

test("real terminal keyboard input obeys read-only and read-write attachment modes", async ({ page }) => {
  await setup(page);
  await page.locator('#home-cards button[data-home-id="mac"]').click(); await open(page);
  await complete(page, "one", "mac", "read_only");
  await page.locator("#panes > .terminal:not([hidden]) .xterm-helper-textarea").focus(); await page.keyboard.type("readonly");
  expect(await page.evaluate(() => (window as any).testActions.filter((a: any) => a.operation?.op === "input"))).toEqual([]);
  await batch(page, [protocol({ type: "ownership", session_id: "one", epoch: 2, writer: { attachment_id: "a-mac-one", label: "fixture" } })]);
  await page.locator("#panes > .terminal:not([hidden]) .xterm-helper-textarea").focus(); await page.keyboard.type("w");
  await expect.poll(() => page.evaluate(() => (window as any).testActions.filter((a: any) => a.operation?.op === "input").length)).toBeGreaterThan(0);
  await healthy(page);
});

test("genuine detach and error remain warnings; ended sessions remain deleted", async ({ page }) => {
  await setup(page);
  await page.locator('#home-cards button[data-home-id="mac"]').click(); await open(page); await complete(page, "one");
  await batch(page, [protocol({ type: "detached", attachment_id: "a-mac-one" })]);
  await expect(page.locator("#terminal-notice")).toContainText("连接中断");
  await expect(page.locator("#terminal-notice")).toHaveAttribute("data-tone", "warning");
  await batch(page, [{ type: "error", home_id: "mac", message: "Fixture connection refused" }]);
  await expect(page.locator("#terminal-notice")).toContainText("Fixture connection refused");
  await batch(page, [protocol({ type: "session_ended", session_id: "one" })]);
  await expect(page.locator("#mode-label")).toHaveText("已删除");
  await expect(page.locator("#mode")).toBeDisabled();
});
