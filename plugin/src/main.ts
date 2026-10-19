import { Notice, Platform, Plugin, TAbstractFile, TFolder, setIcon } from "obsidian";
import { SamsynApi, SharingInfo, guestSession } from "./api";
import { SetupConfirmModal } from "./device-modals";
import { ConfigSync } from "./config-sync";
import { BlobClient } from "./blobs";
import { SyncEngine, EngineStatus, EngineTarget } from "./engine";
import { formatBytes } from "./format";
import { Persistence, deviceInstallId } from "./persistence";
import { MountModal, ShareModal } from "./sharing-modals";
import { DEFAULT_SETTINGS, GuestMount, SamsynSettingTab, SamsynSettings, randomSuffix, serverUrls, slugify } from "./settings";

export default class SamsynPlugin extends Plugin {
  settings!: SamsynSettings;
  /** Running engines: "home" (own server, whole vault) plus one per enabled guest mount. */
  engines = new Map<string, SyncEngine>();
  settingTab: SamsynSettingTab | null = null;
  /** Plugin-settings sync, alive while the home engine runs. */
  configSync: ConfigSync | null = null;
  private statusEl!: HTMLElement;
  private restarting = false;

  /** Shared roots in this vault, kept fresh so folder moves can be followed and warned about. */
  private roots: SharedRoot[] = [];

  /** The home engine (own server). Guest engines are in `engines` under their mount id. */
  get engine(): SyncEngine | null {
    return this.engines.get("home") ?? null;
  }

  guestEngine(mountId: string): SyncEngine | null {
    return this.engines.get(mountId) ?? null;
  }

