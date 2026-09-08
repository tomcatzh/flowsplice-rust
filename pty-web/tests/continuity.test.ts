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
    reset = vi.fn();
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
const ops = (name: string) => actions.filter((a) => a.operation?.op === name);
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

});
afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
});
const native = (op: string) => actions.filter(a => a.op === op);
const saved = () => native("save_workspace").at(-1)?.value;
const tab = (session = "session") => Array.from(document.querySelectorAll<HTMLButtonElement>("#tabs button")).find(b => b.dataset.sessionId === session && b.dataset.homeId === "mac")!;
const homes = [
  { id: "mac", name: "Mac", platform: "macos", relay: "127.0.0.1:7000", wanted: true },
  { id: "vps", name: "VPS", platform: "linux", relay: "127.0.0.2:7000", wanted: false },
];
function workspace(extra: any = {}) {
  window.flowsplice.receive({ type: "workspace", value: {
    version: 1, classMode: false, identityWanted: false, homes,
    tabs: [{ home: "mac", session: "session", name: "Original", mode: "read_write", deleted: false }],
    active: { home: "mac", session: "session" }, selected: "mac", page: "terminal", ...extra,
  } });
}
function catalog(value: any[] = homes) { window.flowsplice.receive({ type: "homes", platform: "macos", homes: value }); }
function protocol(message: any) { send({ type: "protocol", message }); }
function details(ids = ["session"]) { response({ status: "session_details", sessions: ids.map(id => ({ id, name: id === "session" ? "Original" : id, created_at_unix_secs: 1, writer: null, connection_count: 0 })) }); }
function ready(ids = ["session"]) { state(); protocol({ type: "hello", can_write: true }); details(ids); }
function identity(connected: boolean) { window.flowsplice.receive({ type: "identity", installed: true, connected, busy: false }); }

test("fresh class installation waits for native identity before saving its workspace namespace", () => {
  window.flowsplice.receive({ type: "workspace", value: null });
  window.flowsplice.receive({ type: "lifecycle", active: false });
  window.flowsplice.receive({ type: "platform", platform: "ios" });
  expect(native("save_workspace")).toHaveLength(0);
  identity(false);
  expect(saved().classMode).toBe(true);
  expect(native("save_workspace").every(a => a.value.classMode)).toBe(true);
});

test("restore connects only wanted Homes after catalog, joins only after hello and authoritative details, never replays input", () => {
  workspace(); catalog(); state("mac", false); state("vps", false);
  vi.advanceTimersByTime(1000);
  expect(native("connect")).toEqual([{ op: "connect", home_id: "mac", password: "" }]);
  expect(ops("join")).toHaveLength(0);
  state(); expect(ops("join")).toHaveLength(0);
  protocol({ type: "hello", can_write: true }); expect(ops("join")).toHaveLength(0);
  terminals[0].onInput("offline command\r"); details();
  expect(ops("join")).toHaveLength(1);
  expect(ops("join")[0]).toMatchObject({ home_id: "mac", operation: { session_id: "session", mode: "read_write" } });
  const original = tab(); attach("mac", "read_only");
  expect(tab()).toBe(original); expect(tab().dataset.state).toBe("attached");
  expect(el("mode-label").textContent).toBe("只读");
  expect(saved().active).toEqual({ home: "mac", session: "session" });
  expect(saved().tabs[0].mode).toBe("read_only");
  terminals[0].onInput("still blocked");
  expect(ops("input")).toHaveLength(0); expect(ops("new_named")).toHaveLength(0);
  expect(actions.some(a => a.operation?.force === true)).toBe(false);
});

test("read-only restoration and delayed attachment responses retain saved active session", () => {
  workspace({ tabs: [
    { home: "mac", session: "session", name: "Original", mode: "read_only", deleted: false },
    { home: "mac", session: "second", name: "Second", mode: "read_write", deleted: false },
  ] });
  catalog(); ready(["session", "second"]);
  expect(ops("join").find(a => a.operation.session_id === "session").operation.mode).toBe("read_only");
  attach("mac", "read_only");
  response({ status: "attached", session: { id: "second", created_at_unix_secs: 1, writer: null }, attachment_id: "second-attachment", mode: "read_write", writer_epoch: 7 });
  expect(saved().active).toEqual({ home: "mac", session: "session" });
  expect(el("terminal-title").textContent).toContain("Original");
  expect(el("mode-label").textContent).toBe("只读");
});

test("online SessionEnded retains deleted tab and suppresses late output, input and reconnect", () => {
  workspace(); catalog(); ready(); attach(); actions = [];
  protocol({ type: "session_ended", session_id: "session" });
  expect(tab().dataset.state).toBe("deleted"); expect(el("mode-label").textContent).toBe("已删除");
  expect((el("mode") as HTMLButtonElement).disabled).toBe(true);
  expect(saved().tabs[0].deleted).toBe(true);
  protocol({ type: "output", attachment_id: "attachment", data: [65] }); terminals[0].onInput("must not write");
  expect(terminals[0].write).not.toHaveBeenCalled(); expect(ops("input")).toHaveLength(0);
  state("mac", false); ready(); vi.advanceTimersByTime(30000);
  expect(ops("join")).toHaveLength(0); expect(ops("new_named")).toHaveLength(0);
  expect(tab().dataset.state).toBe("deleted");
});

