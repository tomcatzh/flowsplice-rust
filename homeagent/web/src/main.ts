import "./style.css";

type Protocol = "tcp" | "udp";
type Scope =
  | { kind: "home"; home_id: string }
  | { kind: "service"; home_id: string; service_id: string; protocol: Protocol }
  | { kind: "global" };

interface Service { id: string; alias: string; protocol: Protocol; target: string }
interface Status {
  home_id: string;
  home_alias: string;
  default_valid_days: number;
  global_authority_available: boolean;
  services: Service[];
}
interface Credential {
  credential_id: string;
  travel_id: string;
  authority_id: string;
  scope: Scope;
  not_after_unix_secs: number;
  revoked: boolean;
  active: boolean;
}
interface IssueResult { generation: number; enrollment: unknown }

const root = document.querySelector<HTMLElement>("#app");
if (!root) throw new Error("Missing #app root");
const app = root;
let status: Status;
let enrollmentRequest: unknown;

function escapeHtml(value: string): string {
  const span = document.createElement("span");
  span.textContent = value;
  return span.innerHTML;
}

async function json<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: { Accept: "application/json", "Content-Type": "application/json", ...(init?.headers ?? {}) },
  });
  const body = await response.json().catch(() => ({})) as { error?: string };
  if (!response.ok) throw new Error(body.error ?? `请求失败（${response.status}）`);
  return body as T;
}

function scopeLabel(scope: Scope): string {
  if (scope.kind === "global") return "全局超级授权";
  if (scope.kind === "home") return `当前 Home（${scope.home_id}）`;
  return `指定业务（${scope.service_id.toUpperCase()} · ${scope.protocol.toUpperCase()}）`;
}

