import { App, Notice, PluginSettingTab, Setting, requestUrl } from "obsidian";
import type SamsynPlugin from "./main";
import { SharingSection } from "./sharing-ui";
import { DeviceSetupModal } from "./device-modals";
import { DevicePickerModal } from "./device-picker";
import { SamsynApi } from "./api";
import { formatBytes } from "./format";

/** A share mounted straight from the owner's server with a ticket (no server of our own). */
export interface GuestMount {
  id: string;
  /** Owner server's public URL from the ticket. */
  url: string;
  shareId: string;
  secret: string;
  access: "rw" | "ro";
  /** Share-relative paths a read-only ticket may still write (folders end with "/"). */
  rwPaths: string[];
  name: string;
  /** Vault folder, no trailing slash, e.g. "Shared/X". */
  mountPath: string;
  enabled: boolean;
}

export interface SamsynSettings {
  enabled: boolean;
  serverUrl: string;
  token: string;
  vaultId: string;
  deviceName: string;
  textExtensions: string;
  syncBinary: boolean;
  excludePaths: string;
  liveDebounceMs: number;
  guestMounts: GuestMount[];
  /** Local switch: take part in plugin-settings sync on this device (selection is shared). */
  configSync: { enabled: boolean };
}

export const DEFAULT_SETTINGS: SamsynSettings = {
  enabled: true,
  serverUrl: "",
  token: "",
  vaultId: "",
  deviceName: "",
  textExtensions: "md, txt",
  syncBinary: true,
  excludePaths: "",
  liveDebounceMs: 150,
  guestMounts: [],
  configSync: { enabled: false },
};

export function slugify(name: string): string {
  const s = name
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64);
  return s || "vault";
}

export function randomSuffix(len = 4): string {
  const chars = "abcdefghjkmnpqrstuvwxyz23456789";
  let out = "";
  for (let i = 0; i < len; i++) out += chars[Math.floor(Math.random() * chars.length)];
  return out;
}

