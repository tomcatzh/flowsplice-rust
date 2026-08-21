import "./style.css";

type Protocol = "tcp" | "udp";
type Page = "approvals" | "credentials" | "statistics";
type CredentialStatus = "active" | "revoked" | "expired" | "all";
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
  home_enrollment_available: boolean;
  private_key_password_rotation_available: boolean;
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
interface IssueResult { generation: number; enrollment: unknown; reused: boolean }
interface RotatePasswordResult { rotated_keys: number }
interface PendingEnrollment { request_id: string; travel_id: string; home_id: string; received_at_unix_secs: number; approved: boolean; bootstrap: boolean; verification_code?: string }
type HomeProfile = "serving_only" | "home_issuer" | "global_issuer";
interface PendingHomeEnrollment { request_id: string; home_id: string; received_at_unix_secs: number; approved: boolean; verification_code: string; profile?: HomeProfile }
interface Paged<T> { items: T[]; page: number; page_size: number; total: number; total_pages: number }
interface MetricRollup { metric_family: string; dimensions: Record<string, string>; count: number; sum: number; weighted_average: number; average_per_five_minutes: number }
interface Statistics { period: "day" | "week" | "month" | "year"; dropped_events: number; overview: MetricRollup[]; breakdowns: MetricRollup[] }

const root = document.querySelector<HTMLElement>("#app");
if (!root) throw new Error("Missing #app root");
const app = root;
let status: Status;
let statisticsPeriod: Statistics["period"] = "day";
let currentPage: Page = "approvals";
let pendingTravelPage = 1;
let pendingHomePage = 1;
let credentialPage = 1;
let credentialStatus: CredentialStatus = "active";
let credentialSearch = "";
let pendingRequestId: string | null = null;
let pendingHomeRequestId: string | null = null;
let revokeCredentialId: string | null = null;

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

function metricLabel(value: string): string { return value.replaceAll("_", " "); }
function dimensionsLabel(value: Record<string, string>): string { return Object.entries(value).map(([key, item]) => `${key}=${item}`).join(" · ") || "全部业务"; }
function number(value: number): string { return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 }).format(value); }

function flash(message: string, error = false): void {
  const node = document.querySelector<HTMLElement>("#notice");
  if (!node) return;
  node.textContent = message;
  node.className = error ? "notice error" : "notice success";
}

function friendlyError(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  if (message.includes("failed to load management CA private key")) {
    return "无法解密 Home 的管理 CA 私钥。请输入 Home 签发密钥密码，不是生成 Travel 申请时使用的密码。";
  }
  if (message.includes("failed to load business CA private key")) {
    return "无法解密 Home 的业务 CA 私钥。请检查 Home 签发密钥密码及密钥配置。";
  }
  if (message.includes("failed to load Travel authorization private key")) {
    return "无法解密 Home 的授权签名私钥。请检查 Home 签发密钥密码及所选授权范围。";
  }
  if (message.includes("failed to decrypt management CA private key")) {
    return "当前密码无法解密 Home 的管理 CA 私钥。";
  }
  if (message.includes("failed to decrypt business CA private key")) {
    return "当前密码无法解密 Home 的业务 CA 私钥。";
  }
  if (message.includes("failed to decrypt Home authorization private key")) {
    return "当前密码无法解密 Home 授权签名私钥。";
  }
  if (message.includes("failed to decrypt global authorization private key")) {
    return "当前密码无法解密全局授权签名私钥。";
  }
  if (message.includes("credential is no longer active")) {
    return "这份申请的相同授权已经失效或撤销，不能通过重复签发恢复。请在 Travel 上重新发起远程申请。";
  }
  if (message.includes("request id was reused with different request content")) {
    return "申请编号与已记录内容冲突，请在 Travel 上重新发起远程申请。";
  }
  if (message.includes("already used for a different authorization")) {
    return "这份 Travel 申请已经签发过其他授权范围或有效期。每份申请只能产生一张凭据，请在 Travel 上重新发起远程申请。";
  }
  return message.replace(/^Error:\s*/, "");
}

function selectedScope(name: string, serviceSelector: string): Scope {
  const kind = document.querySelector<HTMLInputElement>(`input[name="${name}"]:checked`)?.value;
  if (kind === "global") return { kind: "global" };
  if (kind === "service") {
    const value = (document.querySelector<HTMLSelectElement>(serviceSelector)?.value ?? "").split("\u0000");
    if (value.length !== 2) throw new Error("请选择要授权的业务");
    return { kind: "service", home_id: status.home_id, service_id: value[0], protocol: value[1] as Protocol };
  }
  return { kind: "home", home_id: status.home_id };
}

