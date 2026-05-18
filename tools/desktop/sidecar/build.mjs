// Build a single-file standalone binary for the capture sidecar using
// @yao-pkg/pkg, then copy it into the Tauri externalBin staging directory
// with the platform-specific triple name Tauri expects.
//
// Usage: node ./build.mjs [--target=darwin-arm64|darwin-x64|win-x64]
//
// If --target is omitted we infer from the host. CI invokes this once per
// matrix job with the correct --target.

import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, existsSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoSidecar = __dirname;
const tauriBinaries = resolve(__dirname, "..", "src-tauri", "binaries");

const args = Object.fromEntries(
  process.argv.slice(2).map((a) => {
    const [k, v] = a.replace(/^--/, "").split("=");
    return [k, v ?? "true"];
  }),
);

// pkg target string → Tauri target-triple.
const TARGETS = {
  "darwin-arm64": { pkg: "node20-macos-arm64", triple: "aarch64-apple-darwin" },
  "darwin-x64": { pkg: "node20-macos-x64", triple: "x86_64-apple-darwin" },
  "win-x64": { pkg: "node20-win-x64", triple: "x86_64-pc-windows-msvc" },
  "linux-x64": { pkg: "node20-linux-x64", triple: "x86_64-unknown-linux-gnu" },
};

const hostTarget = (() => {
  if (process.platform === "darwin") {
    return process.arch === "arm64" ? "darwin-arm64" : "darwin-x64";
  }
  if (process.platform === "win32") return "win-x64";
  return "linux-x64";
})();

const targetKey = args.target ?? hostTarget;
const target = TARGETS[targetKey];
if (!target) {
  console.error(`Unknown target: ${targetKey}`);
  process.exit(1);
}

mkdirSync(tauriBinaries, { recursive: true });

const entry = resolve(repoSidecar, "dist", "capture.cjs");
if (!existsSync(entry)) {
  console.error(`Run 'pnpm run build:js' first (missing ${entry}).`);
  process.exit(1);
}

const outName = targetKey === "win-x64" ? "capture.exe" : "capture";
const outPath = resolve(repoSidecar, "dist", outName);

console.log(`[sidecar] pkg target=${target.pkg} → ${outPath}`);
execFileSync(
  "npx",
  [
    "--yes",
    "@yao-pkg/pkg",
    entry,
    "--target",
    target.pkg,
    "--output",
    outPath,
  ],
  { stdio: "inherit", cwd: repoSidecar },
);

const ext = targetKey === "win-x64" ? ".exe" : "";
const finalPath = resolve(tauriBinaries, `capture-${target.triple}${ext}`);
copyFileSync(outPath, finalPath);
console.log(`[sidecar] staged at ${finalPath}`);
