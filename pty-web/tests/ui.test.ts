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
    reset = vi.fn();
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

function classIdentity(connected = false, installed = false, busy = false) {
  window.flowsplice.receive({
    type: "identity",
    label: "My Mac · PTY",
    connected,
    installed,
    busy,
  });
}
function classCatalog(ids = ["one", "two"]) {
  window.flowsplice.receive({
    type: "homes",
    homes: ids.map((id) => ({
      id: JSON.stringify([id, "pty"]),
      home_id: id,
      name: `Home ${id}`,
      service_id: "pty",
      platform: "host",
      relay: "",
    })),
  });
}
test("service class enrollment happens once at app level and connects shared runtime", () => {
  classIdentity();
  classCatalog([]);
  actions = [];
  expect(el("identity-panel").hidden).toBe(false);
  expect(el("identity-label").textContent).toBe("My Mac · PTY");
  (el("identity-relay") as HTMLInputElement).value = "relay.example:7000";
  (el("identity-password") as HTMLInputElement).value = "private-password";
  submit("identity-access");
  expect(actions).toEqual([
    { op: "enroll", relay: "relay.example:7000", password: "private-password" },
  ]);
  window.flowsplice.receive({
    type: "progress",
    progress: { verification_code: "123456" },
  });
  expect(el("identity-code").textContent).toBe("123456");
  classIdentity(false, true, false);
  expect(actions.at(-1)).toEqual({ op: "connect", password: "" });
  expect(ops("new_named")).toHaveLength(0);
});
test("class targets use scoped actions without enrollment or passwords", () => {
  classIdentity(true, true);
  classCatalog();
  actions = [];
  (document.querySelector("#home-cards button") as HTMLButtonElement).click();
  const id = JSON.stringify(["one", "pty"]);
  expect(actions.at(-1)).toEqual({ op: "connect_home", home_id: id });
  expect(el("password-label").hidden).toBe(true);
  state(id, true);
  send({ type: "protocol", message: { type: "hello", can_write: true } }, id);
  attach(id);
  terminals.at(-1).onInput("hello");
  expect(actions.at(-1)).toMatchObject({
    op: "operation_home",
    home_id: id,
    operation: { op: "input" },
  });
  click("manage");
  click("disconnect");
  expect(
    actions.some((a) => a.op === "disconnect_home" && a.home_id === id),
  ).toBe(true);
  expect(actions.some((a) => a.op === "enroll" || a.op === "disconnect")).toBe(
    false,
  );
});
test("class catalog refresh preserves open tabs while a target is unavailable", () => {
  classIdentity(true, true);
  classCatalog();
  const one = JSON.stringify(["one", "pty"]),
    two = JSON.stringify(["two", "pty"]);
  state(one);
  state(two);
  attach(one);
  attach(two);
  classCatalog(["one", "two", "three"]);
  expect(terminals.every((t) => t.dispose.mock.calls.length === 0)).toBe(true);
  expect(document.querySelectorAll("#home-cards button")).toHaveLength(3);
  expect(el("tabs").children).toHaveLength(2);
  classCatalog(["two", "three"]);
  expect(terminals[0].dispose).not.toHaveBeenCalled();
  expect(terminals[1].dispose).not.toHaveBeenCalled();
  expect(el("tabs").children).toHaveLength(2);
  expect((el("tabs").firstElementChild as HTMLElement).dataset.state).toBe("reconnecting");
  terminals[1].onInput("still connected");
  expect(actions.at(-1)).toMatchObject({ op: "operation_home", home_id: two });
});
test("class global recovery sends only shared connection action", () => {
  classIdentity(false, true);
  classCatalog([]);
  actions = [];
  window.flowsplice.receive({
    type: "error",
    code: "credential_required",
    message: "stored password missing",
  });
  expect((el("recovery") as HTMLDialogElement).open).toBe(true);
  (el("recovery-password") as HTMLInputElement).value = "recovered";
  submit("recovery-form");
  expect(actions.at(-1)).toEqual({ op: "connect", password: "recovered" });
});
test("class global disconnect closes all tabs without any automatic rejoin", () => {
  classIdentity(true, true);
  classCatalog();
  const one = JSON.stringify(["one", "pty"]);
  state(one);
  attach(one);
  actions = [];
  click("identity-disconnect");
  classIdentity(false, true);
  expect(terminals[0].dispose).toHaveBeenCalledOnce();
  classIdentity(true, true);
  classCatalog();
  expect(
    actions.some((a) => a.op === "connect_home" || a.operation?.op === "join"),
  ).toBe(false);
});
test("class catalog does not overwrite native macOS platform info", () => {
  window.flowsplice.receive({ type: "platform", platform: "macos" });
  classIdentity();
  classCatalog([]);
  expect(el("keys").hidden).toBe(true);
  expect(el("keys").children).toHaveLength(0);
});