function updateServiceField(name: string, fieldSelector: string, serviceSelector: string): void {
  const serviceSelected = document.querySelector<HTMLInputElement>(`input[name="${name}"]:checked`)?.value === "service";
  const field = document.querySelector<HTMLElement>(fieldSelector);
  const select = document.querySelector<HTMLSelectElement>(serviceSelector);
  if (!field || !select) return;
  select.disabled = !serviceSelected;
  field.classList.toggle("disabled", !serviceSelected);
  field.setAttribute("aria-disabled", String(!serviceSelected));
}

function clearRevokeDialog(): void {
  revokeCredentialId = null;
  document.querySelector<HTMLFormElement>("#revoke-form")?.reset();
  const notice = document.querySelector<HTMLElement>("#revoke-dialog-notice");
  if (notice) notice.textContent = "";
}

function openRevokeDialog(credentialId: string, travelId: string): void {
  revokeCredentialId = credentialId;
  const target = document.querySelector<HTMLElement>("#revoke-target");
  if (target) target.textContent = travelId;
  document.querySelector<HTMLDialogElement>("#revoke-dialog")?.showModal();
}

async function revoke(): Promise<void> {
  if (!revokeCredentialId) throw new Error("未选择要撤销的凭据");
  const reason = document.querySelector<HTMLInputElement>("#revoke-reason")?.value.trim() ?? "";
  const password = document.querySelector<HTMLInputElement>("#revoke-password")?.value ?? "";
  if (!reason) throw new Error("请输入撤销原因");
  if (!password) throw new Error("请输入当前 Home 签发密码");
  const button = document.querySelector<HTMLButtonElement>("#confirm-revoke");
  if (button) button.disabled = true;
  try {
    await json("/api/revoke", { method: "POST", body: JSON.stringify({ credential_id: revokeCredentialId, reason, password }) });
    document.querySelector<HTMLDialogElement>("#revoke-dialog")?.close();
    flash("撤销已同步到 Server、Relay 和 Home。现有连接会被关闭。");
    await renderCredentials();
  } finally {
    if (button) button.disabled = false;
  }
}

function clearPasswordDialog(): void {
  ["#current-key-password", "#new-key-password", "#confirm-key-password"].forEach((selector) => {
    const input = document.querySelector<HTMLInputElement>(selector);
    if (input) input.value = "";
  });
  const saved = document.querySelector<HTMLInputElement>("#password-saved");
  if (saved) saved.checked = false;
  const notice = document.querySelector<HTMLElement>("#password-dialog-notice");
  if (notice) notice.textContent = "";
}

async function rotatePrivateKeyPassword(): Promise<void> {
  const currentPassword = document.querySelector<HTMLInputElement>("#current-key-password")?.value ?? "";
  const newPassword = document.querySelector<HTMLInputElement>("#new-key-password")?.value ?? "";
  const confirmation = document.querySelector<HTMLInputElement>("#confirm-key-password")?.value ?? "";
  const saved = document.querySelector<HTMLInputElement>("#password-saved")?.checked ?? false;
  if (!currentPassword) throw new Error("请输入当前 Home 签发密钥密码");
  if ([...newPassword].length < 12) throw new Error("新密码至少需要 12 个字符");
  if (newPassword !== confirmation) throw new Error("两次输入的新密码不一致");
  if (newPassword === currentPassword) throw new Error("新密码必须与当前密码不同");
  if (!saved) throw new Error("请先确认新密码已经另行保存");
  const button = document.querySelector<HTMLButtonElement>("#rotate-password");
  if (button) {
    button.disabled = true;
    button.textContent = "正在验证并轮换…";
  }
  try {
    const result = await json<RotatePasswordResult>("/api/private-key-password", {
      method: "POST",
      body: JSON.stringify({ current_password: currentPassword, new_password: newPassword }),
    });
    document.querySelector<HTMLDialogElement>("#password-dialog")?.close();
    clearPasswordDialog();
    flash(`密码轮换成功：${result.rotated_keys} 把 Home 签发私钥已使用新密码重新加密。`);
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = "确认轮换";
    }
  }
}

function pagerMarkup(paged: Paged<unknown>): string {
  if (paged.total_pages <= 1) return "";
  const first = (paged.page - 1) * paged.page_size + 1;
  const last = Math.min(paged.page * paged.page_size, paged.total);
  return `<div class="pager"><span>${first}–${last} / 共 ${paged.total} 条</span><div><button type="button" class="secondary page-button" data-page="${paged.page - 1}" ${paged.page <= 1 ? "disabled" : ""}>上一页</button><span>第 ${paged.page} / ${paged.total_pages} 页</span><button type="button" class="secondary page-button" data-page="${paged.page + 1}" ${paged.page >= paged.total_pages ? "disabled" : ""}>下一页</button></div></div>`;
}

