import { InputQueue } from "./input";
import { HistoryView } from "./history";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import "./style.css";
import { readWorkspace, type Workspace } from "./workspace";
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
  detailsTimer?: ReturnType<typeof setTimeout>;
  inputQueue?: InputQueue;
  detailsAgain?: boolean;
  wanted?: boolean;
  available?: boolean;
  manualDisconnected?: boolean;
  hello?: boolean;
  listed?: boolean;
  retryAt?: number;
  retries?: number;
  credentialBlocked?: boolean;
  cancelledJoins?: Set<string>;
};
type Tab = {
  key: string;
  home: Home;
  session: string;
  id: string;
  mode: string;
  epoch: number;
  terminal: Terminal;
  opened: boolean;
  fit: FitAddon;
  pane: HTMLElement;
  history: HistoryView;
  notice: string;
  state: "attached" | "reconnecting" | "deleted";
  hasAttached: boolean;
  name: string;
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
<div id="identity-repair" hidden><button id="identity-unlock">重新输入密码</button><button id="identity-reenroll">重试注册</button></div>
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
<span id="home-repair" hidden><button id="home-unlock" type="button">重新输入密码</button><button id="home-reenroll" type="button">重试注册</button></span>
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
<h2 id="recovery-title">解锁私钥</h2>
<p id="recovery-message">
</p>
<label id="recovery-relay-label" hidden>Relay IP:端口<input id="recovery-relay" autocomplete="off"></label>
<label>私钥密码<input id="recovery-password" type="password" autocomplete="current-password" required>
</label>
<button id="recovery-submit" class="primary">连接</button>
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
let foreground = true, workspaceReady = false, nativeScopeReady = false, lastWorkspace = "";
let identityWanted = false, identityManuallyDisconnected = false;
let identityRetryAt = 0, identityRetries = 0, identityCredentialBlocked = false;
let identityStartupBlocked = false;
let recoveryOperation: "connect" | "enroll" = "connect";
const attachment = (home: string, id: string) => [...tabs.values()].find(t => t.home.id === home && t.id === id && t.state === "attached");
function saveWorkspace() {
  // A fresh installation receives platform information before its identity/catalog.
  // Wait for the native scope before persisting an empty class or legacy workspace.
  if (!workspaceReady || !nativeScopeReady) return;
  const currentTab = activeTab();
  const value: Workspace = {
    version: 1, classMode, identityWanted,
    homes: [...homes.values()].map(h => ({id:h.id, name:h.name, platform:h.platform, relay:h.relay || "", wanted:!!h.wanted})),
    tabs: [...tabs.values()].map(t => ({home:t.home.id, session:t.session, name:title(t), mode:t.mode === "read_write" ? "read_write" : "read_only", deleted:t.state === "deleted"})),
    active: currentTab ? {home:currentTab.home.id, session:currentTab.session} : null,
    selected, page: page as Workspace["page"],
  };
  const encoded = JSON.stringify(value);
  if (encoded !== lastWorkspace && readWorkspace(value)) {
    lastWorkspace = encoded;
    send({op:"save_workspace", value});
  }
}
function restoreWorkspace(value: unknown) {
  const saved = readWorkspace(value);
  workspaceReady = true;
  if (!saved) return;
  classMode = saved.classMode;
  identityWanted = saved.identityWanted;
  identityManuallyDisconnected = !saved.identityWanted;
  for (const h of saved.homes) homes.set(h.id, makeHome({...h, installed:false, available:false, manualDisconnected:!h.wanted}));
  for (const savedTab of saved.tabs) {
    const h = homes.get(savedTab.home)!;
    createTab(h, savedTab.session, savedTab.name, savedTab.mode, savedTab.deleted ? "deleted" : "reconnecting");
  }
  active = saved.active ? key(saved.active.home, saved.active.session) : null;
  selected = saved.selected;
  page = saved.page === "terminal" && !active ? "manage" : saved.page;
  lastWorkspace = JSON.stringify(saved);
  render();
}
function makeHome(item: any): Home {
  return {installed:classMode, connected:false, busy:false, canWrite:false, notice:"", code:"", sessions:new Map(), requests:new Map(), joining:new Set(), pendingNew:null, pendingNewId:null, connectAfterEnroll:false, detailsPending:false, available:true, ...item};
}
function wantHome(h: Home) {
  h.wanted = true;
  h.manualDisconnected = false;
  h.credentialBlocked = false;
}
function retryDelay(attempt: number) { return Math.min(30000, 1000 * 2 ** Math.min(attempt, 5)); }
function tabStatus(t: Tab) {
  if (t.state === "deleted") return "已删除";
  if (t.state === "reconnecting") return t.hasAttached ? "正在恢复" : "连接中";
  return t.mode === "read_write" ? "读写" : "只读";
}
function reconnect() {
  if (!foreground || !workspaceReady || el<HTMLDialogElement>("recovery").open) return;
  const now = Date.now();
  if (classMode && identityWanted && identity.installed && !identity.connected && !identity.busy && !identityCredentialBlocked && !identityStartupBlocked && now >= identityRetryAt) {
    identity.busy = true;
    identityRetryAt = now + retryDelay(identityRetries++);
    send({op:"connect", password:""});
    render();
  }
  for (const h of homes.values()) {
    if (!h.wanted || !h.available || !h.installed || h.credentialBlocked || (classMode && !identity.connected)) continue;
    if (!h.connected && !h.busy && now >= (h.retryAt || 0)) {
      h.busy = true;
      h.retryAt = now + retryDelay(h.retries || 0);
      h.retries = (h.retries || 0) + 1;
      action(h, {op:"connect", password:""});
      render();
    } else if (h.connected && h.hello && h.listed) restoreTabs(h);
  }
}
function restoreTabs(h: Home) {
  if (!foreground || !h.wanted || !h.connected || !h.hello || !h.listed || Date.now() < (h.retryAt || 0)) return;
  for (const t of tabs.values()) {
    if (t.home !== h || t.state !== "reconnecting" || h.joining.has(t.session)) continue;
    if (!h.sessions.has(t.session)) { deleted(t); continue; }
    h.joining.add(t.session);
    operation(h, {op:"join", session_id:t.session, mode:t.mode, columns:80, rows:24});
  }
}
function deleted(t: Tab) {
  t.home.inputQueue?.invalidate(t.id);
  t.history.reset();
  t.name = title(t);
  t.state = "deleted";
  t.id = "";
  t.notice = "远端 tmux 会话已删除。";
  t.home.joining.delete(t.session);
  if (warning?.home === t.home.id && warning.session === t.session) closeWarning();
}
function suspend(home: Home) {
  resetOperations(home);
  for (const t of tabs.values()) if (t.home === home && t.state !== "deleted") {
    t.history.reset();
    t.name = title(t);
    t.state = "reconnecting";
    t.id = "";
    t.notice = t.hasAttached ? "连接中断，正在恢复原会话…" : "";
  }
  home.hello = false;
  home.listed = false;
}
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
        ...(request.input_id ? { input_id: request.input_id } : {}),
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
  clearTimeout(home.detailsTimer);
  home.detailsTimer = setTimeout(() => {
    home.detailsPending = false;
    for (const [id, request] of home.requests) if (request.op === "list_details") home.requests.delete(id);
    if (home.detailsAgain) { home.detailsAgain = false; details(home); }
  }, 10000);
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
    tab.name || `会话 ${tab.session.slice(0, 8)}`
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
  return h.available === false
    ? "等待 Home 上线"
    : h.connected
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
      wantHome(h);
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
  // Native batches defer DOM changes. Never open or measure a terminal against
  // the previous page's hidden layout, including viewport callbacks mid-batch.
  if (renderDepth || !foreground || active !== tab.key || page !== "terminal" ||
      tab.pane.hidden || !tab.pane.clientWidth || !tab.pane.clientHeight) return;
  if (!tab.opened) {
    tab.terminal.open(tab.pane);
    tab.opened = true;
  }
  const size = tab.fit.proposeDimensions();
  if (size && Number.isInteger(size.cols) && Number.isInteger(size.rows) &&
      size.cols > 0 && size.rows > 0)
    tab.terminal.resize(
      Math.max(2, Math.min(512, size.cols)),
      Math.max(1, Math.min(256, size.rows)),
    );
  tab.history.measure();
  if (pendingTerminalFocus === tab.key) {
    pendingTerminalFocus = null;
    tab.history.focus();
  }
}
let pendingTerminalFocus: string | null = null;
function focus(tab: Tab) {
  active = tab.key;
  selected = tab.home.id;
  page = "terminal";
  pendingTerminalFocus = tab.key;
  render();
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
let renderDepth = 0;
let renderDeferred = false;
function render() {
  if (renderDepth) { renderDeferred = true; return; }
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
    el("identity-repair").hidden = !identity.installed || identity.connected || identity.busy;
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
    b.dataset.state = t.state;
    b.dataset.sessionId = t.session;
    b.dataset.homeId = t.home.id;
    b.setAttribute(
      "aria-selected",
      String(active === t.key && page === "terminal"),
    );
    desiredTabs.push(b);
    const card = text("div", "", "opened-card");
    card.append(
      button(`${t.home.name} / ${title(t)}`, () => focus(t)),
      text("span", tabStatus(t), "mode-badge"),
      button("×", () => detach(t), "close-tab"),
    );
    el("opened-list").append(card);
    t.pane.hidden = page !== "terminal" || active !== t.key;
    t.pane.dataset.state = t.state;
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
    el("access-title").textContent = classMode || h.installed ? "连接到 Home" : "注册此设备";
    el("access-description").textContent = classMode && !identity.connected
      ? "请先连接服务目录，再恢复此 Home 的终端。"
      : classMode || h.installed
      ? "使用此设备已保存的凭据连接。"
      : "私钥以密码加密并安全保存，之后连接无需再次输入。";
    el("relay-label").hidden = classMode || h.installed;
    el("password-label").hidden = classMode || h.installed;
    el("home-repair").hidden = classMode || !h.installed || h.connected || h.busy;
    el<HTMLInputElement>("password").required = !classMode && !h.installed;
    el("access").hidden = h.busy && !h.installed;
    el("waiting").hidden = !(h.busy && !h.installed);
    el("verification-code").textContent = h.code || "正在获取校验码…";
    el<HTMLButtonElement>("enter").disabled =
      h.busy || (classMode && !identity.connected);
    el("enter").textContent = classMode || h.installed
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
      tab.state !== "attached" ||
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
    const connecting = tab.state === "reconnecting" && !tab.hasAttached;
    el("terminal-notice").textContent = terminalNotice || (connecting ? "连接中…" : "");
    el("terminal-notice").dataset.tone = terminalNotice ? "warning" : "status";
    el("terminal-notice").hidden = !terminalNotice && !connecting;
    el("terminal-title").replaceChildren(
      text("strong", title(tab)),
      text("small", tab.home.name),
    );
    el("mode-label").textContent = tabStatus(tab);
    el("mode").dataset.state = tab.state;
    el("mode").title =
      tab.state !== "attached" ? tabStatus(tab) : tab.mode === "read_write" ? "切换为只读" : "申请读写权限";
    el("mode").setAttribute(
      "aria-label",
      tab.state !== "attached" ? tabStatus(tab) : tab.mode === "read_write" ? "切换只读" : "申请读写",
    );
    el<HTMLButtonElement>("mode").disabled = tab.state !== "attached" || !tab.home.connected || !tab.home.canWrite;
    const count = tab.home.sessions.get(tab.session)?.connection_count;
    el("terminal-connections").textContent =
      count === undefined ? "" : `${count} 个连接`;
  }
  saveWorkspace();
  // Open/focus only the final active pane after all batched visibility and
  // chrome changes. Inactive terminals can still parse output before opening.
  if (tab && page === "terminal") fit(tab);
  else pendingTerminalFocus = null;
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
  tab.home.inputQueue?.invalidate(tab.id);
  tab.history.dispose();
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
  if (tab.home.joining.has(tab.session)) (tab.home.cancelledJoins ??= new Set()).add(tab.session);
  if (tab.state === "attached") operation(tab.home, { op: "detach", attachment_id: tab.id });
  remove(tab);
  details(tab.home);
}
function resetOperations(home: Home) {
  home.inputQueue?.clear();
  clearTimeout(home.detailsTimer);
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
  home.requests.clear();
  home.joining.clear();
  home.cancelledJoins?.clear();
  home.detailsAgain = false;
  home.pendingNew = null;
  home.pendingNewId = null;
  home.canWrite = false;
  if (warning?.home === home.id) closeWarning();
}
function clear(home: Home) {
  resetOperations(home);
  for (const tab of [...tabs.values()]) if (tab.home === home) remove(tab);
  home.sessions.clear();
}
function recoverHome(h: Home) {
  h.connected = false;
  suspend(h);
  h.retryAt = Date.now() + 1000;
  action(h, { op: "disconnect" });
  render();
}
function input(tab: Tab, value: string) {
  if (tab.history.browsing || !foreground || tab.state !== "attached" || !tab.home.connected || tab.mode !== "read_write") return;
  const h = tab.home;
  h.inputQueue ??= new InputQueue(
    (target, data, input_id) => action(h, { op: "operation", input_id, operation: { op: "input", attachment_id: target.attachment_id, writer_epoch: target.writer_epoch, data } }),
    message => { h.notice = message; for (const t of tabs.values()) if (t.home === h) t.notice = message; render(); },
    () => recoverHome(h),
  );
  h.inputQueue.enqueue({ attachment_id: tab.id, writer_epoch: tab.epoch }, new TextEncoder().encode(value));
}
function attach(h: Home, result: any, name?: string) {
  if (!h.connected) return;
  h.joining.delete(result.session.id);
  if (h.cancelledJoins?.delete(result.session.id)) {
    operation(h, {op:"detach", attachment_id:result.attachment_id});
    return;
  }
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
    if (existing.state === "deleted" || existing.state === "attached") {
      if (existing.id !== result.attachment_id) operation(h, {op:"detach", attachment_id:result.attachment_id});
      return;
    }
    existing.id = result.attachment_id;
    existing.epoch = result.writer_epoch;
    existing.mode = result.mode;
    existing.state = "attached";
    existing.hasAttached = true;
    existing.name = name || old?.name || existing.name;
    existing.notice = "";
    existing.terminal.reset();
    render();
    details(h);
    return;
  }
  const tab = createTab(h, result.session.id, name || old?.name || "", result.mode, "attached");
  tab.id = result.attachment_id;
  tab.epoch = result.writer_epoch;
  focus(tab);
  details(h);
}
function createTab(h: Home, session: string, name: string, mode: string, state: Tab["state"]): Tab {
  const pane = text("div", "", "terminal");
  pane.hidden = true;
  el("panes").append(pane);
  const terminal = new Terminal({
    scrollback: 0, fontSize: 14,
    theme: { background: "#101416", foreground: "#e4e9e7", cursor: "#61d69d" },
    linkHandler: { activate: () => {}, allowNonHttpProtocols: false },
  });
  const addon = new FitAddon();
  terminal.loadAddon(addon);
  terminal.parser.registerOscHandler(52, () => true);
  const history = new HistoryView(pane, () => tab.state === "attached" && h.connected ? tab.id : "", op => operation(h, op as Operation), () => terminal.focus(), () => ({rows:terminal.rows, cols:terminal.cols, font:terminal.options.fontFamily, size:terminal.options.fontSize}));
  // Restoring a saved tab starts a fresh connection; it is not a live disconnect.
  const tab: Tab = {history, key:key(h.id, session), home:h, session, name, id:"", mode, epoch:0, terminal, opened:false, fit:addon, pane, notice:"", state, hasAttached:state === "attached"};
  tabs.set(tab.key, tab);
  terminal.onData(data => input(tab, data));
  terminal.onResize(({cols, rows}) => {
    if (foreground && tab.state === "attached") operation(h, {op:"resize", attachment_id:tab.id, columns:cols, rows});
  });
  return tab;
}
function protocol(h: Home, m: any) {
  if (m.type === "hello") {
    h.canWrite = m.can_write;
    h.hello = true;
    details(h);
  } else if (m.type === "response") {
    const req = h.requests.get(m.request_id);
    h.requests.delete(m.request_id);
    const r = m.result;
    if (h.inputQueue?.response(m.request_id, r.status === "ok", r.message)) return;
    if (r.status === "history") { attachment(h.id, r.attachment_id)?.history.accept(r); return; }
    if (req?.op === "history") { attachment(h.id, req.attachment_id)?.history.fail(req.capture_id, r.message, r.code); return; }
    if (req?.op === "list_details" || r.status === "session_details")
      { h.detailsPending = false; clearTimeout(h.detailsTimer); }
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
    if (req?.op === "join") { h.joining.delete(String(req.session_id)); if (r.status !== "attached") h.cancelledJoins?.delete(String(req.session_id)); }
    if (r.status === "sessions" || r.status === "session_details") {
      const next = new Map<string, Session>();
      for (const s of r.sessions)
        next.set(s.id, { ...h.sessions.get(s.id), ...s });
      h.sessions = next;
      h.listed = true;
      for (const t of tabs.values()) if (t.home === h && t.state === "reconnecting" && !next.has(t.session)) deleted(t);
      restoreTabs(h);
      if (r.status === "session_details" && h.detailsAgain) {
        h.detailsAgain = false;
        details(h);
      }
    } else if (r.status === "attached") attach(h, r, name);
    else if (r.status === "error") {
      h.notice = r.message;
      if (req?.op === "join") {
        h.retryAt = Date.now() + 1000;
        h.listed = false;
        details(h);
      }
    }
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
    const t = attachment(h.id, m.attachment_id);
    if (t) { t.history.output(); t.terminal.write(new Uint8Array(m.data)); }
    return;
  } else if (m.type === "ownership") {
    for (const t of tabs.values())
      if (t.home === h && t.session === m.session_id && t.state === "attached") {
        const wasWriter = t.mode === "read_write";
        if (t.epoch !== m.epoch || m.writer?.attachment_id !== t.id) h.inputQueue?.invalidate(t.id);
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
    const t = attachment(h.id, m.attachment_id);
    if (t) { h.inputQueue?.invalidate(t.id); t.history.reset(); t.state = "reconnecting"; t.id = ""; t.notice = "连接中断，正在恢复原会话…"; }
    h.listed = false;
    details(h);
  } else if (m.type === "session_ended") {
    for (const t of [...tabs.values()])
      if (t.home === h && t.session === m.session_id) deleted(t);
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
    renderDepth++;
    const failedHomes = new Set<string>();
    try {
      for (const e of events) {
        try {
          if (!e || typeof e !== "object") continue;
          if (failedHomes.has(e.home_id)) continue;
          if (e.type === "protocol" && e.message?.type === "output") {
            const t = attachment(e.home_id, e.message.attachment_id);
            if (t) {
              t.history.output();
              await new Promise<void>((resolve) => t.terminal.write(new Uint8Array(e.message.data), resolve));
            }
          } else window.flowsplice.receive(e);
        } catch {
          console.error("FlowSplice event failed");
          const h = homes.get(e?.home_id);
          if (h) {
            failedHomes.add(h.id);
            h.notice = "终端事件处理失败，正在恢复连接。";
            try { recoverHome(h); } catch { console.error("FlowSplice recovery failed"); }
          }
        }
      }
    } finally {
      renderDepth--;
      if (!renderDepth && renderDeferred) { renderDeferred = false; render(); }
    }
  },
  receive(e) {
    if (e.type === "workspace") { restoreWorkspace(e.value); return; }
    if (e.type === "lifecycle") {
      // File protection can temporarily block identity reads while the device
      // locks. A new foreground transition earns one fresh startup attempt.
      if (e.active === true && !foreground) identityStartupBlocked = false;
      foreground = e.active === true;
      if (!foreground) for (const h of homes.values()) h.inputQueue?.clear();
      if (foreground) { reconnect(); for (const h of homes.values()) if (h.connected) details(h); render(); }
      saveWorkspace();
      return;
    }
    if (e.type === "platform") {
      platform(e.platform);
      return;
    }
    if (e.type === "identity") {
      nativeScopeReady = true;
      if (!classMode) {
        for (const home of homes.values()) clear(home);
        homes.clear(); selected = null;
      }
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
          suspend(home);
        }
      } else {
        identityStartupBlocked = false;
        identityRetries = 0; identityRetryAt = 0;
        if (!identityManuallyDisconnected) identityWanted = true;
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
      nativeScopeReady = true;
      if (e.platform) platform(e.platform);
      if (classMode) {
        const retained = new Set(e.homes.map((home: any) => home.id));
        for (const [id, home] of homes)
          if (!retained.has(id)) {
            home.connected = false; home.busy = false; home.available = false;
            suspend(home);
            if (!home.wanted && ![...tabs.values()].some(t => t.home === home)) {
              homes.delete(id);
              if (selected === id) selected = null;
            }
          }
      }
      for (const item of e.homes) {
        const old = homes.get(item.id);
        if (old)
          Object.assign(old, {
            name: item.name,
            platform: item.platform,
            relay: item.relay || "",
            available: true,
            installed: classMode || old.installed,
          });
        else
          homes.set(item.id, makeHome(item));
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
      if (e.code === "identity_unavailable") identityStartupBlocked = true;
      if (e.code === "credential_required") {
        identityCredentialBlocked = true;
        openRecovery(null, "connect", e.message);
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
      if (!h.connected) suspend(h);
      else if (!h.manualDisconnected) h.wanted = true;
      if (!h.installed && !h.busy && wasBusy) h.connectAfterEnroll = false;
      if (h.connected && !was) {
        h.notice = "";
        h.retries = 0; h.retryAt = 0;
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
      if (e.operation.op === "input") {
        h.inputQueue?.submitted(e.input_id, e.request_id);
        return;
      }
      if (e.operation.op === "list_details" && !h.detailsPending) return;
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
      if (e.input_id) { h.inputQueue?.rejected(e.input_id, e.message); return; }
      h.inputQueue?.clear();
      clearTimeout(h.detailsTimer);
      h.cancelledJoins?.clear();
      for (const t of tabs.values()) if (t.home === h) t.history.fail();
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
        h.credentialBlocked = true;
        openRecovery(h, "connect", e.message);
      }
      render();
    }
  },
};
el("identity-access").onsubmit = (e) => {
  e.preventDefault();
  if (identity.busy) return;
  identityWanted = true; identityManuallyDisconnected = false; identityCredentialBlocked = false;
  identityStartupBlocked = false;
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
  identityWanted = false; identityManuallyDisconnected = true;
  for (const h of homes.values()) { h.wanted = false; h.manualDisconnected = true; clear(h); }
  identity.connectAfterEnroll = false;
  saveWorkspace();
  send({ op: "disconnect" });
};
el("identity-cancel").onclick = () => {
  identityWanted = false; identityManuallyDisconnected = true;
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
  wantHome(h);
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
    h.wanted = false; h.manualDisconnected = true;
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
    h.wanted = false; h.manualDisconnected = true;
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
  if (t?.state === "attached")
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
function openRecovery(h: Home | null, op: "connect" | "enroll", message?: string) {
  classRecovery = !h;
  recoveryHome = h?.id ?? null;
  recoveryOperation = op;
  el("recovery-title").textContent = op === "enroll" ? "重试注册" : "解锁私钥";
  el("recovery-message").textContent = message || (op === "enroll"
    ? "使用原私钥密码恢复已有注册或继续未完成的申请。原有身份与终端记录会保留。"
    : "输入原私钥密码；连接成功后会安全保存，之后无需重复输入。");
  el("recovery-relay-label").hidden = op !== "enroll";
  el<HTMLInputElement>("recovery-relay").required = op === "enroll";
  el<HTMLInputElement>("recovery-relay").value = h?.relay || el<HTMLInputElement>("identity-relay").value;
  el<HTMLInputElement>("recovery-password").value = "";
  el("recovery-submit").textContent = op === "enroll" ? "恢复注册" : "连接";
  el<HTMLDialogElement>("recovery").showModal();
}
el("identity-unlock").onclick = () => { if (!identity.busy && !identity.connected) openRecovery(null, "connect"); };
el("identity-reenroll").onclick = () => { if (!identity.busy && !identity.connected) openRecovery(null, "enroll"); };
el("home-unlock").onclick = () => { const h = current(); if (h && !h.busy && !h.connected) openRecovery(h, "connect"); };
el("home-reenroll").onclick = () => { const h = current(); if (h && !h.busy && !h.connected) openRecovery(h, "enroll"); };
el("recovery-form").onsubmit = (e) => {
  e.preventDefault();
  const h = homes.get(recoveryHome || "");
  const request = {
    op: recoveryOperation,
    password: el<HTMLInputElement>("recovery-password").value,
    ...(recoveryOperation === "enroll" ? { relay: el<HTMLInputElement>("recovery-relay").value.trim() } : {}),
  };
  if (classRecovery) {
    if (identity.busy || identity.connected) return;
    identityCredentialBlocked = false;
    identityStartupBlocked = false;
    identityWanted = true; identityManuallyDisconnected = false;
    identity.busy = true;
    identity.connectAfterEnroll = recoveryOperation === "enroll";
    el("global-notice").hidden = true;
    send(request);
  } else if (h) {
    if (h.busy || h.connected) return;
    wantHome(h); h.busy = true;
    h.connectAfterEnroll = recoveryOperation === "enroll";
    h.notice = "";
    action(h, request);
  }
  el<HTMLInputElement>("recovery-password").value = "";
  el<HTMLDialogElement>("recovery").close();
  render();
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
setInterval(reconnect, 1000);
setInterval(() => {
  if (!foreground) return;
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
