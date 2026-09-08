import { afterEach, beforeEach, expect, test, vi } from "vitest";
const terminals: any[] = [];
vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    parser = { registerOscHandler: vi.fn() };
    onInput: any;
    options: any;
    constructor(options: any) {
      this.options = options;
      terminals.push(this);
    }
    loadAddon() {}
    open() {}
    onData(fn: any) {
      this.onInput = fn;
    }
    onResize() {}
    resize() {}
    focus() {}
    dispose = vi.fn();
    write = vi.fn((_data: any, callback?: () => void) => callback?.());
  },
}));
vi.mock("@xterm/addon-fit", () => ({
  FitAddon: class {
    proposeDimensions() {
      return { cols: 80, rows: 24 };
    }
  },
}));
let actions: any[] = [];
const el = (id: string) => document.getElementById(id)!;
const click = (id: string) => (el(id) as HTMLButtonElement).click();
const send = (event: any, home_id = "mac") =>
  window.flowsplice.receive({ ...event, home_id });
const response = (result: any, home = "mac", request_id = "r") =>
  send(
    { type: "protocol", message: { type: "response", request_id, result } },
    home,
  );
const state = (
  home = "mac",
  connected = true,
  installed = true,
  busy = false,
) => send({ type: "state", installed, connected, busy }, home);
const home = (id = "mac") =>
  (
    document.querySelector(
      `#home-cards [data-home-id="${id}"]`,
    ) as HTMLButtonElement
  ).click();
const ops = (name: string) => actions.filter((a) => a.operation?.op === name);
const submit = (id: string) =>
  el(id).dispatchEvent(
    new Event("submit", { bubbles: true, cancelable: true }),
  );