function bindPager(selector: string, changePage: (page: number) => Promise<void>): void {
  document.querySelectorAll<HTMLButtonElement>(`${selector} .page-button`).forEach((button) => {
    button.addEventListener("click", () => {
      const page = Number(button.dataset.page);
      if (Number.isInteger(page) && page > 0) void changePage(page).catch((error) => flash(friendlyError(error), true));
    });
  });
}

async function renderCredentials(): Promise<void> {
  const params = new URLSearchParams({
    page: String(credentialPage),
    page_size: "20",
    status: credentialStatus,
  });
  if (credentialSearch) params.set("search", credentialSearch);
  const response = await json<Paged<Credential>>(`/api/credentials?${params.toString()}`);
  credentialPage = response.page;
  const table = document.querySelector<HTMLElement>("#credentials");
  if (!table) return;
  table.innerHTML = response.items.length ? response.items.map((credential) => `<tr>
    <td><strong>${escapeHtml(credential.travel_id)}</strong><small>${escapeHtml(credential.credential_id)}</small></td>
    <td>${escapeHtml(scopeLabel(credential.scope))}</td>
    <td>${new Date(credential.not_after_unix_secs * 1000).toLocaleDateString("zh-CN")}</td>
    <td><span class="state ${credential.active ? "active" : "inactive"}">${credential.revoked ? "已撤销" : credential.active ? "有效" : "已失效"}</span></td>
    <td>${credential.revoked ? "" : `<button class="danger revoke" data-id="${escapeHtml(credential.credential_id)}" data-travel-id="${escapeHtml(credential.travel_id)}">撤销</button>`}</td>
  </tr>`).join("") : '<tr><td colspan="5" class="empty">当前筛选条件下没有 Travel 凭据</td></tr>';
  table.querySelectorAll<HTMLButtonElement>(".revoke").forEach((button) => {
    button.addEventListener("click", () => openRevokeDialog(button.dataset.id ?? "", button.dataset.travelId ?? ""));
  });
  const pager = document.querySelector<HTMLElement>("#credential-pager");
  if (pager) pager.innerHTML = pagerMarkup(response);
  bindPager("#credential-pager", async (page) => {
    credentialPage = page;
    await renderCredentials();
  });
}

function clearApprovalDialog(): void {
  pendingRequestId = null;
  document.querySelector<HTMLFormElement>("#approval-form")?.reset();
  const days = document.querySelector<HTMLInputElement>("#approval-days");
  if (days) days.value = String(status.default_valid_days);
  const notice = document.querySelector<HTMLElement>("#approval-dialog-notice");
  if (notice) notice.textContent = "";
  updateServiceField("approval-scope", "#approval-service-field", "#approval-service");
}

function openApprovalDialog(requestId: string, travelId: string, verificationCode?: string): void {
  pendingRequestId = requestId;
  const target = document.querySelector<HTMLElement>("#approval-target");
  if (target) target.textContent = travelId;
  const code = document.querySelector<HTMLElement>("#approval-verification-code");
  if (code) {
    code.textContent = verificationCode ?? "";
    code.closest<HTMLElement>(".verification-line")?.classList.toggle("hidden", !verificationCode);
  }
  document.querySelector<HTMLDialogElement>("#approval-dialog")?.showModal();
}

async function approveRemote(): Promise<void> {
  if (!pendingRequestId) throw new Error("未选择待批准申请");
  const password = document.querySelector<HTMLInputElement>("#approval-password")?.value ?? "";
  const days = Number(document.querySelector<HTMLInputElement>("#approval-days")?.value ?? status.default_valid_days);
  if (!password) throw new Error("请输入当前 Home 签发密码");
  const button = document.querySelector<HTMLButtonElement>("#confirm-approval");
  if (button) button.disabled = true;
  try {
    await json<IssueResult>("/api/enrollment/approve", {
      method: "POST",
      body: JSON.stringify({ request_id: pendingRequestId, valid_days: days, scope: selectedScope("approval-scope", "#approval-service"), password }),
    });
    document.querySelector<HTMLDialogElement>("#approval-dialog")?.close();
    flash("远程申请已签发；Travel 将通过控制连接自动取回结果。");
    await renderPending();
  } finally {
    if (button) button.disabled = false;
  }
}