function download(value: unknown, filename: string): void {
  const blob = new Blob([`${JSON.stringify(value, null, 2)}\n`], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.click();
  URL.revokeObjectURL(url);
}

function flash(message: string, error = false): void {
  const node = document.querySelector<HTMLElement>("#notice");
  if (!node) return;
  node.textContent = message;
  node.className = error ? "notice error" : "notice success";
}

function selectedScope(): Scope {
  const kind = document.querySelector<HTMLInputElement>('input[name="scope"]:checked')?.value;
  if (kind === "global") return { kind: "global" };
  if (kind === "service") {
    const value = (document.querySelector<HTMLSelectElement>("#service")?.value ?? "").split("\u0000");
    if (value.length !== 2) throw new Error("请选择要授权的业务");
    return { kind: "service", home_id: status.home_id, service_id: value[0], protocol: value[1] as Protocol };
  }
  return { kind: "home", home_id: status.home_id };
}

async function issue(): Promise<void> {
  if (!enrollmentRequest) throw new Error("请先选择旅行端申请文件");
  const password = document.querySelector<HTMLInputElement>("#password")?.value ?? "";
  const days = Number(document.querySelector<HTMLInputElement>("#days")?.value ?? status.default_valid_days);
  const result = await json<IssueResult>("/api/issue", {
    method: "POST",
    body: JSON.stringify({ request: enrollmentRequest, valid_days: days, scope: selectedScope(), password }),
  });
  const travelId = (enrollmentRequest as { travel_id?: string }).travel_id ?? "travel";
  download(result.enrollment, `flowsplice-${travelId}-response.json`);
  const passwordInput = document.querySelector<HTMLInputElement>("#password");
  if (passwordInput) passwordInput.value = "";
  flash(`签发成功，授权已同步（第 ${result.generation} 代），签发结果已下载。`);
  await renderCredentials();
}

async function revoke(credentialId: string): Promise<void> {
  const reason = window.prompt("请输入撤销原因（撤销立即生效且不可恢复）：");
  if (!reason) return;
  await json("/api/revoke", { method: "POST", body: JSON.stringify({ credential_id: credentialId, reason }) });
  flash("撤销已同步到 Server、Relay 和 Home。现有连接会被关闭。");
  await renderCredentials();
}

async function renderCredentials(): Promise<void> {
  const credentials = await json<Credential[]>("/api/credentials");
  const table = document.querySelector<HTMLElement>("#credentials");
  if (!table) return;
  table.innerHTML = credentials.length ? credentials.map((credential) => `<tr>
    <td><strong>${escapeHtml(credential.travel_id)}</strong><small>${escapeHtml(credential.credential_id)}</small></td>
    <td>${escapeHtml(scopeLabel(credential.scope))}</td>
    <td>${new Date(credential.not_after_unix_secs * 1000).toLocaleDateString("zh-CN")}</td>
    <td><span class="state ${credential.active ? "active" : "inactive"}">${credential.revoked ? "已撤销" : credential.active ? "有效" : "已失效"}</span></td>
    <td>${credential.revoked ? "" : `<button class="danger revoke" data-id="${credential.credential_id}">撤销</button>`}</td>
  </tr>`).join("") : '<tr><td colspan="5" class="empty">当前 Home 尚未签发旅行端授权</td></tr>';
  table.querySelectorAll<HTMLButtonElement>(".revoke").forEach((button) => {
    button.addEventListener("click", () => void revoke(button.dataset.id ?? "").catch((error) => flash(String(error), true)));
  });
}

async function render(): Promise<void> {
  status = await json<Status>("/api/status");
  const serviceOptions = status.services.map((service) =>
    `<option value="${escapeHtml(`${service.id}\u0000${service.protocol}`)}">${escapeHtml(service.alias)} · ${service.protocol.toUpperCase()}</option>`,
  ).join("");
  app.innerHTML = `<header><div><p class="eyebrow">HOME AUTHORITY</p><h1>旅行端凭据签发</h1><p class="subtitle">${escapeHtml(status.home_alias)} · ${escapeHtml(status.home_id)}</p></div><span class="local">本地管理页面</span></header>
  <div id="notice" class="notice"></div>
  <section class="panel issue-panel"><div class="panel-title"><div><p class="eyebrow">签发新凭据</p><h2>选择授权边界</h2></div><span>私钥密码不会保存</span></div>
    <div class="scope-grid">
      <label class="scope"><input type="radio" name="scope" value="home" checked><strong>当前 Home</strong><small>可访问此 Home 当前及以后发布的全部业务</small></label>
      <label class="scope"><input type="radio" name="scope" value="service"><strong>指定业务</strong><small>只允许访问下方选中的一个逻辑业务</small></label>
      ${status.global_authority_available ? '<label class="scope super"><input type="radio" name="scope" value="global"><strong>全局超级授权</strong><small>可访问所有 Home；仅在确有需要时使用</small></label>' : ""}
    </div>
    <div class="form-grid">
      <label>旅行端申请文件<input id="request" type="file" accept="application/json,.json"><small id="request-name">尚未选择</small></label>
      <label>指定业务<select id="service">${serviceOptions}</select></label>
      <label>有效期（天）<input id="days" type="number" min="1" max="3650" value="${status.default_valid_days}"></label>
      <label>签名私钥密码<input id="password" type="password" autocomplete="current-password" placeholder="只用于本次签名"></label>
    </div>
    <div class="actions"><p>旅行端私钥从不上传；Home 只读取申请中的公钥，并把签名授权同步给 Server。</p><button id="issue">签发并下载结果</button></div>
  </section>
  <section class="panel"><div class="panel-title"><div><p class="eyebrow">已签发凭据</p><h2>撤销与状态</h2></div><button id="refresh" class="secondary">刷新</button></div>
    <div class="table-wrap"><table><thead><tr><th>旅行端</th><th>授权范围</th><th>到期日</th><th>状态</th><th></th></tr></thead><tbody id="credentials"></tbody></table></div>
  </section>
  <footer>普通 Home 签名密钥只能签发本 Home 范围；全局超级签名密钥是独立配置的更高权限能力。</footer>`;
  document.querySelector<HTMLInputElement>("#request")?.addEventListener("change", async (event) => {
    const file = (event.target as HTMLInputElement).files?.[0];
    if (!file) return;
    try {
      enrollmentRequest = JSON.parse(await file.text());
      const name = document.querySelector<HTMLElement>("#request-name");
      if (name) name.textContent = file.name;
      flash("申请文件已读取，签发前请确认授权范围。 ");
    } catch { flash("申请文件不是有效 JSON。", true); }
  });
  document.querySelector<HTMLButtonElement>("#issue")?.addEventListener("click", () => void issue().catch((error) => flash(String(error), true)));
  document.querySelector<HTMLButtonElement>("#refresh")?.addEventListener("click", () => void renderCredentials().catch((error) => flash(String(error), true)));
  await renderCredentials();
}

void render().catch((error) => { app.innerHTML = `<section class="fatal"><h1>无法打开签发页面</h1><p>${escapeHtml(String(error))}</p></section>`; });