test("native press target survives repeated catalog and other Home events and still selects its Home", () => {
  classIdentity(true, true);
  classCatalog();
  const one = JSON.stringify(["one", "pty"]),
    two = JSON.stringify(["two", "pty"]);
  state(one);
  state(two);
  attach(one);
  const second = Array.from(
    document.querySelectorAll<HTMLButtonElement>("#home-cards button"),
  ).find((b) => b.dataset.homeId === two)!;
  const navigation = Array.from(
    document.querySelectorAll<HTMLButtonElement>("#home-nav button"),
  ).find((b) => b.dataset.homeId === two)!;
  const terminal = el("tabs").firstElementChild as HTMLButtonElement;
  second.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
  for (let i = 0; i < 3; i++) {
    classCatalog();
    info(one);
    send({ type: "state", installed: true, connected: true, busy: false }, one);
  }
  expect(second.isConnected).toBe(true);
  expect(document.querySelectorAll("#home-cards button")[1]).toBe(second);
  expect(document.querySelectorAll("#home-nav button")[1]).toBe(navigation);
  expect(el("tabs").firstElementChild).toBe(terminal);
  second.dispatchEvent(new MouseEvent("mouseup", { bubbles: true }));
  second.click();
  expect(el("home-name").textContent).toBe("Home two");
  expect(el("app").dataset.page).toBe("manage");
  terminal.click();
  expect(el("terminal-title").textContent).toContain("Home one");
  expect(el("app").dataset.page).toBe("terminal");
});
test("catalog changes update retained controls and remove only absent target nodes", () => {
  classIdentity(true, true);
  classCatalog();
  const one = JSON.stringify(["one", "pty"]),
    two = JSON.stringify(["two", "pty"]);
  state(one);
  state(two);
  attach(one);
  attach(two);
  const cards = Array.from(
    document.querySelectorAll<HTMLButtonElement>("#home-cards button"),
  );
  const first = cards[0],
    second = cards[1],
    tabsBefore = Array.from(el("tabs").children);
  window.flowsplice.receive({
    type: "homes",
    homes: [
      {
        id: two,
        home_id: "two",
        service_id: "pty",
        name: "Renamed VPS",
        platform: "host",
        relay: "",
      },
      {
        id: JSON.stringify(["three", "pty"]),
        home_id: "three",
        service_id: "pty",
        name: "Third",
        platform: "host",
        relay: "",
      },
    ],
  });
  expect(first.isConnected).toBe(true);
  expect(document.querySelectorAll("#home-cards button")[1]).toBe(second);
  expect(second.getAttribute("aria-label")).toBe("Renamed VPS");
  expect(el("tabs").children).toHaveLength(2);
  expect(el("tabs").children[1]).toBe(tabsBefore[1]);
  expect(tabsBefore[0].isConnected).toBe(true);
  expect((tabsBefore[0] as HTMLElement).dataset.state).toBe("reconnecting");
  second.click();
  expect(el("home-name").textContent).toBe("Renamed VPS");
  (tabsBefore[1] as HTMLButtonElement).click();
  expect(el("terminal-title").textContent).toContain("Renamed VPS");
});
test("rename routes Home, waits for Ok, refreshes details and allows error retry", () => {
  info();
  info("vps");
  attach("mac", "read_only");
  click("rename-terminal");
  expect((el("rename-name") as HTMLInputElement).value).toBe("日常开发");
  (el("rename-name") as HTMLInputElement).value = "renamed";
  submit("rename-form");
  submit("rename-form");
  expect(ops("rename")).toHaveLength(1);
  expect(ops("rename")[0]).toMatchObject({
    home_id: "mac",
    operation: { session_id: "session", name: "renamed" },
  });
  send({
    type: "submitted",
    request_id: "rename1",
    operation: ops("rename")[0].operation,
  });
  response({ status: "ok" }, "vps", "rename1");
  expect((el("rename-session") as HTMLDialogElement).open).toBe(true);
  response({ status: "error", message: "retry" }, "mac", "rename1");
  expect(el("rename-error").textContent).toBe("retry");
  submit("rename-form");
  expect(ops("rename")).toHaveLength(2);
  send({
    type: "submitted",
    request_id: "rename2",
    operation: ops("rename")[1].operation,
  });
  info();
  actions = [];
  response({ status: "ok" }, "mac", "rename2");
  expect((el("rename-session") as HTMLDialogElement).open).toBe(false);
  expect(ops("list_details")).toHaveLength(1);
  expect(el("tabs").textContent).toContain("日常开发");
  response({
    status: "session_details",
    sessions: [
      { id: "session", name: "renamed", writer: null, created_at_unix_secs: 1 },
    ],
  });
  expect(el("tabs").textContent).toContain("renamed");
});
test("rename cancel, Escape, invalid names and ended targets never mutate", () => {
  info();
  attach();
  click("rename-terminal");
  click("cancel-rename");
  submit("rename-form");
  click("rename-terminal");
  el("rename-session").dispatchEvent(new Event("cancel"));
  submit("rename-form");
  click("rename-terminal");
  for (const name of [" ", "x".repeat(65), "bad\u0001name"]) {
    (el("rename-name") as HTMLInputElement).value = name;
    submit("rename-form");
  }
  expect(ops("rename")).toHaveLength(0);
  send({
    type: "protocol",
    message: { type: "session_ended", session_id: "session" },
  });
  (el("rename-name") as HTMLInputElement).value = "valid";
  submit("rename-form");
  expect(ops("rename")).toHaveLength(0);
});
test("rename requires Home write permission and a connected target", () => {
  info();
  attach();
  send({ type: "protocol", message: { type: "hello", can_write: false } });
  expect((el("rename-terminal") as HTMLButtonElement).disabled).toBe(true);
  click("rename-terminal");
  expect((el("rename-session") as HTMLDialogElement).open).toBe(false);
  send({ type: "protocol", message: { type: "hello", can_write: true } });
  click("rename-terminal");
  state("mac", false);
  submit("rename-form");
  expect(ops("rename")).toHaveLength(0);
});
test("pending rename locks dismissal until response; disconnect releases another Home", () => {
  info();
  info("vps");
  attach();
  click("rename-terminal");
  submit("rename-form");
  send({
    type: "submitted",
    request_id: "pending",
    operation: ops("rename")[0].operation,
  });
  click("cancel-rename");
  const escape = new Event("cancel", { cancelable: true });
  el("rename-session").dispatchEvent(escape);
  expect(escape.defaultPrevented).toBe(true);
  expect((el("rename-name") as HTMLInputElement).disabled).toBe(true);
  send({ type: "error", message: "unrelated" });
  submit("rename-form");
  expect(ops("rename")).toHaveLength(1);
  state("mac", false);
  expect((el("rename-session") as HTMLDialogElement).open).toBe(false);
  attach("vps");
  click("rename-terminal");
  expect((el("rename-session") as HTMLDialogElement).open).toBe(true);
  expect((el("rename-name") as HTMLInputElement).disabled).toBe(false);
});
test("successful rename schedules fresh details after an already pending response", () => {
  info();
  attach();
  click("rename-terminal");
  submit("rename-form");
  send({
    type: "submitted",
    request_id: "rename",
    operation: ops("rename")[0].operation,
  });
  actions = [];
  response({ status: "ok" }, "mac", "rename");
  expect(ops("list_details")).toHaveLength(0);
  info();
  expect(ops("list_details")).toHaveLength(1);
  info();
  expect(ops("list_details")).toHaveLength(1);
});

test("rename selects the complete existing name without changing it", () => {
  info();
  attach();
  click("rename-terminal");
  const input = el("rename-name") as HTMLInputElement;
  expect(input.value).toBe("日常开发");
  expect(input.selectionStart).toBe(0);
  expect(input.selectionEnd).toBe(input.value.length);
  expect(document.activeElement).toBe(input);
  expect(ops("rename")).toHaveLength(0);
});