async function renderPending(): Promise<void> {
  const response = await json<Paged<PendingEnrollment>>(`/api/enrollment/pending?page=${pendingTravelPage}&page_size=20`);
  pendingTravelPage = response.page;
  const table = document.querySelector<HTMLElement>("#pending-enrollments");
  if (!table) return;
  table.innerHTML = response.items.length ? response.items.map((item) => `<tr><td><strong>${escapeHtml(item.travel_id)}</strong><small>${escapeHtml(item.request_id)}</small>${item.bootstrap && item.verification_code ? `<small>首次注册校验码：<code>${escapeHtml(item.verification_code)}</code></small>` : ""}</td><td>${new Date(item.received_at_unix_secs * 1000).toLocaleString("zh-CN")}</td><td><span class="state inactive">${item.bootstrap ? "首次注册待核对" : "待批准"}</span></td><td><button class="approve-remote" data-id="${escapeHtml(item.request_id)}" data-travel-id="${escapeHtml(item.travel_id)}" data-verification-code="${escapeHtml(item.verification_code ?? "")}">审核并批准</button></td></tr>`).join("") : '<tr><td colspan="4" class="empty">暂无待审批的 Travel 申请</td></tr>';
  table.querySelectorAll<HTMLButtonElement>(".approve-remote").forEach((button) => button.addEventListener("click", () => openApprovalDialog(button.dataset.id ?? "", button.dataset.travelId ?? "", button.dataset.verificationCode || undefined)));
  const pager = document.querySelector<HTMLElement>("#pending-travel-pager");
  if (pager) pager.innerHTML = pagerMarkup(response);
  bindPager("#pending-travel-pager", async (page) => {
    pendingTravelPage = page;
    await renderPending();
  });
}

function clearHomeApprovalDialog(): void {
  pendingHomeRequestId = null;
  document.querySelector<HTMLFormElement>("#home-approval-form")?.reset();
  const days = document.querySelector<HTMLInputElement>("#home-approval-days");
  if (days) days.value = String(status.default_valid_days);
  const notice = document.querySelector<HTMLElement>("#home-approval-dialog-notice");
  if (notice) notice.textContent = "";
}

function openHomeApprovalDialog(requestId: string, homeId: string, verificationCode: string): void {
  pendingHomeRequestId = requestId;
  const target = document.querySelector<HTMLElement>("#home-approval-target");
  if (target) target.textContent = homeId;
  const code = document.querySelector<HTMLElement>("#home-approval-verification-code");
  if (code) code.textContent = verificationCode;
  document.querySelector<HTMLDialogElement>("#home-approval-dialog")?.showModal();
}

async function approveHomeRemote(): Promise<void> {
  if (!pendingHomeRequestId) throw new Error("未选择待批准的 Home");
  const profile = document.querySelector<HTMLInputElement>('input[name="home-approval-profile"]:checked')?.value as HomeProfile | undefined;
  if (!profile) throw new Error("请选择新 Home 的权限");
  const password = document.querySelector<HTMLInputElement>("#home-approval-password")?.value ?? "";
  if (!password) throw new Error("请输入当前 Home 签发密码");
  const validDays = Number(document.querySelector<HTMLInputElement>("#home-approval-days")?.value ?? status.default_valid_days);
  const button = document.querySelector<HTMLButtonElement>("#confirm-home-approval");
  if (button) button.disabled = true;
  try {
    await json("/api/home-enrollment/approve", {
      method: "POST",
      body: JSON.stringify({ request_id: pendingHomeRequestId, profile, valid_days: validDays, password }),
    });
    document.querySelector<HTMLDialogElement>("#home-approval-dialog")?.close();
    flash("新 Home 已批准；对方会通过 Server 自动取回证书、信任与完整配置。 ");
    await renderPendingHomes();
  } finally {
    if (button) button.disabled = false;
  }
}

async function renderPendingHomes(): Promise<void> {
  if (!status.home_enrollment_available) return;
  const response = await json<Paged<PendingHomeEnrollment>>(`/api/home-enrollment/pending?page=${pendingHomePage}&page_size=20`);
  pendingHomePage = response.page;
  const table = document.querySelector<HTMLElement>("#pending-homes");
  if (!table) return;
  table.innerHTML = response.items.length ? response.items.map((item) => `<tr><td><strong>${escapeHtml(item.home_id)}</strong><small>${escapeHtml(item.request_id)}</small><small>校验码：<code>${escapeHtml(item.verification_code)}</code></small></td><td>${new Date(item.received_at_unix_secs * 1000).toLocaleString("zh-CN")}</td><td><span class="state inactive">等待批准</span></td><td><button class="approve-home" data-id="${escapeHtml(item.request_id)}" data-home-id="${escapeHtml(item.home_id)}" data-verification-code="${escapeHtml(item.verification_code)}">审核并批准</button></td></tr>`).join("") : '<tr><td colspan="4" class="empty">暂无待审批的新 Home 申请</td></tr>';
  table.querySelectorAll<HTMLButtonElement>(".approve-home").forEach((button) => button.addEventListener("click", () => openHomeApprovalDialog(button.dataset.id ?? "", button.dataset.homeId ?? "", button.dataset.verificationCode ?? "")));
  const pager = document.querySelector<HTMLElement>("#pending-home-pager");
  if (pager) pager.innerHTML = pagerMarkup(response);
  bindPager("#pending-home-pager", async (page) => {
    pendingHomePage = page;
    await renderPendingHomes();
  });
}