  async onload(): Promise<void> {
    await this.loadSettings();

    this.statusEl = this.addStatusBarItem();
    this.statusEl.addClass("samsyn-status");
    this.statusEl.onClickEvent(() => {
      // @ts-expect-error setting API is not typed but exists on all platforms
      this.app.setting.open();
      // @ts-expect-error same as above
      this.app.setting.openTabById(this.manifest.id);
    });
    this.renderStatus("offline", 0);

    this.settingTab = new SamsynSettingTab(this.app, this);
    this.addSettingTab(this.settingTab);

    this.addCommand({
      id: "share-current",
      name: "Share the current note or folder…",
      checkCallback: (checking) => {
        if (!this.settings.serverUrl) return false;
        const f = this.app.workspace.getActiveFile();
        if (!f) return false;
        if (!checking) new ShareModal(this, f.path).open();
        return true;
      },
    });
    this.addCommand({
      id: "mount-ticket",
      name: "Add a shared folder from a ticket…",
      checkCallback: (checking) => {
        if (!this.settings.serverUrl) return false;
        if (!checking) new MountModal(this).open();
        return true;
      },
    });
    this.addCommand({
      id: "open-sharing",
      name: "Open sharing settings",
      callback: () => this.settingTab?.show("sharing"),
    });

    this.addCommand({
      id: "reconnect",
      name: "Reconnect to server",
      callback: () => void this.restartEngine(),
    });
    this.addCommand({
      id: "reconcile",
      name: "Reconcile all files now",
      callback: async () => {
        await this.engine?.reconcileAll();
        new Notice("Samsyn: reconcile finished");
      },
    });
    this.addCommand({
      id: "apply-deletions",
      name: "Apply held-back deletions to the shared vault (danger)",
      callback: async () => {
        const n = this.engine?.pendingDeletions.length ?? 0;
        if (!this.engine || n === 0) {
          new Notice("Samsyn: nothing held back");
          return;
        }
        // The guard re-downloaded them; remove locally again first so reconcile sees them missing.
        for (const p of this.engine.pendingDeletions) {
          const f = this.app.vault.getAbstractFileByPath(p);
          if (f) await this.app.fileManager.trashFile(f);
        }
        await this.engine.reconcileAll({ allowMassDelete: true });
        new Notice(`Samsyn: removed ${n} files from the shared vault`);
      },
    });
    this.addCommand({
      id: "status",
      name: "Show sync status",
      callback: () => {
        const diags = [...this.engines.values()].map((e) => e.getDiagnostics());
        if (diags.length === 0) {
          new Notice("Samsyn: not running");
          return;
        }
        const lines: string[] = [];
        for (const d of diags) {
          lines.push(`${d.kind === "home" ? "Vault" : d.label}: ${d.status}${d.synced ? ", synced" : ""}, ${d.peers} other(s), ${d.textFiles} text + ${d.blobFiles} other files (${formatBytes(d.textBytes + d.blobBytes)})`);
          if (d.lastError) lines.push(`  error: ${d.lastError}`);
          if (d.hint) lines.push(`  ${d.hint}`);
          if (d.pendingDeletions > 0) lines.push(`  ${d.pendingDeletions} deletions held back by the safety guard`);
        }
        new Notice(lines.join("\n"), 10_000);
      },
    });
    this.addCommand({
      id: "toggle",
      name: "Toggle sync on/off",
      callback: async () => {
        this.settings.enabled = !this.settings.enabled;
        await this.saveSettings();
        await this.restartEngine();
        new Notice(`Samsyn: sync ${this.settings.enabled ? "enabled" : "disabled"}`);
      },
    });

    this.registerEvent(
      this.app.workspace.on("file-menu", (menu, file) => {
        if (!this.settings.serverUrl) return;
        if (this.settings.guestMounts.some((m) => file.path === m.mountPath || file.path.startsWith(m.mountPath + "/"))) return;
        menu.addItem((item) =>
          item
            .setTitle("Samsyn: Share…")
            .setIcon("share-2")
            .onClick(() => new ShareModal(this, file.path).open()),
        );
      }),
    );

    // Deep links: obsidian://samsyn?op=mount&ticket=…  and  obsidian://samsyn?op=setup&server=…&token=…&vault=…
    this.registerObsidianProtocolHandler("samsyn", (params) => {
      const op = params.op ?? (params.ticket ? "mount" : params.server ? "setup" : "");
      console.info("[samsyn] deep link", op, Object.keys(params));
      if (op === "mount" && params.ticket) {
        new MountModal(this, () => this.settingTab?.show("sharing"), params.ticket).open();
      } else if (op === "setup" && params.server && params.token) {
        // `vault` never arrives here (Obsidian consumes it); the vault id is `vid`.
        new SetupConfirmModal(this, { server: params.server, token: params.token, vault: params.vid || params.vault || slugify(this.app.vault.getName()), user: params.user }).open();
      } else {
        new Notice("Samsyn: this link is not understood (expected op=mount with a ticket, or op=setup with server and token).", 8000);
      }
    });

    // Registered before the engines start, so it runs first on every rename.
    this.registerEvent(this.app.vault.on("rename", (f, oldPath) => void this.onRename(f, oldPath)));

    // Wait for the vault index so we do not get a 'create' event for every existing file.
    this.app.workspace.onLayoutReady(() => void this.startEngine());
  }

  // ------------------------------------------------------------- shared roots

  /** Re-read which folders/notes are shared or mounted (server side) plus guest mounts. */
  async refreshSharingRoots(info?: SharingInfo): Promise<void> {
    const roots: SharedRoot[] = this.settings.guestMounts.map((m) => ({ kind: "guest", id: m.id, name: m.name, path: m.mountPath, isFile: false }));
    if (this.settings.serverUrl && this.settings.token) {
      try {
        const data = info ?? (await new SamsynApi(this.settings).sharing());
        for (const s of data.shares) roots.push({ kind: "share", id: s.id, name: s.name, path: s.path, isFile: s.only_file !== null });
        for (const m of data.mounts) if (!m.status.startsWith("revoked")) roots.push({ kind: "mount", id: m.id, name: m.name, path: m.mount_path, isFile: false });
      } catch (e) {
        console.warn("[samsyn] could not load shared roots", e);
      }
    }
    this.roots = roots;
  }

