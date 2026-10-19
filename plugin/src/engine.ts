import { App, Editor, MarkdownView, Notice, Platform, TAbstractFile, TFile, TFolder, normalizePath } from "obsidian";
import * as Y from "yjs";
import { WebsocketProvider } from "y-websocket";
import { BlobClient } from "./blobs";
import { diffText } from "./diff";
import { utf8Length } from "./format";
import { hashText, sha256Hex } from "./hash";
import { isSafeKey } from "./keys";
import { LocalState, Persistence, StorageInfo, deviceInstallId } from "./persistence";
import { SamsynSettings } from "./settings";

export interface BlobMeta {
  hash: string;
  size: number;
  mtime: number;
}

export type EngineStatus = "disabled" | "offline" | "connecting" | "connected";

export interface EngineEvents {
  onStatus(status: EngineStatus, peers: number): void;
  /**
   * Called first on every rename. Return true when the plugin handled it (a shared root was
   * moved and the server moved the keys), so the engine must not replay it as delete + create.
   */
  claimRename?(oldPath: string, newPath: string): boolean;
  /** A folder that is a share or mount root: never pruned when a remote delete empties it. */
  isSharedRoot?(folderPath: string): boolean;
}

/**
 * What one engine syncs against. The home engine syncs the whole vault with the user's own
 * server; a guest engine syncs one folder with a share on someone else's server, authenticated
 * by the ticket secret. Inside the engine every path is a *document key*: for the home engine
 * that is the vault path, for a guest engine the path relative to `root`.
 */
export interface EngineTarget {
  kind: "home" | "guest";
  /** "home", or the guest mount id. */
  id: string;
  label: string;
  /** ws(s)://host/ws or ws(s)://host/guest/ws */
  wsBase: string;
  /** Vault id or share id. */
  room: string;
  /** Query params for the WebSocket: {token} or {secret}. */
  params: Record<string, string>;
  blobClient: BlobClient | null;
  /** "" for the home engine; "Shared/X/" (with trailing slash) for a guest mount. */
  root: string;
  readOnly: boolean;
  /** Keys (relative to `root`, folders with trailing slash) that stay writable when `readOnly`. */
  rwPaths: string[];
  /** Vault paths (with trailing slash) this engine must never touch: the guest mount roots. */
  excludeRoots: string[];
  /**
   * Guests: exchange the ticket secret for a session before connecting (and again when the
   * connection keeps failing, e.g. after the session expired). Returns the WebSocket params,
   * the blob client to use and, if the owner's server says so, the current writable paths.
   */
  prepare?: () => Promise<{ params: Record<string, string>; blobClient: BlobClient; rwPaths?: string[] }>;
}

export interface Diagnostics {
  kind: "home" | "guest";
  label: string;
  root: string;
  readOnly: boolean;
  status: EngineStatus;
  wsUrl: string;
  synced: boolean;
  peers: number;
  textFiles: number;
  blobFiles: number;
  /** UTF-8 size of all synced text files, in bytes. */
  textBytes: number;
  /** Size of all synced attachments, in bytes (from their metadata). */
  blobBytes: number;
  lastError: string | null;
  lastConnectedAt: number | null;
  hint: string | null;
  installId: string;
  pendingDeletions: number;
  storage: StorageInfo | null;
}

/** Refuse to delete from the shared vault when one reconcile wants to remove this many files… */
const MASS_DELETE_MIN = 5;
/** …and they make up more than this share of the shared vault. */
const MASS_DELETE_RATIO = 0.5;

/**
 * Document layout (one Y.Doc per engine):
 *   texts: Y.Map<key, Y.Text>   character-level merged files (.md, .txt, ...)
 *   blobs: Y.Map<key, BlobMeta> last-writer-wins pointers to content-addressed blobs
 */
export class SyncEngine {
  readonly doc = new Y.Doc();
  readonly texts: Y.Map<Y.Text>;
  readonly blobs: Y.Map<BlobMeta>;
  /** Transactions we originate carry this origin so observers can ignore them. */
  readonly localOrigin = { samsyn: "local" };

  private provider: WebsocketProvider | null = null;
  private blobClient: BlobClient | null = null;
  private state: LocalState = { synced: {} };
  private editorTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private textExts: Set<string>;
  private excludes: string[];
  private stopped = false;
  private unsubscribers: Array<() => void> = [];
  private status: EngineStatus = "offline";
  private wsUrl = "";
  private lastError: string | null = null;
  private storage: StorageInfo | null = null;
  private lastConnectedAt: number | null = null;
  private hint: string | null = null;
  private installId = "";
  private warnedReadOnly = false;
  private closesSincePrepare = 0;
  private lastPrepare = 0;
  /** Deletions from the shared vault that the mass-delete guard held back. */
  pendingDeletions: string[] = [];

