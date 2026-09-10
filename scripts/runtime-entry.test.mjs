import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { dshEntryRelativePath, resolveDshEntry } from "./runtime-entry.mjs";

test("uses the published CLI entry instead of a hard-coded bin directory", () => {
  assert.equal(dshEntryRelativePath({ bin: { dsh: "lib/bin.js" } }), "lib/bin.js");
  assert.equal(dshEntryRelativePath({ bin: "dist/cli.js" }), "dist/cli.js");
  for (const entry of [undefined, "", "../bin.js", "/bin.js", "C:/bin.js", "lib\\bin.js"]) {
    assert.throws(() => dshEntryRelativePath({ bin: { dsh: entry } }));
  }
});

test("resolves relocated runtime in paths with spaces and Chinese characters", async () => {
  const root = await mkdtemp(join(tmpdir(), "OpenHarness 测试 "));
  try {
    const pkg = join(root, "node_modules", "@deepseek-ai", "dsh");
    await mkdir(join(pkg, "dist"), { recursive: true });
    await writeFile(join(pkg, "package.json"), JSON.stringify({ bin: { dsh: "dist/cli.js" } }));
    const entry = join(pkg, "dist/cli.js");
    await assert.rejects(resolveDshEntry(root), (error) => error.message.includes(entry));
    await writeFile(entry, "");
    assert.equal(await resolveDshEntry(root), entry);
    await rm(entry);
    await mkdir(entry);
    await assert.rejects(resolveDshEntry(root), /CLI entry is missing/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