  /** The innermost shared root containing `path` (shares may nest; a whole-vault share is ""). */
  private rootOf(path: string): SharedRoot | null {
    let best: SharedRoot | null = null;
    for (const r of this.roots) {
      const inside = path === r.path || (!r.isFile && (r.path === "" || path.startsWith(r.path + "/")));
      if (inside && (!best || r.path.length > best.path.length)) best = r;
    }
    return best;
  }

  /** Renames the engines must not replay: blocked moves, our reverts, explicit mount moves. */
  private claims: Array<{ old: string; next: string; until: number }> = [];
  /** A folder rename we started ourselves (an explicit Move… or a revert). */
  private ownRenames = new Set<string>();
  /** Explicit moves of a shared root (Move… dialog) that must be followed instead of blocked. */
  private expectedRootMoves = new Set<string>();

  private claim(oldPath: string, newPath: string): void {
    this.claims.push({ old: oldPath, next: newPath, until: Date.now() + 15_000 });
  }

  /** Called by engines: true when this rename (or the folder rename it is a child of) is ours. */
  private claimRename(oldPath: string, newPath: string): boolean {
    const now = Date.now();
    this.claims = this.claims.filter((c) => c.until > now);
    return this.claims.some(
      (c) =>
        (c.old === oldPath && c.next === newPath) ||
        (oldPath.startsWith(c.old + "/") && newPath.startsWith(c.next + "/") && oldPath.slice(c.old.length) === newPath.slice(c.next.length)),
    );
  }

  /** True when `oldPath` is inside a folder whose rename we are already handling. */
  private childOfHandled(oldPath: string, newPath: string): boolean {
    return this.claimRename(oldPath, newPath) && !this.claims.some((c) => c.old === oldPath && c.next === newPath);
  }

  private async revert(f: TAbstractFile, oldPath: string): Promise<boolean> {
    const key = `${f.path}\u0000${oldPath}`;
    this.ownRenames.add(key);
    this.claim(f.path, oldPath);
    try {
      await this.app.fileManager.renameFile(f, oldPath);
      return true;
    } catch (e) {
      console.error("[samsyn] moving back failed", e);
      return false;
    } finally {
      setTimeout(() => this.ownRenames.delete(key), 5000);
    }
  }

  private async onRename(f: TAbstractFile, oldPath: string): Promise<void> {
    const newPath = f.path;
    const pair = `${oldPath}\u0000${newPath}`;
    if (this.ownRenames.has(pair)) return; // our own revert or move
    if (this.childOfHandled(oldPath, newPath)) return; // Obsidian also reports every child of a moved folder
    if (this.roots.length === 0) return;

    const name = newPath.split("/").pop();
    const movedRoots = this.roots.filter((r) => r.path === oldPath || (f instanceof TFolder && r.path.startsWith(oldPath + "/")));

    // 1. A shared root (or an ancestor folder of one) was moved.
    if (movedRoots.length > 0) {
      this.claim(oldPath, newPath);
      if (this.expectedRootMoves.delete(pair)) {
        // Move… dialog: follow it on the server / in settings.
        const failures: string[] = [];
        for (const r of movedRoots) {
          try {
            await this.moveSharedRoot(r, newPath + r.path.slice(oldPath.length));
          } catch (e) {
            failures.push(`${r.name}: ${(e as Error).message}`);
          }
        }
        if (failures.length > 0) {
          new Notice(`Samsyn: could not move ${movedRoots.map((r) => `“${r.name}”`).join(", ")} (${failures.join("; ")}). Moving it back.`, 12_000);
          await this.revert(f, oldPath);
        } else {
          new Notice(`Samsyn: ${movedRoots.map((r) => `“${r.name}”`).join(", ")} moved; still shared.`, 6000);
        }
        await this.refreshSharingRoots();
        return;
      }
      const ok = await this.revert(f, oldPath);
      const names = movedRoots.map((r) => `“${r.name}”`).join(", ");
      new Notice(
        ok
          ? `Samsyn: ${names} is shared with other people and cannot be moved here. Use Settings → Samsyn → Sharing → Move… to move it.`
          : `Samsyn: ${names} is shared with other people and must not be moved; moving it back failed. Move “${newPath}” back to “${oldPath}” by hand.`,
        ok ? 10_000 : 0,
      );
      return;
    }

    // 2. Something crossed a share boundary: block it. Inside one share, moves are fine.
    const from = this.rootOf(oldPath);
    const to = this.rootOf(newPath);
    if (from === to) return;
    this.claim(oldPath, newPath);
    const ok = await this.revert(f, oldPath);
    const what = f instanceof TFolder ? "folder" : "note";
    let why: string;
    if (from && !to) why = `is in the shared ${from.kind === "share" ? "folder" : "mount"} “${from.name}”; moving it out would remove it for the people you share with`;
    else if (!from && to) why = `cannot be moved into the shared ${to.kind === "share" ? "folder" : "mount"} “${to.name}” this way, since that shares it with other people`;
    else why = `cannot be moved from “${from!.name}” to “${to!.name}” this way, since both are shared with other people`;
    new Notice(
      ok
        ? `Samsyn: the ${what} “${name}” ${why}. Moved it back. Copy it instead if you want it in both places.`
        : `Samsyn: the ${what} “${name}” ${why}; moving it back failed. Move it back by hand.`,
      ok ? 10_000 : 0,
    );
  }