  constructor(
    private app: App,
    private settings: SamsynSettings,
    private persistence: Persistence,
    readonly target: EngineTarget,
    private events: EngineEvents,
  ) {
    this.texts = this.doc.getMap<Y.Text>("texts");
    this.blobs = this.doc.getMap<BlobMeta>("blobs");
    this.textExts = new Set(
      settings.textExtensions
        .split(",")
        .map((s) => s.trim().toLowerCase().replace(/^\./, ""))
        .filter(Boolean),
    );
    // Exclude paths are a home-vault setting; a guest folder is already a narrow scope.
    this.excludes =
      target.kind === "home"
        ? settings.excludePaths
            .split(",")
            .map((s) => s.trim())
            .filter(Boolean)
        : [];
    this.blobClient = target.blobClient;
  }

  // ---------------------------------------------------------------- lifecycle

  async start(): Promise<void> {
    this.installId = deviceInstallId(this.app);
    this.state = await this.persistence.loadState(this.installId);
    const hadDoc = await this.persistence.loadDoc(this.doc);

    // Observers go in before we connect so nothing that arrives during the initial sync or
    // during reconcile is missed. All handlers are idempotent, so overlap with reconcile is safe.
    this.installObservers();

    if (this.target.wsBase && this.target.room) {
      await this.prepare();
      this.connect();
      if (!hadDoc) {
        // First run on this device: wait for the server state before reconciling so that
        // files which already exist on both sides are merged, not duplicated.
        await this.waitForSync(10_000);
      }
    } else {
      this.setStatus("disabled");
    }

    await this.reconcileAll();
    this.installVaultListeners();
  }

  async stop(): Promise<void> {
    this.stopped = true;
    for (const t of this.editorTimers.values()) clearTimeout(t);
    this.editorTimers.clear();
    for (const u of this.unsubscribers) u();
    this.unsubscribers = [];
    this.provider?.destroy();
    this.provider = null;
    await this.persistence.close();
    this.doc.destroy();
  }

  get isConnected(): boolean {
    return this.status === "connected";
  }

  /** Run the target's prepare step (guest session exchange). Errors are shown, not fatal. */
  private async prepare(): Promise<void> {
    if (!this.target.prepare) return;
    this.lastPrepare = Date.now();
    this.closesSincePrepare = 0;
    try {
      const r = await this.target.prepare();
      this.target.params = r.params;
      this.blobClient = r.blobClient;
      if (r.rwPaths) this.target.rwPaths = r.rwPaths;
      this.lastError = null;
    } catch (e) {
      this.lastError = `${this.target.label}: ${(e as Error).message}`;
      console.warn("[samsyn]", this.lastError);
    }
  }

  private async reprepareAndReconnect(): Promise<void> {
    if (this.stopped || !this.target.prepare) return;
    this.provider?.destroy();
    this.provider = null;
    await this.prepare();
    if (!this.stopped) this.connect();
  }

  private connect(): void {
    const { wsBase, room, params } = this.target;
    this.setStatus("connecting");
    this.wsUrl = `${wsBase}/${room}`;
    if (wsBase.startsWith("ws://") && Platform.isMobileApp) {
      this.hint =
        "Plain ws:// is blocked on mobile (insecure WebSocket from a secure app). Use an https:// server URL with a valid certificate.";
    } else if (typeof WebSocket === "undefined") {
      this.hint = "WebSocket is not available in this environment.";
    } else {
      this.hint = null;
    }
    let provider: WebsocketProvider;
    try {
      provider = new WebsocketProvider(wsBase, room, this.doc, {
        params,
        disableBc: true,
        maxBackoffTime: 10_000,
        resyncInterval: 60_000,
      });
    } catch (e) {
      this.lastError = `WebSocket could not be created: ${(e as Error).message}`;
      console.error("[samsyn]", this.lastError);
      this.setStatus("offline");
      return;
    }
    provider.awareness.setLocalStateField("user", { name: this.settings.deviceName, installId: this.installId });
    provider.on("status", ({ status }: { status: string }) => {
      if (status === "connected") {
        this.lastError = null;
        this.lastConnectedAt = Date.now();
        this.setStatus("connected");
      } else if (status === "connecting") this.setStatus("connecting");
      else this.setStatus("offline");
    });
    provider.on("connection-error", (ev: Event) => {
      this.lastError = `WebSocket error (${ev?.type ?? "error"}) connecting to ${this.wsUrl}`;
      console.warn("[samsyn]", this.lastError, ev);
      this.setStatus("offline");
    });
    provider.on("connection-close", (ev: CloseEvent | null) => {
      if (ev && ev.code !== 1000 && ev.code !== 1005) {
        this.lastError = `WebSocket closed: code ${ev.code}${ev.reason ? " " + ev.reason : ""}`;
        if (ev.code === 1006) this.lastError += " (server unreachable, TLS/certificate problem, or blocked insecure ws://)";
      }
      // A guest session can expire or be revoked; after a few failed attempts get a new one
      // (at most every two minutes) instead of retrying the dead URL forever.
      if (this.target.prepare && ++this.closesSincePrepare >= 4 && Date.now() - this.lastPrepare > 120_000) {
        void this.reprepareAndReconnect();
      }
    });
    provider.awareness.on("change", () => this.setStatus(this.status));
    this.provider = provider;
  }

