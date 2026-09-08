import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
type Operation = {
  op: string;
  [key: string]: any;
};
type Session = {
  id: string;
  name?: string;
  created_at_unix_secs: number;
  last_connected_at_unix_secs?: number | null;
  connection_count?: number;
  writer: any;
};
type Home = {
  id: string;
  name: string;
  platform: string;
  relay: string;
  installed: boolean;
  connected: boolean;
  busy: boolean;
  canWrite: boolean;
  notice: string;
  code: string;
  sessions: Map<string, Session>;
  requests: Map<string, Operation>;
  joining: Set<string>;
  pendingNew: string | null;
  pendingNewId: string | null;
  connectAfterEnroll: boolean;
  detailsPending: boolean;
  detailsAgain?: boolean;
};
type Tab = {
  key: string;
  home: Home;
  session: string;
  id: string;
  mode: string;
  epoch: number;
  terminal: Terminal;
  fit: FitAddon;
  pane: HTMLElement;
  notice: string;
};
type Event = {
  type: string;
  [key: string]: any;
};
declare global {
  interface Window {
    flowsplicePlatform?: "macos" | "ios" | "android";
    FlowSpliceNative?: {
      send(json: string): void;
    };
    webkit?: {
      messageHandlers: {
        pty: {
          postMessage(json: string): void;
        };
      };
    };
    flowsplice: {
      receive(event: Event): void;
      receiveBatch(events: Event[]): Promise<void>;
    };
  }
}
let renaming: {
  home: string;
  session: string;
  pending: boolean;
  request?: string;
} | null = null;
const app = document.querySelector<HTMLElement>("#app")!;
app.innerHTML = `<header id="topbar">
<button id="manage" aria-label="Home 管理">⌘ Home</button>
<nav id="tabs" aria-label="已打开终端">
</nav>
<button id="switcher" aria-label="已打开终端">终端 <span id="tab-count">0</span>
</button>
</header>
<p id="global-notice" role="alert" hidden>
</p>
<div id="workspace">
<aside id="sidebar">
<h2>Home</h2>
<div id="home-nav">
</div>
<button id="add-home">＋ 添加 Home</button>
</aside>
<section id="management">
<section id="identity-panel" hidden>
<div class="heading"><div><h2 id="identity-label"></h2><p class="muted">所有 Home 的 PTY 服务</p><p id="identity-status" role="status"></p></div><button id="identity-disconnect" hidden>断开全部连接</button></div>
<form id="identity-access"><label id="identity-relay-label">Relay IP:端口<input id="identity-relay" autocomplete="off" required></label><label id="identity-password-label">设置私钥密码<input id="identity-password" type="password" autocomplete="new-password" minlength="12" required></label><p id="identity-password-note" class="muted">一次批准即可访问所有已授权的 Home 的 PTY 服务。私钥密码安全保存在此设备，之后无需重复输入。</p><button id="identity-enter" class="primary">注册此设备</button></form>
<div id="identity-waiting" hidden><p>等待超级 Home 批准</p><strong id="identity-code"></strong><button id="identity-cancel">取消注册</button></div>
</section>
<div id="overview">
<div class="heading">
<h1>Home</h1>
<button id="add-mobile">＋ 添加</button>
</div>
<p class="muted">你的设备与远程终端</p>
<div id="home-cards">
</div>
</div>
<div id="detail" hidden>
<button id="back-home" class="back" aria-label="返回 Home">‹ Home</button>
<div class="heading">
<div>
<h1 id="home-name">
</h1>
<p id="status">
</p>
</div>
<button id="disconnect" hidden>断开连接</button>
</div>
<p id="notice" role="status">
</p>
<section id="access-panel">
<h2 id="access-title">
</h2>
<p id="access-description" class="muted">
</p>
<form id="access">
<label id="relay-label">Relay IP:端口<input id="relay" autocomplete="off">
</label>
<label id="password-label">设置私钥密码<input id="password" type="password" autocomplete="new-password" minlength="12">
</label>
<button id="enter" class="primary">连接</button>
</form>
<div id="waiting" hidden>
<p>等待超级 Home 批准</p>
<strong id="verification-code">
</strong>
<p class="muted">请核对校验码后批准此设备。</p>
<button id="cancel-enroll">取消注册</button>
</div>
</section>
<section id="sessions" hidden>
<div class="actions">
<input id="search" aria-label="搜索会话" placeholder="搜索会话">
<button id="refresh" aria-label="刷新会话">↻</button>
<button id="new" class="primary">＋ 新建会话</button>
</div>
<div class="table-heading">
<span>会话名称</span>
<span>创建时间</span>
<span>最近连接</span>
<span>当前连接</span>
<span>操作</span>
</div>
<div id="list">
</div>
<p class="footnote">连接数包含读写与只读；关闭标签不会结束远端会话。</p>
</section>
</div>
</section>
<section id="terminal-view" hidden>
<div id="terminal-controls">
<button id="terminal-back" class="back" aria-label="Home 管理">‹</button>
<div id="terminal-title">
</div>
<span id="terminal-connections">
</span>
<button id="rename-terminal">重命名</button>
<button id="mode">
<span id="mode-label">
</span>
</button>
<button id="terminal-switcher" aria-label="切换终端">▣</button>
<button id="detach" title="关闭标签，不结束会话">关闭标签</button>
</div>
<p id="terminal-notice" role="status" hidden>
</p>
<section id="panes">
</section>
<nav id="keys" aria-label="终端按键">
</nav>
</section>
<section id="opened" hidden>
<div class="heading">
<h1>已打开终端</h1>
<button id="opened-home">Home</button>
</div>
<div id="opened-list">
</div>
<p class="footnote">切换不会断开其他 Home。</p>
</section>
</div>
<nav id="mobile-nav">
<button id="mobile-home">⌂ Home</button>
<button id="mobile-terminals" aria-label="已打开终端">▣ 终端 <span id="mobile-count">0</span>
</button>
</nav>
<dialog id="new-session">
<form id="new-form">
<h2>新建会话</h2>
<p id="new-home" class="muted">
</p>
<label>会话名称<input id="session-name" autocomplete="off" placeholder="例如：日常开发" required>
</label>
<p id="name-error" role="alert">
</p>
<p class="muted">创建后以读写模式进入。</p>
<button id="confirm-new" class="primary">创建并打开</button>
<button id="cancel-new" type="button">取消</button>
</form>
</dialog>
<dialog id="rename-session"><form id="rename-form"><h2>重命名会话</h2><p id="rename-target" class="muted"></p><label>会话名称<input id="rename-name" autocomplete="off" required></label><p id="rename-error" role="alert"></p><button id="save-rename" class="primary">保存</button><button id="cancel-rename" type="button">取消</button></form></dialog>
<dialog id="takeover">
<h2>接管读写权限</h2>
<p id="warning">
</p>
<button id="cancel-takeover">取消</button>
<button id="confirm-takeover" class="primary">确认接管</button>
</dialog>
<dialog id="recovery">
<form id="recovery-form">
<h2>解锁私钥</h2>
<p id="recovery-message">
</p>
<label>私钥密码<input id="recovery-password" type="password" autocomplete="current-password" required>
</label>
<button class="primary">连接</button>
<button id="cancel-recovery" type="button">取消</button>
</form>
</dialog>
<dialog id="catalog">
<h2>添加 Home</h2>
<p class="muted">选择部署配置中已授权的 Home 完成注册。</p>
<div id="catalog-list">
</div>
<button id="close-catalog">关闭</button>
</dialog>`;
const el = <T extends HTMLElement = HTMLElement>(id: string) =>
  document.getElementById(id) as T;
