import "./style.css";

type Protocol = "tcp" | "udp";

interface Mapping {
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
}

interface Service {
  id: string;
  alias: string;
  protocol: Protocol;
  target: string;
}

interface Catalog {
  home_id: string;
  home_alias: string;
  generation: number;
  services: Service[];
}

const root = document.querySelector<HTMLElement>("#app");

if (!root) {
  throw new Error("Missing #app root");
}
const app: HTMLElement = root;

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

async function fetchJson<T>(path: string): Promise<T> {
  const response = await fetch(path, { headers: { Accept: "application/json" } });
  if (!response.ok) {
    throw new Error(`${path} returned ${response.status}`);
  }
  return (await response.json()) as T;
}

async function render(): Promise<void> {
  try {
    const [status, catalog] = await Promise.all([
      fetchJson<Status>("/api/status"),
      fetchJson<Catalog>("/api/catalog"),
    ]);
    const serviceById = new Map(catalog.services.map((service) => [service.id, service]));
    const rows = status.mappings
      .map((mapping) => {
        const service = serviceById.get(mapping.service_id);
        return `<tr>
          <td><span class="protocol ${mapping.protocol}">${mapping.protocol.toUpperCase()}</span></td>
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
    <section class="metrics">
      <article><span>Home</span><strong>${escapeHtml(catalog.home_alias || "Connecting")}</strong></article>
      <article><span>Active flows</span><strong>${status.active_flows}</strong></article>
      <article><span>Uptime</span><strong>${formatUptime(status.uptime_secs)}</strong></article>
      <article><span>Catalog</span><strong>v${status.catalog_generation}</strong></article>
    </section>
    <section class="panel">
      <div class="panel-heading"><div><p class="eyebrow">Local listeners</p><h2>Service mappings</h2></div><span>${status.mappings.length} configured</span></div>
      <div class="table-wrap"><table><thead><tr><th>Protocol</th><th>Service</th><th>Local address</th><th>Status</th></tr></thead><tbody>${rows || '<tr><td colspan="4" class="empty">No mappings configured</td></tr>'}</tbody></table></div>
    </section>
    <footer>All business traffic is mutually authenticated and encrypted to the Home Agent.</footer>`;
  } catch (error) {
    app.innerHTML = `<section class="error"><p class="eyebrow">Local agent unavailable</p><h1>Unable to load status</h1><p>${escapeHtml(String(error))}</p><button type="button">Retry</button></section>`;
    app.querySelector("button")?.addEventListener("click", () => void render());
  }
}

void render();
window.setInterval(() => void render(), 5000);