  getDiagnostics(): Diagnostics {
    let textBytes = 0;
    for (const t of this.texts.values()) textBytes += utf8Length(t.toString());
    let blobBytes = 0;
    for (const b of this.blobs.values()) blobBytes += b.size || 0;
    return {
      kind: this.target.kind,
      label: this.target.label,
      root: this.target.root,
      readOnly: this.target.readOnly,
      status: this.status,
      wsUrl: this.wsUrl,
      synced: this.provider?.synced ?? false,
      peers: this.provider ? Math.max(0, this.provider.awareness.getStates().size - 1) : 0,
      textFiles: this.texts.size,
      blobFiles: this.blobs.size,
      textBytes,
      blobBytes,
      lastError: this.lastError,
      lastConnectedAt: this.lastConnectedAt,
      hint: this.hint,
      installId: this.installId,
      pendingDeletions: this.pendingDeletions.length,
      storage: this.storage,
    };
  }

  getBlobClient(): BlobClient | null {
    return this.blobClient;
  }

  get deviceInstallId(): string {
    return this.installId;
  }

  /** Install ids of the other devices currently connected (from awareness). */
  peerInstallIds(): string[] {
    const out: string[] = [];
    if (!this.provider) return out;
    for (const [id, st] of this.provider.awareness.getStates()) {
      if (id === this.doc.clientID) continue;
      const iid = (st as { user?: { installId?: string } })?.user?.installId;
      if (iid && !out.includes(iid)) out.push(iid);
    }
    return out;
  }

  /** Names of the other devices currently connected (from awareness). */
  peerNames(): string[] {
    const out: string[] = [];
    if (!this.provider) return out;
    for (const [id, st] of this.provider.awareness.getStates()) {
      if (id === this.doc.clientID) continue;
      const name = (st as { user?: { name?: string } })?.user?.name;
      if (name && !out.includes(name)) out.push(name);
    }
    return out;
  }

  /** Refresh the cache-size figures shown in the status section (cheap; doc size is cached). */
  async refreshStorage(): Promise<void> {
    this.storage = await this.persistence.storageInfo(this.doc, Object.keys(this.state.synced).length);
  }

  private waitForSync(timeoutMs: number): Promise<void> {
    const p = this.provider;
    if (!p) return Promise.resolve();
    return new Promise((resolve) => {
      if (p.synced) return resolve();
      const timer = setTimeout(() => {
        p.off("sync", onSync);
        resolve();
      }, timeoutMs);
      const onSync = (synced: boolean) => {
        if (synced) {
          clearTimeout(timer);
          p.off("sync", onSync);
          resolve();
        }
      };
      p.on("sync", onSync);
    });
  }

  private setStatus(s: EngineStatus): void {
    this.status = s;
    const peers = this.provider ? Math.max(0, this.provider.awareness.getStates().size - 1) : 0;
    this.events.onStatus(s, peers);
  }

  /** Tell other clients which file we are in (doc key, or null when outside this scope). */
  setActiveFile(vaultPath: string | null): void {
    const key = vaultPath ? this.keyOf(vaultPath) : null;
    this.provider?.awareness.setLocalStateField("file", key);
  }

  // ---------------------------------------------------------------- scope: vault path <-> doc key

  /** Doc key for a vault path, or null when this engine does not handle it. */
  private keyOf(vaultPath: string): string | null {
    if (vaultPath.startsWith(this.app.vault.configDir + "/") || vaultPath.startsWith(".")) return null;
    const { root, excludeRoots } = this.target;
    if (root) {
      if (!vaultPath.startsWith(root)) return null;
      const key = vaultPath.slice(root.length);
      return key || null;
    }
    for (const r of excludeRoots) if (vaultPath.startsWith(r)) return null;
    if (this.excludes.some((e) => vaultPath === e || vaultPath.startsWith(e.endsWith("/") ? e : e + "/") || vaultPath.startsWith(e))) {
      return null;
    }
    return vaultPath;
  }

