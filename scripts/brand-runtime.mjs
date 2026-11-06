#!/usr/bin/env node

import {
  copyFileSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const packagesRoot = resolve(
  projectRoot,
  "src-tauri/runtime/dsh/node_modules/@deepseek-ai",
);
const frontendDist = resolve(packagesRoot, "dsh-web-frontend/dist");

function read(relativePath) {
  return readFileSync(resolve(packagesRoot, relativePath), "utf8");
}

function write(relativePath, content) {
  writeFileSync(resolve(packagesRoot, relativePath), content);
}

function replaceOnce(content, search, replacement, label) {
  if (content.includes(replacement)) return content;
  const first = content.indexOf(search);
  if (first < 0 || content.indexOf(search, first + search.length) >= 0) {
    throw new Error(`OpenHarness runtime branding: expected one ${label}`);
  }
  return content.slice(0, first) + replacement + content.slice(first + search.length);
}

// Since 0.2 the web frontend ships one minified bundle instead of readable
// package sources. Resolve the minified local name from the exported symbol map
// so branding survives a rebuild that renames locals.
function replaceBundledExport(content, exportName, replacement) {
  const exported = [...content.matchAll(new RegExp(`\\b${exportName}:([A-Za-z_$][\\w$]*)`, "g"))];
  if (exported.length !== 1) {
    throw new Error(
      `OpenHarness runtime branding: expected one ${exportName} export, found ${exported.length}`,
    );
  }
  const localName = exported[0][1];
  const marker = `function ${localName}(`;
  const startIndex = content.indexOf(marker);
  const nextIndex = content.indexOf("function ", startIndex + marker.length);
  if (
    startIndex < 0 ||
    nextIndex < 0 ||
    content.indexOf(marker, startIndex + 1) >= 0 ||
    !content.slice(startIndex, nextIndex).trimEnd().endsWith("}")
  ) {
    throw new Error(`OpenHarness runtime branding: could not locate ${exportName} implementation`);
  }
  return content.slice(0, startIndex) + replacement(localName) + content.slice(nextIndex);
}

function replaceLocalizedString(content, key, marker, replacement, label) {
  const branded = `${key}: ${JSON.stringify(replacement)}`;
  if (content.includes(branded)) return content;
  const pattern = new RegExp(`${key}: "((?:[^"\\\\]|\\\\.)*)"`, "g");
  const matches = [...content.matchAll(pattern)].filter((match) => match[1].includes(marker));
  if (matches.length !== 1) {
    throw new Error(
      `OpenHarness runtime branding: expected one ${label}, found ${matches.length}`,
    );
  }
  const match = matches[0];
  return content.slice(0, match.index) + branded + content.slice(match.index + match[0].length);
}

const openHarnessTheme = `    <meta name="theme-color" content="#0f1115" />
    <style id="openharness-theme">
      :root {
        --dsw-static-deepseek-50: #fff1ee;
        --dsw-static-deepseek-100: #ffe0d9;
        --dsw-static-deepseek-200: #ffc1b6;
        --dsw-static-deepseek-300: #ff9484;
        --dsw-static-deepseek-400: #ef5b49;
        --dsw-static-deepseek-450: #e64d3c;
        --dsw-static-deepseek-500: #d23b2d;
        --dsw-static-deepseek-600: #b72d22;
        --dsw-static-deepseek-700: #93251d;
        --dsw-static-deepseek-800: #72211c;
        --dsw-static-deepseek-900: #481815;
      }
    </style>`;

const indexPath = resolve(frontendDist, "index.html");
let indexHtml = readFileSync(indexPath, "utf8");
const entryBundleMatch = /<script[^>]*type="module"[^>]*src="\.\/(assets\/[^"]+\.js)"/.exec(indexHtml);
if (!entryBundleMatch) {
  throw new Error("OpenHarness runtime branding: missing frontend entry bundle");
}
const entryBundlePath = resolve(frontendDist, entryBundleMatch[1]);
let entryBundle = readFileSync(entryBundlePath, "utf8");
entryBundle = replaceBundledExport(
  entryBundle,
  "FishLogo",
  (name) =>
    `function ${name}({size:e=24,className:t}){return p.jsx("img",{src:"/openharness-icon.png",width:e,height:e,className:t,alt:"","aria-hidden":"true",style:{borderRadius:Math.max(3,e*.2),objectFit:"cover"}})}`,
);
entryBundle = replaceBundledExport(
  entryBundle,
  "BrandWordmark",
  (name) =>
    `function ${name}({size:e=24,className:t,includeMark:r=!0}){return p.jsxs("span",{className:t,"aria-label":"OpenHarness",style:{display:"inline-flex",alignItems:"center",gap:8,height:e,color:"currentColor",fontSize:Math.max(14,e*.72),fontWeight:650,whiteSpace:"nowrap"},children:[r?p.jsx("img",{src:"/openharness-icon.png",width:e,height:e,alt:"","aria-hidden":"true",style:{borderRadius:Math.max(3,e*.2),objectFit:"cover"}}):null,p.jsx("span",{children:"OpenHarness"})]})}`,
);
writeFileSync(entryBundlePath, entryBundle);