  private async moveSharedRoot(r: SharedRoot, target: string): Promise<void> {
    if (r.kind === "guest") {
      const m = this.settings.guestMounts.find((x) => x.id === r.id);
      if (!m) return;
      m.mountPath = target;
      await this.saveSettings();
      // The guest engine's keys do not change, only where they live; restart it and the home
      // engine (whose exclude list changed).
      await this.restartEngine();
      return;
    }
    const api = new SamsynApi(this.settings);
    if (r.kind === "mount") await api.moveMount(r.id, target);
    else await api.moveShare(r.id, target);
  }

  /** Move a mount (server or guest) from the Move… dialog: an ordinary folder rename that the
   *  rename handler is told to follow instead of block. */
  async moveMountFolder(r: SharedRoot, target: string): Promise<void> {
    target = target.replace(/^\/+|\/+$/g, "");
    if (!target) throw new Error("choose a folder");
    if (target === r.path) return;
    const existing = this.app.vault.getAbstractFileByPath(target);
    if (existing && !(existing instanceof TFolder)) throw new Error(`${target} is a file`);
    if (existing instanceof TFolder && existing.children.length > 0) throw new Error(`${target} already contains files`);
    const folder = this.app.vault.getAbstractFileByPath(r.path);
    if (folder instanceof TFolder) {
      const parent = target.includes("/") ? target.slice(0, target.lastIndexOf("/")) : "";
      if (parent && !this.app.vault.getAbstractFileByPath(parent)) await this.app.vault.createFolder(parent);
      if (existing instanceof TFolder) await this.app.fileManager.trashFile(existing);
      this.expectedRootMoves.add(`${r.path}\u0000${target}`);
      await this.app.fileManager.renameFile(folder, target);
    } else {
      // Nothing on disk yet (empty mount): just tell the server / settings.
      await this.moveSharedRoot(r, target);
      await this.refreshSharingRoots();
    }
  }

  private persistence(scope: string): Persistence {
    return new Persistence(
      this.app.vault.adapter,
      `${this.app.vault.configDir}/plugins/${this.manifest.id}`,
      `${deviceInstallId(this.app)}|${scope}`,
    );
  }

  private guestRoots(): string[] {
    return this.settings.guestMounts.map((m) => m.mountPath.replace(/\/+$/, "") + "/");
  }

