import { Notice, Setting } from "obsidian";
import { SamsynApi, SharingInfo } from "./api";
import type SamsynPlugin from "./main";
import { accessText, ConfirmModal, enclosingShares, ForwardModal, MountModal, MoveMountModal, NewTicketModal, pathText, ShareModal } from "./sharing-modals";

const quoted = (names: string[]) => names.map((n) => `“${n}”`).join(", ");

function describeStatus(status: string): string {
  if (status === "active") return "live, same server";
  if (status === "remote: connected") return "live over the server link";
  if (status.startsWith("remote: connecting") || status.startsWith("remote: starting")) return "connecting to the owner's server…";
  if (status.startsWith("remote: reconnecting")) return `owner's server unreachable, retrying (${status.slice("remote: reconnecting".length).trim()})`;
  if (status.startsWith("remote: offline")) return "offline: p2p is disabled on this server";
  return status;
}

/**
 * The Sharing tab: what I share (with its tickets) and what others share with me. Every action
 * that produces a ticket or needs confirmation opens a modal, so the list can refresh underneath
 * without losing what the user is looking at.
 */
export class SharingSection {
  private api: SamsynApi;
  private info: SharingInfo | null = null;
  private error: string | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;
  /** Shares whose revoked tickets are expanded. */
  private showRevoked = new Set<string>();

  constructor(
    private plugin: SamsynPlugin,
    private root: HTMLElement,
  ) {
    this.api = new SamsynApi(plugin.settings);
  }