/** Normalise user-entered server URL into http(s) base and ws(s) base. */
export function serverUrls(raw: string): { http: string; ws: string } | null {
  let s = raw.trim().replace(/\/+$/, "");
  if (!s) return null;
  if (!/^[a-z]+:\/\//i.test(s)) s = "http://" + s;
  let http: string;
  if (s.startsWith("ws://")) http = "http://" + s.slice(5);
  else if (s.startsWith("wss://")) http = "https://" + s.slice(6);
  else http = s;
  const ws = http.startsWith("https://") ? "wss://" + http.slice(8) : "ws://" + http.slice(7);
  return { http, ws: ws + "/ws" };
}

export type SettingsTabName = "server" | "sharing" | "plugins" | "advanced";

const TABS: Array<{ id: SettingsTabName; label: string }> = [
  { id: "server", label: "Server" },
  { id: "sharing", label: "Sharing" },
  { id: "plugins", label: "Plugins" },
  { id: "advanced", label: "Advanced" },
];

export class SamsynSettingTab extends PluginSettingTab {
  private statusTimer: ReturnType<typeof setInterval> | null = null;
  private restartTimer: ReturnType<typeof setTimeout> | null = null;
  /** What the server last said in /health; an error string when it could not be reached. */
  private serverVersion: { version: string } | { error: string } | null = null;
  private sharing: SharingSection | null = null;
  private active: SettingsTabName = "server";

  constructor(
    app: App,
    private plugin: SamsynPlugin,
  ) {
    super(app, plugin);
  }

  /** Open the settings on a given tab (used by commands and the file menu). */
  show(tab: SettingsTabName): void {
    this.active = tab;
    // @ts-expect-error setting API is not typed but exists on all platforms
    this.app.setting.open();
    // @ts-expect-error same as above
    this.app.setting.openTabById(this.plugin.manifest.id);
  }

  hide(): void {
    this.clearTimers();
  }

  /**
   * What the engine syncs (extensions, attachments, excludes) is fixed when it is built, so a
   * change needs a restart, which also runs the reconcile that pushes or pulls the newly
   * included files. Text fields fire per keystroke; wait for typing to stop.
   */
  private scheduleRestart(): void {
    if (this.restartTimer) clearTimeout(this.restartTimer);
    this.restartTimer = setTimeout(() => {
      this.restartTimer = null;
      void this.plugin.restartEngine();
    }, 1500);
  }

  private clearTimers(): void {
    if (this.statusTimer) clearInterval(this.statusTimer);
    this.statusTimer = null;
    if (this.restartTimer) {
      // The tab closed before the debounce fired: apply the pending scope change now.
      clearTimeout(this.restartTimer);
      this.restartTimer = null;
      void this.plugin.restartEngine();
    }
    this.sharing?.stop();
    this.sharing = null;
  }

  display(): void {
    const { containerEl } = this;
    containerEl.empty();
    this.clearTimers();

    const bar = containerEl.createDiv({ cls: "samsyn-tabs" });
    for (const t of TABS) {
      const btn = bar.createEl("button", { text: t.label, cls: "samsyn-tab" + (t.id === this.active ? " is-active" : "") });
      btn.onclick = () => {
        this.active = t.id;
        this.display();
      };
    }
    const body = containerEl.createDiv({ cls: "samsyn-tab-body" });
    switch (this.active) {
      case "server":
        this.renderServerTab(body);
        break;
      case "sharing":
        this.renderSharingTab(body);
        break;
      case "plugins":
        this.renderConfigSync(body);
        break;
      case "advanced":
        this.renderAdvancedTab(body);
        break;
    }
  }

  // ------------------------------------------------------------------ server tab

  private renderServerTab(containerEl: HTMLElement): void {
    const s = this.plugin.settings;

    new Setting(containerEl).setName("Status").setHeading();
    const statusEl = containerEl.createDiv({ cls: "samsyn-diag" });
    this.renderStatus(statusEl);
    this.statusTimer = setInterval(() => this.renderStatus(statusEl), 2000);
    void this.refreshServerVersion(statusEl);

    new Setting(containerEl).setName("Connection").setHeading();

    new Setting(containerEl)
      .setName("Enable sync")
      .setDesc("Turn live synchronisation on or off for this device.")
      .addToggle((t) =>
        t.setValue(s.enabled).onChange(async (v) => {
          s.enabled = v;
          await this.plugin.saveSettings();
          await this.plugin.restartEngine();
        }),
      );

    new Setting(containerEl)
      .setName("Server URL")
      .setDesc("Address of your samsyn, e.g. https://sync.example.com or http://192.168.1.10:8787")
      .addText((t) =>
        t
          .setPlaceholder("http://localhost:8787")
          .setValue(s.serverUrl)
          .onChange(async (v) => {
            s.serverUrl = v.trim();
            await this.plugin.saveSettings();
          }),
      );

    new Setting(containerEl)
      .setName("Token")
      .setDesc("Your user token (rp_…). The server prints one on first start; admins create more with `samsyn user add`.")
      .addText((t) => {
        t.inputEl.type = "password";
        t.setValue(s.token).onChange(async (v) => {
          s.token = v.trim();
          await this.plugin.saveSettings();
        });
      });

    new Setting(containerEl)
      .setName("Vault ID")
      .setDesc("Vault name on the server, scoped to your user. Use the same ID on every device that should share this vault. Defaults to the vault name.")
      .addText((t) =>
        t.setValue(s.vaultId).onChange(async (v) => {
          s.vaultId = slugify(v);
          await this.plugin.saveSettings();
        }),
      );

    new Setting(containerEl)
      .setName("Device name")
      .setDesc("Shown to other devices and used in conflict file names.")
      .addText((t) =>
        t.setValue(s.deviceName).onChange(async (v) => {
          s.deviceName = v.trim() || s.deviceName;
          await this.plugin.saveSettings();
        }),
      );

    new Setting(containerEl)
      .setName("Connect")
      .setDesc("Apply the settings above and (re)connect to the server.")
      .addButton((b) =>
        b.setButtonText("Test connection").onClick(async () => {
          const urls = serverUrls(s.serverUrl);
          if (!urls) {
            new Notice("Samsyn: enter a server URL first");
            return;
          }
          try {
            const res = await requestUrl({
              url: `${urls.http}/me`,
              headers: { Authorization: `Bearer ${s.token}` },
              throw: false,
            });
            if (res.status === 200) new Notice(`Samsyn: server OK, token valid for user "${res.json?.user ?? "?"}"`);
            else if (res.status === 401) new Notice("Samsyn: server reachable but token rejected");
            else new Notice(`Samsyn: server responded ${res.status}`);
            void this.refreshServerVersion(statusEl);
          } catch (e) {
            new Notice(`Samsyn: cannot reach server (${(e as Error).message})`);
          }
        }),
      )
      .addButton((b) =>
        b
          .setButtonText("Reconnect")
          .setCta()
          .onClick(async () => {
            await this.plugin.restartEngine();
            new Notice("Samsyn: reconnecting");
          }),
      );

    if (s.serverUrl && s.token) {
      new Setting(containerEl)
        .setName("Device tokens")
        .setHeading()
        .setDesc("Each device should have its own token, so one can be revoked without the others.")
        .addButton((b) => b.setButtonText("Set up another device…").setCta().onClick(() => new DeviceSetupModal(this.plugin).open()));
      const devicesEl = containerEl.createDiv();
      void this.renderDevices(devicesEl);
    }

    new Setting(containerEl).setName("What to sync").setHeading();

    new Setting(containerEl)
      .setName("Text file extensions")
      .setDesc("Comma separated. These are merged character by character. Everything else is synced as a whole file.")
      .addText((t) =>
        t.setValue(s.textExtensions).onChange(async (v) => {
          s.textExtensions = v;
          await this.plugin.saveSettings();
          this.scheduleRestart();
        }),
      );

    new Setting(containerEl)
      .setName("Sync attachments and other files")
      .setDesc(
        "Images, PDFs and other non-text files. Last writer wins. Files Obsidian does not show (.js, .css, .html, …) " +
          "are picked up at startup and by \"Reconcile all files now\"; turn on Obsidian's \"Detect all file extensions\" to sync them live.",
      )
      .addToggle((t) =>
        t.setValue(s.syncBinary).onChange(async (v) => {
          s.syncBinary = v;
          await this.plugin.saveSettings();
          await this.plugin.restartEngine();
        }),
      );

    new Setting(containerEl)
      .setName("Exclude paths")
      .setDesc("Comma separated folders or files to skip, e.g. Private/, Scratch.md")
      .addText((t) =>
        t.setValue(s.excludePaths).onChange(async (v) => {
          s.excludePaths = v;
          await this.plugin.saveSettings();
          this.scheduleRestart();
        }),
      );
  }

  /** Plugin settings sync: which plugins, which devices, with or without the plugin files. */
  private renderConfigSync(containerEl: HTMLElement): void {
    const s = this.plugin.settings;
    containerEl.createEl("p", {
      cls: "samsyn-muted",
      text: "Keep chosen plugins' settings (their data.json) the same on your devices, e.g. Iconize's icons and colours. Optionally the plugin's files too, so it installs itself on a new device. Last change wins. The selection and the device list are shared between devices; the switch below is per device.",
    });
    if (!s.serverUrl || !s.token) {
      containerEl.createDiv({ cls: "samsyn-empty", text: "Needs a server connection (Server tab)." });
      return;
    }
    new Setting(containerEl)
      .setName("Sync plugin settings on this device")
      .setDesc("Off: this device neither sends nor receives plugin settings.")
      .addToggle((t) =>
        t.setValue(s.configSync.enabled).onChange(async (v) => {
          s.configSync.enabled = v;
          await this.plugin.saveSettings();
          // No restart needed: the switch is read live; just register this device and sync.
          if (v) {
            this.plugin.configSync?.touchSelf();
            void this.plugin.configSync?.reconcileAll();
          }
          this.display();
        }),
      );
    const cs = this.plugin.configSync;
    if (!cs) {
      const d = this.plugin.engine?.getDiagnostics();
      containerEl.createDiv({
        cls: "samsyn-empty",
        text: d ? "Starting…" : s.enabled ? "Not connected: press Reconnect on the Server tab, then come back." : "Sync is disabled (Server tab).",
      });
      // The engine is being (re)started; try again shortly.
      if (this.active === "plugins") setTimeout(() => this.active === "plugins" && !this.plugin.configSync && this.display(), 2000);
      return;
    }
    const known = cs.knownDevices();
    containerEl.createDiv({
      cls: "samsyn-muted samsyn-small",
      text: `Known devices: ${known.map((d) => `${d.info.name}${d.online ? " (online)" : ""}`).join(", ") || "none yet"}. This device is “${s.deviceName}” (rename it under Connection).`,
    });

    const manifests = (this.app as unknown as { plugins: { manifests: Record<string, { id: string; name: string }> } }).plugins?.manifests ?? {};
    const ids = new Set<string>(Object.keys(manifests));
    for (const [id] of cs.entries()) ids.add(id);
    ids.delete(this.plugin.manifest.id);
    if (ids.size === 0) containerEl.createDiv({ cls: "samsyn-empty", text: "No community plugins installed." });
    for (const id of [...ids].sort()) {
      const entry = cs.get(id);
      const name = manifests[id]?.name ?? id;
      const installed = !!manifests[id];
      const mine = entry ? cs.participating(id) : false;
      const row = new Setting(containerEl)
        .setClass("samsyn-grant")
        .setName(name + (installed ? "" : " (not installed here)"))
        .setDesc(
          entry
            ? `${cs.describeDevices(entry)}${entry.code ? " · with plugin files" : ""}${s.configSync.enabled ? (mine ? " · this device takes part" : " · not this device") : ""}`
            : "not synced",
        );
      row.addToggle((t) =>
        t.setValue(!!entry).onChange((v) => {
          cs.set(id, v ? { devices: "all", code: false } : null);
          this.display();
        }),
      );
      if (entry) {
        row.addButton((b) =>
          b.setButtonText("Devices…").onClick(() => new DevicePickerModal(this.plugin, cs, id, name, entry, () => this.display()).open()),
        );
        row.addExtraButton((b) =>
          b
            .setIcon(entry.code ? "package-check" : "package")
            .setTooltip(entry.code ? "Plugin files are synced too (click to stop)" : "Also sync the plugin's files (main.js, manifest.json, styles.css) so it installs on other devices")
            .onClick(() => {
              cs.set(id, { ...entry, code: !entry.code });
              this.display();
            }),
        );
      }
    }
  }

  private async renderDevices(el: HTMLElement): Promise<void> {
    el.empty();
    try {
      const r = await new SamsynApi(this.plugin.settings).listTokens();
      if (r.tokens.length === 0) el.createDiv({ cls: "samsyn-empty", text: "No tokens listed." });
      for (const t of r.tokens) {
        const row = new Setting(el)
          .setClass("samsyn-grant")
          .setName(`token “${t.label}”${t.current ? " — this device" : ""}`)
          .setDesc(`issued ${t.created_at.slice(0, 10)}${t.last_used_at ? ` · last used ${t.last_used_at.slice(0, 16).replace("T", " ")}` : " · never used"}`);
        if (t.current) {
          // Revoking it would cut this device off; switch to another token first.
          row.addExtraButton((b) => b.setIcon("lock").setTooltip("This is the token this device uses. To retire it, set up another device token, paste it under Connection, then revoke this one.").setDisabled(true));
          continue;
        }
        row.addButton((b) =>
          b.setButtonText("Revoke").onClick(async () => {
            try {
              await new SamsynApi(this.plugin.settings).revokeToken(t.label);
              new Notice(`Samsyn: token “${t.label}” revoked.`);
              await this.renderDevices(el);
            } catch (e) {
              new Notice(`Samsyn: ${(e as Error).message}`);
            }
          }),
        );
      }
    } catch (e) {
      el.createDiv({ cls: "samsyn-empty", text: `Cannot list tokens: ${(e as Error).message}` });
    }
  }

  /** Ask the server for its version and redraw the status panel with the answer. */
  private async refreshServerVersion(statusEl: HTMLElement): Promise<void> {
    const s = this.plugin.settings;
    if (!serverUrls(s.serverUrl)) {
      this.serverVersion = null;
      return;
    }
    try {
      const h = await new SamsynApi(s).health();
      this.serverVersion = { version: h.version || "unknown" };
    } catch (e) {
      this.serverVersion = { error: (e as Error).message };
    }
    if (statusEl.isConnected) this.renderStatus(statusEl);
  }

  /** "plugin 0.3.0 · server 0.3.0", flagged when the two are from different releases. */
  private versionLine(): { text: string; cls: string } {
    const plugin = this.plugin.manifest.version;
    let text = `plugin ${plugin}`;
    let cls = "";
    const sv = this.serverVersion;
    if (sv && "version" in sv) {
      text += ` · server ${sv.version}`;
      if (releaseOf(sv.version) !== releaseOf(plugin)) {
        text += " (different releases; update the older one)";
        cls = "is-warn";
      }
    } else if (sv) {
      text += ` · server unreachable (${sv.error})`;
      cls = "is-warn";
    } else if (serverUrls(this.plugin.settings.serverUrl)) {
      text += " · server …";
    }
    return { text, cls };
  }

  private renderStatus(el: HTMLElement): void {
    el.empty();
    const line = (label: string, value: string, cls?: string) => {
      const row = el.createDiv({ cls: "samsyn-diag-row" });
      row.createSpan({ cls: "samsyn-diag-label", text: label });
      row.createSpan({ cls: "samsyn-diag-value" + (cls ? " " + cls : ""), text: value });
    };
    const version = this.versionLine();
    line("Version", version.text, version.cls);
    const d = this.plugin.engine?.getDiagnostics();
    if (!d) {
      const guests = this.plugin.settings.guestMounts.length;
      el.createDiv({
        text: !this.plugin.settings.enabled
          ? "Sync is disabled."
          : guests > 0
            ? `No server of your own configured; ${guests} shared folder${guests === 1 ? "" : "s"} syncing as guest (see the Sharing tab).`
            : "Not running. Set a server URL and press Reconnect, or add a shared folder from a ticket on the Sharing tab.",
      });
      return;
    }
    const statusText: Record<string, string> = {
      connected: "Connected, live",
      connecting: "Connecting…",
      offline: "Offline (changes queued locally)",
      disabled: "Disabled / not configured",
    };
    line("Status", statusText[d.status] ?? d.status, d.status === "connected" ? "is-ok" : d.status === "offline" ? "is-bad" : "");
    line("Initial sync", d.synced ? "done" : "pending");
    line("Other devices", String(d.peers));
    line("Synced files", `${d.textFiles} text, ${d.blobFiles} other · ${formatBytes(d.textBytes + d.blobBytes)}`);
    line("WebSocket", d.wsUrl || "–");
    if (d.lastConnectedAt) line("Last connected", new Date(d.lastConnectedAt).toLocaleTimeString());
    if (d.lastError) line("Last error", d.lastError, "is-bad");
    if (d.hint) line("Hint", d.hint, "is-warn");
    if (d.pendingDeletions > 0) line("Held back", `${d.pendingDeletions} deletions (see command "Apply held-back deletions")`, "is-warn");
    line("Install id", d.installId);
    void this.plugin.engine?.refreshStorage();
    if (d.storage) {
      const where = d.storage.backend === "indexeddb" ? "IndexedDB" : "files in plugin folder";
      let txt = `${where} · document ${formatBytes(d.storage.docBytes)} · ${d.storage.syncedEntries} entries`;
      if (d.storage.usageBytes !== null && d.storage.quotaBytes) {
        txt += ` · ${formatBytes(d.storage.usageBytes)} of ${formatBytes(d.storage.quotaBytes)} used`;
      }
      line("Local cache", txt, d.storage.docBytes > 100 * 1_048_576 ? "is-warn" : "");
    }
  }

  // ------------------------------------------------------------------ sharing tab

  private renderSharingTab(containerEl: HTMLElement): void {
    const s = this.plugin.settings;
    containerEl.createEl("p", {
      cls: "samsyn-muted",
      text: "Share one folder or note, not the whole vault. The other person pastes a ticket and gets the same folder in their vault, live in both directions. Tickets can be read-only and can be revoked at any time.",
    });
    if (!s.serverUrl || !s.token) {
      containerEl.createDiv({ cls: "samsyn-empty", text: "Sharing your own folders needs a server connection (Server tab). Folders others share with you can still be added below with their ticket." });
    }
    const sharingEl = containerEl.createDiv();
    const section = new SharingSection(this.plugin, sharingEl);
    this.sharing = section;
    section.render();
    void section.load();
    section.start();
  }

  // ------------------------------------------------------------------ advanced tab

  private renderAdvancedTab(containerEl: HTMLElement): void {
    const s = this.plugin.settings;

    new Setting(containerEl)
      .setName("Live typing delay (ms)")
      .setDesc("How long to wait after a keystroke before sending changes. Lower is more live.")
      .addSlider((sl) =>
        sl
          .setLimits(50, 2000, 50)
          .setValue(s.liveDebounceMs)
          .setDynamicTooltip()
          .onChange(async (v) => {
            s.liveDebounceMs = v;
            await this.plugin.saveSettings();
          }),
      );

    new Setting(containerEl)
      .setName("Full reconcile")
      .setDesc("Compare every file against the shared document and fix any drift.")
      .addButton((b) =>
        b.setButtonText("Reconcile now").onClick(async () => {
          await this.plugin.engine?.reconcileAll();
          new Notice("Samsyn: reconcile finished");
        }),
      );

    new Setting(containerEl)
      .setName("Reset local sync state")
      .setDesc("Forget the locally cached document and re-download from the server. Your files are not touched.")
      .addButton((b) =>
        b
          .setButtonText("Reset")
          .setWarning()
          .onClick(async () => {
            await this.plugin.resetLocalState();
            new Notice("Samsyn: local state reset, reconnecting");
          }),
      );
  }
}

/** "0.3.0" for "0.3.0", "0.3.0-beta.2" and "0.3.0+1a2b3c4": the release a version belongs to. */
function releaseOf(version: string): string {
  return version.split(/[-+]/)[0];
}