function pageTabs(): string {
  return `<nav class="page-tabs" role="tablist" aria-label="Home 后台页面">
    <button type="button" role="tab" aria-selected="${currentPage === "approvals"}" class="page-tab ${currentPage === "approvals" ? "active" : ""}" data-page="approvals">待审批</button>
    <button type="button" role="tab" aria-selected="${currentPage === "credentials"}" class="page-tab ${currentPage === "credentials" ? "active" : ""}" data-page="credentials">凭据</button>
    <button type="button" role="tab" aria-selected="${currentPage === "statistics"}" class="page-tab ${currentPage === "statistics" ? "active" : ""}" data-page="statistics">统计</button>
  </nav>`;
}

function header(): string {
  return `<header><div><p class="eyebrow">HOME AUTHORITY</p><h1>Home ${escapeHtml(status.home_id)} 后台管理</h1><p class="subtitle">${escapeHtml(status.home_alias)}</p></div><span class="local">本地管理页面</span></header>`;
}

function updateDocumentTitle(): void {
  const label = currentPage === "approvals" ? "待审批" : currentPage === "credentials" ? "凭据" : "业务统计";
  document.title = `Home ${status.home_id} ${label} · FlowSplice`;
}

function renderFailure(title: string, error: unknown): void {
  document.title = "FlowSplice · Home 后台管理";
  app.innerHTML = `<section class="fatal"><h1>${title}</h1><p>${escapeHtml(String(error))}</p></section>`;
}

function bindPageTabs(): void {
  document.querySelectorAll<HTMLButtonElement>(".page-tab").forEach((button) => {
    button.addEventListener("click", () => {
      const page = button.dataset.page as Page | undefined;
      if (!page || page === currentPage) return;
      currentPage = page;
      void render().catch((error) => renderFailure("无法打开页面", error));
    });
  });
}

async function renderStatisticsPage(): Promise<void> {
  updateDocumentTitle();
  const statistics = await json<Statistics>(`/api/statistics?period=${statisticsPeriod}`);
  const statisticCards = statistics.overview.map((item) => `<article class="stat-card"><span>${escapeHtml(metricLabel(item.metric_family))}</span><strong>${number(item.sum)}</strong><small>${number(item.average_per_five_minutes)} / 5 分钟 · ${number(item.count)} 次观测</small></article>`).join("");
  const statisticRows = statistics.breakdowns.map((item) => `<tr><td>${escapeHtml(metricLabel(item.metric_family))}</td><td>${escapeHtml(dimensionsLabel(item.dimensions))}</td><td>${number(item.sum)}</td><td>${number(item.count)}</td><td>${number(item.weighted_average)}</td><td>${number(item.average_per_five_minutes)}</td></tr>`).join("");
  app.innerHTML = `${header()}
  ${pageTabs()}
  <section class="panel statistics-panel"><div class="panel-title"><div><p class="eyebrow">业务统计</p><h2>交付流量与 Relay 路径</h2></div><div class="stats-controls"><label class="period-label">报表周期<select id="statistics-period"><option value="day" ${statisticsPeriod === "day" ? "selected" : ""}>日</option><option value="week" ${statisticsPeriod === "week" ? "selected" : ""}>周</option><option value="month" ${statisticsPeriod === "month" ? "selected" : ""}>月</option><option value="year" ${statisticsPeriod === "year" ? "selected" : ""}>年</option></select></label><button id="refresh-statistics" type="button" class="secondary">刷新</button></div></div>
    <section class="stats-grid">${statisticCards || '<article class="stat-card"><span>暂无业务观测</span><strong>0</strong><small>产生流量后会写入当前五分钟桶。</small></article>'}</section>
    <div class="table-wrap"><table><thead><tr><th>指标</th><th>业务 / Travel / Relay</th><th>总量</th><th>观测次数</th><th>加权平均</th><th>五分钟平均</th></tr></thead><tbody>${statisticRows || '<tr><td colspan="6" class="empty">当前周期暂无统计数据</td></tr>'}</tbody></table></div>
    <div class="stats-note">只统计业务交付、目标结果和签发/撤销结果；控制指令与报表上传流量不计入。丢弃的本地统计事件：${statistics.dropped_events}</div>
  </section>
  <footer>统计数据只在打开本页、切换周期或点击刷新时读取。</footer>`;
  bindPageTabs();
  document.querySelector<HTMLSelectElement>("#statistics-period")?.addEventListener("change", (event) => {
    statisticsPeriod = (event.target as HTMLSelectElement).value as Statistics["period"];
    void renderStatisticsPage().catch((error) => renderFailure("无法加载统计", error));
  });
  document.querySelector<HTMLButtonElement>("#refresh-statistics")?.addEventListener("click", () => {
    void renderStatisticsPage().catch((error) => renderFailure("无法加载统计", error));
  });
}