const homes = new Map<string, Home>(),
  tabs = new Map<string, Tab>();
let selected: string | null = null,
  active: string | null = null,
  page = "manage",
  newHome: string | null = null,
  recoveryHome: string | null = null;
let classMode = false;
let identity = {
  installed: false,
  connected: false,
  busy: false,
  label: "此设备 · PTY",
  code: "",
  connectAfterEnroll: false,
};
let classRecovery = false;
let warning: {
  home: string;
  attachment: string;
  session: string;
  epoch: number;
  stale: boolean;
} | null = null;
const key = (home: string, id: string) => JSON.stringify([home, id]);
const current = () => homes.get(selected || "");
const activeTab = () => tabs.get(active || "");
function send(value: unknown) {
  const json = JSON.stringify(value);
  if (window.FlowSpliceNative) window.FlowSpliceNative.send(json);
  else window.webkit?.messageHandlers.pty.postMessage(json);
}
function action(home: Home, value: object) {
  if (classMode) {
    if (!identity.connected) return;
    const request = value as Operation;
    if (request.op === "connect")
      send({ op: "connect_home", home_id: home.id });
    else if (request.op === "disconnect")
      send({ op: "disconnect_home", home_id: home.id });
    else if (request.op === "operation")
      send({
        op: "operation_home",
        home_id: home.id,
        operation: request.operation,
      });
    return;
  }
  send({ ...value, home_id: home.id });
}
function operation(home: Home, value: Operation) {
  if (home.connected) action(home, { op: "operation", operation: value });
}
function details(home: Home) {
  if (!home.connected || home.detailsPending) return;
  home.detailsPending = true;
  operation(home, { op: "list_details" });
}
function text(tag: string, value: string, cls = "") {
  const node = document.createElement(tag);
  node.textContent = value;
  node.className = cls;
  return node;
}
function button(label: string, fn: () => void, cls = "") {
  const node = document.createElement("button");
  node.textContent = label;
  node.onclick = fn;
  node.className = cls;
  return node;
}
function title(tab: Tab) {
  return (
    tab.home.sessions.get(tab.session)?.name ||
    `会话 ${tab.session.slice(0, 8)}`
  );
}
function date(value: number | null | undefined) {
  return value
    ? new Date(value * 1000).toLocaleString("zh-CN", {
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
        hour12: false,
      })
    : "尚未连接";
}
function status(h: Home) {
  return h.connected
    ? "已连接"
    : h.busy
      ? "正在连接 / 等待批准"
      : h.installed
        ? "未连接"
        : "待注册";
}
function selectHome(id: string, connect = true) {
  selected = id;
  page = "manage";
  const h = current();
  el<HTMLInputElement>("search").value = "";
  if (h) {
    el<HTMLInputElement>("relay").value = h.relay;
    if (h.connected) details(h);
    else if (
      connect &&
      h.installed &&
      !h.busy &&
      (!classMode || identity.connected)
    ) {
      h.busy = true;
      action(h, { op: "connect", password: "" });
    }
  }
  render();
}
function manage() {
  page = "manage";
  render();
  if (current()?.connected) details(current()!);
}
function fit(tab: Tab) {
  if (active !== tab.key || page !== "terminal") return;
  const size = tab.fit.proposeDimensions();
  if (size)
    tab.terminal.resize(
      Math.max(2, Math.min(512, size.cols)),
      Math.max(1, Math.min(256, size.rows)),
    );
}
function focus(tab: Tab) {
  active = tab.key;
  selected = tab.home.id;
  page = "terminal";
  render();
  fit(tab);
  tab.terminal.focus();
}
// Native pointer down/up can straddle a catalog or protocol event batch.
function reconcileChildren(parent: HTMLElement, desired: HTMLElement[]) {
  const retained = new Set(desired);
  for (const child of Array.from(parent.children))
    if (!retained.has(child as HTMLElement)) child.remove();
  desired.forEach((child, index) => {
    if (parent.children[index] !== child)
      parent.insertBefore(child, parent.children[index] ?? null);
  });
}
function updateText(node: HTMLElement, value: string) {
  if (node.textContent !== value) node.textContent = value;
}
function render() {
  const h = current(),
    tab = activeTab();
  app.dataset.page = page;
  el("identity-panel").hidden = !classMode;
  if (classMode) {
    el("identity-label").textContent = identity.label;
    el("identity-status").textContent = identity.connected
      ? "服务目录已连接"
      : identity.busy
        ? "正在连接 / 等待批准"
        : "服务目录未连接";
    el("identity-disconnect").hidden = !identity.connected;
    el("identity-access").hidden =
      identity.connected || (identity.busy && !identity.installed);
    el("identity-waiting").hidden = identity.installed || !identity.busy;
    el("identity-code").textContent = identity.code || "正在获取校验码…";
    for (const id of [
      "identity-relay-label",
      "identity-password-label",
      "identity-password-note",
    ])
      el(id).hidden = identity.installed;
    el<HTMLInputElement>("identity-relay").required = !identity.installed;
    el<HTMLInputElement>("identity-password").required = !identity.installed;
    el<HTMLButtonElement>("identity-enter").disabled = identity.busy;
    el("identity-enter").textContent = identity.installed
      ? "连接服务目录"
      : "注册此设备";
  }
  for (const id of ["add-home", "add-mobile"]) el(id).hidden = classMode;
  el("management").hidden = page !== "manage";
  el("sidebar").hidden = page !== "manage";
  el("terminal-view").hidden = page !== "terminal";
  el("opened").hidden = page !== "opened";
  el("overview").hidden = !!h;
  el("detail").hidden = !h;
  el("manage").setAttribute("aria-selected", String(page === "manage"));
  for (const id of ["tab-count", "mobile-count"])
    el(id).textContent = String(tabs.size);
  for (const target of ["home-nav", "home-cards"]) {
    const parent = el(target);
    const existing = new Map(
      Array.from(parent.children).map((child) => [
        (child as HTMLElement).dataset.homeId,
        child as HTMLButtonElement,
      ]),
    );
    const desired: HTMLElement[] = [];
    for (const home of homes.values()) {
      let b = existing.get(home.id);
      if (!b) {
        const id = home.id;
        b = button("", () => selectHome(id), "home-card");
        b.dataset.homeId = id;
        const copy = text("span", "", "home-copy");
        copy.append(text("strong", ""), text("small", ""));
        b.append(
          text("span", ""),
          text("span", "", "device"),
          copy,
          text("span", "›"),
        );
      }
      b.setAttribute("aria-label", home.name);
      b.setAttribute("aria-selected", String(selected === home.id));
      const [dot, device, copy] = Array.from(b.children) as HTMLElement[];
      updateText(dot, home.connected ? "●" : "○");
      dot.className = home.connected ? "online" : "muted";
      updateText(device, home.platform === "macos" ? "▣" : "▤");
      updateText(copy.children[0] as HTMLElement, home.name);
      updateText(copy.children[1] as HTMLElement, status(home));
      copy.children[1].className = home.connected ? "online" : "muted";
      desired.push(b);
    }
    reconcileChildren(parent, desired);
  }
  const tabParent = el("tabs");
  const existingTabs = new Map(
    Array.from(tabParent.children).map((child) => [
      (child as HTMLElement).dataset.tabKey,
      child as HTMLButtonElement,
    ]),
  );
  const desiredTabs: HTMLElement[] = [];
  el("opened-list").replaceChildren();
  for (const t of tabs.values()) {
    let b = existingTabs.get(t.key);
    if (!b) {
      const key = t.key;
      b = button("", () => {
        const current = tabs.get(key);
        if (current) focus(current);
      });
      b.dataset.tabKey = key;
    }
    updateText(b, `${t.home.name} / ${title(t)}`);
    b.title = b.textContent!;
    b.setAttribute(
      "aria-selected",
      String(active === t.key && page === "terminal"),
    );
    desiredTabs.push(b);
    const card = text("div", "", "opened-card");
    card.append(
      button(`${t.home.name} / ${title(t)}`, () => focus(t)),
      text("span", t.mode === "read_write" ? "读写" : "只读", "mode-badge"),
      button("×", () => detach(t), "close-tab"),
    );
    el("opened-list").append(card);
    t.pane.hidden = page !== "terminal" || active !== t.key;
  }
  reconcileChildren(tabParent, desiredTabs);
  if (!tabs.size)
    el("opened-list").append(
      text("p", "还没有打开的终端。选择 Home 加入或新建会话。", "muted"),
    );
  if (h) {
    el("home-name").textContent = h.name;
    el("status").textContent =
      `${h.platform === "macos" ? "macOS" : h.platform === "linux" ? "Linux" : "PTY 服务"} · ${status(h)}`;
    el("status").className = h.connected ? "online" : "muted";
    el("notice").textContent = h.notice;
    el("notice").hidden = !h.notice;
    el("disconnect").hidden = !h.connected;
    el("sessions").hidden = !h.connected;
    el("access-panel").hidden = h.connected;
    el("access-title").textContent = h.installed ? "连接到 Home" : "注册此设备";
    el("access-description").textContent = h.installed
      ? "使用此设备已保存的凭据连接。"
      : "私钥以密码加密并安全保存，之后连接无需再次输入。";
    el("relay-label").hidden = classMode || h.installed;
    el("password-label").hidden = classMode || h.installed;
    el<HTMLInputElement>("password").required = !classMode && !h.installed;
    el("access").hidden = h.busy && !h.installed;
    el("waiting").hidden = !(h.busy && !h.installed);
    el("verification-code").textContent = h.code || "正在获取校验码…";
    el<HTMLButtonElement>("enter").disabled =
      h.busy || (classMode && !identity.connected);
    el("enter").textContent = h.installed
      ? h.busy
        ? "连接中…"
        : "连接"
      : "注册";
    el<HTMLButtonElement>("new").disabled =
      !h.canWrite || h.pendingNew !== null;
    renderList(h);
  }
  if (renaming) {
    const target = homes.get(renaming.home);
    const available =
      target?.connected &&
      target.canWrite &&
      target.sessions.has(renaming.session);
    el<HTMLButtonElement>("save-rename").disabled =
      !available || renaming.pending;
    el<HTMLInputElement>("rename-name").disabled = renaming.pending;
    el<HTMLButtonElement>("cancel-rename").disabled = renaming.pending;
    if (!available)
      el("rename-error").textContent = "会话已不可用或没有重命名权限。";
  }
  if (tab) {
    el<HTMLButtonElement>("rename-terminal").disabled =
      !tab.home.connected ||
      !tab.home.canWrite ||
      !tab.home.sessions.has(tab.session);
    const terminalNotice = [
      tab.notice,
      tab.home.notice
        ? `${tab.home.name} / ${title(tab)}：${tab.home.notice}`
        : "",
    ]
      .filter(Boolean)
      .join(" · ");
    el("terminal-notice").textContent = terminalNotice;
    el("terminal-notice").hidden = !terminalNotice;
    el("terminal-title").replaceChildren(
      text("strong", title(tab)),
      text("small", tab.home.name),
    );
    el("mode-label").textContent = tab.mode === "read_write" ? "读写" : "只读";
    el("mode").title =
      tab.mode === "read_write" ? "切换为只读" : "申请读写权限";
    el("mode").setAttribute(
      "aria-label",
      tab.mode === "read_write" ? "切换只读" : "申请读写",
    );
    el<HTMLButtonElement>("mode").disabled = !tab.home.canWrite;
    const count = tab.home.sessions.get(tab.session)?.connection_count;
    el("terminal-connections").textContent =
      count === undefined ? "" : `${count} 个连接`;
  }
}
function renderList(h: Home) {
  el("list").replaceChildren();
  const query = el<HTMLInputElement>("search").value.trim().toLowerCase();
  for (const s of h.sessions.values()) {
    const name = s.name || `会话 ${s.id.slice(0, 8)}`;
    if (!name.toLowerCase().includes(query)) continue;
    const row = text("article", "", "session");
    row.dataset.sessionId = s.id;
    row.append(text("strong", name, "session-name"));
    for (const [label, value] of [
      ["创建", date(s.created_at_unix_secs)],
      ["最近连接", date(s.last_connected_at_unix_secs)],
    ]) {
      const cell = text("span", value, "session-time");
      cell.dataset.label = label;
      row.append(cell);
    }
    const connections = text("span", "", "connection");
    connections.append(
      text(
        "span",
        s.connection_count === undefined ? "—" : `${s.connection_count} 个连接`,
      ),
      text(
        "small",
        s.writer ? `${s.writer.label || s.writer.travel_id} 写入` : "无人写入",
        s.writer ? "online" : "muted",
      ),
    );
    row.append(connections);
    const actions = text("div", "", "session-actions");
    const existing = [...tabs.values()].find(
      (t) => t.home === h && t.session === s.id,
    );
    if (existing) actions.append(button("继续", () => focus(existing)));
    else {
      const join = (mode: string) => {
        if (h.joining.has(s.id)) return;
        h.joining.add(s.id);
        operation(h, {
          op: "join",
          session_id: s.id,
          mode,
          columns: 80,
          rows: 24,
        });
        renderList(h);
      };
      const read = button("只读加入", () => join("read_only"));
      read.disabled = h.joining.has(s.id);
      actions.append(read);
      if (h.canWrite && !s.writer) {
        const write = button("打开", () => join("read_write"));
        write.disabled = h.joining.has(s.id);
        actions.append(write);
      }
    }
    if (h.canWrite) actions.append(button("重命名", () => openRename(h, s.id)));
    row.append(actions);
    el("list").append(row);
  }
  if (!el("list").children.length)
    el("list").append(
      text(
        "p",
        query ? "没有匹配的会话" : "暂无会话，点击「新建会话」开始。",
        "empty",
      ),
    );
}
function closeWarning() {
  warning = null;
  el<HTMLDialogElement>("takeover").close();
}
function remove(tab: Tab) {
  tab.terminal.dispose();
  tab.pane.remove();
  tabs.delete(tab.key);
  if (warning?.home === tab.home.id && warning.attachment === tab.id)
    closeWarning();
  if (active === tab.key) {
    active = null;
    page = "manage";
    selected = tab.home.id;
  }
  render();
}
function detach(tab: Tab) {
  operation(tab.home, { op: "detach", attachment_id: tab.id });
  remove(tab);
  details(tab.home);
}
function clear(home: Home) {
  if (renaming?.home === home.id) {
    renaming.pending = false;
    renaming.request = undefined;
    el<HTMLDialogElement>("rename-session").close();
    renaming = null;
  }
  home.detailsPending = false;
  if (newHome === home.id) {
    newHome = null;
    el<HTMLDialogElement>("new-session").close();
  }
  for (const tab of [...tabs.values()]) if (tab.home === home) remove(tab);
  home.requests.clear();
  home.joining.clear();
  home.detailsAgain = false;
  home.pendingNew = null;
  home.pendingNewId = null;
  home.canWrite = false;
  home.sessions.clear();
  if (warning?.home === home.id) closeWarning();
}
function input(tab: Tab, value: string) {
  if (!tab.home.connected || tab.mode !== "read_write") return;
  const data = new TextEncoder().encode(value),
    writer_epoch = tab.epoch;
  for (let i = 0; i < data.length; i += 16384)
    operation(tab.home, {
      op: "input",
      attachment_id: tab.id,
      writer_epoch,
      data: Array.from(data.subarray(i, i + 16384)),
    });
}
function attach(h: Home, result: any, name?: string) {
  h.joining.delete(result.session.id);
  const old = h.sessions.get(result.session.id);
  h.sessions.set(result.session.id, {
    ...old,
    ...result.session,
    name: name || old?.name,
  });
  const existing = [...tabs.values()].find(
    (t) => t.home === h && t.session === result.session.id,
  );
  if (existing) {
    if (existing.id !== result.attachment_id)
      operation(h, { op: "detach", attachment_id: result.attachment_id });
    focus(existing);
    return;
  }
  const pane = text("div", "", "terminal");
  el("panes").append(pane);
  const terminal = new Terminal({
    scrollback: 0,
    fontSize: 14,
    theme: { background: "#101416", foreground: "#e4e9e7", cursor: "#61d69d" },
    linkHandler: { activate: () => {}, allowNonHttpProtocols: false },
  });
  const addon = new FitAddon();
  terminal.loadAddon(addon);
  terminal.open(pane);
  terminal.parser.registerOscHandler(52, () => true);
  const tab: Tab = {
    key: key(h.id, result.attachment_id),
    home: h,
    session: result.session.id,
    id: result.attachment_id,
    mode: result.mode,
    epoch: result.writer_epoch,
    terminal,
    fit: addon,
    pane,
    notice: "",
  };
  tabs.set(tab.key, tab);
  terminal.onData((data) => input(tab, data));
  terminal.onResize(({ cols, rows }) =>
    operation(h, { op: "resize", attachment_id: tab.id, columns: cols, rows }),
  );
  focus(tab);
  details(h);
}
function protocol(h: Home, m: any) {
  if (m.type === "hello") {
    h.canWrite = m.can_write;
    details(h);
  } else if (m.type === "response") {
    const req = h.requests.get(m.request_id);
    h.requests.delete(m.request_id);
    const r = m.result;
    if (req?.op === "list_details" || r.status === "session_details")
      h.detailsPending = false;
    if (req?.op === "rename") {
      if (renaming?.home === h.id && renaming.request === m.request_id) {
        renaming.pending = false;
        if (r.status === "ok") {
          el<HTMLDialogElement>("rename-session").close();
          renaming = null;
        } else
          el("rename-error").textContent = r.message || "重命名失败，请重试。";
      }
      if (r.status === "ok") {
        if (h.detailsPending) h.detailsAgain = true;
        else details(h);
      }
      render();
      return;
    }
    if (r.status === "ok" && req?.op !== "new_named" && req?.op !== "join")
      return;
    const name = req?.op === "new_named" ? String(req.name) : undefined;
    if (req?.op === "new_named" || m.request_id === h.pendingNewId) {
      h.pendingNew = null;
      h.pendingNewId = null;
    }
    if (req?.op === "join") h.joining.delete(String(req.session_id));
    if (r.status === "sessions" || r.status === "session_details") {
      const next = new Map<string, Session>();
      for (const s of r.sessions)
        next.set(s.id, { ...h.sessions.get(s.id), ...s });
      h.sessions = next;
      if (r.status === "session_details" && h.detailsAgain) {
        h.detailsAgain = false;
        details(h);
      }
    } else if (r.status === "attached") attach(h, r, name);
    else if (r.status === "error") h.notice = r.message;
    else if (r.status === "takeover_required" && req?.op === "set_mode") {
      warning = {
        home: h.id,
        attachment: String(req.attachment_id),
        session: r.session_id,
        epoch: r.epoch,
        stale: false,
      };
      el("warning").textContent =
        `${h.name} / ${h.sessions.get(r.session_id)?.name || `会话 ${r.session_id.slice(0, 8)}`} · 当前写入者：${r.writer.label || r.writer.travel_id}。确认接管后，远端将被强制切换为只读。`;
      el<HTMLDialogElement>("takeover").showModal();
    }
  } else if (m.type === "output") {
    tabs
      .get(key(h.id, m.attachment_id))
      ?.terminal.write(new Uint8Array(m.data));
    return;
  } else if (m.type === "ownership") {
    for (const t of tabs.values())
      if (t.home === h && t.session === m.session_id) {
        const wasWriter = t.mode === "read_write";
        t.epoch = m.epoch;
        t.mode = m.writer?.attachment_id === t.id ? "read_write" : "read_only";
        if (wasWriter && t.mode === "read_only" && m.writer)
          t.notice = `${h.name} / ${title(t)}：读写权限已被 ${m.writer.label || m.writer.travel_id} 接管，当前为只读。`;
        else if (t.mode === "read_write") t.notice = "";
      }
    if (
      warning?.home === h.id &&
      warning.session === m.session_id &&
      warning.epoch !== m.epoch
    ) {
      warning.stale = true;
      el("warning").textContent =
        `${h.name} / ${h.sessions.get(m.session_id)?.name || `会话 ${m.session_id.slice(0, 8)}`}：写入者已变化，请重新申请后核对新的交接提示。`;
    }
    details(h);
  } else if (m.type === "detached") {
    const t = tabs.get(key(h.id, m.attachment_id));
    if (t) remove(t);
    details(h);
  } else if (m.type === "session_ended") {
    for (const t of [...tabs.values()])
      if (t.home === h && t.session === m.session_id) remove(t);
    h.sessions.delete(m.session_id);
    details(h);
  }
  render();
}
function platform(value: string | undefined) {
  app.dataset.platform = value || "ios";
  el("keys").replaceChildren();
  el("keys").hidden = value === "macos";
  if (value !== "macos")
    for (const [label, data] of [
      ["Ctrl+C", "\x03"],
      ["Tab", "\t"],
      ["Esc", "\x1b"],
      ["↑", "\x1b[A"],
      ["↓", "\x1b[B"],
      ["←", "\x1b[D"],
      ["→", "\x1b[C"],
    ]) {
      const b = button(label, () => {
        const t = activeTab();
        if (t) {
          input(t, data);
          t.terminal.focus();
        }
      });
      b.onpointerdown = (e) => e.preventDefault();
      el("keys").append(b);
    }
}
window.flowsplice = {
  async receiveBatch(events) {
    for (const e of events) {
      if (e.type === "protocol" && e.message?.type === "output") {
        const t = tabs.get(key(e.home_id, e.message.attachment_id));
        if (t)
          await new Promise<void>((resolve) =>
            t.terminal.write(new Uint8Array(e.message.data), resolve),
          );
      } else window.flowsplice.receive(e);
    }
  },
  receive(e) {
    if (e.type === "platform") {
      platform(e.platform);
      return;
    }
    if (e.type === "identity") {
      classMode = true;
      const wasBusy = identity.busy;
      identity = {
        ...identity,
        installed: e.installed,
        connected: e.connected,
        busy: e.busy,
        label: e.label || identity.label,
      };
      if (!identity.connected) {
        for (const home of homes.values()) {
          home.connected = false;
          home.busy = false;
          clear(home);
        }
      }
      if (!identity.installed && !identity.busy && wasBusy)
        identity.connectAfterEnroll = false;
      if (
        identity.connectAfterEnroll &&
        identity.installed &&
        !identity.busy &&
        !identity.connected
      ) {
        identity.connectAfterEnroll = false;
        identity.busy = true;
        send({ op: "connect", password: "" });
      }
      render();
      return;
    }
    if (e.type === "homes") {
      if (e.platform) platform(e.platform);
      if (classMode) {
        const retained = new Set(e.homes.map((home: any) => home.id));
        for (const [id, home] of homes)
          if (!retained.has(id)) {
            clear(home);
            homes.delete(id);
            if (selected === id) selected = null;
          }
      }
      for (const item of e.homes) {
        const old = homes.get(item.id);
        if (old)
          Object.assign(old, {
            name: item.name,
            platform: item.platform,
            relay: item.relay,
          });
        else
          homes.set(item.id, {
            ...item,
            installed: classMode,
            connected: false,
            busy: false,
            canWrite: false,
            notice: "",
            code: "",
            sessions: new Map(),
            requests: new Map(),
            joining: new Set(),
            pendingNew: null,
            pendingNewId: null,
            connectAfterEnroll: false,
            detailsPending: false,
          });
      }
      render();
      return;
    }
    if (classMode && !e.home_id && e.type === "progress") {
      identity.code = e.progress.verification_code || identity.code;
      render();
      return;
    }
    if (classMode && !e.home_id && e.type === "error") {
      identity.busy = false;
      identity.connectAfterEnroll = false;
      if (e.code === "credential_required") {
        classRecovery = true;
        recoveryHome = null;
        el("recovery-message").textContent = e.message;
        el<HTMLInputElement>("recovery-password").value = "";
        el<HTMLDialogElement>("recovery").showModal();
      }
      el("global-notice").textContent = e.message;
      el("global-notice").hidden = false;
      render();
      return;
    }
    const h = homes.get(e.home_id);
    if (!h) {
      if (e.type === "error") {
        el("global-notice").textContent =
          `配置或本机连接错误：${e.message || "无法初始化 Home，请检查部署配置。"}`;
        el("global-notice").hidden = false;
      }
      return;
    }
    if (e.type === "state") {
      const was = h.connected,
        wasBusy = h.busy;
      h.installed = e.installed;
      h.connected = e.connected;
      h.busy = e.busy;
      if (!h.connected) clear(h);
      if (!h.installed && !h.busy && wasBusy) h.connectAfterEnroll = false;
      if (h.connected && !was) {
        h.notice = "";
        details(h);
      }
      if (h.connectAfterEnroll && h.installed && !h.connected && !h.busy) {
        h.connectAfterEnroll = false;
        h.busy = true;
        action(h, { op: "connect", password: "" });
      }
      render();
    } else if (e.type === "protocol") protocol(h, e.message);
    else if (e.type === "submitted") {
      h.requests.set(e.request_id, e.operation);
      if (
        e.operation.op === "rename" &&
        renaming?.home === h.id &&
        renaming.session === e.operation.session_id &&
        renaming.pending
      )
        renaming.request = e.request_id;
      if (e.operation.op === "new_named") h.pendingNewId = e.request_id;
      if (h.requests.size > 256)
        h.requests.delete(h.requests.keys().next().value!);
    } else if (e.type === "progress") {
      h.code = e.progress.verification_code || h.code;
      h.notice = e.progress.verification_code ? "" : e.progress.phase;
      render();
    } else if (e.type === "error") {
      h.detailsPending = false;
      h.connectAfterEnroll = false;
      h.pendingNew = null;
      h.pendingNewId = null;
      h.joining.clear();
      h.notice = e.message;
      if (renaming?.home === h.id && !renaming.request) {
        renaming.pending = false;
        el("rename-error").textContent = e.message;
      }
      h.busy = false;
      if (e.code === "credential_required") {
        classRecovery = false;
        recoveryHome = h.id;
        el("recovery-message").textContent = e.message;
        el<HTMLInputElement>("recovery-password").value = "";
        el<HTMLDialogElement>("recovery").showModal();
      }
      render();
    }
  },
};
el("identity-access").onsubmit = (e) => {
  e.preventDefault();
  if (identity.busy) return;
  identity.busy = true;
  el("global-notice").hidden = true;
  if (identity.installed) send({ op: "connect", password: "" });
  else {
    identity.connectAfterEnroll = true;
    send({
      op: "enroll",
      relay: el<HTMLInputElement>("identity-relay").value.trim(),
      password: el<HTMLInputElement>("identity-password").value,
    });
    el<HTMLInputElement>("identity-password").value = "";
  }
  render();
};
el("identity-disconnect").onclick = () => {
  identity.connectAfterEnroll = false;
  send({ op: "disconnect" });
};
el("identity-cancel").onclick = () => {
  identity.connectAfterEnroll = false;
  identity.busy = false;
  identity.code = "";
  send({ op: "disconnect" });
  render();
};
el("manage").onclick = manage;
el("back-home").onclick = () => {
  selected = null;
  manage();
};
el("terminal-back").onclick = manage;
el("opened-home").onclick = manage;
el("mobile-home").onclick = () => {
  selected = null;
  manage();
};
for (const id of ["switcher", "mobile-terminals", "terminal-switcher"])
  el(id).onclick = () => {
    page = "opened";
    render();
  };