  private homeTarget(): EngineTarget | null {
    const urls = serverUrls(this.settings.serverUrl);
    if (!urls || !this.settings.vaultId) return null;
    return {
      kind: "home",
      id: "home",
      label: "your vault",
      wsBase: urls.ws,
      room: this.settings.vaultId,
      params: { token: this.settings.token },
      blobClient: BlobClient.forVault(urls.http, this.settings.vaultId, this.settings.token),
      root: "",
      readOnly: false,
      rwPaths: [],
      excludeRoots: this.guestRoots(),
    };
  }

  private guestTarget(m: GuestMount): EngineTarget | null {
    const urls = serverUrls(m.url);
    if (!urls) return null;
    return {
      kind: "guest",
      id: m.id,
      label: `“${m.name}” (shared with you)`,
      wsBase: urls.ws.replace(/\/ws$/, "/guest/ws"),
      room: m.shareId,
      // Placeholder until prepare() exchanges the secret for a session.
      params: {},
      blobClient: null,
      root: m.mountPath.replace(/\/+$/, "") + "/",
      readOnly: m.access === "ro",
      rwPaths: m.rwPaths ?? [],
      excludeRoots: [],
      prepare: async () => {
        const s = await guestSession(urls.http, m.shareId, m.secret);
        // The owner's server is authoritative for what the ticket may write.
        const rwPaths = Array.isArray(s.rw_paths) ? s.rw_paths : (m.rwPaths ?? []);
        if (JSON.stringify(rwPaths) !== JSON.stringify(m.rwPaths ?? [])) {
          m.rwPaths = rwPaths;
          void this.saveSettings();
        }
        return { params: { session: s.session }, blobClient: BlobClient.forShare(urls.http, m.shareId, s.session), rwPaths };
      },
    };
  }

  private async launch(target: EngineTarget, scope: string, created?: (e: SyncEngine) => void): Promise<void> {
    if (this.engines.has(target.id)) return;
    const engine = new SyncEngine(this.app, this.settings, this.persistence(scope), target, {
      onStatus: () => this.renderAggregateStatus(),
      claimRename: (o, n) => this.claimRename(o, n),
      isSharedRoot: (p) => this.roots.some((r) => !r.isFile && r.path === p),
    });
    this.engines.set(target.id, engine);
    created?.(engine);
    try {
      await engine.start();
    } catch (e) {
      console.error(`[samsyn] ${target.label} failed to start`, e);
      new Notice(`Samsyn: ${target.label} failed to start (${(e as Error).message})`);
      this.renderAggregateStatus();
    }
  }

  async startEngine(): Promise<void> {
    if (!this.settings.enabled) {
      this.renderStatus("disabled", 0);
      return;
    }
    const home = this.homeTarget();
    const guests = this.settings.guestMounts.filter((m) => m.enabled);
    if (!home && guests.length === 0) {
      this.renderStatus("disabled", 0);
      new Notice("Samsyn: set a server URL in settings, or add a shared folder from a ticket, to start syncing");
      return;
    }
    await this.refreshSharingRoots();
    const starts: Promise<void>[] = [];
    if (home) {
      starts.push(
        this.launch(home, this.settings.vaultId, (e) => {
          // Created right away so the Plugins tab can show the selection; file syncing starts
          // once the engine has its first server state (see below).
          this.configSync = new ConfigSync(this.app, this.settings, e);
        }).then(() => {
          if (this.engines.has("home")) this.configSync?.start();
        }),
      );
    }
    for (const m of guests) {
      const t = this.guestTarget(m);
      if (t) starts.push(this.launch(t, `guest:${m.shareId}`));
    }
    await Promise.all(starts);
  }

  async stopEngine(): Promise<void> {
    this.configSync?.stop();
    this.configSync = null;
    const all = [...this.engines.values()];
    this.engines.clear();
    await Promise.all(all.map((e) => e.stop()));
  }

  /** Start syncing a guest mount that was just added (the home engine must re-exclude it). */
  async addGuestMount(m: GuestMount): Promise<void> {
    this.settings.guestMounts.push(m);
    await this.saveSettings();
    await this.restartEngine();
  }

