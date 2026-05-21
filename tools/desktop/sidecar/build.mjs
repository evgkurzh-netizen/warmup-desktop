// Stage the capture sidecar as Tauri resources: a per-platform Node.js
// runtime + the bundled capture.cjs + the production node_modules tree.
//
// Replaces the previous @yao-pkg/pkg single-binary build, which couldn't
// package patchright (patchright-core's coreBundle.js performs dynamic
// requires that don't survive pkg's snapshot filesystem). See v0.1.35
// MODULE_NOT_FOUND on patchright-core/lib/coreBundle.js for the failure.
//
// Layout produced under src-tauri/resources/sidecar/:
//   node                  (or node.exe on win)
//   capture.cjs           (esbuild bundle, built by `npm run build:js`)
//   node_modules/         (production-only deps after `npm prune`)
//
// At runtime the Rust side spawns:
//   <resource_dir>/resources/sidecar/node[.exe]  <…>/capture.cjs <argv>
//
// Usage: node ./build.mjs [--target=darwin-arm64|darwin-x64|win-x64]
//        CI invokes this once per matrix job with the correct --target.

import { execFileSync, execSync } from "node:child_process";
import {
  copyFileSync,
  cpSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  rmSync,
  statSync,
} from "node:fs";
import { resolve, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";

const __dirname = dirname(fileURLToPath(import.meta.url));
const sidecarDir = __dirname;
const stageDir = resolve(__dirname, "..", "src-tauri", "resources", "sidecar");

const args = Object.fromEntries(
  process.argv.slice(2).map((a) => {
    const [k, v] = a.replace(/^--/, "").split("=");
    return [k, v ?? "true"];
  }),
);

// Pinned Node version. Node 22 is required on macOS 15 / Apple Silicon —
// Node 20's V8 hits "Fatal process OOM in Failed to reserve virtual memory
// for CodeRange" because of macOS 15's new VM layout. Fixed in V8 12.x
// (Node 22). Keep Windows on the same major for parity.
const NODE_VERSION = "22.10.0";

const TARGETS = {
  "darwin-arm64": {
    archive: `node-v${NODE_VERSION}-darwin-arm64.tar.gz`,
    binPathInArchive: `node-v${NODE_VERSION}-darwin-arm64/bin/node`,
    outBin: "node",
    isWindows: false,
  },
  "darwin-x64": {
    archive: `node-v${NODE_VERSION}-darwin-x64.tar.gz`,
    binPathInArchive: `node-v${NODE_VERSION}-darwin-x64/bin/node`,
    outBin: "node",
    isWindows: false,
  },
  "win-x64": {
    archive: `node-v${NODE_VERSION}-win-x64.zip`,
    binPathInArchive: `node-v${NODE_VERSION}-win-x64/node.exe`,
    outBin: "node.exe",
    isWindows: true,
  },
};

const hostTarget = (() => {
  if (process.platform === "darwin") {
    return process.arch === "arm64" ? "darwin-arm64" : "darwin-x64";
  }
  if (process.platform === "win32") return "win-x64";
  return null;
})();

const targetKey = args.target ?? hostTarget;
const target = TARGETS[targetKey];
if (!target) {
  console.error(`Unknown or unsupported target: ${targetKey}`);
  process.exit(1);
}

const captureJs = resolve(sidecarDir, "dist", "capture.cjs");
if (!existsSync(captureJs)) {
  console.error(`Run 'npm run build:js' first (missing ${captureJs}).`);
  process.exit(1);
}

const nodeModulesDir = resolve(sidecarDir, "node_modules");
if (!existsSync(nodeModulesDir)) {
  console.error(`Run 'npm install' first (missing ${nodeModulesDir}).`);
  process.exit(1);
}

// --- 1. Wipe + recreate stage dir ---
console.log(`[sidecar] staging into ${stageDir}`);
rmSync(stageDir, { recursive: true, force: true });
mkdirSync(stageDir, { recursive: true });

// --- 2. Download + extract Node ---
const cacheDir = join(tmpdir(), "warmup-node-cache");
mkdirSync(cacheDir, { recursive: true });
const archivePath = join(cacheDir, target.archive);
if (!existsSync(archivePath)) {
  const url = `https://nodejs.org/dist/v${NODE_VERSION}/${target.archive}`;
  console.log(`[sidecar] downloading ${url}`);
  const resp = await fetch(url);
  if (!resp.ok || !resp.body) {
    console.error(`Failed to download Node: HTTP ${resp.status}`);
    process.exit(1);
  }
  await pipeline(Readable.fromWeb(resp.body), createWriteStream(archivePath));
  console.log(`[sidecar] saved ${statSync(archivePath).size} bytes`);
} else {
  console.log(`[sidecar] using cached ${archivePath}`);
}

const extractDir = join(cacheDir, `extract-${targetKey}`);
rmSync(extractDir, { recursive: true, force: true });
mkdirSync(extractDir, { recursive: true });

if (target.isWindows) {
  // Cross-platform unzip. On Windows runners PowerShell's Expand-Archive
  // works; on macOS/linux we use `unzip`.
  if (process.platform === "win32") {
    execFileSync(
      "powershell",
      [
        "-NoProfile",
        "-Command",
        `Expand-Archive -Path '${archivePath}' -DestinationPath '${extractDir}' -Force`,
      ],
      { stdio: "inherit" },
    );
  } else {
    execFileSync("unzip", ["-q", archivePath, "-d", extractDir], {
      stdio: "inherit",
    });
  }
} else {
  execFileSync("tar", ["-xzf", archivePath, "-C", extractDir], {
    stdio: "inherit",
  });
}

const srcBin = join(extractDir, target.binPathInArchive);
const dstBin = join(stageDir, target.outBin);
copyFileSync(srcBin, dstBin);
if (!target.isWindows) {
  execFileSync("chmod", ["+x", dstBin]);
}

// --- 3. Re-sign node on macOS with JIT entitlements ---
// Same reason as before: macOS 15 / Apple Silicon refuses to launch a
// modified Mach-O without a valid signature, and V8 needs the
// allow-jit + allow-unsigned-executable-memory entitlements to allocate
// MAP_JIT pages. Node ships with these entitlements upstream, but
// copying the binary out of the tarball does NOT preserve them on
// Apple Silicon (the loader still wants the signature to match the
// embedded entitlements blob, which it does here, but we re-sign anyway
// in case of future tarball signing changes).
if (targetKey === "darwin-arm64" || targetKey === "darwin-x64") {
  const entitlements = resolve(sidecarDir, "entitlements.plist");
  console.log(`[sidecar] re-signing ${dstBin} with JIT entitlements …`);
  execFileSync(
    "codesign",
    [
      "--force",
      "--sign",
      "-",
      "--entitlements",
      entitlements,
      "--options",
      "runtime",
      dstBin,
    ],
    { stdio: "inherit" },
  );
}

// --- 4. Copy capture.cjs ---
copyFileSync(captureJs, join(stageDir, "capture.cjs"));

// --- 5. Copy node_modules (production only) ---
// Prune dev deps in-place first so we don't ship esbuild / pkg / etc.
console.log(`[sidecar] pruning dev dependencies …`);
execSync("npm prune --omit=dev", { cwd: sidecarDir, stdio: "inherit" });

console.log(`[sidecar] copying node_modules → ${stageDir}/node_modules`);
cpSync(nodeModulesDir, join(stageDir, "node_modules"), {
  recursive: true,
  dereference: false,
});

console.log(`[sidecar] done. resources staged at ${stageDir}`);
