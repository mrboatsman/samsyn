import type { ListedFiles } from "obsidian";

/** The part of `DataAdapter` the walk needs; kept small so tests can fake it. */
export interface DiskLister {
  list(normalizedPath: string): Promise<ListedFiles>;
}

/**
 * Every file on disk under `dir` (`""` for the vault root), depth first, as vault paths
 * without a leading slash. This reads the adapter, not Obsidian's index: files with
 * extensions Obsidian does not detect (.js, .css, .html, .txt, … unless "Detect all file
 * extensions" is on) are absent from `vault.getFiles()` but present here. `descend` prunes
 * whole folders; folders whose name starts with "." are never entered. A folder that
 * cannot be listed (vanished, no permission) contributes nothing.
 */
export async function walkDisk(adapter: DiskLister, dir: string, descend: (folderPath: string) => boolean): Promise<string[]> {
  const out: string[] = [];
  const visit = async (d: string): Promise<void> => {
    let listed: ListedFiles;
    try {
      listed = await adapter.list(d || "/");
    } catch {
      return;
    }
    for (const f of listed.files) out.push(clean(f));
    for (const raw of listed.folders) {
      const folder = clean(raw);
      const name = folder.slice(folder.lastIndexOf("/") + 1);
      if (name.startsWith(".") || !descend(folder)) continue;
      await visit(folder);
    }
  };
  await visit(clean(dir));
  return out;
}

function clean(p: string): string {
  return p.replace(/^\/+/, "").replace(/\/+$/, "");
}
