import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, join, resolve, win32 } from "node:path";
import { resolveDshEntry } from "./runtime-entry.mjs";

// Windows installers report verbatim (`\\?\C:\...`) resource paths, which Node
// cannot load as a module entry. Interpret Windows paths with Windows semantics
// so this script can prove the conversion, and reproduce the failure off-Windows.
const argument = process.argv[2] ?? "src-tauri/runtime";
const windowsArgument = /^[A-Za-z]:[\\/]/.test(argument) || argument.startsWith("\\\\");
const normalize = (value) =>
  value.startsWith("\\\\?\\UNC\\")
    ? `\\\\${value.slice(8)}`
    : value.startsWith("\\\\?\\")
      ? value.slice(4)
      : value;
const root = windowsArgument ? normalize(win32.resolve(argument)) : resolve(argument);
const node = join(root, process.platform === "win32" ? "node.exe" : "node");
const dsh = join(root, "dsh");
const entry = await resolveDshEntry(dsh);
const home = await mkdtemp(join(tmpdir(), "OpenHarness smoke 测试 "));
const launcher = join(dsh, "openharness-bin");
const env = Object.fromEntries(
  ["SystemRoot", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP", "PATH"].filter((key) => process.env[key]).map((key) => [key, process.env[key]]),
);
Object.assign(env, {
  HOME: home, USERPROFILE: home, DSH_HOME: home,
  PATH: [launcher, root, env.PATH].filter(Boolean).join(delimiter),
  DSH_TELEMETRY_DISABLED: "1",
  OPENHARNESS_MANAGED_RUNTIME: "1", OPENHARNESS_MANAGED_RUNTIME_PROTOCOL: "1",
  OPENHARNESS_PROFILE_NAME: "web", OPENHARNESS_PROFILE_DIRECTORY: join(home, "profiles", "web"),
  OPENHARNESS_RUNTIME_ROOT: root, OPENHARNESS_NODE_PATH: node,
  OPENHARNESS_DSH_ENTRY: entry, OPENHARNESS_PACKAGE_MANAGER_BIN: launcher,
  OPENHARNESS_RESTART_EXIT_CODE: "75",
});
let child;
try {
  for (const file of [node, join(launcher, process.platform === "win32" ? "pnpm.cmd" : "pnpm"), join(dsh, "openharness.patch.yml"), join(dsh, "openharness-find.patch.yml")]) {
    assert.ok((await stat(file)).isFile(), `Missing installed runtime file: ${file}`);
  }
  const manifest = JSON.parse(await readFile(join(dsh, "node_modules/@deepseek-ai/dsh/package.json"), "utf8"));
  const expected = JSON.parse(await readFile(new URL("../runtime/package.json", import.meta.url), "utf8")).dependencies["@deepseek-ai/dsh"];
  assert.equal(manifest.version, expected);
  for (const args of [[entry, "--version"], [entry, "--profile", "web", "--help"], ["-e", "require('node-pty'); require('sharp'); require('koffi'); Promise.all([import('@openharness/native-bridge'), import('@microspotlight/openharness-find-plugin')]).catch(error => { console.error(error); process.exitCode = 1; })"], [join(launcher, "pnpm"), "--version"]]) {
    const result = spawnSync(node, args, { cwd: dsh, env, encoding: "utf8", timeout: 60000, windowsHide: true });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stderr);
    if (args.includes("--help")) assert.match(result.stdout, /--no-open/);
  }
  child = spawn(node, [entry, "--profile", "web", "--patch", join(dsh, "openharness.patch.yml"), "--patch", join(dsh, "openharness-find.patch.yml"), "--host", "127.0.0.1", "--port", "0", "--no-open"], {
    cwd: home, env, detached: process.platform !== "win32", windowsHide: true, stdio: ["ignore", "pipe", "pipe"],
  });
  const url = await new Promise((resolveUrl, reject) => {
    let output = "";
    const diagnostics = () => output.replace(/dsh web: [^\r\n]*/g, "dsh web: [startup URL omitted]");
    const timer = setTimeout(() => reject(new Error(`Runtime startup timed out: ${diagnostics()}`)), 60000);
    const finish = (error, url) => { clearTimeout(timer); error ? reject(error) : resolveUrl(url); };
    child.once("error", finish);
    child.once("exit", (code) => finish(new Error(`Runtime exited (${code}): ${diagnostics()}`)));
    const accept = (data) => {
      output = (output + data.toString()).slice(-32768);
      const match = output.match(/dsh web: (http:\/\/[^\s\u001b]+)/);
      if (match) finish(null, new URL(match[1]));
    };
    child.stdout.on("data", accept);
    child.stderr.on("data", accept);
  });
  assert.equal(url.hostname, "127.0.0.1");
  assert.ok(url.port);
  // Like a WebView, retain the cookie issued by the launch-token redirect.
  const bootstrap = await fetch(url, { redirect: "manual", signal: AbortSignal.timeout(10000) });
  assert.equal(bootstrap.status, 303);
  assert.equal(bootstrap.headers.get("location"), "/");
  const cookie = bootstrap.headers.getSetCookie().map((value) => value.split(";")[0]).join("; ");
  assert.ok(cookie, "Launch URL must issue a browser authentication cookie");
  const response = await fetch(new URL("/", url), { headers: { cookie }, signal: AbortSignal.timeout(10000) });
  assert.equal(response.status, 200);
  assert.match(await response.text(), /<title>OpenHarness<\/title>/);
  console.log(`PASS: installed runtime ${manifest.version}, native modules, CLI, managed plugins, and branded loopback page`);
} finally {
  if (child?.pid) {
    const exited = new Promise((resolveExit) => child.exitCode !== null ? resolveExit() : child.once("exit", resolveExit));
    if (process.platform === "win32") {
      spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true, stdio: "ignore", timeout: 10000 });
    } else {
      try { process.kill(-child.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
    }
    await exited;
  }
  await rm(home, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