  /** Stop syncing a guest mount and forget it. Files stay in the vault. */
  async removeGuestMount(id: string): Promise<void> {
    const m = this.settings.guestMounts.find((x) => x.id === id);
    this.settings.guestMounts = this.settings.guestMounts.filter((x) => x.id !== id);
    await this.saveSettings();
    const e = this.engines.get(id);
    if (e) {
      this.engines.delete(id);
      await e.stop();
    }
    if (m) await this.persistence(`guest:${m.shareId}`).clear();
    await this.restartEngine();
  }

  private renderAggregateStatus(): void {
    const diags = [...this.engines.values()].map((e) => e.getDiagnostics());
    if (diags.length === 0) {
      this.renderStatus("disabled", 0);
      return;
    }
    const rank: Record<EngineStatus, number> = { disabled: 0, connected: 1, connecting: 2, offline: 3 };
    let worst: EngineStatus = "disabled";
    let peers = 0;
    for (const d of diags) {
      if (rank[d.status] > rank[worst]) worst = d.status;
      peers += d.peers;
    }
    this.renderStatus(worst, peers);
  }

  async restartEngine(): Promise<void> {
    if (this.restarting) return;
    this.restarting = true;
    try {
      await this.stopEngine();
      await this.startEngine();
    } finally {
      this.restarting = false;
    }
  }

  async resetLocalState(): Promise<void> {
    await this.stopEngine();
    await this.persistence(this.settings.vaultId).clear();
    for (const m of this.settings.guestMounts) await this.persistence(`guest:${m.shareId}`).clear();
    await this.startEngine();
  }

  // ------------------------------------------------------------- settings

  async loadSettings(): Promise<void> {
    this.settings = Object.assign({}, DEFAULT_SETTINGS, (await this.loadData()) as Partial<SamsynSettings>);
    let dirty = false;
    if (!this.settings.vaultId) {
      this.settings.vaultId = slugify(this.app.vault.getName());
      dirty = true;
    }
    if (!this.settings.deviceName) {
      this.settings.deviceName = `${Platform.isMobile ? "Mobile" : "Desktop"}-${randomSuffix()}`;
      dirty = true;
    }
    for (const m of this.settings.guestMounts) {
      if (!Array.isArray(m.rwPaths)) {
        m.rwPaths = [];
        dirty = true;
      }
    }
    if (dirty) await this.saveSettings();
  }

  async saveSettings(): Promise<void> {
    await this.saveData(this.settings);
  }

  // ------------------------------------------------------------- status bar

  private renderStatus(status: EngineStatus, peers: number): void {
    const el = this.statusEl;
    el.empty();
    const icon = el.createSpan({ cls: "samsyn-status-icon" });
    const label = el.createSpan({ cls: "samsyn-status-label" });
    el.removeClass("is-connected", "is-offline", "is-connecting", "is-disabled");
    switch (status) {
      case "connected":
        setIcon(icon, "refresh-cw");
        label.setText(peers > 0 ? `Samsyn · ${peers} other${peers === 1 ? "" : "s"}` : "Samsyn");
        el.addClass("is-connected");
        el.setAttr("aria-label", "Samsyn: connected and live");
        break;
      case "connecting":
        setIcon(icon, "loader");
        label.setText("Samsyn…");
        el.addClass("is-connecting");
        el.setAttr("aria-label", "Samsyn: connecting");
        break;
      case "offline":
        setIcon(icon, "cloud-off");
        label.setText("Samsyn offline");
        el.addClass("is-offline");
        el.setAttr("aria-label", "Samsyn: offline, changes are queued locally");
        break;
      case "disabled":
        setIcon(icon, "pause");
        label.setText("Samsyn off");
        el.addClass("is-disabled");
        el.setAttr("aria-label", "Samsyn: sync disabled or not configured");
        break;
    }
  }
}

export interface SharedRoot {
  kind: "share" | "mount" | "guest";
  id: string;
  name: string;
  /** Folder without trailing slash, or the note path for single-note shares. */
  path: string;
  isFile: boolean;
}
