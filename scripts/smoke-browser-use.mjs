import assert from "node:assert/strict";
import { mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

// The bundled browser MCP is the one component that needs Node, Python,
// Chrome/Edge, and a browser profile at the same time. Exercising a real
// browser_open through the real MCP protocol is the only check that proves the
// offline runtime can drive a page.
const runtime = resolve(process.argv[2] ?? "src-tauri/runtime");
const browser = process.argv[3] ?? process.env.OPENHARNESS_BROWSER_EXECUTABLE;
const node = join(runtime, process.platform === "win32" ? "node.exe" : "node");
const dsh = join(runtime, "dsh");
const packageDir = join(dsh, "node_modules/@ricardochen1996/browser-use-mcp");
const entry = join(packageDir, "bin/browser-use-mcp.mjs");
const python = join(runtime, process.platform === "win32" ? "python/python.exe" : "python/bin/python3");
for (const file of [node, entry, python]) {
  assert.ok((await stat(file).catch(() => undefined))?.isFile(), `Missing browser MCP file: ${file}`);
}
assert.ok(browser, "Pass the Chrome/Edge executable as the second argument");
assert.ok((await stat(browser).catch(() => undefined))?.isFile(), `Browser executable is missing: ${browser}`);

const require = createRequire(join(packageDir, "package.json"));
const { Client } = await import(require.resolve("@modelcontextprotocol/sdk/client/index.js"));
const { StdioClientTransport } = await import(require.resolve("@modelcontextprotocol/sdk/client/stdio.js"));

const home = await mkdtemp(join(tmpdir(), "OpenHarness browser MCP smoke "));
const fixture = join(home, "fixture.html");
await writeFile(
  fixture,
  "<!doctype html><html><head><title>OpenHarness browser smoke</title></head>" +
    "<body><h1>Offline browser fixture</h1><button>Click me</button></body></html>",
);

const transport = new StdioClientTransport({
  command: node,
  args: [entry, "serve"],
  cwd: packageDir,
  stderr: "pipe",
  env: {
    ...process.env,
    BROWSER_USE_MCP_HOME: home,
    BROWSER_USE_MCP_PYTHON: python,
    BROWSER_USE_MCP_MODE: "launch",
    BROWSER_USE_MCP_EXECUTABLE_PATH: browser,
    BROWSER_USE_MCP_HEADLESS: "true",
    BROWSER_USE_MCP_NO_JEV: "1",
    BROWSER_USE_MCP_LOG_LEVEL: "error",
  },
});
let stderr = "";
transport.stderr?.on("data", (chunk) => {
  stderr += chunk.toString();
});
const client = new Client({ name: "openharness-browser-smoke", version: "0.0.0" });
try {
  await client.connect(transport);
  const tools = await client.listTools();
  assert.ok(
    tools.tools.some((tool) => tool.name === "browser_open"),
    "The MCP server did not expose browser_open",
  );
  const opened = await client.callTool(
    { name: "browser_open", arguments: { url: pathToFileURL(fixture).href } },
    undefined,
    { timeout: 120000 },
  );
  const text = opened.content
    .filter((item) => item.type === "text")
    .map((item) => item.text)
    .join("\n");
  assert.match(text, /Observation 1/, `browser_open returned no observation:\n${text}\n${stderr}`);
  assert.match(text, /button/i, `browser_open returned no button target:\n${text}`);
  await client.callTool({ name: "browser_close", arguments: {} }, undefined, { timeout: 30000 });
  console.log(`PASS: bundled browser MCP opened a page with ${browser}`);
} finally {
  await client.close().catch(() => {});
  await rm(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }).catch(() => {
    console.warn(`Could not remove the browser MCP smoke home: ${home}`);
  });
}