  /** Doc-key prefix covered by a vault folder, or null if the folder is outside this scope. */
  private keyPrefixOf(folderPath: string): string | null {
    const p = folderPath + "/";
    const { root } = this.target;
    if (root) {
      if (p.startsWith(root)) return p.slice(root.length);
      if (root.startsWith(p)) return ""; // an ancestor of the mount: everything
      return null;
    }
    return this.keyOf(p) === null ? null : p;
  }

  private pathOf(key: string): string {
    return this.target.root + key;
  }

  private fileOf(key: string): TAbstractFile | null {
    return this.app.vault.getAbstractFileByPath(this.pathOf(key));
  }

  // ---------------------------------------------------------------- helpers

  private isText(key: string): boolean {
    const dot = key.lastIndexOf(".");
    if (dot < 0) return false;
    return this.textExts.has(key.slice(dot + 1).toLowerCase());
  }

  private shouldSync(key: string): boolean {
    // Keys come from other people's documents (shares): never map anything that could leave
    // the vault folder (absolute, "..", empty segments, control characters) to a file.
    if (!isSafeKey(key)) return false;
    if (key.split("/").some((seg) => seg.startsWith("."))) return false;
    if (this.isText(key)) return true;
    return this.settings.syncBinary;
  }

  /** Is this key writable for us? Read-only targets may still allow a few paths. */
  private canWrite(key: string): boolean {
    if (!this.target.readOnly) return true;
    return this.target.rwPaths.some((p) => (p.endsWith("/") ? key.startsWith(p) : key === p));
  }

  /** Read-only guests never write to the document (outside their writable paths); say so once. */
  private blockedReadOnly(key: string): boolean {
    if (this.canWrite(key)) return false;
    if (!this.warnedReadOnly) {
      this.warnedReadOnly = true;
      const except = this.target.rwPaths.length ? ` except ${this.target.rwPaths.map((p) => this.target.root + p).join(", ")}` : "";
      new Notice(`Samsyn: ${this.target.label} is read-only${except}. Your other edits under ${this.target.root} are not sent and will be overwritten by the shared version.`, 10_000);
    }
    console.info(`[samsyn] read-only ${this.target.label}: not pushing ${key}`);
    return true;
  }

  private markSynced(key: string, hash: string): void {
    this.state.synced[key] = hash;
    this.persistence.noteSynced(key, hash, this.state);
  }

  private forgetSynced(key: string): void {
    if (key in this.state.synced) {
      delete this.state.synced[key];
      this.persistence.noteForgotten(key, this.state);
    }
  }

  private async ensureFolder(vaultPath: string): Promise<void> {
    const idx = vaultPath.lastIndexOf("/");
    if (idx <= 0) return;
    const folder = vaultPath.slice(0, idx);
    if (this.app.vault.getAbstractFileByPath(folder)) return;
    await this.ensureFolder(folder);
    try {
      await this.app.vault.createFolder(folder);
    } catch {
      /* raced with another create */
    }
  }

