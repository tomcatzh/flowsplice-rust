// Durable connection intent only. Credentials and terminal input/output never enter this file.
export type Workspace = {
  version: 1;
  classMode: boolean;
  identityWanted: boolean;
  homes: { id: string; name: string; platform: string; relay: string; wanted: boolean }[];
  tabs: { home: string; session: string; name: string; mode: "read_only" | "read_write"; deleted: boolean }[];
  active: { home: string; session: string } | null;
  selected: string | null;
  page: "manage" | "terminal" | "opened";
};

export function readWorkspace(value: unknown): Workspace | null {
  try {
    if (!value || typeof value !== "object" || JSON.stringify(value).length > 65536) return null;
    const v = value as Workspace;
    const object = (x: any, keys: string[]) => x && typeof x === "object" && !Array.isArray(x) && Object.keys(x).length === keys.length && keys.every(k => Object.hasOwn(x, k));
    const string = (x: unknown, limit: number, empty = false) => typeof x === "string" && (empty || x.length > 0) && new TextEncoder().encode(x).length <= limit && !/[\u0000-\u001f\u007f-\u009f]/u.test(x);
    if (!object(v, ["version", "classMode", "identityWanted", "homes", "tabs", "active", "selected", "page"]) || v.version !== 1 || typeof v.classMode !== "boolean" || typeof v.identityWanted !== "boolean" || !["manage", "terminal", "opened"].includes(v.page)) return null;
    if (!Array.isArray(v.homes) || v.homes.length > 8 || !Array.isArray(v.tabs) || v.tabs.length > 64) return null;
    const ids = new Set<string>();
    for (const h of v.homes) {
      if (!object(h, ["id", "name", "platform", "relay", "wanted"]) || !string(h.id, 128) || ids.has(h.id) || !string(h.name, 128) || !["host", "linux", "macos"].includes(h.platform) || !string(h.relay, 512, true) || typeof h.wanted !== "boolean") return null;
      ids.add(h.id);
    }
    const tabs = new Set<string>();
    for (const t of v.tabs) {
      const key = JSON.stringify([t.home, t.session]);
      if (!object(t, ["home", "session", "name", "mode", "deleted"]) || !ids.has(t.home) || !string(t.session, 128) || !string(t.name, 256, true) || !["read_only", "read_write"].includes(t.mode) || typeof t.deleted !== "boolean" || tabs.has(key)) return null;
      tabs.add(key);
    }
    if (v.selected !== null && !ids.has(v.selected)) return null;
    if (v.active !== null && (!object(v.active, ["home", "session"]) || !tabs.has(JSON.stringify([v.active.home, v.active.session])))) return null;
    return v;
  } catch { return null; }
}