async function renderApprovalsPage(): Promise<void> {
  updateDocumentTitle();
  const serviceOptions = status.services.map((service) =>
    `<option value="${escapeHtml(`${service.id}\u0000${service.protocol}`)}">${escapeHtml(service.alias)} · ${service.protocol.toUpperCase()}</option>`,
  ).join("");
  app.innerHTML = `${header()}
  ${pageTabs()}
  <div id="notice" class="notice"></div>
  <section class="panel"><div class="panel-title"><div><p class="eyebrow">TRAVEL 待审批</p><h2>等待本机批准</h2></div><button id="refresh-pending" class="secondary">刷新</button></div>
    <div class="table-wrap"><table><thead><tr><th>旅行端</th><th>收到时间</th><th>状态</th><th></th></tr></thead><tbody id="pending-enrollments"></tbody></table></div>
    <div id="pending-travel-pager"></div>
  </section>
  ${status.home_enrollment_available ? `<section class="panel"><div class="panel-title"><div><p class="eyebrow">新 Home 加入</p><h2>分配运行权限</h2></div><button id="refresh-pending-homes" class="secondary">刷新</button></div>
    <div class="table-wrap"><table><thead><tr><th>Home</th><th>收到时间</th><th>状态</th><th></th></tr></thead><tbody id="pending-homes"></tbody></table></div>
    <div id="pending-home-pager"></div>
  </section>
  <dialog id="home-approval-dialog" class="wide-dialog"><form id="home-approval-form" autocomplete="off"><div class="dialog-title"><p class="eyebrow">新 Home 加入</p><h2>批准 <span id="home-approval-target"></span></h2><p class="verification-line">请先与新 Home 核对校验码：<code id="home-approval-verification-code"></code></p></div>
    <div id="home-approval-dialog-notice" class="dialog-notice"></div>
    <div class="dialog-scope-grid">
      <label class="scope"><input type="radio" name="home-approval-profile" value="serving_only" checked><strong>仅承载业务</strong><small>可以运行本 Home 的业务，不能签发 Travel，也不能批准其他 Home</small></label>
      <label class="scope"><input type="radio" name="home-approval-profile" value="home_issuer"><strong>本 Home 签发者</strong><small>可以运行业务，并为访问本 Home 的 Travel 签发凭据</small></label>
      <label class="scope super"><input type="radio" name="home-approval-profile" value="global_issuer"><strong>全局签发者</strong><small>可以签发全局 Travel 凭据，也可以批准以后加入的新 Home</small></label>
    </div>
    <label>有效期（天）<input id="home-approval-days" type="number" min="1" max="3650" value="${status.default_valid_days}"></label>
    <label>当前 Home 签发密码<input id="home-approval-password" type="password" autocomplete="current-password" placeholder="只在本机解锁签发密钥"><small>密码不会保存，也不会发送给 Server、Relay 或新 Home。</small></label>
    <div class="dialog-actions"><button id="close-home-approval-dialog" type="button" class="secondary">取消</button><button id="confirm-home-approval" type="submit">批准并远程返回</button></div>
  </form></dialog>` : ""}
  <dialog id="approval-dialog" class="wide-dialog"><form id="approval-form" autocomplete="off"><div class="dialog-title"><p class="eyebrow">远程申请</p><h2>批准 <span id="approval-target"></span></h2><p class="verification-line">请先与 Travel 端核对校验码：<code id="approval-verification-code"></code></p></div>
    <div id="approval-dialog-notice" class="dialog-notice"></div>
    <div class="dialog-scope-grid">
      <label class="scope"><input type="radio" name="approval-scope" value="home"><strong>当前 Home</strong><small>可访问此 Home 当前及以后发布的全部业务</small></label>
      <label class="scope"><input type="radio" name="approval-scope" value="service" checked><strong>指定业务</strong><small>只允许访问下方选中的一个逻辑业务</small></label>
      ${status.global_authority_available ? '<label class="scope super"><input type="radio" name="approval-scope" value="global"><strong>全局超级授权</strong><small>可访问所有 Home；仅在确有需要时使用</small></label>' : ""}
    </div>
    <div class="dialog-form-grid"><label id="approval-service-field" class="conditional-field">指定业务<select id="approval-service">${serviceOptions}</select></label><label>有效期（天）<input id="approval-days" type="number" min="1" max="3650" value="${status.default_valid_days}"></label></div>
    <label>Home 签发密码<input id="approval-password" type="password" autocomplete="current-password" placeholder="只在本机解锁签发密钥"><small>密码不会保存，也不会发送给 Server、Relay 或 Travel。</small></label>
    <div class="dialog-actions"><button id="close-approval-dialog" type="button" class="secondary">取消</button><button id="confirm-approval" type="submit">批准并远程返回</button></div>
  </form></dialog>
  <footer>首页只显示仍需人工处理的申请；批准完成后立即离开本页。</footer>`;
  bindPageTabs();
  document.querySelector<HTMLButtonElement>("#refresh-pending")?.addEventListener("click", () => void renderPending().catch((error) => flash(friendlyError(error), true)));
  document.querySelector<HTMLButtonElement>("#refresh-pending-homes")?.addEventListener("click", () => void renderPendingHomes().catch((error) => flash(friendlyError(error), true)));
  const homeApprovalDialog = document.querySelector<HTMLDialogElement>("#home-approval-dialog");
  document.querySelector<HTMLButtonElement>("#close-home-approval-dialog")?.addEventListener("click", () => homeApprovalDialog?.close());
  homeApprovalDialog?.addEventListener("close", clearHomeApprovalDialog);
  document.querySelector<HTMLFormElement>("#home-approval-form")?.addEventListener("submit", (event) => {
    event.preventDefault();
    void approveHomeRemote().catch((error) => {
      const notice = document.querySelector<HTMLElement>("#home-approval-dialog-notice");
      if (notice) notice.textContent = friendlyError(error);
    });
  });
  const approvalDialog = document.querySelector<HTMLDialogElement>("#approval-dialog");
  document.querySelector<HTMLButtonElement>("#close-approval-dialog")?.addEventListener("click", () => approvalDialog?.close());
  approvalDialog?.addEventListener("close", clearApprovalDialog);
  document.querySelector<HTMLFormElement>("#approval-form")?.addEventListener("submit", (event) => {
    event.preventDefault();
    void approveRemote().catch((error) => {
      const notice = document.querySelector<HTMLElement>("#approval-dialog-notice");
      if (notice) notice.textContent = friendlyError(error);
    });
  });
  document.querySelectorAll<HTMLInputElement>('input[name="approval-scope"]').forEach((input) => input.addEventListener("change", () => updateServiceField("approval-scope", "#approval-service-field", "#approval-service")));
  updateServiceField("approval-scope", "#approval-service-field", "#approval-service");
  await Promise.all([renderPending(), renderPendingHomes()]);
}