test("authoritative absence deletes restored session but unavailable catalog retains intent", () => {
  workspace({ classMode: true, identityWanted: true }); identity(true);
  catalog([]); vi.advanceTimersByTime(5000);
  expect(tab().dataset.state).toBe("reconnecting"); expect(saved().tabs[0].deleted).toBe(false);
  expect(saved().homes.find((h: any) => h.id === "mac").wanted).toBe(true);
  expect(native("connect_home")).toHaveLength(0);
  catalog(); ready([]);
  expect(tab().dataset.state).toBe("deleted"); expect(ops("join")).toHaveLength(0);
});

test("background pauses retries and class identity restores using keychain without enrollment", () => {
  workspace({ classMode: true, identityWanted: true }); catalog(); identity(false);
  window.flowsplice.receive({ type: "lifecycle", active: false }); vi.advanceTimersByTime(60000);
  expect(native("connect")).toHaveLength(0); expect(native("connect_home")).toHaveLength(0);
  window.flowsplice.receive({ type: "lifecycle", active: true });
  expect(native("connect")).toEqual([{ op: "connect", password: "" }]);
  identity(true); vi.advanceTimersByTime(1000);
  expect(native("connect_home")).toEqual([{ op: "connect_home", home_id: "mac" }]);
  expect(native("enroll")).toHaveLength(0); expect(ops("new_named")).toHaveLength(0);
});

test("explicit tab close and Home disconnect remove restoration intent", () => {
  workspace(); catalog(); ready(); attach(); click("detach");
  expect(saved().tabs).toEqual([]);
  state("mac", false); ready(); expect(ops("join")).toHaveLength(1);
  click("manage"); click("disconnect");
  expect(saved().homes.find((h: any) => h.id === "mac").wanted).toBe(false);
  actions = []; state("mac", false); vi.advanceTimersByTime(60000);
  expect(native("connect")).toHaveLength(0); expect(ops("join")).toHaveLength(0);
});

test("failed Home retries back off to at most thirty seconds and stop in background", () => {
  workspace(); catalog(); state("mac", false);
  vi.advanceTimersByTime(1000);
  expect(native("connect")).toHaveLength(1);
  // Native clears busy after each failed attempt. The delay grows, then caps at 30s.
  for (const delay of [1000, 2000, 4000, 8000, 16000, 30000, 30000]) {
    state("mac", false);
    const count = native("connect").length;
    vi.advanceTimersByTime(delay - 1);
    expect(native("connect")).toHaveLength(count);
    vi.advanceTimersByTime(1);
    expect(native("connect")).toHaveLength(count + 1);
  }
  state("mac", false);
  window.flowsplice.receive({ type: "lifecycle", active: false });
  const count = native("connect").length;
  vi.advanceTimersByTime(60000);
  expect(native("connect")).toHaveLength(count);
});


test("closing a tab during restore detaches the late attachment without reopening it", () => {
  workspace(); catalog(); ready();
  expect(ops("join")).toHaveLength(1);
  click("detach");
  attach();
  expect(document.querySelectorAll("#tabs button")).toHaveLength(0);
  expect(ops("detach").at(-1)?.operation.attachment_id).toBe("attachment");
  expect(saved().tabs).toEqual([]);
});

test("workspace rejects malformed metadata instead of reconnecting or replaying arbitrary actions", async () => {
  const { readWorkspace } = await import("../src/workspace");
  workspace(); catalog(); ready(); attach("mac", "read_only");
  const valid = saved();
  expect(readWorkspace(valid), JSON.stringify(valid)).not.toBeNull();
  for (const invalid of [
    {...valid, password:"secret"}, {...valid, version:2},
    {...valid, homes:[valid.homes[0], valid.homes[0]]},
    {...valid, tabs:[{...valid.tabs[0], mode:"force"}]},
    {...valid, tabs:[{...valid.tabs[0], home:"not-in-catalog"}]},
    {...valid, tabs:[{...valid.tabs[0], output:[65]}]},
    {...valid, active:{home:"mac", session:"missing"}},
  ]) expect(readWorkspace(invalid)).toBeNull();
});

test("closing a pending restore does not cancel a fresh explicit join after reconnect", () => {
  workspace(); catalog(); ready();
  expect(ops("join")).toHaveLength(1);
  click("detach");
  expect(saved().tabs).toEqual([]);
  // The old connection dies before its pending Attached response can arrive.
  state("mac", false); ready();
  expect(ops("join")).toHaveLength(1);
  const reopen = Array.from(document.querySelectorAll<HTMLButtonElement>("#list [data-session-id=\"session\"] button"))
    .find(button => button.textContent === "打开")!;
  expect(reopen).toBeDefined();
  expect(reopen.disabled).toBe(false);
  reopen.click();
  expect(ops("join")).toHaveLength(2);
  expect(ops("join")[1].operation).toMatchObject({ session_id: "session", mode: "read_write" });
  response({
    status: "attached",
    session: { id: "session", created_at_unix_secs: 1, writer: null },
    attachment_id: "fresh-after-reconnect", mode: "read_write", writer_epoch: 8,
  });
  expect(ops("detach").filter(action => action.operation.attachment_id === "fresh-after-reconnect")).toHaveLength(0);
  expect(tab().dataset.state).toBe("attached");
  expect(saved().tabs).toHaveLength(1);
});
