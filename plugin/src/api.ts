import { requestUrl } from "obsidian";
import { SamsynSettings, serverUrls } from "./settings";

export interface Grant {
  label: string;
  access: string;
  /** Share-relative paths a read-only ticket may still write; folders end with "/". */
  rw_paths: string[];
  created_at: string;
  revoked_at: string | null;
  mounted_by: number;
  last_seen_at: string | null;
}
export interface ShareRow {
  id: string;
  name: string;
  /** Folder (no trailing slash) or note path. */
  path: string;
  prefix: string;
  only_file: string | null;
  grants: Grant[];
  mounted_by: number;
}
export interface MountRow {
  id: string;
  share_id: string;
  name: string;
  mount_path: string;
  access: string;
  rw_paths: string[];
  status: string;
  /** Ids of my own shares that enclose this mount and carry its content onward to their holders. */
  forward_shares: string[];
}
export interface TicketInfo {
  share_id: string;
  name: string;
  access: string;
  rw_paths: string[];
  node_id: string;
  same_server: boolean;
  p2p_available: boolean;
  /** true/false for shares on this server; null when the owner is another server. */
  valid: boolean | null;
  /** Set when the ticket points at a share from one of your own vaults. */
  from_own_vault: string | null;
}
export interface SharingInfo {
  node_id: string;
  shares: ShareRow[];
  mounts: MountRow[];
}
export interface Health {
  ok: boolean;
  /** The server's release version, e.g. "0.3.0", "0.3.0-beta.2" or "0.3.0+1a2b3c4" from main. */
  version: string;
  node_id: string;
}

/** Thin client for the server's sharing API. Everything else in the plugin is plain y-websocket. */
export class SamsynApi {
  constructor(private settings: SamsynSettings) {}

  private base(): string {
    const u = serverUrls(this.settings.serverUrl);
    if (!u) throw new Error("no server URL configured");
    return u.http;
  }

  /** Unauthenticated liveness and version check. */
  health(): Promise<Health> {
    return this.call<Health>("GET", "/health");
  }

  private async call<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await requestUrl({
      url: `${this.base()}${path}`,
      method,
      headers: { Authorization: `Bearer ${this.settings.token}`, "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
      throw: false,
    });
    if (res.status === 204) return undefined as T;
    let data: any = null;
    try {
      data = res.json;
    } catch {
      /* non-json */
    }
    if (res.status >= 400) {
      throw new Error(data?.error ?? `HTTP ${res.status}`);
    }
    return data as T;
  }

  private vault(): string {
    return encodeURIComponent(this.settings.vaultId);
  }

  sharing(): Promise<SharingInfo> {
    return this.call("GET", `/api/vaults/${this.vault()}/sharing`);
  }
  /** `path` empty = the whole vault. `rwPaths` only with access "ro". `forwardMounts`: ids of
   *  mounts inside the path whose content the new share carries onward (the rest are skipped). */
  createShare(
    path: string,
    name: string,
    access: string,
    rwPaths: string[] = [],
    forwardMounts: string[] = [],
  ): Promise<{ share: ShareRow; path: string; ticket: string; label: string; access: string; rw_paths: string[]; forward_mounts: string[] }> {
    return this.call("POST", `/api/vaults/${this.vault()}/shares`, {
      path,
      name: name || undefined,
      access,
      rw_paths: rwPaths.length ? rwPaths : undefined,
      forward_mounts: forwardMounts.length ? forwardMounts : undefined,
    });
  }
  newTicket(shareId: string, access: string, label?: string, rwPaths: string[] = []): Promise<{ ticket: string; label: string; access: string; rw_paths: string[] }> {
    return this.call("POST", `/api/shares/${shareId}/tickets`, { access, label: label || undefined, rw_paths: rwPaths.length ? rwPaths : undefined });
  }
  revoke(shareId: string, label: string): Promise<void> {
    return this.call("DELETE", `/api/shares/${shareId}/grants/${encodeURIComponent(label)}`);
  }
  restoreTicket(shareId: string, label: string): Promise<{ restored_mounts: number }> {
    return this.call("POST", `/api/shares/${shareId}/grants/${encodeURIComponent(label)}/restore`);
  }
  /** Remove an already revoked ticket from the list (same route; second DELETE purges). */
  deleteRevoked(shareId: string, label: string): Promise<void> {
    return this.revoke(shareId, label);
  }
  unshare(shareId: string): Promise<void> {
    return this.call("DELETE", `/api/shares/${shareId}`);
  }
  /** `forwardShares`: ids of my shares enclosing `mountPath` that should carry the mounted content onward. */
  mount(ticket: string, mountPath: string, forwardShares: string[] = []): Promise<{ mount_path: string; access: string; rw_paths: string[]; forward_shares: string[] }> {
    return this.call("POST", `/api/vaults/${this.vault()}/mounts`, { ticket, mount_path: mountPath || undefined, forward_shares: forwardShares.length ? forwardShares : undefined });
  }
  /** Replace the set of enclosing shares that carry a mount onward. */
  setMountForward(mountId: string, shareIds: string[]): Promise<{ forward_shares: string[] }> {
    return this.call("PUT", `/api/mounts/${mountId}/forward`, { share_ids: shareIds });
  }
  moveMount(mountId: string, path: string): Promise<{ mount_path: string }> {
    return this.call("PATCH", `/api/mounts/${mountId}`, { path });
  }
  moveShare(shareId: string, path: string): Promise<{ path: string }> {
    return this.call("PATCH", `/api/shares/${shareId}`, { path });
  }
  listTokens(): Promise<{ user: string; tokens: Array<{ label: string; created_at: string; last_used_at: string | null; current: boolean }> }> {
    return this.call("GET", `/api/me/tokens`);
  }
  createToken(label: string): Promise<{ token: string; label: string; user: string }> {
    return this.call("POST", `/api/me/tokens`, { label });
  }
  revokeToken(label: string): Promise<void> {
    return this.call("DELETE", `/api/me/tokens/${encodeURIComponent(label)}`);
  }
  inspectTicket(ticket: string): Promise<TicketInfo> {
    return this.call("POST", `/api/tickets/inspect`, { ticket });
  }
  unmount(mountId: string): Promise<void> {
    return this.call("DELETE", `/api/mounts/${mountId}`);
  }
}

export interface GuestSession {
  session: string;
  expires_in: number;
  share_id: string;
  name: string;
  access: string;
  rw_paths: string[];
  only_file: string | null;
}

/** Exchange a ticket secret for a session id on the owner's server (guest mode). */
export async function guestSession(httpBase: string, shareId: string, secret: string): Promise<GuestSession> {
  const res = await requestUrl({
    url: `${httpBase}/guest/session/${encodeURIComponent(shareId)}`,
    method: "POST",
    headers: { Authorization: `Bearer ${secret}` },
    throw: false,
  });
  if (res.status === 403) throw new Error("the ticket is invalid or has been revoked");
  if (res.status === 429) throw new Error("too many failed attempts from this address; try again later");
  if (res.status >= 400) throw new Error(`owner's server answered HTTP ${res.status}`);
  return res.json as GuestSession;
}