async function renderCredentialsPage(): Promise<void> {
  updateDocumentTitle();
  app.innerHTML = `${header()}
  ${pageTabs()}
  <div id="notice" class="notice"></div>
  <section class="panel"><div class="panel-title"><div><p class="eyebrow">TRAVEL 凭据</p><h2>撤销与状态</h2></div><button id="refresh-credentials" class="secondary">刷新</button></div>
    <form id="credential-filters" class="list-controls"><label>状态<select id="credential-status"><option value="active" ${credentialStatus === "active" ? "selected" : ""}>有效</option><option value="revoked" ${credentialStatus === "revoked" ? "selected" : ""}>已撤销</option><option value="expired" ${credentialStatus === "expired" ? "selected" : ""}>已失效</option><option value="all" ${credentialStatus === "all" ? "selected" : ""}>全部</option></select></label><label class="search-field">搜索 Travel ID 或凭据 ID<input id="credential-search" type="search" maxlength="128" autocomplete="off" placeholder="输入关键字"></label><button type="submit">查询</button></form>
    <div class="table-wrap"><table><thead><tr><th>旅行端</th><th>授权范围</th><th>到期日</th><th>状态</th><th></th></tr></thead><tbody id="credentials"></tbody></table></div>
    <div id="credential-pager"></div>
  </section>
  <dialog id="revoke-dialog"><form id="revoke-form" autocomplete="off"><div class="dialog-title"><p class="eyebrow">撤销凭据</p><h2>撤销 <span id="revoke-target"></span></h2><p>撤销立即生效且不可恢复，现有业务连接会被关闭。</p></div>
    <div id="revoke-dialog-notice" class="dialog-notice"></div>
    <label>撤销原因<input id="revoke-reason" type="text" autocomplete="off" placeholder="例如：测试完成或设备遗失"></label>
    <label>Home 签发密码<input id="revoke-password" type="password" autocomplete="current-password"><small>必须用签发密码确认，错误密码不会改变状态。</small></label>
    <div class="dialog-actions"><button id="close-revoke-dialog" type="button" class="secondary">取消</button><button id="confirm-revoke" type="submit" class="danger-solid">确认撤销</button></div>
  </form></dialog>
  ${status.private_key_password_rotation_available ? `<section class="panel maintenance-panel"><div class="panel-title"><div><p class="eyebrow">本机密钥维护</p><h2>Home 签发密码</h2></div><button id="open-password-dialog" class="secondary">更改密码</button></div>
    <div class="maintenance-copy"><p>同时重新加密管理 CA、业务 CA、Home 授权${status.global_authority_available ? "和全局授权" : ""}私钥。运行中的业务连接不受影响。</p><p>FlowSplice 不保存旧密码或新密码，也不会写入 macOS 钥匙串。</p></div>
  </section>` : ""}
  <dialog id="password-dialog"><form id="password-form" autocomplete="off"><div class="dialog-title"><p class="eyebrow">本机操作</p><h2>更改 Home 签发密码</h2><p>全部私钥验证成功后才会切换。中断时，Home 会在下次启动继续完成同一轮切换。</p></div>
    <div id="password-dialog-notice" class="dialog-notice"></div>
    <label>当前密码<input id="current-key-password" type="password" autocomplete="current-password"></label>
    <label>新密码<input id="new-key-password" type="password" autocomplete="new-password"><small>至少 12 个字符</small></label>
    <label>确认新密码<input id="confirm-key-password" type="password" autocomplete="new-password"></label>
    <label class="saved-confirmation"><input id="password-saved" type="checkbox">我已将新密码保存在自己的密码管理工具中</label>
    <div class="dialog-actions"><button id="close-password-dialog" type="button" class="secondary">取消</button><button id="rotate-password" type="submit">确认轮换</button></div>
  </form></dialog>
  <footer>凭据历史只在打开本页、筛选、翻页或点击刷新时读取。</footer>`;
  bindPageTabs();
  const search = document.querySelector<HTMLInputElement>("#credential-search");
  if (search) search.value = credentialSearch;
  document.querySelector<HTMLFormElement>("#credential-filters")?.addEventListener("submit", (event) => {
    event.preventDefault();
    credentialStatus = (document.querySelector<HTMLSelectElement>("#credential-status")?.value as CredentialStatus | undefined) ?? "active";
    credentialSearch = document.querySelector<HTMLInputElement>("#credential-search")?.value.trim() ?? "";
    credentialPage = 1;
    void renderCredentials().catch((error) => flash(friendlyError(error), true));
  });
  document.querySelector<HTMLSelectElement>("#credential-status")?.addEventListener("change", () => {
    document.querySelector<HTMLFormElement>("#credential-filters")?.requestSubmit();
  });
  document.querySelector<HTMLButtonElement>("#refresh-credentials")?.addEventListener("click", () => void renderCredentials().catch((error) => flash(friendlyError(error), true)));
  const revokeDialog = document.querySelector<HTMLDialogElement>("#revoke-dialog");
  document.querySelector<HTMLButtonElement>("#close-revoke-dialog")?.addEventListener("click", () => revokeDialog?.close());
  revokeDialog?.addEventListener("close", clearRevokeDialog);
  document.querySelector<HTMLFormElement>("#revoke-form")?.addEventListener("submit", (event) => {
    event.preventDefault();
    void revoke().catch((error) => {
      const notice = document.querySelector<HTMLElement>("#revoke-dialog-notice");
      if (notice) notice.textContent = friendlyError(error);
    });
  });
  const passwordDialog = document.querySelector<HTMLDialogElement>("#password-dialog");
  document.querySelector<HTMLButtonElement>("#open-password-dialog")?.addEventListener("click", () => {
    clearPasswordDialog();
    passwordDialog?.showModal();
  });
  document.querySelector<HTMLButtonElement>("#close-password-dialog")?.addEventListener("click", () => passwordDialog?.close());
  passwordDialog?.addEventListener("close", clearPasswordDialog);
  document.querySelector<HTMLFormElement>("#password-form")?.addEventListener("submit", (event) => {
    event.preventDefault();
    void rotatePrivateKeyPassword().catch((error) => {
      const notice = document.querySelector<HTMLElement>("#password-dialog-notice");
      if (notice) notice.textContent = friendlyError(error);
    });
  });
  await renderCredentials();
}

async function render(): Promise<void> {
  status = await json<Status>("/api/status");
  if (currentPage === "statistics") {
    await renderStatisticsPage();
  } else if (currentPage === "credentials") {
    await renderCredentialsPage();
  } else {
    await renderApprovalsPage();
  }
}

void render().catch((error) => renderFailure("无法打开 Home 后台", error));
