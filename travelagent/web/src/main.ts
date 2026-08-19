import "./style.css";

type Protocol = "tcp" | "udp";

interface Mapping {
  home_id: string;
  service_id: string;
  protocol: Protocol;
  bind: string;
}

interface Status {
  ok: boolean;
  travel_id: string;
  uptime_secs: number;
  active_flows: number;
  catalog_generation: number;
  mappings: Mapping[];
  private_key_password_rotation_available: boolean;
}

interface Service {
  id: string;
  alias: string;
  protocol: Protocol;
  target: string;
}

interface HomeCatalog {
  home_id: string;
  home_alias: string;
  services: Service[];
}

interface Catalog {
  generation: number;
  homes: HomeCatalog[];
}

interface RotatePasswordResult {
  rotated_keys: number;
}

const root = document.querySelector<HTMLElement>("#app");

if (!root) {
  throw new Error("Missing #app root");
}
const app: HTMLElement = root;
let passwordDialogOpen = false;
let noticeMessage = "";

function escapeHtml(value: string): string {
  const span = document.createElement("span");
  span.textContent = value;
  return span.innerHTML;
}

function formatUptime(seconds: number): string {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  return `${hours}h ${minutes}m`;
}

function businessKey(homeId: string, serviceId: string, protocol: Protocol): string {
  return `${homeId}\u0000${serviceId}\u0000${protocol}`;
}

async function requestJson<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: { Accept: "application/json", "Content-Type": "application/json", ...(init?.headers ?? {}) },
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({})) as { error?: string };
    throw new Error(body.error ?? `${path} returned ${response.status}`);
  }
  return (await response.json()) as T;
}

function fetchJson<T>(path: string): Promise<T> {
  return requestJson<T>(path);
}

function friendlyError(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  if (message.includes("failed to decrypt Travel management private key")) {
    return "The current password cannot decrypt the Travel management private key.";
  }
  if (message.includes("failed to decrypt Travel business private key")) {
    return "The current password cannot decrypt the Travel business private key.";
  }
  return message.replace(/^Error:\s*/, "");
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
  if (!currentPassword) throw new Error("Enter the current Travel private-key password.");
  if ([...newPassword].length < 12) throw new Error("The new password must contain at least 12 characters.");
  if (newPassword !== confirmation) throw new Error("The new passwords do not match.");
  if (newPassword === currentPassword) throw new Error("The new password must differ from the current password.");
  if (!saved) throw new Error("Confirm that you stored the new password separately.");
  const button = document.querySelector<HTMLButtonElement>("#rotate-password");
  if (button) {
    button.disabled = true;
    button.textContent = "Verifying and rotating…";
  }
  try {
    const result = await requestJson<RotatePasswordResult>("/api/private-key-password", {
      method: "POST",
      body: JSON.stringify({ current_password: currentPassword, new_password: newPassword }),
    });
    noticeMessage = `Password rotation complete: ${result.rotated_keys} Travel private keys now use the new password.`;
    document.querySelector<HTMLDialogElement>("#password-dialog")?.close();
    clearPasswordDialog();
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = "Rotate password";
    }
  }
}

