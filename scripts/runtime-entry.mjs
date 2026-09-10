import { readFile, stat } from "node:fs/promises";
import { join } from "node:path";

// The installed package is authoritative; its CLI layout can change between releases.
export function dshEntryRelativePath(manifest) {
  const entry = typeof manifest.bin === "string" ? manifest.bin : manifest.bin?.dsh;
  if (typeof entry !== "string" || !entry || entry.includes("\\") || entry.includes(":")) {
    throw new Error("DSH package.json must declare a relative bin.dsh entry");
  }
  if (entry.split("/").some((part) => !part || part === ".." || part === ".")) {
    throw new Error(`Invalid DSH CLI entry: ${entry}`);
  }
  return entry;
}

export async function resolveDshEntry(dshDirectory) {
  const packageDirectory = join(dshDirectory, "node_modules", "@deepseek-ai", "dsh");
  const manifest = JSON.parse(await readFile(join(packageDirectory, "package.json"), "utf8"));
  const entry = join(packageDirectory, dshEntryRelativePath(manifest));
  if (!(await stat(entry).catch(() => undefined))?.isFile()) {
    throw new Error(`Bundled DSH CLI entry is missing: ${entry}`);
  }
  return entry;
}