function catalog() {
  el("catalog-list").replaceChildren();
  const available = [...homes.values()].filter((h) => !h.installed);
  for (const h of available)
    el("catalog-list").append(
      button(h.name, () => {
        el<HTMLDialogElement>("catalog").close();
        selectHome(h.id, false);
      }),
    );
  if (!available.length)
    el("catalog-list").append(text("p", "配置中的 Home 均已注册。", "muted"));
  el<HTMLDialogElement>("catalog").showModal();
}
for (const id of ["add-home", "add-mobile"]) el(id).onclick = catalog;
el("close-catalog").onclick = () => el<HTMLDialogElement>("catalog").close();
el("access").onsubmit = (e) => {
  e.preventDefault();
  const h = current();
  if (!h || h.busy) return;
  h.busy = true;
  h.notice = "";
  if (h.installed) action(h, { op: "connect", password: "" });
  else {
    h.connectAfterEnroll = true;
    action(h, {
      op: "enroll",
      relay: el<HTMLInputElement>("relay").value.trim() || h.relay,
      password: el<HTMLInputElement>("password").value,
    });
    el<HTMLInputElement>("password").value = "";
  }
  render();
};
el("disconnect").onclick = () => {
  const h = current();
  if (h) {
    action(h, { op: "disconnect" });
    h.connected = false;
    h.busy = false;
    h.connectAfterEnroll = false;
    clear(h);
    render();
  }
};
el("cancel-enroll").onclick = () => {
  const h = current();
  if (h) {
    action(h, { op: "disconnect" });
    h.busy = false;
    h.code = "";
    h.connectAfterEnroll = false;
    render();
  }
};
el("refresh").onclick = () => {
  if (current()) details(current()!);
};
el("search").oninput = () => {
  if (current()) renderList(current()!);
};
function openRename(h: Home, session: string) {
  if (
    !h.connected ||
    !h.canWrite ||
    !h.sessions.has(session) ||
    renaming?.pending
  )
    return;
  renaming = { home: h.id, session, pending: false };
  el("rename-target").textContent =
    `${h.name} / ${h.sessions.get(session)?.name || session}`;
  el<HTMLInputElement>("rename-name").value =
    h.sessions.get(session)?.name || session;
  el("rename-error").textContent = "";
  el<HTMLDialogElement>("rename-session").showModal();
  render();
  el<HTMLInputElement>("rename-name").focus();
  el<HTMLInputElement>("rename-name").select();
}
el("rename-terminal").onclick = () => {
  const t = activeTab();
  if (t) openRename(t.home, t.session);
};
el("cancel-rename").onclick = () => {
  if (renaming?.pending) return;
  el<HTMLDialogElement>("rename-session").close();
  renaming = null;
};
el("rename-session").addEventListener("cancel", (event) => {
  if (renaming?.pending) event.preventDefault();
  else renaming = null;
});
el("rename-form").onsubmit = (event) => {
  event.preventDefault();
  const target = renaming,
    h = homes.get(target?.home || "");
  if (
    !target ||
    target.pending ||
    !h?.connected ||
    !h.canWrite ||
    !h.sessions.has(target.session)
  )
    return;
  const name = el<HTMLInputElement>("rename-name").value.trim();
  if (
    !name ||
    Array.from(name).length > 64 ||
    new TextEncoder().encode(name).length > 256 ||
    /[\u0000-\u001f\u007f-\u009f]/u.test(name)
  ) {
    el("rename-error").textContent =
      "请输入 1–64 个字符的名称，不可包含控制字符。";
    return;
  }
  target.request = undefined;
  target.pending = true;
  el("rename-error").textContent = "";
  operation(h, { op: "rename", session_id: target.session, name });
  render();
};
el("new").onclick = () => {
  const h = current();
  if (!h?.connected || !h.canWrite || h.pendingNew !== null) return;
  newHome = h.id;
  el("new-home").textContent = h.name;
  el<HTMLInputElement>("session-name").value = "";
  el("name-error").textContent = "";
  el<HTMLDialogElement>("new-session").showModal();
  el("session-name").focus();
};
el("cancel-new").onclick = () => el<HTMLDialogElement>("new-session").close();
el("new-form").onsubmit = (e) => {
  e.preventDefault();
  const h = homes.get(newHome || ""),
    name = el<HTMLInputElement>("session-name").value.trim();
  if (
    !name ||
    Array.from(name).length > 64 ||
    new TextEncoder().encode(name).length > 256 ||
    /[\u0000-\u001f\u007f-\u009f]/u.test(name)
  ) {
    el("name-error").textContent =
      "请输入 1–64 个字符的名称，不可包含控制字符。";
    return;
  }
  if (!h?.connected || h.pendingNew !== null) return;
  h.pendingNew = name;
  operation(h, { op: "new_named", name, columns: 80, rows: 24 });
  el<HTMLDialogElement>("new-session").close();
  render();
};
el("mode").onclick = () => {
  const t = activeTab();
  if (t)
    operation(t.home, {
      op: "set_mode",
      attachment_id: t.id,
      mode: t.mode === "read_write" ? "read_only" : "read_write",
      force: false,
      expected_epoch: null,
    });
};
el("detach").onclick = () => {
  const t = activeTab();
  if (t) detach(t);
};
el("cancel-takeover").onclick = closeWarning;
el("confirm-takeover").onclick = () => {
  const w = warning;
  if (!w) return;
  closeWarning();
  const h = homes.get(w.home);
  if (h)
    operation(h, {
      op: "set_mode",
      attachment_id: w.attachment,
      mode: "read_write",
      force: !w.stale,
      expected_epoch: w.stale ? null : w.epoch,
    });
};
el("recovery-form").onsubmit = (e) => {
  e.preventDefault();
  const h = homes.get(recoveryHome || "");
  if (classRecovery)
    send({
      op: "connect",
      password: el<HTMLInputElement>("recovery-password").value,
    });
  else if (h)
    action(h, {
      op: "connect",
      password: el<HTMLInputElement>("recovery-password").value,
    });
  el<HTMLInputElement>("recovery-password").value = "";
  el<HTMLDialogElement>("recovery").close();
};
el("cancel-recovery").onclick = () => {
  el<HTMLInputElement>("recovery-password").value = "";
  el<HTMLDialogElement>("recovery").close();
};
new ResizeObserver(() => {
  const t = activeTab();
  if (t) fit(t);
}).observe(el("panes"));
function viewport() {
  app.style.height = `${window.visualViewport?.height || window.innerHeight}px`;
  const t = activeTab();
  if (t) fit(t);
}
window.visualViewport?.addEventListener("resize", viewport);
window.addEventListener("resize", viewport);
setInterval(() => {
  for (const h of homes.values())
    if (
      h.connected &&
      ((page === "manage" && selected === h.id) ||
        [...tabs.values()].some((t) => t.home === h))
    )
      details(h);
}, 5000);
document.addEventListener("click", (e) => {
  if ((e.target as Element).closest("a")) e.preventDefault();
});
platform(window.flowsplicePlatform);
viewport();
render();
send({ op: "ready" });