function attach(home = "mac", mode = "read_write") {
  response(
    {
      status: "attached",
      session: { id: "session", created_at_unix_secs: 10, writer: null },
      attachment_id: "attachment",
      mode,
      writer_epoch: 3,
    },
    home,
  );
}
function info(home = "mac") {
  response(
    {
      status: "session_details",
      sessions: [
        {
          id: "session",
          name: home === "mac" ? "日常开发" : "服务日志",
          created_at_unix_secs: 1750000000,
          last_connected_at_unix_secs: 1750010000,
          connection_count: 2,
          writer: {
            attachment_id: "other",
            travel_id: "travel",
            label: "iPad mini",
          },
        },
      ],
    },
    home,
  );
}
beforeEach(async () => {
  vi.useFakeTimers();
  vi.resetModules();
  terminals.length = 0;
  actions = [];
  delete window.webkit;
  delete window.flowsplicePlatform;
  document.body.innerHTML = '<main id="app"></main>';
  window.FlowSpliceNative = { send: (json) => actions.push(JSON.parse(json)) };
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
    },
  );
  HTMLDialogElement.prototype.showModal = function () {
    this.open = true;
  };
  HTMLDialogElement.prototype.close = function () {
    this.open = false;
  };
  await import("../src/main");
  send({
    type: "homes",
    platform: "ios",
    homes: [
      {
        id: "mac",
        name: "Mac 工作站",
        platform: "macos",
        relay: "127.0.0.1:7000",
      },
      {
        id: "vps",
        name: "香港 VPS",
        platform: "linux",
        relay: "127.0.0.2:7000",
      },
    ],
  });
  state();
  state("vps");
  for (const id of ["mac", "vps"])
    send({ type: "protocol", message: { type: "hello", can_write: true } }, id);
  home();
});
afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
});
test("named create requires valid confirmation, remains guarded until response", () => {
  actions = [];
  click("new");
  expect(ops("new_named")).toHaveLength(0);
  (el("session-name") as HTMLInputElement).value = " ".repeat(3);
  submit("new-form");
  expect(ops("new_named")).toHaveLength(0);
  expect(el("name-error").textContent).toContain("64");
  (el("session-name") as HTMLInputElement).value = "😀".repeat(65);
  submit("new-form");
  expect(ops("new_named")).toHaveLength(0);
  (el("session-name") as HTMLInputElement).value = " 日常开发 ";
  submit("new-form");
  submit("new-form");
  expect(ops("new_named")).toHaveLength(1);
  expect(ops("new_named")[0]).toMatchObject({
    home_id: "mac",
    operation: { name: "日常开发" },
  });
  send({
    type: "submitted",
    request_id: "new",
    operation: ops("new_named")[0].operation,
  });
  response({ status: "error", message: "failed" }, "mac", "new");
  expect((el("new") as HTMLButtonElement).disabled).toBe(false);
});
test("cancel never creates session", () => {
  click("new");
  click("cancel-new");
  expect(ops("new_named")).toHaveLength(0);
});
test("same attachment and session IDs across Homes stay isolated", () => {
  attach();
  attach("vps");
  expect(terminals).toHaveLength(2);
  send(
    {
      type: "protocol",
      message: {
        type: "ownership",
        session_id: "session",
        epoch: 8,
        writer: null,
      },
    },
    "mac",
  );
  terminals[0].onInput("blocked");
  terminals[1].onInput("hello");
  expect(ops("input")).toHaveLength(1);
  expect(ops("input")[0].home_id).toBe("vps");
  send(
    {
      type: "protocol",
      message: { type: "output", attachment_id: "attachment", data: [65] },
    },
    "mac",
  );
  expect(terminals[0].write).toHaveBeenCalled();
  expect(terminals[1].write).not.toHaveBeenCalled();
});
test("disconnect one Home preserves another terminal and its input", () => {
  attach();
  attach("vps");
  home("mac");
  click("disconnect");
  expect(terminals[0].dispose).toHaveBeenCalledOnce();
  expect(terminals[1].dispose).not.toHaveBeenCalled();
  terminals[1].onInput("still works");
  expect(ops("input").at(-1).home_id).toBe("vps");
  expect(el("tabs").children).toHaveLength(1);
});
test("management and switcher navigation preserve connections and detach only local tab", () => {
  attach();
  attach("vps");
  actions = [];
  click("manage");
  click("switcher");
  click("mobile-home");
  expect(actions.some((a) => a.op === "disconnect")).toBe(false);
  expect(terminals.every((t) => t.dispose.mock.calls.length === 0)).toBe(true);
  (el("tabs").children[0] as HTMLButtonElement).click();
  click("detach");
  expect(ops("detach")[0].home_id).toBe("mac");
  expect(ops("input")).toHaveLength(0);
  expect(terminals[1].dispose).not.toHaveBeenCalled();
});
test("metadata and friendly names survive fallback List response", () => {
  info();
  expect(el("list").textContent).toContain("日常开发");
  expect(el("list").textContent).toContain("2 个连接");
  expect(el("list").textContent).toContain("iPad mini 写入");
  expect(el("list").textContent).not.toContain("尚未连接");
  response({
    status: "sessions",
    sessions: [
      { id: "session", created_at_unix_secs: 1750000000, writer: null },
    ],
  });
  expect(el("list").textContent).toContain("日常开发");
  attach();
  expect(el("tabs").textContent).toContain("Mac 工作站 / 日常开发");
});
test("connect uses empty password; recovery shown only for credential_required", () => {
  state("mac", false);
  home();
  expect(actions.at(-1)).toEqual({
    op: "connect",
    password: "",
    home_id: "mac",
  });
  expect(el("password-label").hidden).toBe(true);
  send({ type: "error", code: "transport", message: "offline" });
  expect((el("recovery") as HTMLDialogElement).open).toBe(false);
  send({
    type: "error",
    code: "credential_required",
    message: "missing stored key password",
  });
  expect((el("recovery") as HTMLDialogElement).open).toBe(true);
  (el("recovery-password") as HTMLInputElement).value = "private password";
  submit("recovery-form");
  expect(actions.at(-1)).toMatchObject({
    home_id: "mac",
    password: "private password",
  });
  expect((el("recovery-password") as HTMLInputElement).value).toBe("");
});
test("enrollment waits for approval then automatically connects without creating shell", () => {
  state("mac", false, false);
  home();
  (el("password") as HTMLInputElement).value = "password123456";
  submit("access");
  expect(actions.at(-1)).toMatchObject({
    op: "enroll",
    home_id: "mac",
    relay: "127.0.0.1:7000",
  });
  send({
    type: "progress",
    progress: { verification_code: "123456", phase: "waiting" },
  });
  expect(el("verification-code").textContent).toBe("123456");
  state("mac", false, true, false);
  expect(actions.at(-1)).toEqual({
    op: "connect",
    home_id: "mac",
    password: "",
  });
  expect(ops("new_named")).toHaveLength(0);
});
test("takeover warns and stale epoch requests a fresh handoff", () => {
  attach("mac", "read_only");
  click("mode");
  send({
    type: "submitted",
    request_id: "take",
    operation: actions.at(-1).operation,
  });
  response(
    {
      status: "takeover_required",
      session_id: "session",
      epoch: 4,
      writer: { label: "remote", travel_id: "travel" },
    },
    "mac",
    "take",
  );
  expect(el("warning").textContent).toContain("remote");
  send({
    type: "protocol",
    message: {
      type: "ownership",
      session_id: "session",
      epoch: 5,
      writer: null,
    },
  });
  click("confirm-takeover");
  expect(actions.at(-1).operation).toMatchObject({
    force: false,
    expected_epoch: null,
  });
  send({
    type: "submitted",
    request_id: "again",
    operation: actions.at(-1).operation,
  });
  response(
    {
      status: "takeover_required",
      session_id: "session",
      epoch: 5,
      writer: { label: "remote" },
    },
    "mac",
    "again",
  );
  click("confirm-takeover");
  expect(actions.at(-1).operation).toMatchObject({
    force: true,
    expected_epoch: 5,
  });
});
test("input is bounded, readonly suppressed and captured epoch remains stable", () => {
  attach("mac", "read_only");
  terminals[0].onInput("no");
  expect(ops("input")).toHaveLength(0);
  send({
    type: "protocol",
    message: {
      type: "ownership",
      session_id: "session",
      epoch: 9,
      writer: { attachment_id: "attachment" },
    },
  });
  terminals[0].onInput("€".repeat(6000));
  expect(ops("input")).toHaveLength(2);
  expect(
    ops("input").every(
      (a) => a.operation.data.length <= 16384 && a.operation.writer_epoch === 9,
    ),
  ).toBe(true);
  send({
    type: "protocol",
    message: {
      type: "ownership",
      session_id: "session",
      epoch: 10,
      writer: null,
    },
  });
  expect(ops("input")[0].operation.writer_epoch).toBe(9);
  expect(terminals[0].options.scrollback).toBe(0);
  expect(terminals[0].parser.registerOscHandler).toHaveBeenCalledWith(
    52,
    expect.any(Function),
  );
});
test("batch waits for terminal parsing before later ownership", async () => {
  attach();
  let parsed: (() => void) | undefined;
  terminals[0].write.mockImplementation((_data: any, callback: () => void) => {
    parsed = callback;
  });
  let done = false;
  const promise = window.flowsplice
    .receiveBatch([
      {
        type: "protocol",
        home_id: "mac",
        message: { type: "output", attachment_id: "attachment", data: [65] },
      },
      {
        type: "protocol",
        home_id: "mac",
        message: {
          type: "ownership",
          session_id: "session",
          epoch: 10,
          writer: null,
        },
      },
    ])
    .then(() => {
      done = true;
    });
  await Promise.resolve();
  expect(done).toBe(false);
  expect(el("mode-label").textContent).toBe("读写");
  parsed!();
  await promise;
  expect(el("mode-label").textContent).toBe("只读");
});
test("macOS never has virtual key buttons, mobile retains seven keys", () => {
  expect(el("keys").children).toHaveLength(7);
  send({ type: "homes", platform: "macos", homes: [] });
  expect(el("keys").hidden).toBe(true);
  expect(el("keys").children).toHaveLength(0);
});
test("visible or attached Homes refresh details periodically", () => {
  attach("vps");
  home("mac");
  info("mac");
  info("vps");
  actions = [];
  vi.advanceTimersByTime(5000);
  expect(
    ops("list_details")
      .map((a) => a.home_id)
      .sort(),
  ).toEqual(["mac", "vps"]);
});
test("ready is sent after bridge installation on either platform", () => {
  expect(actions[0]).toEqual({ op: "ready" });
  expect(typeof window.flowsplice.receiveBatch).toBe("function");
});
test("global configuration errors remain visible without a Home identifier", () => {
  window.flowsplice.receive({ type: "error", message: "invalid catalog" });
  expect(el("global-notice").hidden).toBe(false);
  expect(el("global-notice").textContent).toContain("invalid catalog");
});
test("terminal shows scoped errors and writer displacement with Home and session", () => {
  info();
  attach();
  send({ type: "error", message: "transport stalled" });
  expect(el("terminal-notice").hidden).toBe(false);
  expect(el("terminal-notice").textContent).toContain("Mac 工作站 / 日常开发");
  expect(el("terminal-notice").textContent).toContain("transport stalled");
  send({
    type: "protocol",
    message: {
      type: "ownership",
      session_id: "session",
      epoch: 8,
      writer: { attachment_id: "remote", label: "iPad mini" },
    },
  });
  expect(el("terminal-notice").textContent).toContain(
    "读写权限已被 iPad mini 接管",
  );
  expect(el("mode-label").textContent).toBe("只读");
});
test("takeover confirmation identifies Home and session", () => {
  info();
  attach("mac", "read_only");
  click("mode");
  send({
    type: "submitted",
    request_id: "take-context",
    operation: actions.at(-1).operation,
  });
  response(
    {
      status: "takeover_required",
      session_id: "session",
      epoch: 4,
      writer: { label: "remote" },
    },
    "mac",
    "take-context",
  );
  expect(el("warning").textContent).toContain("Mac 工作站 / 日常开发");
  expect(el("warning").textContent).toContain("remote");
});
test("output and ordinary Ok responses preserve chrome DOM identity", () => {
  attach();
  const tab = el("tabs").firstChild;
  send({
    type: "protocol",
    message: { type: "output", attachment_id: "attachment", data: [65] },
  });
  expect(el("tabs").firstChild).toBe(tab);
  response({ status: "ok" });
  expect(el("tabs").firstChild).toBe(tab);
});
test("details requests never accumulate while a Home is stalled", () => {
  info();
  actions = [];
  click("refresh");
  click("refresh");
  vi.advanceTimersByTime(30000);
  expect(ops("list_details")).toHaveLength(1);
  send({
    type: "submitted",
    request_id: "details",
    operation: { op: "list_details" },
  });
  response({ status: "error", message: "temporary failure" }, "mac", "details");
  click("refresh");
  expect(ops("list_details")).toHaveLength(2);
  state("mac", false);
  state("mac", true);
  expect(ops("list_details")).toHaveLength(3);
});
test("not connected closes only its Home new dialog and clears pending create", () => {
  click("new");
  state("vps", false);
  expect((el("new-session") as HTMLDialogElement).open).toBe(true);
  state("mac", false);
  expect((el("new-session") as HTMLDialogElement).open).toBe(false);
  state("mac", false);
  expect((el("new-session") as HTMLDialogElement).open).toBe(false);
});
test("cancelled or failed enrollment cannot trigger a later automatic connection", () => {
  state("mac", false, false);
  home();
  (el("password") as HTMLInputElement).value = "password123456";
  submit("access");
  state("mac", false, false, false);
  actions = [];
  state("mac", false, true, false);
  expect(actions.some((a) => a.op === "connect")).toBe(false);
  state("mac", false, false);
  home();
  submit("access");
  send({ type: "error", message: "cancelled" });
  actions = [];
  state("mac", false, true, false);
  expect(actions.some((a) => a.op === "connect")).toBe(false);
});