const openHarnessChineseWelcome = "OpenHarness 是由 MicroSpotlight 独立开发的桌面应用。内置的 DeepSeek Harness 上游运行时组件单独采用 MIT License；OpenHarness 原创代码采用 Apache License 2.0。";
const openHarnessEnglishWelcome = "OpenHarness is an independent desktop app developed by MicroSpotlight. Its bundled DeepSeek Harness upstream runtime is separately licensed under MIT; OpenHarness original code is licensed under Apache License 2.0.";

let settings = read("dsh-client-ui-settings-models/lib/client.js");
settings = replaceLocalizedString(
  settings,
  "welcomeBody",
  "DeepSeek Harness 0.2 is still in preview",
  openHarnessEnglishWelcome,
  "English welcome notice",
);
settings = replaceLocalizedString(
  settings,
  "welcomeBody",
  "目前的 0.2 版本",
  openHarnessChineseWelcome,
  "Chinese welcome notice",
);
write("dsh-client-ui-settings-models/lib/client.js", settings);

let webApp = read("dsh-web-app/lib/index.js");
webApp = webApp.replaceAll(
  "DeepSeek Harness Web GUI",
  "desktop interface provided by OpenHarness",
);
write("dsh-web-app/lib/index.js", webApp);

let webStartup = read("dsh-web-app/lib/startup.js");
webStartup = webStartup.replaceAll(
  "Serve the DeepSeek Harness browser UI.",
  "Serve the OpenHarness interface.",
);
write("dsh-web-app/lib/startup.js", webStartup);

let staticHost = read("dsh-host-frontend-static/lib/index.js");
staticHost = replaceOnce(
  staticHost,
  '\t".map": "application/json",\n\t".webmanifest": "application/manifest+json"',
  '\t".map": "application/json",\n\t".png": "image/png",\n\t".webmanifest": "application/manifest+json"',
  "PNG MIME type",
);
write("dsh-host-frontend-static/lib/index.js", staticHost);

indexHtml = indexHtml
  .replace(
    '<link rel="icon" type="image/svg+xml" href="./favicon-dark.svg" media="(prefers-color-scheme: dark)" />',
    '<link rel="icon" type="image/png" href="/openharness-icon.png" />',
  )
  .replace(
    '<link rel="icon" type="image/svg+xml" href="./favicon.svg" media="(prefers-color-scheme: light)" />',
    "",
  )
  .replace(
    '<link rel="apple-touch-icon" href="./apple-touch-icon.png" />',
    '<link rel="apple-touch-icon" href="/openharness-icon.png" />',
  )
  .replace(
    '<meta name="apple-mobile-web-app-title" content="DSH" />',
    '<meta name="apple-mobile-web-app-title" content="OpenHarness" />',
  )
  .replace("<title>DeepSeek Harness</title>", "<title>OpenHarness</title>");
if (
  !indexHtml.includes("<title>OpenHarness</title>") ||
  !indexHtml.includes('href="/openharness-icon.png"') ||
  !indexHtml.includes('<meta name="apple-mobile-web-app-title" content="OpenHarness" />')
) {
  throw new Error("OpenHarness runtime branding: could not brand the frontend document");
}
indexHtml = replaceOnce(
  indexHtml,
  "  </head>",
  `${openHarnessTheme}\n  </head>`,
  "HTML theme",
);
writeFileSync(indexPath, indexHtml);

const manifestPath = resolve(frontendDist, "manifest.webmanifest");
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
manifest.name = "OpenHarness";
manifest.short_name = "OpenHarness";
manifest.theme_color = "#0f1115";
manifest.background_color = "#f9fafb";
manifest.icons = [
  {
    src: "/openharness-icon.png",
    sizes: "1254x1254",
    type: "image/png",
    purpose: "any",
  },
];
writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

copyFileSync(
  resolve(projectRoot, "assets/openharness-icon.png"),
  resolve(frontendDist, "openharness-icon.png"),
);

// DSH resolves profile plugins through its installation dependency closure.
// Declare OpenHarness-owned plugins there so its standard fallback links the
// packages without modifying the user's profile manifest.
const dshManifest = JSON.parse(read("dsh/package.json"));
dshManifest.dependencies["@openharness/native-bridge"] = "0.1.0";
dshManifest.dependencies["@microspotlight/openharness-find-plugin"] = "0.1.0";
write("dsh/package.json", `${JSON.stringify(dshManifest, null, 2)}\n`);

console.log(">> OpenHarness runtime branding applied");