async function render(): Promise<void> {
  try {
    const [status, catalog] = await Promise.all([
      fetchJson<Status>("/api/status"),
      fetchJson<Catalog>("/api/catalog"),
    ]);
    const homeById = new Map(catalog.homes.map((home) => [home.home_id, home]));
    const serviceByBusiness = new Map(
      catalog.homes.flatMap((home) =>
        home.services.map((service) => [
          businessKey(home.home_id, service.id, service.protocol),
          service,
        ] as const),
      ),
    );
    const rows = status.mappings
      .map((mapping) => {
        const home = homeById.get(mapping.home_id);
        const service = serviceByBusiness.get(
          businessKey(mapping.home_id, mapping.service_id, mapping.protocol),
        );
        return `<tr>
          <td><span class="protocol ${mapping.protocol}">${mapping.protocol.toUpperCase()}</span></td>
          <td><strong>${escapeHtml(home?.home_alias ?? mapping.home_id)}</strong><small>${escapeHtml(mapping.home_id)}</small></td>
          <td><strong>${escapeHtml(service?.alias ?? mapping.service_id)}</strong><small>${escapeHtml(mapping.service_id)}</small></td>
          <td><code>${escapeHtml(mapping.bind)}</code></td>
          <td><span class="state ${service ? "ready" : "waiting"}">${service ? "Ready" : "Waiting for catalog"}</span></td>
        </tr>`;
      })
      .join("");

    app.innerHTML = `<header>
      <div><p class="eyebrow">Private service access</p><h1>FlowSplice</h1></div>
      <span class="agent-id">${escapeHtml(status.travel_id)}</span>
    </header>
    <div id="notice" class="notice ${noticeMessage ? "success" : ""}">${escapeHtml(noticeMessage)}</div>
    <section class="metrics">
      <article><span>Homes</span><strong>${catalog.homes.length || "Connecting"}</strong></article>
      <article><span>Active flows</span><strong>${status.active_flows}</strong></article>
      <article><span>Uptime</span><strong>${formatUptime(status.uptime_secs)}</strong></article>
      <article><span>Catalog</span><strong>v${status.catalog_generation}</strong></article>
    </section>
    <section class="panel">
      <div class="panel-heading"><div><p class="eyebrow">Local listeners</p><h2>Service mappings</h2></div><span>${status.mappings.length} configured</span></div>
      <div class="table-wrap"><table><thead><tr><th>Protocol</th><th>Home</th><th>Service</th><th>Local address</th><th>Status</th></tr></thead><tbody>${rows || '<tr><td colspan="5" class="empty">No mappings configured</td></tr>'}</tbody></table></div>
    </section>
    ${status.private_key_password_rotation_available ? `<section class="panel key-panel"><div class="panel-heading"><div><p class="eyebrow">Local key maintenance</p><h2>Travel private-key password</h2></div><button id="open-password-dialog" class="secondary">Change password</button></div>
      <div class="maintenance-copy"><p>Re-encrypts both local Travel private keys. Active flows and the running process continue without interruption.</p><p>FlowSplice does not store either password and does not write to the system keychain.</p></div>
    </section>` : ""}
    <dialog id="password-dialog"><form id="password-form" autocomplete="off"><div class="dialog-title"><p class="eyebrow">Local operation</p><h2>Change Travel key password</h2><p>Both keys are verified before replacement. An interrupted replacement is completed automatically on the next start.</p></div>
      <div id="password-dialog-notice" class="dialog-notice"></div>
      <label>Current password<input id="current-key-password" type="password" autocomplete="current-password"></label>
      <label>New password<input id="new-key-password" type="password" autocomplete="new-password"><small>At least 12 characters</small></label>
      <label>Confirm new password<input id="confirm-key-password" type="password" autocomplete="new-password"></label>
      <label class="saved-confirmation"><input id="password-saved" type="checkbox">I stored the new password in my own password manager</label>
      <div class="dialog-actions"><button id="close-password-dialog" type="button" class="secondary">Cancel</button><button id="rotate-password" type="submit">Rotate password</button></div>
    </form></dialog>
    <footer>Every logical service is pinned to its configured Home Agent with mutual authentication and end-to-end encryption.</footer>`;
    const passwordDialog = document.querySelector<HTMLDialogElement>("#password-dialog");
    document.querySelector<HTMLButtonElement>("#open-password-dialog")?.addEventListener("click", () => {
      clearPasswordDialog();
      passwordDialogOpen = true;
      passwordDialog?.showModal();
    });
    document.querySelector<HTMLButtonElement>("#close-password-dialog")?.addEventListener("click", () => passwordDialog?.close());
    passwordDialog?.addEventListener("close", () => {
      passwordDialogOpen = false;
      clearPasswordDialog();
    });
    document.querySelector<HTMLFormElement>("#password-form")?.addEventListener("submit", (event) => {
      event.preventDefault();
      void rotatePrivateKeyPassword().catch((error) => {
        const notice = document.querySelector<HTMLElement>("#password-dialog-notice");
        if (notice) notice.textContent = friendlyError(error);
      });
    });
  } catch (error) {
    app.innerHTML = `<section class="error"><p class="eyebrow">Local agent unavailable</p><h1>Unable to load status</h1><p>${escapeHtml(String(error))}</p><button type="button">Retry</button></section>`;
    app.querySelector("button")?.addEventListener("click", () => void render());
  }
}

void render();
window.setInterval(() => {
  if (!passwordDialogOpen) void render();
}, 5000);