  /** Poll while the tab is open so link status and revocations show up on their own. */
  start(): void {
    this.stop();
    this.timer = setInterval(() => void this.load(true), 10_000);
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  private hasServer(): boolean {
    return !!(this.plugin.settings.serverUrl && this.plugin.settings.token);
  }

  async load(quiet = false): Promise<void> {
    if (!this.hasServer()) {
      this.render();
      return;
    }
    const before = this.info;
    try {
      const next = await this.api.sharing();
      this.error = null;
      if (before) this.announceChanges(before, next);
      void this.plugin.refreshSharingRoots(next);
      if (quiet && JSON.stringify(before) === JSON.stringify(next)) return;
      this.info = next;
    } catch (e) {
      if (quiet) return;
      this.info = null;
      this.error = (e as Error).message;
    }
    this.render();
  }

  private announceChanges(before: SharingInfo, after: SharingInfo): void {
    for (const m of after.mounts) {
      const old = before.mounts.find((x) => x.id === m.id);
      if (old && old.status !== m.status && m.status.startsWith("revoked")) {
        new Notice(`Samsyn: access to "${m.name}" (${m.mount_path}) was revoked by the owner. Your copy stays as ordinary notes.`, 10000);
      }
    }
    for (const s of after.shares) {
      const old = before.shares.find((x) => x.id === s.id);
      if (old && s.mounted_by > old.mounted_by) new Notice(`Samsyn: someone mounted "${s.name}"`);
    }
  }

  private async act(label: string, fn: () => Promise<unknown>): Promise<void> {
    try {
      await fn();
      await this.load();
    } catch (e) {
      new Notice(`Samsyn: ${label} failed: ${(e as Error).message}`, 8000);
    }
  }

  private confirm(title: string, body: string, button: string, fn: () => Promise<void>): void {
    new ConfirmModal(this.plugin.app, title, body, button, async () => {
      await fn();
      await this.load();
    }).open();
  }

  render(): void {
    const el = this.root;
    el.empty();
    const reload = () => void this.load();

    if (!this.hasServer()) {
      this.renderGuestMounts(el, reload);
      return;
    }
    if (this.error) {
      el.createDiv({ cls: "samsyn-diag samsyn-error", text: `Cannot reach the sharing API: ${this.error}` });
      new Setting(el).addButton((b) => b.setButtonText("Retry").onClick(() => void this.load()));
      this.renderGuestMounts(el, reload);
      return;
    }
    if (!this.info) {
      el.createDiv({ cls: "samsyn-diag", text: "Loading…" });
      return;
    }
    const info = this.info;

    // ---------------------------------------------------------- shared by me
    new Setting(el)
      .setName("Shared by me")
      .setHeading()
      .setDesc("Folders or notes from this vault that other people edit live with you. Each person gets their own ticket.")
      .addButton((b) =>
        b
          .setButtonText("Share a folder or note…")
          .setCta()
          .onClick(() => new ShareModal(this.plugin, "", reload).open()),
      );

    if (info.shares.length === 0) {
      el.createDiv({ cls: "samsyn-empty", text: "Nothing shared yet. Right-click a folder or note and choose “Samsyn: Share…”, or use the button above." });
    }
    for (const s of info.shares) {
      const card = el.createDiv({ cls: "samsyn-card" });
      const active = s.grants.filter((g) => !g.revoked_at);
      new Setting(card)
        .setName(s.name)
        .setDesc(`${pathText(s.path)} · ${active.length} active ticket${active.length === 1 ? "" : "s"} · mounted by ${s.mounted_by}`)
        .setClass("samsyn-card-head")
        .addButton((b) => b.setButtonText("New ticket…").onClick(() => new NewTicketModal(this.plugin, s, reload).open()))
        .addButton((b) =>
          b
            .setButtonText("Unshare")
            .setWarning()
            .onClick(() =>
              this.confirm(
                `Stop sharing ${s.name}?`,
                `Everyone who mounted ${pathText(s.path)} stops receiving changes. They keep their copy as ordinary notes; your files are untouched.`,
                "Unshare",
                () => this.api.unshare(s.id),
              ),
            ),
        );
      const live = s.grants.filter((g) => !g.revoked_at);
      const revoked = s.grants.filter((g) => g.revoked_at);
      for (const g of live) {
        const uses = g.mounted_by ? ` · mounted by ${g.mounted_by}` : "";
        const seen = g.last_seen_at ? ` · last used ${g.last_seen_at.slice(0, 16).replace("T", " ")}` : "";
        new Setting(card)
          .setClass("samsyn-grant")
          .setName(`ticket “${g.label}”`)
          .setDesc(`${accessText(g.access, g.rw_paths)} · issued ${g.created_at.slice(0, 10)}${uses}${seen}`)
          .addButton((b) =>
            b
              .setButtonText("Revoke")
              .setWarning()
              .onClick(() =>
                this.confirm(
                  `Revoke ticket “${g.label}” for ${s.name}?`,
                  `This cannot be undone. Everyone who joined with this ticket is cut off: their copy stops syncing and their edits no longer reach you. Other tickets for ${s.name} keep working. To let them back in, issue a new ticket.`,
                  "Yes, revoke",
                  () => this.api.revoke(s.id, g.label),
                ),
              ),
          );
      }
      if (revoked.length > 0) {
        const open = this.showRevoked.has(s.id);
        const toggle = new Setting(card)
          .setClass("samsyn-grant")
          .setClass("samsyn-muted")
          .setName(`${revoked.length} revoked ticket${revoked.length === 1 ? "" : "s"}`)
          .addButton((b) =>
            b.setButtonText(open ? "Hide" : "Show").onClick(() => {
              if (open) this.showRevoked.delete(s.id);
              else this.showRevoked.add(s.id);
              this.render();
            }),
          );
        if (open) {
          toggle.addButton((b) =>
            b.setButtonText("Delete all revoked").onClick(() =>
              this.act("delete revoked tickets", async () => {
                for (const g of revoked) await this.api.deleteRevoked(s.id, g.label);
              }),
            ),
          );
          for (const g of revoked) {
            new Setting(card)
              .setClass("samsyn-grant")
              .setClass("samsyn-revoked")
              .setName(`ticket “${g.label}”`)
              .setDesc(`${accessText(g.access, g.rw_paths)} · revoked ${g.revoked_at!.slice(0, 10)}`)
              .addButton((b) =>
                b.setButtonText("Restore").onClick(() =>
                  this.confirm(
                    `Restore ticket “${g.label}”?`,
                    `The same ticket becomes valid again. Guests reconnect by themselves and mounts on this server resume; people on other servers have to mount it again.`,
                    "Restore",
                    async () => {
                      const r = await this.api.restoreTicket(s.id, g.label);
                      new Notice(`Samsyn: ticket “${g.label}” restored${r.restored_mounts ? `, ${r.restored_mounts} mount(s) resumed` : ""}`);
                    },
                  ),
                ),
              )
              .addButton((b) =>
                b.setButtonText("Delete").onClick(() => this.act("delete ticket", () => this.api.deleteRevoked(s.id, g.label))),
              );
          }
        }
      }
    }

    // ---------------------------------------------------------- shared with me
    new Setting(el)
      .setName("Shared with me")
      .setHeading()
      .setDesc("Folders or notes other people share with you. They live in a folder of your choice and sync live.")
      .addButton((b) =>
        b
          .setButtonText("Add from ticket…")
          .setCta()
          .onClick(() => new MountModal(this.plugin, reload).open()),
      );
    if (info.mounts.length === 0 && this.plugin.settings.guestMounts.length === 0) {
      el.createDiv({ cls: "samsyn-empty", text: "Nothing mounted. Paste a ticket someone sent you with the button above." });
    }
    this.renderGuestCards(el);
    for (const m of info.mounts) {
      const revoked = m.status.startsWith("revoked");
      const card = el.createDiv({ cls: "samsyn-card" + (revoked ? " samsyn-revoked" : "") });
      const row = new Setting(card).setName(m.name).setClass("samsyn-card-head");
      // Shares of mine this mount sits inside: each either carries it onward or skips it.
      const enclosing = enclosingShares(info, m.mount_path);
      const onward = enclosing.filter((s) => (m.forward_shares ?? []).includes(s.id));
      const insideText = enclosing.length
        ? ` · inside ${quoted(enclosing.map((s) => s.name))}: ${onward.length ? `shared onward with ${quoted(onward.map((s) => s.name))}` : "not shared onward"}`
        : "";
      if (revoked) row.setDesc(`${m.mount_path} · access revoked by the owner · no longer syncing · your copy stays as ordinary notes${enclosing.length ? " · kept out of your shares until removed" : ""}`);
      else {
        row.setDesc(`${m.mount_path} · ${accessText(m.access, m.rw_paths)} · ${describeStatus(m.status)}${insideText}`);
        if (enclosing.length) row.addButton((b) => b.setButtonText("Onward…").onClick(() => new ForwardModal(this.plugin, m, enclosing, reload).open()));
        row.addButton((b) =>
          b.setButtonText("Move…").onClick(() =>
            new MoveMountModal(this.plugin, { kind: "mount", id: m.id, name: m.name, path: m.mount_path, isFile: false }, reload).open(),
          ),
        );
      }
      const leftovers = enclosing.length
        ? ` As ordinary notes they are then part of ${quoted(enclosing.map((s) => s.name))} like everything else there, and reach the people holding tickets for ${enclosing.length === 1 ? "it" : "them"}.`
        : "";
      row.addButton((b) =>
        b.setButtonText(revoked ? "Remove" : "Unmount").onClick(() =>
          this.confirm(
            revoked ? `Remove ${m.name} from the list?` : `Unmount ${m.name}?`,
            `The files under ${m.mount_path} stay in your vault as ordinary notes; they just stop syncing with the owner.${leftovers}`,
            revoked ? "Remove" : "Unmount",
            () => this.api.unmount(m.id),
          ),
        ),
      );
    }

    el.createDiv({ cls: "samsyn-diag samsyn-muted", text: `This server's node id: ${info.node_id.slice(0, 16)}…` });
  }

  /** "Shared with me" when there is no server of our own: guest mounts only. */
  private renderGuestMounts(el: HTMLElement, reload: () => void): void {
    new Setting(el)
      .setName("Shared with me")
      .setHeading()
      .setDesc("Folders or notes other people share with you. This device talks to their server directly.")
      .addButton((b) =>
        b
          .setButtonText("Add from ticket…")
          .setCta()
          .onClick(() => new MountModal(this.plugin, reload).open()),
      );
    if (this.plugin.settings.guestMounts.length === 0) {
      el.createDiv({ cls: "samsyn-empty", text: "Nothing mounted. Paste a ticket someone sent you with the button above." });
    }
    this.renderGuestCards(el);
  }

  private renderGuestCards(el: HTMLElement): void {
    for (const m of this.plugin.settings.guestMounts) {
      const d = this.plugin.guestEngine(m.id)?.getDiagnostics();
      const status = !m.enabled
        ? "paused"
        : !d
          ? "starting…"
          : d.status === "connected"
            ? `live as guest · ${d.peers} other${d.peers === 1 ? "" : "s"} online`
            : d.status === "connecting"
              ? "connecting to the owner's server…"
              : `offline${d.lastError ? ` · ${d.lastError}` : ""}`;
      const card = el.createDiv({ cls: "samsyn-card" });
      // A guest folder never reaches the home server, so a share around it always skips it.
      const enclosing = enclosingShares(this.info, m.mountPath);
      const insideText = enclosing.length ? ` · inside ${quoted(enclosing.map((s) => s.name))}: never shared onward as a guest mount` : "";
      new Setting(card)
        .setName(m.name)
        .setClass("samsyn-card-head")
        .setDesc(`${m.mountPath} · ${accessText(m.access, m.rwPaths)} · guest of ${m.url.replace(/^https?:\/\//, "")} · ${status}${insideText}`)
        .addButton((b) =>
          b.setButtonText("Move…").onClick(() =>
            new MoveMountModal(this.plugin, { kind: "guest", id: m.id, name: m.name, path: m.mountPath, isFile: false }, () => this.render()).open(),
          ),
        )
        .addButton((b) =>
          b.setButtonText("Unmount").onClick(() =>
            this.confirm(
              `Unmount ${m.name}?`,
              `The files under ${m.mountPath} stay in your vault as ordinary notes; they just stop syncing with the owner.`,
              "Unmount",
              () => this.plugin.removeGuestMount(m.id),
            ),
          ),
        );
    }
  }
}