  private conflictPath(key: string): string {
    const dot = key.lastIndexOf(".");
    const slash = key.lastIndexOf("/");
    const base = dot > slash ? key.slice(0, dot) : key;
    const ext = dot > slash ? key.slice(dot) : "";
    const d = new Date();
    const stamp = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}${pad(d.getMinutes())}`;
    return `${base} (conflict ${this.settings.deviceName} ${stamp})${ext}`;
  }

  // ---------------------------------------------------------------- disk writes

  private async writeTextToDisk(key: string, content: string): Promise<void> {
    this.markSynced(key, hashText(content));
    const path = this.pathOf(key);
    const existing = this.app.vault.getAbstractFileByPath(path);
    if (existing instanceof TFile) {
      if ((await this.app.vault.read(existing)) !== content) await this.app.vault.modify(existing, content);
      return;
    }
    await this.ensureFolder(path);
    try {
      await this.app.vault.create(path, content);
    } catch (e) {
      // Raced with another writer (observer vs reconcile); fall back to modify.
      const f = this.app.vault.getAbstractFileByPath(path);
      if (f instanceof TFile) await this.app.vault.modify(f, content);
      else throw e;
    }
  }

  private async writeBinaryToDisk(key: string, data: ArrayBuffer, hash: string): Promise<void> {
    this.markSynced(key, hash);
    const path = this.pathOf(key);
    const existing = this.app.vault.getAbstractFileByPath(path);
    if (existing instanceof TFile) {
      await this.app.vault.modifyBinary(existing, data);
      return;
    }
    await this.ensureFolder(path);
    try {
      await this.app.vault.createBinary(path, data);
    } catch (e) {
      const f = this.app.vault.getAbstractFileByPath(path);
      if (f instanceof TFile) await this.app.vault.modifyBinary(f, data);
      else throw e;
    }
  }

  private async deleteFromDisk(key: string): Promise<void> {
    this.forgetSynced(key);
    const f = this.fileOf(key);
    if (f instanceof TFile) {
      const parent = f.parent?.path ?? "";
      await this.app.fileManager.trashFile(f);
      await this.pruneEmptyFolders(parent);
    }
  }

  /**
   * A remote delete emptied a folder (typically a folder renamed on another device, which
   * arrives here as delete + create): remove it and any parent it leaves empty, so the old
   * name does not linger as an empty shell. Stops at the engine root and at shared roots.
   */
  private async pruneEmptyFolders(folderPath: string): Promise<void> {
    const root = this.target.root.replace(/\/$/, "");
    let p = folderPath;
    while (p && p !== "/" && p !== root) {
      if (this.events.isSharedRoot?.(p)) return;
      const folder = this.app.vault.getAbstractFileByPath(p);
      if (!(folder instanceof TFolder) || folder.children.length > 0) return;
      try {
        await this.app.vault.delete(folder);
      } catch {
        return; // something arrived meanwhile; leave it
      }
      const i = p.lastIndexOf("/");
      if (i < 0) return;
      p = p.slice(0, i);
    }
  }

  private async writeConflictCopy(key: string, content: string | ArrayBuffer): Promise<void> {
    const cp = this.pathOf(this.conflictPath(key));
    await this.ensureFolder(cp);
    if (typeof content === "string") await this.app.vault.create(cp, content);
    else await this.app.vault.createBinary(cp, content);
    new Notice(`Samsyn: conflict on ${this.pathOf(key)}, kept your version as ${cp.split("/").pop()}`);
  }

  /** Editors currently showing a file, so remote changes can be applied without losing the cursor. */
  private editorsFor(key: string): Editor[] {
    const path = this.pathOf(key);
    const out: Editor[] = [];
    for (const leaf of this.app.workspace.getLeavesOfType("markdown")) {
      const view = leaf.view;
      if (view instanceof MarkdownView && view.file?.path === path && view.editor) out.push(view.editor);
    }
    return out;
  }

  // ---------------------------------------------------------------- local -> doc

  /** Push the current local content of a text file into the shared doc. */
  pushLocalText(key: string, content: string): void {
    if (this.stopped || !this.shouldSync(key) || !this.isText(key)) return;
    if (this.blockedReadOnly(key)) return;
    const existing = this.texts.get(key);
    this.doc.transact(() => {
      if (!existing) {
        this.texts.set(key, new Y.Text(content));
        return;
      }
      const d = diffText(existing.toString(), content);
      if (!d) return;
      if (d.remove > 0) existing.delete(d.index, d.remove);
      if (d.insert) existing.insert(d.index, d.insert);
    }, this.localOrigin);
    this.markSynced(key, hashText(content));
  }

  private async pushLocalBlob(key: string, file: TFile): Promise<void> {
    if (this.stopped || !this.shouldSync(key) || this.isText(key)) return;
    if (this.blockedReadOnly(key)) return;
    const data = await this.app.vault.readBinary(file);
    const hash = await sha256Hex(data);
    const current = this.blobs.get(key);
    if (current?.hash === hash) {
      this.markSynced(key, hash);
      return;
    }
    if (this.blobClient) {
      try {
        await this.blobClient.upload(hash, data);
      } catch (e) {
        console.warn(`[samsyn] upload of ${this.pathOf(key)} failed, will retry on next change`, e);
        return;
      }
    }
    this.doc.transact(() => {
      this.blobs.set(key, { hash, size: data.byteLength, mtime: file.stat.mtime });
    }, this.localOrigin);
    this.markSynced(key, hash);
  }

  private removeFromDoc(key: string): void {
    if (this.blockedReadOnly(key)) return;
    this.doc.transact(() => {
      if (this.texts.has(key)) this.texts.delete(key);
      if (this.blobs.has(key)) this.blobs.delete(key);
    }, this.localOrigin);
    this.forgetSynced(key);
  }

  private moveInDoc(oldKey: string, newKey: string): void {
    if (this.blockedReadOnly(oldKey)) return;
    const t = this.texts.get(oldKey);
    const b = this.blobs.get(oldKey);
    this.doc.transact(() => {
      if (t) {
        this.texts.set(newKey, new Y.Text(t.toString()));
        this.texts.delete(oldKey);
      }
      if (b) {
        this.blobs.set(newKey, { ...b });
        this.blobs.delete(oldKey);
      }
    }, this.localOrigin);
    const h = this.state.synced[oldKey];
    this.forgetSynced(oldKey);
    if (h) this.markSynced(newKey, h);
  }

  private docKeysWithPrefix(prefix: string): string[] {
    const out: string[] = [];
    for (const k of this.texts.keys()) if (k.startsWith(prefix)) out.push(k);
    for (const k of this.blobs.keys()) if (k.startsWith(prefix)) out.push(k);
    return out;
  }

  // ---------------------------------------------------------------- doc -> local

  private async applyRemoteTextDelta(key: string, ytext: Y.Text, delta: Y.YTextEvent["delta"]): Promise<void> {
    const editors = this.editorsFor(key);
    const target = ytext.toString();
    if (editors.length === 0) {
      await this.writeTextToDisk(key, target);
      return;
    }
    const editor = editors[0];
    let offset = 0;
    for (const op of delta) {
      if (op.retain !== undefined) {
        offset += op.retain;
      } else if (op.delete !== undefined) {
        editor.replaceRange("", editor.offsetToPos(offset), editor.offsetToPos(offset + op.delete));
      } else if (op.insert !== undefined && typeof op.insert === "string") {
        editor.replaceRange(op.insert, editor.offsetToPos(offset));
        offset += op.insert.length;
      }
    }
    if (editor.getValue() !== target) {
      // Delta did not line up with the editor (edits raced); fall back to a full replace.
      const cursor = editor.getCursor();
      editor.setValue(target);
      editor.setCursor(cursor);
    }
    this.markSynced(key, hashText(target));
  }

  private async applyRemoteBlob(key: string, meta: BlobMeta): Promise<void> {
    if (!this.shouldSync(key)) return;
    if (this.state.synced[key] === meta.hash) return;
    const existing = this.fileOf(key);
    if (existing instanceof TFile) {
      const localHash = await sha256Hex(await this.app.vault.readBinary(existing));
      if (localHash === meta.hash) {
        this.markSynced(key, meta.hash);
        return;
      }
      const lastSynced = this.state.synced[key];
      if (lastSynced && lastSynced !== localHash) {
        await this.writeConflictCopy(key, await this.app.vault.readBinary(existing));
      }
    }
    if (!this.blobClient) return;
    // The metadata can arrive before the bytes (slow uploader, or a server-side bridge still
    // copying), so a 404 is retried with backoff until the blob shows up or it is superseded.
    const delays = [2000, 5000, 15000, 30000];
    for (let attempt = 0; ; attempt++) {
      const client = this.blobClient;
      if (!client) return;
      try {
        const data = await client.download(meta.hash);
        if (data) {
          await this.writeBinaryToDisk(key, data, meta.hash);
          return;
        }
      } catch (e) {
        console.warn(`[samsyn] download of ${this.pathOf(key)} failed`, e);
        return;
      }
      if (attempt >= delays.length) {
        console.warn(`[samsyn] blob for ${this.pathOf(key)} never appeared on server, giving up`);
        return;
      }
      console.warn(`[samsyn] blob for ${this.pathOf(key)} not on server yet, retrying in ${delays[attempt] / 1000}s`);
      await new Promise((r) => setTimeout(r, delays[attempt]));
      if (this.blobs.get(key)?.hash !== meta.hash) return; // superseded or removed meanwhile
    }
  }

  private installObservers(): void {
    const onTexts = (events: Y.YEvent<any>[], txn: Y.Transaction) => {
      if (txn.origin === this.localOrigin) return;
      for (const ev of events) {
        if (ev.target === this.texts) {
          const mapEv = ev as Y.YMapEvent<Y.Text>;
          for (const [key, change] of mapEv.changes.keys) {
            if (!this.shouldSync(key)) continue;
            if (change.action === "delete") {
              void this.deleteFromDisk(key);
            } else {
              const ytext = this.texts.get(key);
              if (ytext) void this.applyRemoteTextReplace(key, ytext.toString(), change.action === "update");
            }
          }
        } else if (ev.target instanceof Y.Text && ev.path.length === 1) {
          const key = String(ev.path[0]);
          if (!this.shouldSync(key)) continue;
          void this.applyRemoteTextDelta(key, ev.target, (ev as Y.YTextEvent).delta);
        }
      }
    };
    this.texts.observeDeep(onTexts);
    this.unsubscribers.push(() => this.texts.unobserveDeep(onTexts));

    const onBlobs = (ev: Y.YMapEvent<BlobMeta>, txn: Y.Transaction) => {
      if (txn.origin === this.localOrigin) return;
      for (const [key, change] of ev.changes.keys) {
        if (!this.shouldSync(key)) continue;
        if (change.action === "delete") void this.deleteFromDisk(key);
        else {
          const meta = this.blobs.get(key);
          if (meta) void this.applyRemoteBlob(key, meta);
        }
      }
    };
    this.blobs.observe(onBlobs);
    this.unsubscribers.push(() => this.blobs.unobserve(onBlobs));
  }

  /** A whole Y.Text was added or replaced remotely (new file, rename, or concurrent create). */
  private async applyRemoteTextReplace(key: string, content: string, replaced: boolean): Promise<void> {
    const existing = this.fileOf(key);
    if (existing instanceof TFile) {
      const local = await this.app.vault.read(existing);
      if (local === content) {
        this.markSynced(key, hashText(content));
        return;
      }
      const lastSynced = this.state.synced[key];
      if (!replaced && lastSynced === undefined && local !== content) {
        // Concurrent create on both sides with different content: keep ours as a conflict copy.
        await this.writeConflictCopy(key, local);
      }
      const editors = this.editorsFor(key);
      if (editors.length > 0) {
        const cursor = editors[0].getCursor();
        editors[0].setValue(content);
        editors[0].setCursor(cursor);
        this.markSynced(key, hashText(content));
        return;
      }
    }
    await this.writeTextToDisk(key, content);
  }

  // ---------------------------------------------------------------- vault listeners

  private installVaultListeners(): void {
    const v = this.app.vault;
    const w = this.app.workspace;

    const refs = [
      v.on("create", (f) => void this.onLocalChanged(f)),
      v.on("modify", (f) => void this.onLocalChanged(f)),
      v.on("delete", (f) => this.onLocalDeleted(f)),
      v.on("rename", (f, oldPath) => this.onLocalRenamed(f, oldPath)),
      w.on("editor-change", (editor, info) => {
        const file = info.file;
        if (!file) return;
        const key = this.keyOf(file.path);
        if (!key || !this.isText(key) || !this.shouldSync(key)) return;
        this.scheduleEditorPush(key, editor);
      }),
      w.on("active-leaf-change", () => {
        const f = w.getActiveFile();
        this.setActiveFile(f ? f.path : null);
      }),
    ];
    for (const r of refs) this.unsubscribers.push(() => this.app.vault.offref(r));
  }

  private scheduleEditorPush(key: string, editor: Editor): void {
    const prev = this.editorTimers.get(key);
    if (prev) clearTimeout(prev);
    this.editorTimers.set(
      key,
      setTimeout(() => {
        this.editorTimers.delete(key);
        this.pushLocalText(key, editor.getValue());
      }, this.settings.liveDebounceMs),
    );
  }

  private async onLocalChanged(f: TAbstractFile): Promise<void> {
    if (!(f instanceof TFile) || this.stopped) return;
    const key = this.keyOf(f.path);
    if (!key || !this.shouldSync(key)) return;
    if (this.isText(key)) {
      // If an editor is open the editor-change path already handles it; still verify disk content.
      const content = await this.app.vault.read(f);
      this.pushLocalText(key, content);
    } else {
      await this.pushLocalBlob(key, f);
    }
  }

  private onLocalDeleted(f: TAbstractFile): void {
    if (this.stopped) return;
    if (f instanceof TFolder) {
      const prefix = this.keyPrefixOf(f.path);
      if (prefix === null) return;
      for (const k of this.docKeysWithPrefix(prefix)) this.removeFromDoc(k);
      return;
    }
    const key = this.keyOf(f.path);
    if (key && this.shouldSync(key)) this.removeFromDoc(key);
  }

  private onLocalRenamed(f: TAbstractFile, oldPath: string): void {
    if (this.stopped) return;
    if (this.events.claimRename?.(oldPath, f.path)) return;
    if (f instanceof TFolder) {
      const oldPrefix = this.keyPrefixOf(oldPath);
      const newPrefix = this.keyPrefixOf(f.path);
      if (oldPrefix !== null && newPrefix !== null) {
        for (const k of this.docKeysWithPrefix(oldPrefix)) this.moveInDoc(k, newPrefix + k.slice(oldPrefix.length));
      } else if (oldPrefix !== null) {
        for (const k of this.docKeysWithPrefix(oldPrefix)) this.removeFromDoc(k);
      } else if (newPrefix !== null) {
        for (const child of this.app.vault.getFiles()) {
          if (child.path.startsWith(f.path + "/")) void this.onLocalChanged(child);
        }
      }
      return;
    }
    const oldKey = this.keyOf(oldPath);
    const newKey = this.keyOf(f.path);
    const oldOk = oldKey !== null && this.shouldSync(oldKey);
    const newOk = newKey !== null && this.shouldSync(newKey);
    if (oldOk && newOk) this.moveInDoc(oldKey!, newKey!);
    else if (oldOk) this.removeFromDoc(oldKey!);
    else if (newOk) void this.onLocalChanged(f);
  }

  // ---------------------------------------------------------------- reconcile

  /**
   * Three-way compare of every file: local disk, shared doc, and the hash we recorded the
   * last time the two agreed. Runs at startup and on demand. Idempotent.
   */
  async reconcileAll(opts: { allowMassDelete?: boolean } = {}): Promise<void> {
    const localFiles = new Map<string, TFile>();
    for (const f of this.app.vault.getFiles()) {
      const key = this.keyOf(f.path);
      if (key && this.shouldSync(key)) localFiles.set(key, f);
    }

    // Local files first.
    for (const [key, file] of localFiles) {
      try {
        if (this.isText(key)) await this.reconcileText(key, file);
        else await this.reconcileBlob(key, file);
      } catch (e) {
        console.error(`[samsyn] reconcile failed for ${this.pathOf(key)}`, e);
      }
    }

    // Then entries in the doc that are missing locally.
    const toDelete: string[] = [];
    for (const key of [...this.texts.keys()]) {
      if (localFiles.has(key) || !this.shouldSync(key)) continue;
      if (this.state.synced[key] && this.canWrite(key)) {
        // We had it and it is gone locally: deleted while offline.
        toDelete.push(key);
      } else {
        const t = this.texts.get(key);
        if (t) await this.writeTextToDisk(key, t.toString());
      }
    }
    for (const key of [...this.blobs.keys()]) {
      if (localFiles.has(key) || !this.shouldSync(key)) continue;
      if (this.state.synced[key] && this.canWrite(key)) {
        toDelete.push(key);
      } else {
        const m = this.blobs.get(key);
        if (m) await this.applyRemoteBlob(key, m);
      }
    }

    const total = this.texts.size + this.blobs.size;
    const massDelete = toDelete.length >= MASS_DELETE_MIN && toDelete.length > total * MASS_DELETE_RATIO;
    if (massDelete && !opts.allowMassDelete) {
      this.pendingDeletions = toDelete;
      console.warn(`[samsyn] refusing to delete ${toDelete.length} of ${total} shared files; held back`, toDelete);
      new Notice(
        `Samsyn: ${toDelete.length} files are missing here but exist in ${this.target.label}. ` +
          `Not deleting them for safety. They will be re-downloaded on next reconcile, or run ` +
          `"Samsyn: Apply held-back deletions" if you really removed them.`,
        15_000,
      );
      // Bring them back locally instead, so this device matches the shared vault.
      for (const key of toDelete) {
        this.forgetSynced(key);
        const t = this.texts.get(key);
        if (t) await this.writeTextToDisk(key, t.toString());
        else {
          const m = this.blobs.get(key);
          if (m) await this.applyRemoteBlob(key, m);
        }
      }
    } else {
      this.pendingDeletions = [];
      for (const key of toDelete) this.removeFromDoc(key);
    }
    await this.persistence.flushState(this.state);
  }

  private async reconcileText(key: string, file: TFile): Promise<void> {
    const local = await this.app.vault.read(file);
    const localHash = hashText(local);
    const ytext = this.texts.get(key);
    const lastSynced = this.state.synced[key];

    if (!ytext) {
      if (lastSynced) {
        // Was in the doc before, now gone: deleted remotely while we were offline.
        await this.deleteFromDisk(key);
      } else if (!this.target.readOnly) {
        this.pushLocalText(key, local);
      }
      return;
    }

    const remote = ytext.toString();
    if (remote === local) {
      this.markSynced(key, localHash);
      return;
    }
    if (lastSynced === localHash) {
      // Only the remote side changed.
      await this.writeTextToDisk(key, remote);
    } else if (lastSynced === hashText(remote) && !this.target.readOnly) {
      // Only we changed.
      this.pushLocalText(key, local);
    } else {
      // Both changed, or first contact with differing content: keep both.
      await this.writeConflictCopy(key, local);
      await this.writeTextToDisk(key, remote);
    }
  }

  private async reconcileBlob(key: string, file: TFile): Promise<void> {
    const meta = this.blobs.get(key);
    const lastSynced = this.state.synced[key];
    if (!meta) {
      if (lastSynced) await this.deleteFromDisk(key);
      else if (!this.target.readOnly) await this.pushLocalBlob(key, file);
      return;
    }
    const localHash = await sha256Hex(await this.app.vault.readBinary(file));
    if (localHash === meta.hash) {
      this.markSynced(key, localHash);
      return;
    }
    if (lastSynced === localHash) {
      await this.applyRemoteBlob(key, meta);
    } else if (lastSynced === meta.hash && !this.target.readOnly) {
      await this.pushLocalBlob(key, file);
    } else {
      // Both changed: newer mtime wins, the loser is kept as a conflict copy.
      if (file.stat.mtime > meta.mtime && !this.target.readOnly) {
        await this.pushLocalBlob(key, file);
      } else {
        await this.writeConflictCopy(key, await this.app.vault.readBinary(file));
        await this.applyRemoteBlob(key, meta);
      }
    }
  }
}

function pad(n: number): string {
  return n < 10 ? "0" + n : String(n);
}

export { normalizePath };
